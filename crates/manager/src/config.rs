//! Chargement et structures de configuration du manager (`manager-config.toml`).

use serde::Deserialize;

/// Racine de la configuration du manager.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub mqtt: MqttConfig,
    pub database: DatabaseConfig,
    #[serde(default)]
    pub stream: StreamConfig,
    /// OBLIGATOIRE, même pour la désactiver explicitement : voir
    /// [`AuthConfig`].
    pub auth: Option<AuthConfig>,
}

/// Authentification de l'interface et de l'API (voir `crate::auth`).
///
/// La section est OBLIGATOIRE : le manager refuse de démarrer sans elle. Un
/// manager ouvert donne l'historique de toutes les caméras, leurs directs et
/// leurs interrupteurs à quiconque atteint son port — cela ne doit pas
/// pouvoir arriver par simple oubli. La désactiver reste possible, mais il
/// faut l'ÉCRIRE (`disabled = true`).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AuthConfig {
    /// Désactive l'authentification. À réserver à un manager qui n'est
    /// joignable que derrière un proxy qui authentifie lui-même.
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub username: String,
    /// Hachage Argon2id du mot de passe, produit par
    /// `foxguard-manager hash-password`. Jamais le mot de passe en clair.
    #[serde(default)]
    pub password_hash: String,
}

/// Direct des caméras dans l'interface (section optionnelle).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct StreamConfig {
    /// Secret partagé avec les caméras, avec lequel le manager signe des
    /// tickets de visionnage (voir `foxguard_protocol::stream_ticket`).
    ///
    /// Il doit être IDENTIQUE à `[server] stream_ticket_secret` de chaque
    /// caméra. Le manager ne connaît ainsi le jeton d'API d'aucune caméra :
    /// un ticket n'ouvre que le flux vidéo, en lecture seule, pour une caméra
    /// et quelques minutes.
    ///
    /// Vide (le défaut), l'interface ne propose pas de direct.
    #[serde(default)]
    pub ticket_secret: String,
}

/// Connexion PostgreSQL. Section OBLIGATOIRE : le manager n'a pas de mode
/// dégradé sans base, il perdrait silencieusement tout ce qu'il reçoit.
#[derive(Debug, Clone, Deserialize)]
pub struct DatabaseConfig {
    /// URL de connexion, ex :
    /// `postgres://foxguard:motdepasse@postgres:5432/foxguard`.
    ///
    /// Peut être laissée vide ici et fournie par la variable d'environnement
    /// `DATABASE_URL` (voir [`Config::load`]) : c'est la forme habituelle en
    /// conteneur, et elle évite d'écrire un mot de passe dans un fichier.
    #[serde(default)]
    pub url: String,

    /// Taille maximale du pool de connexions.
    #[serde(default = "default_max_connections")]
    pub max_connections: u32,

    /// Durée de conservation des événements, en jours. Une tâche de fond
    /// supprime les plus anciens.
    ///
    /// **`0` désactive la purge** : aucun événement n'est alors jamais
    /// effacé (même garde-fou que `[recording] retention_days` côté caméra).
    #[serde(default = "default_event_retention_days")]
    pub retention_days: u32,
}

fn default_max_connections() -> u32 {
    5
}

fn default_event_retention_days() -> u32 {
    90
}

/// Serveur HTTP : API de consultation et, à terme, service du bundle React
/// (voir `ui/`).
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    /// Dossier statique servi à la racine : le bundle produit par `ui/`.
    /// Tant que l'interface React n'existe pas, ce dossier est simplement
    /// absent et les requêtes retournent 404.
    #[serde(default = "default_ui_dir")]
    pub ui_dir: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            ui_dir: default_ui_dir(),
        }
    }
}

fn default_host() -> String {
    "0.0.0.0".to_string()
}

fn default_port() -> u16 {
    8090
}

fn default_ui_dir() -> String {
    "ui/dist".to_string()
}

/// Connexion au broker MQTT sur lequel les caméras publient leurs
/// événements. C'est la seule section obligatoire : sans broker, le manager
/// n'a rien à agréger.
#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    pub broker_host: String,
    #[serde(default = "default_mqtt_port")]
    pub broker_port: u16,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    /// Topic (ou motif d'abonnement) sur lequel écouter. Doit correspondre à
    /// ce que publient les caméras (`[mqtt] topic` de leur configuration).
    #[serde(default = "default_mqtt_topic")]
    pub topic: String,
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_mqtt_topic() -> String {
    "foxguard/detections".to_string()
}

/// Nom de la variable d'environnement qui, si elle est renseignée, prend le
/// pas sur `[database] url` du fichier de configuration.
pub const DATABASE_URL_ENV: &str = "DATABASE_URL";

impl Config {
    /// Charge et parse `manager-config.toml` (ou un autre chemin TOML), puis
    /// applique les surcharges par variables d'environnement.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let mut config: Config = toml::from_str(&content)?;
        config.apply_env_overrides();

        if config.database.url.is_empty() {
            anyhow::bail!(
                "URL de base de données absente : renseignez `[database] url` \
                 ou la variable d'environnement {DATABASE_URL_ENV}"
            );
        }

        foxguard_protocol::ticket::validate_secret(&config.stream.ticket_secret)
            .map_err(|e| anyhow::anyhow!("[stream] ticket_secret : {e}"))?;

        match &config.auth {
            None => anyhow::bail!(
                "section `[auth]` absente : renseignez `username` et `password_hash` \
                 (généré par `foxguard-manager hash-password`), ou écrivez \
                 explicitement `disabled = true` pour laisser le manager ouvert"
            ),
            Some(auth)
                if !auth.disabled
                    && (auth.username.is_empty() || auth.password_hash.is_empty()) =>
            {
                anyhow::bail!(
                    "`[auth]` incomplète : `username` et `password_hash` sont requis \
                     (ou `disabled = true`)"
                )
            }
            Some(_) => {}
        }

        Ok(config)
    }

    /// Applique les surcharges par variables d'environnement. Seule l'URL de
    /// base est concernée : c'est la seule donnée de la configuration qui
    /// porte un secret (le mot de passe y est inclus), et celle qu'on ne veut
    /// pas voir traîner en clair dans un fichier. Une variable vide est
    /// ignorée, pour qu'un environnement où elle est déclarée mais non
    /// renseignée n'efface pas la valeur du fichier.
    fn apply_env_overrides(&mut self) {
        if let Ok(url) = std::env::var(DATABASE_URL_ENV)
            && !url.is_empty()
        {
            self.database.url = url;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // `[database]` en DERNIER : certains tests y ajoutent des clés à la
    // fin du texte.
    const MINIMAL_TOML: &str = r#"
        [auth]
        disabled = true

        [mqtt]
        broker_host = "192.168.1.50"

        [database]
        url = "postgres://foxguard:secret@localhost/foxguard"
    "#;

    fn write_temp_toml(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("fichier temporaire");
        file.write_all(content.as_bytes()).expect("écriture");
        file
    }

    #[test]
    fn only_the_broker_and_the_database_are_required() {
        let file = write_temp_toml(MINIMAL_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config minimale valide");

        assert_eq!(config.mqtt.broker_host, "192.168.1.50");
        assert_eq!(config.mqtt.broker_port, 1883);
        assert_eq!(config.mqtt.topic, "foxguard/detections");
        assert_eq!(config.server.port, 8090);
        assert_eq!(config.database.max_connections, 5);
        assert_eq!(config.database.retention_days, 90);
    }

    #[test]
    fn a_missing_database_url_is_rejected() {
        // Sans base, le manager perdrait silencieusement tout ce qu'il
        // reçoit : mieux vaut refuser de démarrer.
        let file = write_temp_toml("[mqtt]\nbroker_host = \"h\"\n\n[database]\n");
        let err = Config::load(file.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("base de données"), "{err}");
    }

    #[test]
    fn the_database_url_env_var_overrides_the_file_unless_empty() {
        // Un seul test pour les deux cas : `set_var` agit sur tout le
        // processus, or les tests tournent en parallèle.
        let base: Config = toml::from_str(MINIMAL_TOML).expect("TOML valide");

        let mut config = base.clone();
        unsafe { std::env::set_var(DATABASE_URL_ENV, "postgres://depuis/env") };
        config.apply_env_overrides();
        assert_eq!(config.database.url, "postgres://depuis/env");

        let mut config = base.clone();
        let from_file = config.database.url.clone();
        unsafe { std::env::set_var(DATABASE_URL_ENV, "") };
        config.apply_env_overrides();
        assert_eq!(config.database.url, from_file);

        let mut config = base;
        unsafe { std::env::remove_var(DATABASE_URL_ENV) };
        config.apply_env_overrides();
        assert_eq!(config.database.url, from_file);
    }

    #[test]
    fn a_retention_of_zero_is_accepted_and_means_disabled() {
        let toml = format!("{MINIMAL_TOML}\nretention_days = 0\n");
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");
        assert_eq!(config.database.retention_days, 0);
    }

    #[test]
    fn a_missing_broker_host_is_rejected() {
        // Sans broker, le manager n'a rien à agréger : mieux vaut échouer au
        // démarrage que tourner en silence sans jamais rien recevoir.
        let file = write_temp_toml("[mqtt]\n");
        assert!(Config::load(file.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn every_section_can_be_overridden() {
        let toml = r#"
            [server]
            port = 9000
            ui_dir = "/srv/foxguard-ui"

            [mqtt]
            broker_host = "broker.local"
            broker_port = 8883
            topic = "maison/+/detections"

            [database]
            url = "postgres://u:p@db/foxguard"
            max_connections = 20
            retention_days = 30

            [auth]
            username = "admin"
            password_hash = "$argon2id$v=19$m=19456,t=2,p=1$c2VsZGVzZWxkZXNlbA$aGFjaGFnZWhhY2hhZ2VoYWNoYWdl"
        "#;
        let file = write_temp_toml(toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.server.port, 9000);
        assert_eq!(config.server.ui_dir, "/srv/foxguard-ui");
        assert_eq!(config.mqtt.broker_port, 8883);
        assert_eq!(config.mqtt.topic, "maison/+/detections");
        assert_eq!(config.database.max_connections, 20);
        assert_eq!(config.database.retention_days, 30);
    }

    #[test]
    fn a_missing_auth_section_is_rejected() {
        // Un manager ouvert par simple OUBLI donnerait toutes les caméras à
        // quiconque atteint son port.
        let toml = MINIMAL_TOML.replace("[auth]\n        disabled = true", "");
        let file = write_temp_toml(&toml);
        let err = Config::load(file.path().to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("[auth]"), "{err}");
    }

    #[test]
    fn an_incomplete_auth_section_is_rejected() {
        let toml = MINIMAL_TOML.replace("disabled = true", "username = \"admin\"");
        let file = write_temp_toml(&toml);
        assert!(Config::load(file.path().to_str().unwrap()).is_err());
    }

    #[test]
    fn load_fails_on_a_missing_file() {
        assert!(Config::load("/chemin/inexistant/manager-config.toml").is_err());
    }
}
