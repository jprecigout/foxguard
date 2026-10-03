import { useEffect, useRef } from "react";

// Affiche une page servie par une CAMÉRA : le clip d'une détection, ou sa
// vue en direct.
//
// # Pourquoi un cadre, et pas un lecteur écrit ici
//
// Les deux vidéos vivent sur la caméra, et pour deux raisons différentes :
//
//  - les CLIPS, parce que quelques secondes de vidéo pèsent plusieurs
//    mégaoctets, qui n'ont aucune raison de traverser le broker MQTT pour
//    finir dans la base du manager (seule la vignette, légère, fait ce
//    voyage) ;
//  - le DIRECT, parce qu'il est authentifié par un jeton propre à chaque
//    caméra. Le confier au manager voudrait dire le recopier dans une base
//    puis dans une page web : une dégradation nette du modèle de sécurité
//    pour afficher une image.
//
// Cette interface, servie par le manager, ne peut donc lire ni l'un ni
// l'autre : ils appartiennent à une autre origine, et le navigateur le lui
// interdit. Ouvrir les flux de la caméra à toutes les origines
// (`Access-Control-Allow-Origin: *`) serait une bien mauvaise façon de
// contourner cette protection.
//
// La caméra sert donc elle-même les deux pages (`GET /play/<fichier>` et
// `GET /live`, voir `crates/camera/static/`), et ce sont elles qu'on affiche
// ici. La politique de même origine est respectée sans rien assouplir, le
// jeton ne quitte jamais la caméra, et les formats d'enregistrement restent
// connus du seul composant qui les écrit.
//
// Conséquence assumée : il faut que la CAMÉRA soit joignable depuis ce
// navigateur. C'est inévitable — ces vidéos n'existent nulle part ailleurs —
// d'où le lien « ouvrir dans un onglet », qui donne au navigateur l'occasion
// d'expliquer lui-même ce qui ne va pas.

export function CameraFrameDialog({
  url,
  title,
  hint,
  onClose,
}: {
  url: string;
  title: string;
  hint: string;
  onClose: () => void;
}) {
  const closeButton = useRef<HTMLButtonElement>(null);

  useEffect(() => {
    // Échap ferme : c'est le réflexe attendu d'une boîte de dialogue, et le
    // seul moyen d'en sortir au clavier sans aller chercher le bouton.
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };

    window.addEventListener("keydown", onKeyDown);
    closeButton.current?.focus();

    return () => window.removeEventListener("keydown", onKeyDown);
  }, [onClose]);

  return (
    // Le clic sur le fond ferme, celui sur le panneau non : d'où le
    // `stopPropagation` sur ce dernier.
    <div className="overlay" onClick={onClose} role="presentation">
      <div
        className="dialog"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(event) => event.stopPropagation()}
      >
        <header>
          <h3>{title}</h3>
          <a href={url} target="_blank" rel="noreferrer">
            Ouvrir dans un onglet ↗
          </a>
          <button ref={closeButton} onClick={onClose} aria-label="Fermer">
            ✕
          </button>
        </header>

        <iframe src={url} title={title} />

        <p className="hint">{hint}</p>
      </div>
    </div>
  );
}
