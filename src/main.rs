//! Point d'entrée de FoxGuard : charge la configuration, démarre la boucle
//! de capture caméra (thread bloquant) et le serveur HTTP / WebSocket (Axum)
//! qui sert l'interface de contrôle et le flux vidéo.
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
use tokio::sync::broadcast;

use foxguard::api;
use foxguard::capture::{self, SharedState};
use foxguard::config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Affichage de la bannière console et de la version
    print_banner();

    println!("🚀 Démarrage du système FoxGuard...");

    // Chargement de la configuration
    let config = Config::load("config.toml")?;

    // Création du canal Broadcast pour le flux vidéo WebSocket (capacité de 16 frames)
    let (tx, _) = broadcast::channel(16);

    // Initialisation de l'état partagé
    let state = Arc::new(SharedState {
        detection_enabled: AtomicBool::new(config.detection.enabled),
        recording_enabled: AtomicBool::new(false),
        tx,
        api_token: config.server.api_token.clone(),
        pending_enrollment: Mutex::new(None),
    });

    // Lancement de la boucle de capture caméra dans une tâche blocking
    let camera_config = config.clone();
    let camera_state = Arc::clone(&state);

    tokio::task::spawn_blocking(move || {
        if let Err(e) = capture::start_camera_loop(camera_config, camera_state) {
            eprintln!("❌ Erreur critique dans la caméra : {}", e);
        }
    });

    // Configuration et lancement du serveur HTTP / WebSocket (Axum)
    let app = api::create_router(Arc::clone(&state));

    let bind_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("Adresse IP ou Port invalides");

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    println!("🚀 Serveur démarré sur http://{}", bind_addr);

    // Transmet l'adresse SocketAddr aux extracteurs ConnectInfo
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
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
