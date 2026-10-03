//! Point d'entrée de FoxGuard : charge la configuration, démarre la boucle
//! de capture caméra (thread bloquant), le serveur HTTP / WebSocket (Axum)
//! qui sert l'interface de contrôle et le flux vidéo H.264, et — si
//! `[rtsp] enabled = true` — le serveur RTSP qui met le même flux à
//! disposition des lecteurs vidéo du réseau.
//!
//! Toute la logique applicative vit dans la bibliothèque (`src/lib.rs` et
//! ses modules) : ce fichier ne fait qu'orchestrer le démarrage. Ce
//! découpage binaire/bibliothèque permet aux tests d'intégration
//! (`tests/`) de dépendre de la bibliothèque `foxguard` (routeur Axum,
//! `Config::load`, ...) sans dupliquer de code.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use foxguard_camera::api;
use foxguard_camera::capture::{self, ClipRecorder, SharedState};
use foxguard_camera::config::Config;
use foxguard_camera::h264::H264Stream;
use foxguard_camera::retention;
use foxguard_camera::rtsp;

/// Fichier de configuration de la caméra, relatif au répertoire de travail.
///
/// `cargo run -p foxguard-camera` exécute le binaire depuis la RACINE du
/// workspace : c'est donc là que le fichier est attendu en développement, et
/// à la racine de `/app` dans l'image Docker. Le préfixe `camera-` le
/// distingue de `manager-config.toml`, le dépôt hébergeant plusieurs
/// composants.
const CAMERA_CONFIG_PATH: &str = "camera-config.toml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Affichage de la bannière console et de la version.
    // Volontairement en `println!` : c'est un affichage de démarrage destiné
    // à la console, pas un événement à journaliser, filtrer ou horodater.
    print_banner();

    init_tracing();

    info!("🚀 Démarrage du système FoxGuard...");

    // Chargement de la configuration
    let config = Config::load(CAMERA_CONFIG_PATH)?;

    // Flux H.264 partagé par tous ses consommateurs : serveur RTSP, WebSocket
    // des interfaces web, enregistrements. Toujours créé — c'est le seul
    // chemin vidéo de la caméra — mais rien n'y est encodé tant que personne
    // n'est abonné.
    let h264 = Arc::new(H264Stream::new());

    // Initialisation de l'état partagé
    let state = Arc::new(SharedState {
        detection_enabled: AtomicBool::new(config.detection.enabled),
        recording_enabled: AtomicBool::new(false),
        h264: Arc::clone(&h264),
        api_token: config.server.api_token.clone(),
        pending_enrollment: Mutex::new(None),
        recordings_dir: config.recording.dir.clone(),
    });

    // Enregistreur de clips d'événement, partagé entre la boucle de capture
    // (qui l'alimente en frames) et le thread de reconnaissance (qui
    // déclenche les clips). Voir `capture::clips`.
    let clips = Arc::new(Mutex::new(ClipRecorder::new(&config.recording)));

    // Serveur RTSP : un abonné du flux parmi d'autres (voir `crate::rtsp`).
    // Optionnel, contrairement à l'encodage : ne pas l'activer n'économise
    // qu'un port ouvert sur le réseau.
    if config.rtsp.enabled {
        rtsp::spawn(
            config.rtsp.clone(),
            config.server.api_token.clone(),
            config.camera.name.clone(),
            Arc::clone(&h264),
        );
    }

    // Lancement de la boucle de capture caméra dans une tâche blocking
    let camera_config = config.clone();
    let camera_state = Arc::clone(&state);
    let camera_clips = Arc::clone(&clips);
    let camera_h264 = Arc::clone(&h264);

    tokio::task::spawn_blocking(move || {
        if let Err(e) =
            capture::start_camera_loop(camera_config, camera_state, camera_clips, camera_h264)
        {
            error!("❌ Erreur critique dans la caméra : {}", e);
        }
    });

    // Nettoyage automatique des enregistrements trop anciens (voir
    // `[recording]` dans config.toml). Démarré avant le serveur pour qu'un
    // premier passage ait lieu dès le lancement : après un arrêt prolongé,
    // les fichiers périmés sont purgés sans attendre le premier intervalle.
    retention::spawn_cleanup_task(config.recording.clone());

    // Configuration et lancement du serveur HTTP / WebSocket (Axum)
    let app = api::create_router(Arc::clone(&state));

    let bind_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("Adresse IP ou Port invalides");

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    info!("🚀 Serveur démarré sur http://{}", bind_addr);

    // Transmet l'adresse SocketAddr aux extracteurs ConnectInfo
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Initialise la journalisation.
///
/// Tout le code applicatif utilise `tracing` plutôt que `println!` /
/// `eprintln!`, pour deux raisons :
///
/// 1. FILTRAGE PAR NIVEAU. Les diagnostics par visage détecté, par personne
///    suivie et par fenêtre de scan sont précieux pour régler les seuils de
///    `[detection]`, mais ils noient la console en fonctionnement normal. Ils
///    sont donc en `debug!` : muets par défaut, réactivables à la demande avec
///    `RUST_LOG=foxguard_camera=debug`, sans recompiler.
///
/// 2. CODE PARALLÈLE. `println!` et `eprintln!` écrivent sur stdout/stderr,
///    protégés par un verrou global : les tâches Rayon du pipeline de vision
///    se sérialisaient sur leurs propres messages de diagnostic.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("foxguard_camera=info,warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

// Affiche le logo ASCII et la version dans la console au démarrage.
fn print_banner() {
    let version = env!("CARGO_PKG_VERSION");

    println!(
        r#"
███████╗  ██████╗  ██╗  ██╗  ██████╗  ██╗   ██╗   █████╗  ██████╗   ██████╗   
██╔════╝ ██╔═══██╗ ╚██╗██╔╝ ██╔════╝  ██║   ██║  ██╔══██╗ ██╔══██╗  ██╔══██╗  
█████╗   ██║   ██║  ╚███╔╝  ██║  ███╗ ██║   ██║  ███████║ ██████╔╝  ██║  ██║  
██╔══╝   ██║   ██║  ██╔██╗  ██║   ██║ ██║   ██║  ██╔══██║ ██╔══██╗  ██║  ██║  
██║      ╚██████╔╝ ██╔╝ ██╗ ╚██████╔╝ ╚██████╔╝  ██║  ██║ ██║  ██║  ██████╔╝  
╚═╝       ╚═════╝  ╚═╝  ╚═╝  ╚═════╝   ╚═════╝   ╚═╝  ╚═╝ ╚═╝  ╚═╝  ╚═════╝   
        "#
    );
    println!("=========================================================================");
    println!(" 🦊 FoxGuard - Système de Vidéosurveillance Intelligent");
    println!(" 📦 Version  : v{}", version);
    println!("=========================================================================\n");
}
