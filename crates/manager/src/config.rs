//! Chargement et structures de configuration du manager (`manager-config.toml`).

use serde::Deserialize;

/// Racine de la configuration du manager.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub store: StoreConfig,
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

/// Conservation des événements en mémoire.
#[derive(Debug, Clone, Deserialize)]
pub struct StoreConfig {
    /// Nombre maximal d'événements conservés. Au-delà, les plus anciens sont
    /// oubliés.
    ///
    /// Le stockage est VOLATILE : tout est perdu au redémarrage. C'est un
    /// choix assumé pour cette première version — la persistance (SQLite ou
    /// autre) viendra quand le besoin sera précisé.
    #[serde(default = "default_capacity")]
    pub capacity: usize,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            capacity: default_capacity(),
        }
    }
}

fn default_capacity() -> usize {
    1000
}

impl Config {
    /// Charge et parse `manager-config.toml` (ou un autre chemin TOML).
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(toml::from_str(&content)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const MINIMAL_TOML: &str = r#"
        [mqtt]
        broker_host = "192.168.1.50"
    "#;

    fn write_temp_toml(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("fichier temporaire");
        file.write_all(content.as_bytes()).expect("écriture");
        file
    }

    #[test]
    fn only_the_broker_host_is_required() {
        let file = write_temp_toml(MINIMAL_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config minimale valide");

        assert_eq!(config.mqtt.broker_host, "192.168.1.50");
        assert_eq!(config.mqtt.broker_port, 1883);
        assert_eq!(config.mqtt.topic, "foxguard/detections");
        assert_eq!(config.server.port, 8090);
        assert_eq!(config.store.capacity, 1000);
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

            [store]
            capacity = 50
        "#;
        let file = write_temp_toml(toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.server.port, 9000);
        assert_eq!(config.server.ui_dir, "/srv/foxguard-ui");
        assert_eq!(config.mqtt.broker_port, 8883);
        assert_eq!(config.mqtt.topic, "maison/+/detections");
        assert_eq!(config.store.capacity, 50);
    }

    #[test]
    fn load_fails_on_a_missing_file() {
        assert!(Config::load("/chemin/inexistant/manager-config.toml").is_err());
    }
}
