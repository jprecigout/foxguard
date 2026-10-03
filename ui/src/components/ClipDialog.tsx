import { useEffect, useRef } from "react";

// Lecteur du clip d'une détection.
//
// # Pourquoi un cadre, et pas un lecteur écrit ici
//
// Les clips restent SUR LA CAMÉRA : quelques secondes de vidéo pèsent
// plusieurs mégaoctets, qui n'ont aucune raison de traverser le broker MQTT
// pour finir dans la base du manager (seule la vignette, légère, fait ce
// voyage).
//
// Cette interface, servie par le manager, ne peut donc pas lire ces fichiers
// elle-même : ils appartiennent à une autre origine, et le navigateur le lui
// interdit. Ouvrir les enregistrements de la caméra à toutes les origines
// (`Access-Control-Allow-Origin: *`) serait une bien mauvaise façon de
// contourner cette protection — ces routes ne sont déjà pas authentifiées.
//
// La caméra sert donc elle-même la page qui sait lire son format
// (`GET /play/<fichier>`, voir `crates/camera/static/clip-player.html`), et
// c'est cette page qu'on affiche ici. La politique de même origine est
// respectée sans rien assouplir, et le format d'enregistrement reste connu du
// seul composant qui l'écrit.
//
// Conséquence assumée : la lecture demande que la CAMÉRA soit joignable
// depuis ce navigateur. C'est inévitable — le clip n'existe nulle part
// ailleurs — d'où le lien « ouvrir dans un onglet », qui donne au navigateur
// l'occasion d'expliquer lui-même ce qui ne va pas.

export function ClipDialog({
  url,
  title,
  onClose,
}: {
  url: string;
  title: string;
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
        aria-label={`Clip de ${title}`}
        onClick={(event) => event.stopPropagation()}
      >
        <header>
          <h3>{title}</h3>
          <a href={url} target="_blank" rel="noreferrer">
            Ouvrir dans un onglet ↗
          </a>
          <button ref={closeButton} onClick={onClose} aria-label="Fermer le clip">
            ✕
          </button>
        </header>

        <iframe src={url} title={`Clip de ${title}`} />

        <p className="hint">
          Le clip est lu depuis la caméra elle-même : elle doit être joignable
          depuis ce navigateur.
        </p>
      </div>
    </div>
  );
}
