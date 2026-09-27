// Manipulation des journées, en heure LOCALE.
//
// `toISOString()` est volontairement évité : il convertit en UTC, ce qui
// décale la date d'un jour en soirée pour les fuseaux à l'est de Greenwich —
// un clic sur « aujourd'hui » à 23 h afficherait alors demain.

/** Journée au format `AAAA-MM-JJ`, dans le fuseau du navigateur. */
export function toDayString(date: Date): string {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  return `${year}-${month}-${day}`;
}

export function today(): string {
  return toDayString(new Date());
}

/** Journée décalée de `days` (négatif pour reculer). */
export function shiftDay(day: string, days: number): string {
  const [year, month, date] = day.split("-").map(Number);
  // Le constructeur `Date(y, m, d)` normalise les débordements : le 0 octobre
  // devient le 30 septembre, sans arithmétique de calendrier à écrire.
  return toDayString(new Date(year ?? 1970, (month ?? 1) - 1, (date ?? 1) + days));
}

/** Libellé lisible, ex. « samedi 27 septembre 2026 ». */
export function formatDayLabel(day: string): string {
  const [year, month, date] = day.split("-").map(Number);
  return new Date(year ?? 1970, (month ?? 1) - 1, date ?? 1).toLocaleDateString("fr-FR", {
    weekday: "long",
    day: "numeric",
    month: "long",
    year: "numeric",
  });
}

/** Heure d'un horodatage RFC 3339, ex. « 15:41:33 ». */
export function formatTime(timestamp: string): string {
  return new Date(timestamp).toLocaleTimeString("fr-FR", {
    hour: "2-digit",
    minute: "2-digit",
    second: "2-digit",
  });
}
