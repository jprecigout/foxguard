//! État partagé de la caméra, exposé au serveur HTTP / WebSocket (voir
//! `crate::api`) pour piloter et consulter l'état de la surveillance.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;

use crate::h264::H264Stream;

/// État partagé de la caméra pour la gestion par le WebSocket
pub struct SharedState {
    // Surveillance IA (YOLO/YuNet/ArcFace) activée ou non
    pub detection_enabled: AtomicBool,
    // Enregistrement disque activé ou non
    pub recording_enabled: AtomicBool,
    // Jeton attendu en paramètre ?token= pour se connecter au WebSocket
    pub api_token: String,

    /// Flux H.264 encodé — le SEUL flux vidéo de la caméra.
    ///
    /// Porté par l'état parce que le serveur HTTP en est un consommateur :
    /// `GET /ws` y abonne les navigateurs, qui décodent le flux eux-mêmes
    /// (voir `crate::api`). S'y abonner suffit à déclencher l'encodage, et
    /// s'en désabonner à l'arrêter.
    ///
    /// Il n'y a plus de canal de diffusion JPEG à côté : le flux MJPEG
    /// historique a été retiré, et avec lui la double diffusion.
    pub h264: Arc<H264Stream>,

    // Nom en attente de capture pour une nouvelle photo de référence
    // (voir ClientCommand::CaptureReference dans api.rs). La boucle caméra
    // consomme (take()) cette valeur dès qu'une frame est disponible (voir
    // `super::known_faces::try_capture_reference`).
    pub pending_enrollment: Mutex<Option<String>>,

    // Dossier des enregistrements, issu de `[recording] dir` (voir
    // `crate::config`). Porté par l'état plutôt que par une constante
    // globale : c'est ce qui permet aux tests d'intégration de travailler
    // dans un dossier temporaire, au lieu d'écrire dans l'arborescence du
    // dépôt (Cargo exécute les tests d'intégration avec le répertoire courant
    // positionné sur le paquet, pas sur le workspace).
    pub recordings_dir: String,
}
