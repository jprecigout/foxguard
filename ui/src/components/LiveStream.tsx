import { useEffect, useRef, useState } from "react";

import { ApiError, fetchStreamTicket } from "../api";

// Le direct d'une caméra, décodé ici même.
//
// # Le chemin de la vidéo
//
//  1. on demande au MANAGER un ticket de visionnage (`stream_url`) : il ne
//     connaît pas le jeton d'API de la caméra, mais il partage avec elle un
//     secret qui lui permet de signer un ticket court, en lecture seule
//     (voir `foxguard_protocol::stream_ticket`) ;
//  2. on ouvre avec lui le WebSocket de la CAMÉRA, directement : la vidéo ne
//     transite pas par le manager. Un WebSocket n'est pas soumis à la
//     politique de même origine, c'est le ticket qui fait office de contrôle ;
//  3. la caméra annonce son codec (`{"type":"config","codec":…}`), puis
//     envoie chaque unité d'accès H.264 précédée d'un en-tête de 9 octets
//     (`[u8 image clé][u64 horodatage]`, voir `crates/camera/src/api.rs`) ;
//  4. le navigateur décode avec WebCodecs (`VideoDecoder`) vers un canevas.
//
// C'est le protocole de `crates/camera/static/live.html`, réécrit en
// composant : les deux doivent évoluer ensemble.
//
// # Ce qu'un flux coûte
//
// S'abonner au WebSocket est ce qui DÉMARRE l'encodeur H.264 de la caméra, le
// poste le plus lourd de son Raspberry Pi. Le composant ferme donc la
// connexion dès que l'onglet est masqué OU que la vignette sort de l'écran
// (mosaïque défilée), et la rouvre au retour : une mosaïque oubliée dans un
// onglet de fond ne doit pas faire chauffer toutes les caméras de la maison.
//
// Côté navigateur, le décodage peut prendre du retard (machine lente, beaucoup
// de vignettes). Plutôt que de laisser la file du décodeur grossir — et le
// « direct » dériver de plusieurs secondes —, on jette les images jusqu'à la
// prochaine image clé : l'affichage saute, mais il reste en direct.

/** Délai avant reconnexion, après une coupure. */
const RETRY_MS = 2000;

/** Taille de l'en-tête binaire précédant chaque unité d'accès. */
const FRAME_HEADER = 1 + 8;

/**
 * Images en attente dans le décodeur au-delà desquelles on rattrape le direct
 * (voir l'en-tête du fichier). À 12 images par seconde, une demi-seconde.
 */
const MAX_DECODE_QUEUE = 6;

/**
 * Marge autour de l'écran dans laquelle une vignette est considérée visible :
 * le flux démarre un peu avant qu'elle n'apparaisse au défilement.
 */
const ON_SCREEN_MARGIN = "200px";

type StreamStatus =
  | { kind: "connecting" }
  | { kind: "live" }
  | { kind: "paused" }
  | { kind: "retrying"; reason: string }
  /** Panne sans issue : réessayer ne changerait rien. */
  | { kind: "failed"; reason: string };

/** Vrai si ce navigateur sait décoder du H.264 lui-même. */
function supportsWebCodecs(): boolean {
  return typeof window.VideoDecoder === "function" && typeof window.EncodedVideoChunk === "function";
}

/** Vrai tant que l'onglet est affiché. */
function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => document.visibilityState === "visible");

  useEffect(() => {
    const onChange = () => setVisible(document.visibilityState === "visible");
    document.addEventListener("visibilitychange", onChange);
    return () => document.removeEventListener("visibilitychange", onChange);
  }, []);

  return visible;
}

/** Vrai tant que l'élément est à l'écran (ou presque, voir `ON_SCREEN_MARGIN`). */
function useOnScreen(ref: React.RefObject<HTMLElement | null>): boolean {
  // Vrai par défaut : sans `IntersectionObserver`, mieux vaut un flux de trop
  // qu'une vignette qui ne démarre jamais.
  const [onScreen, setOnScreen] = useState(true);

  useEffect(() => {
    const element = ref.current;
    if (!element || typeof IntersectionObserver !== "function") return;

    const observer = new IntersectionObserver(
      (entries) => setOnScreen(entries.some((entry) => entry.isIntersecting)),
      { rootMargin: ON_SCREEN_MARGIN },
    );
    observer.observe(element);
    return () => observer.disconnect();
  }, [ref]);

  return onScreen;
}

/** Texte affiché par-dessus le canevas, ou `null` quand l'image suffit. */
function statusText(status: StreamStatus): string | null {
  switch (status.kind) {
    case "connecting":
      return "Connexion au flux…";
    case "live":
      return null;
    case "paused":
      return "En pause";
    case "retrying":
      return `${status.reason} — nouvelle tentative…`;
    case "failed":
      return status.reason;
  }
}

export function LiveStream({ streamUrl, camera }: { streamUrl: string; camera: string }) {
  const containerRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const [status, setStatus] = useState<StreamStatus>({ kind: "connecting" });
  const pageVisible = usePageVisible();
  const onScreen = useOnScreen(containerRef);
  const visible = pageVisible && onScreen;

  useEffect(() => {
    // Il n'y a qu'un format : sans WebCodecs, aucun repli possible. On le
    // DIT, plutôt que de laisser un cadre noir faire croire à une caméra en
    // panne.
    if (!supportsWebCodecs()) {
      setStatus({
        kind: "failed",
        reason: "Ce navigateur ne sait pas décoder le H.264 (WebCodecs indisponible).",
      });
      return;
    }

    if (!visible) {
      setStatus({ kind: "paused" });
      return;
    }

    let stopped = false;
    let socket: WebSocket | null = null;
    let decoder: VideoDecoder | null = null;
    let retryTimer: number | undefined;
    const controller = new AbortController();

    const closeDecoder = () => {
      if (decoder && decoder.state !== "closed") decoder.close();
      decoder = null;
    };

    const retry = (reason: string) => {
      closeDecoder();
      if (stopped) return;

      setStatus({ kind: "retrying", reason });
      retryTimer = window.setTimeout(() => void connect(), RETRY_MS);
    };

    const connect = async () => {
      setStatus((current) => (current.kind === "retrying" ? current : { kind: "connecting" }));

      // Un ticket NEUF à chaque tentative : celui de la connexion précédente
      // a sans doute expiré depuis.
      let url: string;
      try {
        url = (await fetchStreamTicket(streamUrl, controller.signal)).url;
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        retry(cause instanceof ApiError ? cause.message : "Ticket de direct indisponible");
        return;
      }

      if (stopped) return;

      let ws: WebSocket;
      try {
        ws = new WebSocket(url);
      } catch {
        // Le cas typique : une interface servie en HTTPS vers une caméra en
        // `ws://`. Le navigateur refuse ce contenu mixte, et réessayer n'y
        // changera rien.
        setStatus({
          kind: "failed",
          reason:
            "Le navigateur refuse d'ouvrir le flux de la caméra (une page HTTPS ne peut pas joindre une caméra en HTTP).",
        });
        return;
      }

      ws.binaryType = "arraybuffer";
      socket = ws;

      // L'état « en direct » n'est signalé qu'à la première image : le
      // repasser à chaque frame ferait un rendu React par image.
      let showing = false;

      // Vrai quand on a jeté des images pour rattraper le direct : plus rien
      // n'est décodable avant la prochaine image clé.
      let awaitingKeyframe = false;

      ws.onmessage = (event: MessageEvent) => {
        if (typeof event.data === "string") {
          let announcement: { type?: string; codec?: string };
          try {
            announcement = JSON.parse(event.data);
          } catch {
            ws.close();
            return;
          }

          if (announcement.type !== "config" || !announcement.codec) return;

          closeDecoder();
          decoder = new VideoDecoder({
            output: (frame) => {
              const canvas = canvasRef.current;

              if (canvas) {
                if (canvas.width !== frame.displayWidth || canvas.height !== frame.displayHeight) {
                  canvas.width = frame.displayWidth;
                  canvas.height = frame.displayHeight;
                }
                canvas.getContext("2d")?.drawImage(frame, 0, 0);
              }

              // Obligatoire : une VideoFrame retient de la mémoire graphique
              // que le ramasse-miettes ne libère pas.
              frame.close();

              if (!showing) {
                showing = true;
                setStatus({ kind: "live" });
              }
            },
            // Un flux momentanément abîmé fait échouer le décodeur : on
            // ferme, et la reconnexion repart sur l'image clé suivante.
            error: () => ws.close(),
          });
          decoder.configure({ codec: announcement.codec, optimizeForLatency: true });
          return;
        }

        if (!(event.data instanceof ArrayBuffer) || decoder?.state !== "configured") return;
        if (event.data.byteLength <= FRAME_HEADER) return;

        const view = new DataView(event.data);
        const keyframe = view.getUint8(0) === 1;

        if (decoder.decodeQueueSize > MAX_DECODE_QUEUE) awaitingKeyframe = true;
        if (awaitingKeyframe && !keyframe) return;
        awaitingKeyframe = false;

        try {
          decoder.decode(
            new EncodedVideoChunk({
              type: keyframe ? "key" : "delta",
              timestamp: Number(view.getBigUint64(1)),
              data: new Uint8Array(event.data, FRAME_HEADER),
            }),
          );
        } catch {
          ws.close();
        }
      };

      ws.onclose = () => {
        if (socket === ws) socket = null;
        retry(showing ? "Flux interrompu" : "Caméra injoignable");
      };

      ws.onerror = () => ws.close();
    };

    void connect();

    return () => {
      stopped = true;
      controller.abort();
      window.clearTimeout(retryTimer);

      if (socket) {
        // Fermeture VOULUE : pas de reconnexion derrière.
        socket.onclose = null;
        socket.close();
      }

      closeDecoder();
    };
  }, [streamUrl, visible]);

  const text = statusText(status);

  return (
    <div className="stream" ref={containerRef}>
      <canvas
        ref={canvasRef}
        aria-label={`Direct de ${camera}`}
        // Masqué tant qu'il n'y a rien à montrer, plutôt que de laisser la
        // dernière image d'une connexion perdue passer pour du direct.
        hidden={status.kind !== "live"}
      />

      {text && <p className={`stream-status ${status.kind}`}>{text}</p>}

      {status.kind === "live" && (
        <span className="stream-badge" aria-hidden="true">
          <span className="dot" /> Direct
        </span>
      )}
    </div>
  );
}
