import { useState } from "react";

import { CameraFrameDialog } from "./components/CameraFrameDialog";
import { LiveWall } from "./components/LiveWall";
import { TimelineView } from "./components/TimelineView";
import type { OpenFrame } from "./frames";
import { routeHref, useRoute } from "./route";

export default function App() {
  const route = useRoute();
  const [openFrame, setOpenFrame] = useState<OpenFrame | null>(null);

  return (
    <div className={`app${route.view === "live" ? " wide" : ""}`}>
      <header>
        <div className="brand">
          {/* Servi depuis `public/` : le fichier est copié tel quel à la
              racine du bundle, donc référencé en chemin absolu. */}
          <img className="logo" src="/logo.svg" alt="" aria-hidden="true" />
          <h1>
            Fox<span className="fox">Guard</span>
          </h1>
        </div>
        <p className="subtitle">
          {route.view === "live" ? "Direct de toutes les caméras" : "Timeline des détections, par caméra"}
        </p>
      </header>

      {/* Des liens et non des boutons : chaque vue a son adresse, que le
          bouton Précédent et les favoris retrouvent (voir `route.ts`). */}
      <nav className="tabs">
        <a href={routeHref({ view: "timeline" })} aria-current={route.view === "timeline" ? "page" : undefined}>
          Timeline
        </a>
        <a href={routeHref({ view: "live", camera: null })} aria-current={route.view === "live" ? "page" : undefined}>
          ● Direct
        </a>
      </nav>

      {/* Une seule vue montée à la fois : quitter la mosaïque ferme ses
          flux (et soulage les encodeurs des caméras), quitter la timeline
          arrête son rafraîchissement périodique. */}
      {route.view === "live" ? (
        <LiveWall focusedCamera={route.camera} onOpenFrame={setOpenFrame} />
      ) : (
        <TimelineView onOpenFrame={setOpenFrame} />
      )}

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
