//! Point d'entrée de `foxguard-manager`.
//!
//! Le manager tourne sur un SERVEUR ANNEXE (pas sur le Raspberry Pi). Il
//! s'abonne au broker MQTT sur lequel les caméras publient leurs événements
//! de détection, les conserve dans PostgreSQL (voir [`EventRepository`]) et
//! les expose par une API HTTP, qui sert aussi l'interface React (voir
//! `ui/`).
//!
//! ÉTAT : la chaîne caméra → MQTT → manager → PostgreSQL → HTTP → interface
//! marche de bout en bout, vignettes et clips des détections compris. L'API
//! reste en LECTURE SEULE — le pilotage d'une caméra passe par son interface
//! embarquée, qui doit rester le secours disponible quand ce serveur est en
//! panne. Restent à construire : les notifications.

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::info;
use tracing_subscriber::EnvFilter;

use foxguard_manager::api::{self, AppState};
use foxguard_manager::config::Config;
use foxguard_manager::db::EventRepository;
use foxguard_manager::{ingest, retention};

/// Fichier de configuration du manager, relatif au répertoire de travail
/// (voir la note équivalente dans `foxguard-camera`).
const MANAGER_CONFIG_PATH: &str = "manager-config.toml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    info!(
        "🦊 Démarrage de FoxGuard Manager v{}",
        env!("CARGO_PKG_VERSION")
    );

    let config = Config::load(MANAGER_CONFIG_PATH)?;

    // Connexion à PostgreSQL et application des migrations. Volontairement
    // AVANT tout le reste : sans base, le manager perdrait silencieusement
    // tout ce qu'il reçoit, mieux vaut échouer au démarrage.
    let repository = Arc::new(
        EventRepository::connect(&config.database.url, config.database.max_connections).await?,
    );

    // Réception des événements des caméras, en tâche de fond.
    ingest::spawn(config.mqtt.clone(), Arc::clone(&repository));

    // Purge des événements trop anciens.
    retention::spawn_cleanup_task(Arc::clone(&repository), config.database.retention_days);

    let state = Arc::new(AppState {
        repository: Arc::clone(&repository),
    });
    let app = api::create_router(state, &config.server.ui_dir);

    let bind_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("Adresse IP ou Port invalides");

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    info!("🚀 Manager démarré sur http://{bind_addr}");
    info!("   Interface servie depuis « {} »", config.server.ui_dir);

    axum::serve(listener, app).await?;

    Ok(())
}

/// Initialise la journalisation. `RUST_LOG=foxguard_manager=debug` ajoute une
/// ligne par événement reçu.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("foxguard_manager=info,warn"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}
