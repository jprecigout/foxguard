// Géométrie temporelle de la bande de 24 heures.
//
// Volontairement séparée des composants, comme `grouping.ts` : c'est de
// l'arithmétique, et c'est la seule chose de la timeline qui mérite d'être
// relue attentivement.
//
// # Le fuseau
//
// Les positions sont calculées dans le fuseau du NAVIGATEUR, exactement comme
// les heures affichées par `formatTime`. C'est un choix de cohérence interne :
// une marque placée à 15 h doit porter l'étiquette « 15:41 », et non une heure
// convertie d'après le fuseau du serveur.
//
// Le découpage des journées, lui, reste celui du SERVEUR (voir
// `fetchEventsForDay`). Les deux ne coïncident que si navigateur et serveur
// partagent le même fuseau — c'est le cas d'une installation domestique, et
// autrement les positions sont simplement bornées à la journée affichée
// (voir `dayFraction`).

/** Nombre de millisecondes dans une journée. */
const DAY_MS = 24 * 60 * 60 * 1000;

/**
 * Graduations de la bande : une toutes les trois heures, bornes incluses.
 *
 * Trois heures, et non une : vingt-cinq étiquettes sur une bande de 800 px
 * seraient illisibles, et huit intervalles suffisent à situer un événement
 * dans la journée.
 */
export const HOUR_TICKS = [0, 3, 6, 9, 12, 15, 18, 21, 24] as const;

/**
 * Position d'un horodatage sur la bande, de 0 (minuit) à 1 (minuit suivant).
 *
 * Bornée à cet intervalle : un événement dont l'heure locale tombe hors de la
 * journée affichée — décalage de fuseau entre le serveur, qui découpe les
 * journées, et le navigateur, qui les affiche — est ramené sur le bord plutôt
 * que placé hors de la bande, où il serait invisible.
 */
export function dayFraction(timestamp: string): number {
  const date = new Date(timestamp);

  if (Number.isNaN(date.getTime())) return 0;

  const sinceMidnight =
    date.getHours() * 3_600_000 +
    date.getMinutes() * 60_000 +
    date.getSeconds() * 1000 +
    date.getMilliseconds();

  return Math.min(1, Math.max(0, sinceMidnight / DAY_MS));
}

/**
 * Position de l'instant présent sur la bande, ou `null` si `day` n'est pas la
 * journée en cours.
 *
 * Sert à afficher un repère « maintenant » : sur la journée du jour, il dit
 * d'un coup d'œil quelle part de la bande est encore à venir. Sur une journée
 * passée, il n'aurait aucun sens.
 */
export function nowFraction(day: string, now: Date = new Date()): number | null {
  const [year, month, date] = day.split("-").map(Number);

  if (
    now.getFullYear() !== year ||
    now.getMonth() + 1 !== month ||
    now.getDate() !== date
  ) {
    return null;
  }

  return dayFraction(now.toISOString());
}

/** Étiquette d'une graduation, ex. « 09 h ». */
export function formatHourTick(hour: number): string {
  // 24 h est minuit du lendemain : l'écrire « 24 h » est plus clair que
  // « 00 h » à l'extrémité droite d'une bande qui va de minuit à minuit.
  return `${String(hour).padStart(2, "0")} h`;
}

/** Position en pourcentage, prête à être posée dans un style CSS. */
export function percent(fraction: number): string {
  return `${(fraction * 100).toFixed(4)}%`;
}
