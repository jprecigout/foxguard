import { useEffect, useRef, useState } from "react";

import { ApiError, fetchCameras } from "../api";
import type { CameraInfo } from "../api";
import { controlFrame } from "../frames";
import type { OpenFrame } from "../frames";
import { navigate } from "../route";
import { LiveStream } from "./LiveStream";

// Mosaïque des directs : toutes les caméras d'un coup d'œil, ou une seule en
// grand.
//
// Chaque vignette ouvre SON flux, directement auprès de sa caméra (voir
// `LiveStream`). Le manager ne fait que signer les tickets : ajouter une
// caméra à la mosaïque ne lui coûte rien en bande passante.

/** Rafraîchissement de la liste des caméras : une nouvelle venue apparaît sans recharger. */
const CAMERAS_REFRESH_MS = 60_000;

/** Une caméra de la mosaïque. */
function LiveTile({
  camera,
  focused,
  onOpenFrame,
}: {
  camera: CameraInfo;
  focused: boolean;
  onOpenFrame: (frame: OpenFrame) => void;
}) {
  const tile = useRef<HTMLElement>(null);

  return (
    <section className="live-tile" ref={tile}>
      <header>
        <h2>{camera.name}</h2>

        {focused ? (
          <button
            onClick={() => void tile.current?.requestFullscreen()}
            title="Afficher en plein écran"
          >
            ⛶ Plein écran
          </button>
        ) : (
          camera.stream_url && (
            <button
              onClick={() => navigate({ view: "live", camera: camera.name })}
              title={`Afficher ${camera.name} en grand`}
            >
              ⤢ Agrandir
            </button>
          )
        )}

        {/* L'interrupteur reste servi par la caméra (voir
            `CameraFrameDialog`) : le ticket de direct ne permet que de
            regarder, et c'est voulu. */}
        {camera.control_url && (
          <button
            className="control"
            onClick={() => onOpenFrame(controlFrame(camera.control_url!, camera.name))}
            title={`Activer ou couper la surveillance de ${camera.name}`}
          >
            🛡 Surveillance
          </button>
        )}
      </header>

      {camera.stream_url ? (
        <LiveStream streamUrl={camera.stream_url} camera={camera.name} />
      ) : (
        // Une caméra sans direct reste dans la mosaïque : la voir absente
        // laisserait croire qu'elle n'existe pas.
        <div className="stream">
          <p className="stream-status">
            Direct indisponible : la caméra n'a pas déclaré son URL publique, ou les
            tickets de direct ne sont pas configurés sur le manager.
          </p>
        </div>
      )}
    </section>
  );
}

export function LiveWall({
  focusedCamera,
  onOpenFrame,
}: {
  /** Caméra affichée seule, en grand ; `null` pour la mosaïque complète. */
  focusedCamera: string | null;
  onOpenFrame: (frame: OpenFrame) => void;
}) {
  const [cameras, setCameras] = useState<CameraInfo[] | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    const controller = new AbortController();

    const load = async () => {
      try {
        const known = await fetchCameras(controller.signal);
        setCameras([...known].sort((a, b) => a.name.localeCompare(b.name, "fr")));
        setError(null);
      } catch (cause) {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        setError(cause instanceof ApiError ? cause.message : "Erreur inattendue");
      }
    };

    void load();
    const timer = window.setInterval(() => void load(), CAMERAS_REFRESH_MS);

    return () => {
      window.clearInterval(timer);
      controller.abort();
    };
  }, []);

  const shown =
    focusedCamera === null ? cameras : cameras?.filter((camera) => camera.name === focusedCamera);

  return (
    <>
      {focusedCamera !== null && (
        <div className="daybar">
          <button onClick={() => navigate({ view: "live", camera: null })}>
            ← Toutes les caméras
          </button>
          <span className="camera-name">{focusedCamera}</span>
        </div>
      )}

      {error && (
        <div className="banner error">
          {error}. Vérifiez que le manager tourne et que la base est joignable.
        </div>
      )}

      {!error && cameras === null && <p className="placeholder">Chargement…</p>}

      {cameras !== null && cameras.length === 0 && (
        <p className="placeholder">
          Aucune caméra connue. Les caméras apparaissent ici dès leur première détection
          publiée sur MQTT.
        </p>
      )}

      {focusedCamera !== null && shown?.length === 0 && cameras !== null && cameras.length > 0 && (
        <p className="placeholder">Caméra « {focusedCamera} » inconnue.</p>
      )}

      <div className={`live-wall${focusedCamera !== null ? " focused" : ""}`}>
        {shown?.map((camera) => (
          // La clé porte l'URL du flux : si elle change (caméra qui change
          // d'adresse), la vignette repart de zéro au lieu de garder un
          // décodeur configuré pour l'ancienne connexion.
          <LiveTile
            key={`${camera.name}|${camera.stream_url ?? ""}`}
            camera={camera}
            focused={focusedCamera !== null}
            onOpenFrame={onOpenFrame}
          />
        ))}
      </div>
    </>
  );
}
