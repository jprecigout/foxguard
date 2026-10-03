import { formatDayLabel, shiftDay, today } from "../dates";

/** Barre de navigation entre les journées. */
export function DayBar({
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
