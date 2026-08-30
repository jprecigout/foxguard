mod api;
mod camera;
mod config;
mod mail;
mod vision;

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use tokio::sync::broadcast;

use camera::SharedState;
use config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Affichage de la bannière console et de la version
    print_banner();

    println!("🚀 Démarrage du système FoxGuard...");

    // 1. Chargement de la configuration
    let config = Config::load("config.toml")?;

    // 2. Création du canal Broadcast pour le flux vidéo WebSocket (capacité de 16 frames)
    let (tx, _) = broadcast::channel(16);

    // 3. Initialisation de l'état partagé
    let state = Arc::new(SharedState {
        detection_enabled: AtomicBool::new(config.detection.enabled),
        recording_enabled: AtomicBool::new(false),
        tx,
        api_token: config.server.api_token.clone(),
    });

    // 4. Lancement de la boucle de capture caméra dans une tâche blocking
    // (Conserve le contexte Tokio nécessaire pour l'envoi des e-mails)
    let camera_config = config.clone();
    let camera_state = Arc::clone(&state);

    tokio::task::spawn_blocking(move || {
        if let Err(e) = camera::start_camera_loop(camera_config, camera_state) {
            eprintln!("❌ Erreur critique dans la caméra : {}", e);
        }
    });

    // 5. Configuration et lancement du serveur HTTP / WebSocket (Axum)
    let app = api::create_router(Arc::clone(&state));

    let bind_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("Adresse IP ou Port invalides");

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    println!("🚀 Serveur démarré sur http://{}", bind_addr);

    // transmet l'adresse SocketAddr aux extracteurs ConnectInfo
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

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
    println!(" 🦊 FoxGuard - Système de Vidéosurveillance IA");
    println!(" 📦 Version  : v{}", version);
    println!("=========================================================================\n");
}
