// Les pages de caméra que l'interface peut ouvrir dans son cadre (voir
// `components/CameraFrameDialog.tsx`), avec le texte d'aide qui va avec.
//
// Regroupées ici parce que plusieurs vues ouvrent les mêmes : l'interrupteur
// de surveillance s'atteint depuis la timeline comme depuis la mosaïque.
//
// Le DIRECT n'en fait plus partie : il est décodé par l'interface elle-même
// (voir `components/LiveStream.tsx`), sans cadre.

/** La page de caméra actuellement affichée dans le cadre. */
export interface OpenFrame {
  url: string;
  title: string;
  hint: string;
}

/** Le clip d'une détection. */
export function clipFrame(url: string, title: string): OpenFrame {
  return {
    url,
    title,
    hint: "Le clip est lu depuis la caméra elle-même : elle doit être joignable depuis ce navigateur.",
  };
}

/** La page de pilotage de la surveillance d'une caméra. */
export function controlFrame(url: string, camera: string): OpenFrame {
  return {
    url,
    title: `${camera} — surveillance`,
    hint: "L'interrupteur est servi par la caméra, qui l'applique elle-même : le manager n'écrit rien et n'a pas son jeton.",
  };
}
