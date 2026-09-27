import { useCallback, useEffect, useState } from "react";

import { ApiError, fetchCameras, fetchEventsForDay } from "./api";
import type { EventsResponse } from "./api";
import { formatDayLabel, formatTime, shiftDay, today } from "./dates";
import { groupByCamera } from "./grouping";
import type { CameraDay } from "./grouping";

/** Barre de navigation entre les journées. */
function DayBar({
  day,
  onChange,
  busy,
}: {
  day: string;
  onChange: (day: string) => void;
  busy: boolean;
}) {
  const isToday = day === today();

  return (
    <div className="daybar">
      <button onClick={() => onChange(shiftDay(day, -1))} aria-label="Jour précédent">
        ← Veille
      </button>

      <span className="label">{formatDayLabel(day)}</span>

      <input
        type="date"
        value={day}
        max={today()}
        onChange={(e) => e.target.value && onChange(e.target.value)}
      />

      {/* Désactivé sur aujourd'hui : il n'y a rien à afficher dans le futur. */}
      <button onClick={() => onChange(shiftDay(day, 1))} disabled={isToday}>
        Lendemain →
      </button>
      <button onClick={() => onChange(today())} disabled={isToday}>
        Aujourd'hui
      </button>

      <span className="count">{busy ? "chargement…" : ""}</span>
    </div>
  );
}

/** Détections d'une caméra pour la journée affichée. */
function CameraCard({ day }: { day: CameraDay }) {
  return (
    <section className="camera">
      <header>
        <h2>{day.camera}</h2>
        <span className="count">
          {day.events.length} détection{day.events.length > 1 ? "s" : ""}
        </span>
        <div className="chips">
          {day.people.map((person) => (
            <span key={person} className="chip known">
              {person}
            </span>
          ))}
          {day.unknownCount > 0 && (
            <span className="chip unknown">{day.unknownCount} inconnu(s)</span>
          )}
        </div>
      </header>

      {day.events.length === 0 ? (
        <p className="empty">Aucune détection ce jour-là.</p>
      ) : (
        <ul className="events">
          {day.events.map((event, index) => (
            // L'horodatage seul ne suffit pas comme clé : deux caméras
            // peuvent détecter à la même seconde, et une même caméra peut
            // émettre deux états dans la même seconde.
            <li key={`${event.timestamp}-${index}`}>
              <span className="time">{formatTime(event.timestamp)}</span>
              <span className={`who ${event.status}`}>
                {event.status === "known" ? event.name : "Personne inconnue"}
              </span>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}

export default function App() {
  const [day, setDay] = useState(today);
  const [cameras, setCameras] = useState<string[]>([]);
  const [response, setResponse] = useState<EventsResponse | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

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
        <p className="subtitle">Détections du jour, par caméra</p>
      </header>

      <DayBar day={day} onChange={setDay} busy={busy} />

      {error && (
        <div className="banner error">
          {error}. Vérifiez que le manager tourne et que la base est joignable.
        </div>
      )}

      {response?.truncated && (
        <div className="banner warn">
          Cette journée comporte plus de détections que le serveur n'en renvoie :
          la liste ci-dessous est incomplète.
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
          <CameraCard key={cameraDay.camera} day={cameraDay} />
        ))}
      </div>
    </div>
  );
}
