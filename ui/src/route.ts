// Navigation entre les vues de l'interface, portée par le FRAGMENT de l'URL
// (`#/direct`, `#/direct/<caméra>`).
//
// Le fragment plutôt que le chemin : le manager sert le bundle avec un simple
// `ServeDir`, sans repli vers `index.html`. Un chemin comme `/direct`
// répondrait 404 au rechargement, alors que le fragment ne quitte jamais le
// navigateur. Et une vue reste ainsi un lien qu'on peut garder en favori —
// « le direct du jardin » sur l'écran de l'entrée, par exemple.

import { useEffect, useState } from "react";

export type Route =
  | { view: "timeline" }
  /** `camera` : la caméra affichée seule, en grand. */
  | { view: "live"; camera: string | null };

export function parseRoute(hash: string): Route {
  const [view, camera] = hash.replace(/^#\/?/, "").split("/", 2);

  if (view === "direct") {
    return { view: "live", camera: camera ? decodeURIComponent(camera) : null };
  }

  return { view: "timeline" };
}

export function routeHref(route: Route): string {
  if (route.view === "timeline") return "#/";
  return route.camera ? `#/direct/${encodeURIComponent(route.camera)}` : "#/direct";
}

/** La vue courante, tenue à jour au fil de la navigation (boutons Précédent compris). */
export function useRoute(): Route {
  const [route, setRoute] = useState(() => parseRoute(window.location.hash));

  useEffect(() => {
    const onHashChange = () => setRoute(parseRoute(window.location.hash));
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  return route;
}

export function navigate(route: Route): void {
  window.location.hash = routeHref(route);
}
