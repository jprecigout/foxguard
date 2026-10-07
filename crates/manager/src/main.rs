//! Point d'entrée de `foxguard-manager`.
//!
//! Le manager tourne sur un SERVEUR ANNEXE (pas sur le Raspberry Pi). Il
//! s'abonne au broker MQTT sur lequel les caméras publient leurs événements
//! de détection, les conserve dans PostgreSQL (voir [`EventRepository`]) et
//! les expose par une API HTTP, qui sert aussi l'interface React (voir
//! `ui/`).
//!
//! ÉTAT : la chaîne caméra → MQTT → manager → PostgreSQL → HTTP → interface
//! marche de bout en bout, vignettes et clips des détections compris, ainsi
//! que le DIRECT de chaque caméra. L'API reste en LECTURE SEULE, et le direct
//! n'y transite pas : le manager signe des tickets à portée limitée (voir
//! `foxguard_protocol::ticket`), avec lesquels le navigateur s'adresse à la
//! caméra elle-même. Le pilotage complet d'une caméra passe par son interface
//! embarquée, qui doit rester le secours disponible quand ce serveur est en
//! panne. Restent à construire : les notifications.
//!
//! # Sous-commande
//!
//! `foxguard-manager hash-password` lit un mot de passe sur l'entrée standard
//! et affiche son hachage, à copier dans `[auth] password_hash`.

use std::net::SocketAddr;
use std::sync::Arc;

use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use foxguard_manager::api::{self, AppState};
use foxguard_manager::auth::{self, Authenticator};
use foxguard_manager::config::Config;
use foxguard_manager::db::EventRepository;
use foxguard_manager::{ingest, retention};

/// Fichier de configuration du manager, relatif au répertoire de travail
/// (voir la note équivalente dans `foxguard-camera`).
const MANAGER_CONFIG_PATH: &str = "manager-config.toml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("hash-password") {
        return print_password_hash();
    }

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

    // Section `[auth]` présente et complète : vérifié par `Config::load`.
    let authenticator = match config.auth.as_ref() {
        Some(auth) if !auth.disabled => Some(Arc::new(Authenticator::new(
            &auth.username,
            &auth.password_hash,
        )?)),
        _ => {
            warn!(
                "⚠️ Authentification DÉSACTIVÉE (`[auth] disabled = true`) : quiconque \
                 atteint ce port voit l'historique, les directs et les interrupteurs des caméras."
            );
            None
        }
    };

    let state = Arc::new(AppState {
        repository: Arc::clone(&repository),
        authenticator,
        stream_ticket_secret: Some(config.stream.ticket_secret.clone())
            .filter(|secret| !secret.is_empty()),
    });
    let app = api::create_router(state, &config.server.ui_dir);

    let bind_addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("Adresse IP ou Port invalides");

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;

    info!("🚀 Manager démarré sur http://{bind_addr}");
    info!("   Interface servie depuis « {} »", config.server.ui_dir);

    // Avec l'adresse du client : c'est elle que compte la limitation des
    // échecs d'authentification (voir `foxguard_protocol::auth`).
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// `hash-password` : hache le mot de passe lu sur l'entrée standard.
///
/// Sur l'entrée standard et non en argument : un argument finirait dans
/// l'historique du shell et dans la liste des processus.
fn print_password_hash() -> anyhow::Result<()> {
    eprintln!("Mot de passe (fin de saisie : Entrée) :");

    let mut password = String::new();
    std::io::stdin().read_line(&mut password)?;
    let password = password.trim_end_matches(['\r', '\n']);

    if password.is_empty() {
        anyhow::bail!("mot de passe vide");
    }

    println!("{}", auth::hash_password(password)?);
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
