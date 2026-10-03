// Regroupement des détections par caméra.
//
// Volontairement séparé des composants : c'est la seule logique métier de
// l'interface, et la seule chose qui mérite d'être relue attentivement.

import type { CameraInfo, EventRecord } from "./api";

export interface CameraDay {
  camera: string;
  /** Vue en direct de cette caméra, si elle est joignable. */
  liveUrl: string | null;
  /** Du plus récent au plus ancien. */
  events: EventRecord[];
  /** Détections dont le visage n'a PAS été reconnu. */
  unknownCount: number;
  /** Personnes distinctes identifiées ce jour-là, triées. */
  people: string[];
}

/**
 * Regroupe les événements d'une journée par caméra.
 *
 * `knownCameras` sert à faire apparaître les caméras SANS détection ce
 * jour-là : « rien à signaler » et « caméra hors service » se ressemblent
 * beaucoup sur un écran, mais une caméra absente de la liste passerait
 * totalement inaperçue.
 *
 * Les caméras présentes dans les événements mais absentes de `knownCameras`
 * sont ajoutées malgré tout — une caméra branchée aujourd'hui ne doit pas
 * être invisible en attendant le rafraîchissement de la liste.
 */
export function groupByCamera(events: EventRecord[], knownCameras: CameraInfo[]): CameraDay[] {
  const byCamera = new Map<string, EventRecord[]>();
  const liveUrls = new Map(knownCameras.map((camera) => [camera.name, camera.live_url]));

  for (const camera of knownCameras) {
    byCamera.set(camera.name, []);
  }

  for (const event of events) {
    const existing = byCamera.get(event.camera);
    if (existing) {
      existing.push(event);
    } else {
      byCamera.set(event.camera, [event]);
    }
  }

  return [...byCamera.entries()]
    .map(([camera, cameraEvents]) => ({
      camera,
      liveUrl: liveUrls.get(camera) ?? null,
      events: cameraEvents,
      unknownCount: cameraEvents.filter((e) => e.status === "unknown").length,
      // Le test porte sur `status` et non sur la présence de `name` : le type
      // généré est une union discriminée, `name` n'existe tout simplement pas
      // sur la branche « inconnu ». TypeScript refuse donc d'y accéder sans
      // avoir d'abord restreint la branche — ce qu'un `e.name ? …` ne fait
      // pas.
      people: [
        ...new Set(cameraEvents.flatMap((e) => (e.status === "known" ? [e.name] : []))),
      ].sort((a, b) => a.localeCompare(b, "fr")),
    }))
    .sort((a, b) => a.camera.localeCompare(b.camera, "fr"));
}
