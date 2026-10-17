import { useCallback, useEffect, useMemo, useRef, useState } from "react";

import { ApiError, fetchCameras, fetchFaces } from "../api";
import type { CameraInfo, FaceRecord } from "../api";
import { LiveStream } from "./LiveStream";

// Enregistrement des visages de la reconnaissance faciale.
//
// # Le chemin d'une photo
//
//  1. on choisit la caméra devant laquelle se placer : la vue ouvre son
//     WebSocket d'ENRÔLEMENT, avec un ticket que le manager signe pour cette
//     seule portée (voir `enrollment_url` dans `CameraInfo`) — le direct,
//     plus la capture et la suppression de photos, et rien d'autre ;
//  2. « Capturer » envoie `capture_reference` sur ce WebSocket. La CAMÉRA
//     prend la frame suivante, vérifie qu'un visage y figure, l'écrit dans
//     son dossier et répond (`{"type":"capture","ok":…}`) — exactement comme
//     depuis son interface embarquée ;
//  3. elle publie la photo, signée, sur le broker MQTT : toutes les autres
//     caméras la reçoivent et l'ajoutent à leur base, et le manager l'ajoute
//     à la liste ci-dessous (voir `foxguard_protocol::faces`).
//
// Le manager ne touche donc à aucune photo : il signe l'autorisation, et
// affiche ce que les caméras se sont partagé.

/** Rafraîchissement de la liste des photos. */
const FACES_REFRESH_MS = 5_000;

/**
 * Délai avant de relire la liste après une capture ou une suppression : le
 * temps que la photo fasse le tour caméra → broker → manager.
 */
const AFTER_CHANGE_REFRESH_MS = 1_000;

/** Ce que la caméra accepte comme nom (voir `validate_name` côté caméra). */
const NAME_PATTERN = /^[A-Za-z0-9_-]{1,64}$/;

type Status = { kind: "info" | "ok" | "error"; text: string };

/** Réponse de la caméra à une commande de photo (voir `CommandReply` côté Rust). */
interface CommandReply {
  type: "capture" | "delete";
  ok: boolean;
  file?: string;
  error?: string;
}

function isCommandReply(message: Record<string, unknown>): message is CommandReply & Record<string, unknown> {
  return (message.type === "capture" || message.type === "delete") && typeof message.ok === "boolean";
}

/** Date et heure d'une capture, ex. « 17 oct. 2026, 09:36 ». */
function formatCapturedAt(timestamp: string): string {
  return new Date(timestamp).toLocaleString("fr-FR", {
    day: "numeric",
    month: "short",
    year: "numeric",
    hour: "2-digit",
    minute: "2-digit",
  });
}

/** Photos regroupées par personne, dans l'ordre reçu (déjà trié par le manager). */
function groupByPerson(faces: FaceRecord[]): [string, FaceRecord[]][] {
  const people = new Map<string, FaceRecord[]>();

  for (const face of faces) {
    const photos = people.get(face.name);
    if (photos) photos.push(face);
    else people.set(face.name, [face]);
  }

  return [...people];
}

export function FacesView() {
  const [cameras, setCameras] = useState<CameraInfo[] | null>(null);
  const [camerasError, setCamerasError] = useState<string | null>(null);
  const [selected, setSelected] = useState<string | null>(null);

  const [faces, setFaces] = useState<FaceRecord[] | null>(null);
  const [facesError, setFacesError] = useState<string | null>(null);

  const [name, setName] = useState("");
  const [status, setStatus] = useState<Status | null>(null);
  const [capturing, setCapturing] = useState(false);
  const [deleting, setDeleting] = useState<ReadonlySet<string>>(new Set());

  // Le WebSocket d'enrôlement de la caméra choisie, ouvert par `LiveStream`.
  const socketRef = useRef<WebSocket | null>(null);
  const [connected, setConnected] = useState(false);
  const nameInput = useRef<HTMLInputElement>(null);

  const enrollable = useMemo(
    () =>
      (cameras ?? [])
        .filter((camera) => camera.enrollment_url !== null)
        .sort((a, b) => a.name.localeCompare(b.name, "fr")),
    [cameras],
  );
  const camera = enrollable.find((candidate) => candidate.name === selected) ?? enrollable[0] ?? null;

  // --- Chargements ---

  useEffect(() => {
    const controller = new AbortController();

    fetchCameras(controller.signal)
      .then((known) => {
        setCameras(known);
        setCamerasError(null);
      })
      .catch((cause: unknown) => {
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        setCamerasError(cause instanceof ApiError ? cause.message : "Erreur inattendue");
      });

    return () => controller.abort();
  }, []);

  const loadFaces = useCallback(async (signal?: AbortSignal) => {
    try {
      setFaces(await fetchFaces(signal));
      setFacesError(null);
    } catch (cause) {
      if (cause instanceof DOMException && cause.name === "AbortError") return;
      setFacesError(cause instanceof ApiError ? cause.message : "Erreur inattendue");
    }
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    void loadFaces(controller.signal);
    const timer = window.setInterval(() => void loadFaces(controller.signal), FACES_REFRESH_MS);

    return () => {
      window.clearInterval(timer);
      controller.abort();
    };
  }, [loadFaces]);

  const refreshSoon = useCallback(() => {
    window.setTimeout(() => void loadFaces(), AFTER_CHANGE_REFRESH_MS);
  }, [loadFaces]);

  // --- Connexion à la caméra ---

  const onSocket = useCallback((socket: WebSocket | null) => {
    socketRef.current = socket;
    setConnected(socket !== null);

    if (socket === null) {
      // Une réponse en attente n'arrivera plus sur une connexion fermée. Le
      // statut « info » est celui d'une capture en cours.
      setCapturing(false);
      setDeleting(new Set());
      setStatus((current) =>
        current?.kind === "info"
          ? { kind: "error", text: "Connexion à la caméra perdue avant sa réponse." }
          : current,
      );
    }
  }, []);

  const onMessage = useCallback(
    (message: Record<string, unknown>) => {
      if (!isCommandReply(message)) return;

      if (message.type === "capture") {
        setCapturing(false);

        if (message.ok) {
          setStatus({
            kind: "ok",
            text: "Photo enregistrée. Elle est partagée avec toutes les caméras — prenez-en d'autres sous des angles différents.",
          });
          refreshSoon();
        } else {
          setStatus({ kind: "error", text: message.error ?? "Capture refusée par la caméra." });
        }
        return;
      }

      const file = message.file;
      setDeleting((current) => {
        const next = new Set(current);
        if (file) next.delete(file);
        else next.clear();
        return next;
      });

      if (message.ok) {
        // Retirée tout de suite de la liste : la confirmation par le broker
        // suivra, au prochain rafraîchissement.
        setFaces((current) => current?.filter((face) => face.file !== file) ?? null);
        refreshSoon();
      } else {
        setStatus({ kind: "error", text: message.error ?? "Suppression refusée par la caméra." });
      }
    },
    [refreshSoon],
  );

  /** Envoie une commande sur le WebSocket d'enrôlement. Faux s'il est fermé. */
  const send = (command: Record<string, string>): boolean => {
    const socket = socketRef.current;
    if (!socket || socket.readyState !== WebSocket.OPEN) {
      setStatus({ kind: "error", text: "Pas de connexion à la caméra." });
      return false;
    }

    socket.send(JSON.stringify(command));
    return true;
  };

  // --- Actions ---

  const trimmedName = name.trim();
  const nameIsValid = NAME_PATTERN.test(trimmedName);

  const capture = (event: React.FormEvent) => {
    event.preventDefault();
    if (!camera || !nameIsValid || capturing) return;

    if (send({ command: "capture_reference", name: trimmedName })) {
      setCapturing(true);
      setStatus({ kind: "info", text: `Capture en cours devant « ${camera.name} »… ne bougez pas.` });
    }
  };

  const remove = (face: FaceRecord) => {
    if (!window.confirm(`Supprimer cette photo de « ${face.name} » sur toutes les caméras ?`)) return;

    if (send({ command: "delete_reference", file: face.file })) {
      setDeleting((current) => new Set(current).add(face.file));
    }
  };

  const addPhotoFor = (person: string) => {
    setName(person);
    setStatus(null);
    nameInput.current?.focus();
    nameInput.current?.scrollIntoView({ behavior: "smooth", block: "center" });
  };

  // --- Rendu ---

  const people = faces ? groupByPerson(faces) : [];

  return (
    <div className="faces-view">
      <section className="enroll">
        <header>
          <h2>Enregistrer un visage</h2>
          {enrollable.length > 0 && (
            <label className="camera-picker">
              Caméra
              <select
                value={camera?.name ?? ""}
                onChange={(event) => {
                  setSelected(event.target.value);
                  setStatus(null);
                }}
                disabled={capturing}
              >
                {enrollable.map((candidate) => (
                  <option key={candidate.name} value={candidate.name}>
                    {candidate.name}
                  </option>
                ))}
              </select>
            </label>
          )}
        </header>

        {camerasError && <div className="banner error">{camerasError}.</div>}

        {cameras !== null && enrollable.length === 0 && (
          <p className="empty">
            Aucune caméra ne permet l'enregistrement depuis cette interface. Il faut que la caméra
            déclare son URL publique (<code>[server] public_url</code>) et que le secret des tickets
            soit configuré des deux côtés (<code>[stream] ticket_secret</code> sur le manager,{" "}
            <code>[server] stream_ticket_secret</code> sur les caméras).
          </p>
        )}

        {camera?.enrollment_url && (
          <div className="enroll-body">
            {/* La clé porte la caméra : en changer ferme la connexion
                précédente et en ouvre une neuve, avec son propre ticket. */}
            <LiveStream
              key={`${camera.name}|${camera.enrollment_url}`}
              streamUrl={camera.enrollment_url}
              camera={camera.name}
              onSocket={onSocket}
              onMessage={onMessage}
            />

            <form className="enroll-form" onSubmit={capture}>
              <label htmlFor="enroll-name">Nom de la personne</label>
              <input
                id="enroll-name"
                ref={nameInput}
                type="text"
                list="enroll-known-names"
                value={name}
                onChange={(event) => setName(event.target.value)}
                placeholder="ex. jerome"
                autoComplete="off"
                spellCheck={false}
                aria-invalid={trimmedName !== "" && !nameIsValid}
              />
              <datalist id="enroll-known-names">
                {people.map(([person]) => (
                  <option key={person} value={person} />
                ))}
              </datalist>
              <p className={`hint${trimmedName !== "" && !nameIsValid ? " invalid" : ""}`}>
                Lettres sans accents, chiffres, « - » ou « _ ». Un nom déjà enregistré ajoute une
                photo à cette personne.
              </p>

              <button
                type="submit"
                className="primary"
                disabled={!connected || !nameIsValid || capturing}
                title={connected ? undefined : "En attente de la connexion à la caméra"}
              >
                {capturing ? "Capture…" : "📸 Capturer"}
              </button>

              {status && (
                <p className={`enroll-status ${status.kind}`} role="status">
                  {status.text}
                </p>
              )}

              <ul className="enroll-tips">
                <li>Placez-vous face à la caméra, à un ou deux mètres, le visage bien éclairé.</li>
                <li>
                  Capturez plusieurs photos : de face, légèrement de profil, avec et sans lunettes.
                  Chacune s'ajoute aux précédentes.
                </li>
                <li>Chaque photo est envoyée à toutes les caméras, qui la reconnaîtront aussitôt.</li>
              </ul>
            </form>
          </div>
        )}
      </section>

      <section className="people">
        <header>
          <h2>Personnes enregistrées</h2>
          {faces && (
            <span className="count">
              {people.length} personne{people.length > 1 ? "s" : ""} · {faces.length} photo
              {faces.length > 1 ? "s" : ""}
            </span>
          )}
        </header>

        {facesError && (
          <div className="banner error">
            {facesError}. Vérifiez que le manager tourne et que la base est joignable.
          </div>
        )}

        {!facesError && faces === null && <p className="placeholder">Chargement…</p>}

        {faces !== null && faces.length === 0 && (
          <p className="empty">
            Aucune photo partagée pour l'instant. Les photos apparaissent ici dès qu'une caméra les
            publie — y compris celles déjà présentes dans son dossier <code>known_faces/</code>, si
            leur nom ne contient que des lettres sans accents, des chiffres, « - » ou « _ ».
          </p>
        )}

        {people.map(([person, photos]) => (
          <article className="person" key={person}>
            <header>
              <h3>{person}</h3>
              <span className="count">
                {photos.length} photo{photos.length > 1 ? "s" : ""}
              </span>
              <button onClick={() => addPhotoFor(person)} disabled={!camera}>
                + Ajouter une photo
              </button>
            </header>

            <div className="face-grid">
              {photos.map((face) => (
                <figure className="face" key={face.file}>
                  <a href={face.photo_url} target="_blank" rel="noreferrer" title="Ouvrir la photo">
                    <img src={face.photo_url} alt={`${person}, photo prise par ${face.camera}`} loading="lazy" />
                  </a>
                  <figcaption>
                    <span>{face.camera}</span>
                    <span>{formatCapturedAt(face.captured_at)}</span>
                  </figcaption>
                  <button
                    className="face-delete"
                    onClick={() => remove(face)}
                    disabled={!connected || deleting.has(face.file)}
                    title={
                      connected
                        ? "Supprimer cette photo sur toutes les caméras"
                        : "Une connexion à une caméra est nécessaire pour supprimer"
                    }
                    aria-label={`Supprimer la photo ${face.file}`}
                  >
                    {deleting.has(face.file) ? "…" : "✕"}
                  </button>
                </figure>
              ))}
            </div>
          </article>
        ))}
      </section>
    </div>
  );
}
