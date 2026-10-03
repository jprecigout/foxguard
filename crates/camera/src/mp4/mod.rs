//! Écriture de fichiers MP4 fragmentés (fMP4) contenant le flux H.264 de la
//! caméra.
//!
//! # Pourquoi ce format pour les enregistrements
//!
//! Les enregistrements étaient jusqu'ici une suite d'images JPEG horodatées —
//! un format maison, que seule l'interface de la caméra savait relire, et dix
//! fois plus lourd que nécessaire puisque chaque image y était transmise en
//! entier. Une fois l'encodeur H.264 en place, le conserver n'avait plus de
//! justification.
//!
//! Le MP4 se lit en revanche partout : par un `<video>` de navigateur, par
//! VLC, par ffmpeg, par n'importe quel outil d'archivage. Et sa variante
//! FRAGMENTÉE survit à une troncature, ce qui compte pour un enregistrement
//! que peut interrompre une coupure de courant (voir [`writer`]).

mod boxes;
mod writer;

pub use writer::{Fmp4Writer, TIMESCALE};
