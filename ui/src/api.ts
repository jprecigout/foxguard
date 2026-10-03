// Appels de l'API du manager.
//
// Les types de l'API ne sont PAS écrits ici : ils sont générés depuis les
// types Rust par ts-rs (voir `./generated/`, et les dérives `TS` dans
// `crates/manager/src/api.rs` et `crates/protocol/src/lib.rs`).
//
// Conséquence : renommer ou supprimer un champ côté Rust casse la compilation
// de cette interface. C'était auparavant la seule frontière du projet où une
// dérive de contrat n'apparaissait qu'à l'exécution.
//
// Les fichiers générés sont COMMITÉS : l'interface se construit sans chaîne
// Rust. Ils sont régénérés par `cargo test -p foxguard-manager`.

// `EventRecord` et non `DetectionEvent` : le premier est le format de l'API
// HTTP, le second celui du fil MQTT. L'interface a besoin d'un identifiant
// (pour demander la vignette) et d'URL de média prêtes à l'emploi — voir
// `crates/manager/src/api.rs` pour le pourquoi de cette distinction.
export type { EventRecord } from "./generated/EventRecord";
export type { EventsResponse } from "./generated/EventsResponse";
export type { PersonStatus } from "./generated/PersonStatus";

import type { EventsResponse } from "./generated/EventsResponse";

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
