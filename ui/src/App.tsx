import { useCallback, useEffect, useState } from "react";

import { ApiError, fetchCameras, fetchEventsForDay } from "./api";
import type { CameraInfo, EventsResponse } from "./api";
import { CameraFrameDialog } from "./components/CameraFrameDialog";
import { CameraTimeline } from "./components/CameraTimeline";
import { DayBar } from "./components/DayBar";
import { today } from "./dates";
import { groupByCamera } from "./grouping";

/** La page de caméra actuellement affichée dans le cadre. */
interface OpenFrame {
  url: string;
  title: string;
  hint: string;
}

export default function App() {
  const [day, setDay] = useState(today);
  const [cameras, setCameras] = useState<CameraInfo[]>([]);
  const [response, setResponse] = useState<EventsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  // L'événement mis en évidence, partagé entre la bande de 24 h et la
  // pellicule de vignettes : cliquer une marque désigne une vignette, et
  // réciproquement.
  const [selectedId, setSelectedId] = useState<number | null>(null);
  const [openFrame, setOpenFrame] = useState<OpenFrame | null>(null);

  const load = useCallback(
    async (signal: AbortSignal) => {
      setBusy(true);
      setError(null);

      try {
        // Les deux requêtes sont indépendantes : les lancer en parallèle
        // évite d'attendre deux allers-retours.
        const [knownCameras, events] = await Promise.all([
          fetchCameras(signal),
          fetchEventsForDay(day, signal),
        ]);

        setCameras(knownCameras);
        setResponse(events);
      } catch (cause) {
        // Une requête annulée (changement de jour rapide) n'est pas une
        // erreur : la suivante est déjà partie.
        if (cause instanceof DOMException && cause.name === "AbortError") return;
        setError(cause instanceof ApiError ? cause.message : "Erreur inattendue");
        setResponse(null);
      } finally {
        setBusy(false);
      }
    },
    [day],
  );

  useEffect(() => {
    const controller = new AbortController();
    void load(controller.signal);

    // Rafraîchissement périodique, uniquement sur la journée EN COURS : une
    // journée passée ne bouge plus, l'interroger en boucle ne ferait que
    // charger la base pour rien.
    if (day !== today()) return () => controller.abort();

    const timer = window.setInterval(() => void load(controller.signal), 15000);
    return () => {
      window.clearInterval(timer);
      controller.abort();
    };
  }, [load, day]);

  // Changer de journée invalide la sélection : l'événement désigné n'est plus
  // affiché, et un identifiant résiduel mettrait en évidence une marque au
  // hasard dès qu'un autre événement réutiliserait ce numéro.
  const changeDay = useCallback((next: string) => {
    setSelectedId(null);
    setDay(next);
  }, []);

  const grouped = response ? groupByCamera(response.events, cameras) : [];

  return (
    <div className="app">
      <header>
        <div className="brand">
          {/* Servi depuis `public/` : le fichier est copié tel quel à la
              racine du bundle, donc référencé en chemin absolu. */}
          <img className="logo" src="/logo.svg" alt="" aria-hidden="true" />
          <h1>
            Fox<span className="fox">Guard</span>
          </h1>
        </div>
        <p className="subtitle">Timeline des détections, par caméra</p>
      </header>

      <DayBar day={day} onChange={changeDay} busy={busy} />

      {error && (
        <div className="banner error">
          {error}. Vérifiez que le manager tourne et que la base est joignable.
        </div>
      )}

      {response?.truncated && (
        <div className="banner warn">
          Cette journée comporte plus de détections que le serveur n'en renvoie :
          la timeline ci-dessous est incomplète.
        </div>
      )}

      {!error && response === null && <p className="placeholder">Chargement…</p>}

      {!error && response !== null && grouped.length === 0 && (
        <p className="placeholder">
          Aucune caméra connue. Les caméras apparaissent ici dès leur première
          détection publiée sur MQTT.
        </p>
      )}

      <div className="cameras">
        {grouped.map((cameraDay) => (
          <CameraTimeline
            key={cameraDay.camera}
            day={day}
            cameraDay={cameraDay}
            selectedId={selectedId}
            onSelect={setSelectedId}
            onPlay={(url, title) =>
              setOpenFrame({
                url,
                title,
                hint: "Le clip est lu depuis la caméra elle-même : elle doit être joignable depuis ce navigateur.",
              })
            }
            onWatchLive={(url, camera) =>
              setOpenFrame({
                url,
                title: `${camera} — direct`,
                hint: "Le direct vient de la caméra elle-même, qui l'encode en H.264 quand le navigateur sait le décoder.",
              })
            }
            onControl={(url, camera) =>
              setOpenFrame({
                url,
                title: `${camera} — surveillance`,
                hint: "L'interrupteur est servi par la caméra, qui l'applique elle-même : le manager n'écrit rien et n'a pas son jeton.",
              })
            }
          />
        ))}
      </div>

      {openFrame && (
        <CameraFrameDialog
          url={openFrame.url}
          title={openFrame.title}
          hint={openFrame.hint}
          onClose={() => setOpenFrame(null)}
        />
      )}
    </div>
  );
}
