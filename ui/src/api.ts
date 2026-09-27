// Types et appels de l'API du manager.
//
// ⚠️ Ces types sont écrits À LA MAIN et doivent rester alignés sur
// `crates/manager/src/api.rs` et `crates/protocol/src/lib.rs`. Rien ne le
// vérifie aujourd'hui : c'est la frontière Rust ↔ TypeScript, et donc le seul
// endroit du projet où une dérive de contrat passerait inaperçue jusqu'à
// l'exécution (voir la note sur `ts-rs` dans le README de ce dossier).

/** Statut de reconnaissance, tel que sérialisé par `foxguard-protocol`. */
export type DetectionStatus = "known" | "unknown";

export interface DetectionEvent {
  camera: string;
  /** Horodatage RFC 3339 avec décalage, ex. `2026-09-27T15:41:33+02:00`. */
  timestamp: string;
  status: DetectionStatus;
  /** Présent uniquement quand `status === "known"`. */
  name?: string;
}

export interface EventsResponse {
  count: number;
  total: number;
  /** Vrai si la journée comporte plus d'événements que le serveur n'en renvoie. */
  truncated: boolean;
  events: DetectionEvent[];
}

/** Erreur portant le code HTTP, pour distinguer « serveur injoignable » de « 500 ». */
export class ApiError extends Error {
  constructor(
    message: string,
    readonly status?: number,
  ) {
    super(message);
  }
}

async function getJson<T>(path: string, signal?: AbortSignal): Promise<T> {
  let response: Response;

  try {
    response = await fetch(path, { signal });
  } catch (cause) {
    // `fetch` ne rejette que sur une panne réseau : le manager est arrêté, ou
    // le proxy de développement ne l'atteint pas.
    if (cause instanceof DOMException && cause.name === "AbortError") throw cause;
    throw new ApiError("Manager injoignable");
  }

  if (!response.ok) {
    throw new ApiError(`Le serveur a répondu ${response.status}`, response.status);
  }

  return (await response.json()) as T;
}

/** Caméras ayant déjà émis au moins un événement encore conservé. */
export function fetchCameras(signal?: AbortSignal): Promise<string[]> {
  return getJson<string[]>("/api/cameras", signal);
}

/**
 * Détections d'une journée, du plus récent au plus ancien.
 *
 * `day` est au format `AAAA-MM-JJ` et interprété dans le fuseau du SERVEUR :
 * c'est lui qui découpe les journées, pour que deux navigateurs dans des
 * fuseaux différents voient la même chose.
 */
export function fetchEventsForDay(day: string, signal?: AbortSignal): Promise<EventsResponse> {
  return getJson<EventsResponse>(`/api/events?date=${encodeURIComponent(day)}`, signal);
}
