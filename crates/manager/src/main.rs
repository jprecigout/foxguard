//! Point d'entrée de `foxguard-manager`.
//!
//! Le manager tourne sur un SERVEUR ANNEXE (pas sur le Raspberry Pi). Il
//! s'abonne au broker MQTT sur lequel les caméras publient leurs événements
//! de détection, conserve un historique en mémoire, et l'expose par une API
//! HTTP destinée à l'interface React (voir `ui/`).
//!
//! ÉTAT : squelette fonctionnel. La chaîne complète caméra → MQTT → manager →
//! HTTP marche de bout en bout, mais l'historique est volatile (voir
//! `store`) et l'API se limite à la consultation. C'est la base sur laquelle
//! greffer la persistance, les notifications et l'interface.

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::info;
use tracing_subscriber::EnvFilter;

use foxguard_manager::api::{self, AppState};
use foxguard_manager::config::Config;
use foxguard_manager::ingest;
use foxguard_manager::store::EventStore;

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

    let store = Arc::new(EventStore::new(config.store.capacity));

    // Réception des événements des caméras, en tâche de fond.
    ingest::spawn(config.mqtt.clone(), Arc::clone(&store));

    let state = Arc::new(AppState {
        store: Arc::clone(&store),
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
