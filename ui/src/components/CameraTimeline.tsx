import type { EventRecord } from "../api";
import { formatTime } from "../dates";
import type { CameraDay } from "../grouping";
import { routeHref } from "../route";
import { HOUR_TICKS, dayFraction, formatHourTick, nowFraction, percent } from "../timeline";

// Timeline d'une caméra sur une journée.
//
// Deux lectures du même jeu d'événements, parce qu'elles répondent à deux
// questions différentes :
//
//  1. la BANDE de 24 heures répond à « quand s'est-il passé quelque chose ? ».
//     Une marque par détection, posée à son heure réelle : une rafale à 3 h du
//     matin et une journée tranquille ne se ressemblent pas, et ça se voit
//     avant d'avoir lu une seule ligne ;
//  2. la PELLICULE de vignettes répond à « que s'est-il passé ? ». C'est la
//     vignette qui fait le travail : un nom et une heure ne disent pas si
//     c'était le facteur ou un inconnu dans le jardin.
//
// Les deux sont reliées : survoler ou cliquer une marque met en évidence la
// vignette correspondante, et inversement.

/** Libellé d'un événement : le nom reconnu, ou « inconnu ». */
function label(event: EventRecord): string {
  return event.status === "known" ? event.name : "Inconnu";
}

/**
 * Bande de 24 heures : les graduations horaires, une marque par détection, et
 * le repère de l'instant présent sur la journée en cours.
 */
function HourBand({
  day,
  events,
  selectedId,
  onSelect,
}: {
  day: string;
  events: EventRecord[];
  selectedId: number | null;
  onSelect: (id: number) => void;
}) {
  const now = nowFraction(day);

  return (
    <div className="band">
      <div className="band-track">
        {/* Graduations, posées sous les marques pour ne jamais les masquer. */}
        {HOUR_TICKS.map((hour) => (
          <span
            key={hour}
            className="band-tick"
            style={{ left: percent(hour / 24) }}
            aria-hidden="true"
          />
        ))}

        {now !== null && (
          <span
            className="band-now"
            style={{ left: percent(now) }}
            title="Maintenant"
            aria-hidden="true"
          />
        )}

        {events.map((event) => (
          <button
            key={event.id}
            type="button"
            className={`band-mark ${event.status}${
              event.id === selectedId ? " selected" : ""
            }`}
            style={{ left: percent(dayFraction(event.timestamp)) }}
            title={`${formatTime(event.timestamp)} — ${label(event)}`}
            aria-label={`${formatTime(event.timestamp)}, ${label(event)}`}
            onClick={() => onSelect(event.id)}
          />
        ))}
      </div>

      <div className="band-labels" aria-hidden="true">
        {HOUR_TICKS.map((hour) => (
          <span key={hour} style={{ left: percent(hour / 24) }}>
            {formatHourTick(hour)}
          </span>
        ))}
      </div>
    </div>
  );
}

/** Une vignette de la pellicule. */
function EventThumbnail({
  event,
  selected,
  onSelect,
  onPlay,
}: {
  event: EventRecord;
  selected: boolean;
  onSelect: () => void;
  onPlay: (url: string, title: string) => void;
}) {
  const time = formatTime(event.timestamp);
  const title = `${event.camera} — ${time}`;

  return (
    <li
      className={`shot ${event.status}${selected ? " selected" : ""}`}
      id={`event-${event.id}`}
    >
      <button
        type="button"
        className="shot-frame"
        onClick={() => {
          onSelect();
          if (event.clip_url) onPlay(event.clip_url, title);
        }}
        // Le bouton reste actionnable sans clip : il sert alors uniquement à
        // sélectionner l'événement (ce qui le met en évidence sur la bande).
        title={event.clip_url ? `Lire le clip de ${time}` : `Détection de ${time}`}
      >
        {event.thumbnail_url ? (
          // `lazy` : une journée chargée compte des centaines de vignettes,
          // et le navigateur ne doit télécharger que celles qu'on fait
          // défiler.
          <img src={event.thumbnail_url} alt={`Détection de ${time}`} loading="lazy" />
        ) : (
          <span className="shot-missing" aria-hidden="true">
            Pas de vignette
          </span>
        )}

        {event.clip_url && (
          <span className="shot-play" aria-hidden="true">
            ▶
          </span>
        )}
      </button>

      <span className="shot-time">{time}</span>
      <span className="shot-who">{label(event)}</span>
    </li>
  );
}

/** Détections d'une caméra pour la journée affichée. */
export function CameraTimeline({
  day,
  cameraDay,
  selectedId,
  onSelect,
  onPlay,
  onControl,
}: {
  day: string;
  cameraDay: CameraDay;
  selectedId: number | null;
  onSelect: (id: number) => void;
  onPlay: (url: string, title: string) => void;
  onControl: (url: string, camera: string) => void;
}) {
  // La pellicule se lit dans le sens du temps, alors que l'API renvoie du plus
  // récent au plus ancien (l'ordre d'une liste). On la retourne donc, sans
  // toucher au tableau reçu.
  const chronological = [...cameraDay.events].reverse();

  return (
    <section className="camera">
      <header>
        <h2>{cameraDay.camera}</h2>

        {/* Le direct ne dépend pas de la journée affichée ni des détections :
            une caméra sans rien à signaler se regarde quand même. Il ouvre la
            mosaïque des directs, centrée sur cette caméra — un lien, pour
            que le bouton Précédent ramène à la timeline. */}
        {cameraDay.streamUrl && (
          <a
            className="live"
            href={routeHref({ view: "live", camera: cameraDay.camera })}
            title={`Voir le direct de ${cameraDay.camera}`}
          >
            ● Direct
          </a>
        )}

        {/* L'interrupteur de surveillance, lui aussi servi par la caméra
            (voir `CameraFrameDialog` pour pourquoi le manager n'en pilote
            aucune lui-même). Même condition que le direct : sans URL
            publique déclarée, le manager ne sait pas où est la caméra. */}
        {cameraDay.controlUrl && (
          <button
            className="control"
            onClick={() => onControl(cameraDay.controlUrl!, cameraDay.camera)}
            title={`Activer ou couper la surveillance de ${cameraDay.camera}`}
          >
            🛡 Surveillance
          </button>
        )}

        <span className="count">
          {cameraDay.events.length} détection{cameraDay.events.length > 1 ? "s" : ""}
        </span>
        <div className="chips">
          {cameraDay.people.map((person) => (
            <span key={person} className="chip known">
              {person}
            </span>
          ))}
          {cameraDay.unknownCount > 0 && (
            <span className="chip unknown">{cameraDay.unknownCount} inconnu(s)</span>
          )}
        </div>
      </header>

      {cameraDay.events.length === 0 ? (
        <p className="empty">Aucune détection ce jour-là.</p>
      ) : (
        <>
          <HourBand
            day={day}
            events={cameraDay.events}
            selectedId={selectedId}
            onSelect={onSelect}
          />

          <ul className="strip">
            {chronological.map((event) => (
              <EventThumbnail
                key={event.id}
                event={event}
                selected={event.id === selectedId}
                onSelect={() => onSelect(event.id)}
                onPlay={onPlay}
              />
            ))}
          </ul>
        </>
      )}
    </section>
  );
}
