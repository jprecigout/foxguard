//! État partagé de la caméra, exposé au serveur HTTP / WebSocket (voir
//! `crate::api`) pour piloter et consulter l'état de la surveillance.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize};

use foxguard_protocol::auth::FailureThrottle;

use crate::auth::SessionKey;
use crate::h264::H264Stream;

/// Nombre de clients du flux vidéo admis par défaut (`[server]
/// max_stream_clients`).
pub const DEFAULT_MAX_STREAM_CLIENTS: usize = 8;

/// État partagé de la caméra pour la gestion par le WebSocket
pub struct SharedState {
    // Surveillance IA (YOLO/YuNet/ArcFace) activée ou non
    pub detection_enabled: AtomicBool,
    // Enregistrement disque activé ou non
    pub recording_enabled: AtomicBool,
    // Jeton attendu en paramètre ?token= pour se connecter au WebSocket
    pub api_token: String,

    /// Nom de la caméra (`[camera] name`) : il entre dans la signature des
    /// tickets de visionnage, qui ne valent que pour la caméra pour laquelle
    /// ils ont été émis.
    pub camera_name: String,

    /// Secret partagé avec le manager (`[server] stream_ticket_secret`), ou
    /// `None` si ses tickets sont refusés (voir `crate::auth`).
    pub stream_ticket_secret: Option<String>,

    /// Clé des tickets de SESSION que la caméra injecte dans ses propres
    /// pages (voir `crate::auth::SessionKey`).
    pub session_key: SessionKey,

    /// Échecs d'authentification récents, par adresse (voir
    /// `crate::auth::throttle_failures`).
    pub auth_failures: FailureThrottle,

    /// Origine de l'interface du manager (`[server] manager_origin`), seule
    /// autorisée à afficher les pages de la caméra dans un cadre.
    pub manager_origin: Option<String>,

    /// Clients du flux vidéo actuellement connectés, et leur plafond.
    ///
    /// Chaque connexion fait produire une image clé à l'encodeur, le poste le
    /// plus coûteux du Raspberry Pi : sans plafond, quelques dizaines de
    /// connexions suffiraient à le saturer.
    pub stream_clients: AtomicUsize,
    pub max_stream_clients: usize,

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

impl SharedState {
    /// État initial : surveillance et enregistrement coupés, tickets du
    /// manager refusés, clé de session tirée au hasard.
    ///
    /// Les champs restent publics pour que `main` (ou un test) ajuste ce qui
    /// vient de la configuration.
    pub fn new(
        api_token: impl Into<String>,
        camera_name: impl Into<String>,
        h264: Arc<H264Stream>,
        recordings_dir: impl Into<String>,
    ) -> Self {
        Self {
            detection_enabled: AtomicBool::new(false),
            recording_enabled: AtomicBool::new(false),
            api_token: api_token.into(),
            camera_name: camera_name.into(),
            stream_ticket_secret: None,
            session_key: SessionKey::random()
                .expect("lecture de /dev/urandom impossible pour la clé de session"),
            auth_failures: FailureThrottle::new(),
            manager_origin: None,
            stream_clients: AtomicUsize::new(0),
            max_stream_clients: DEFAULT_MAX_STREAM_CLIENTS,
            h264,
            pending_enrollment: Mutex::new(None),
            recordings_dir: recordings_dir.into(),
        }
    }
}
