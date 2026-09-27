//! État partagé de la caméra, exposé au serveur HTTP / WebSocket (voir
//! `crate::api`) pour piloter et consulter l'état de la surveillance.

use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use tokio::sync::broadcast;

/// État partagé de la caméra pour la gestion par le WebSocket
pub struct SharedState {
    // Surveillance IA (YOLO/YuNet/ArcFace) activée ou non
    pub detection_enabled: AtomicBool,
    // Enregistrement disque activé ou non
    pub recording_enabled: AtomicBool,
    // Jeton attendu en paramètre ?token= pour se connecter au WebSocket
    pub api_token: String,
    // Canal de diffusion des frames JPEG encodées vers tous les clients WS
    pub tx: broadcast::Sender<Vec<u8>>,

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
