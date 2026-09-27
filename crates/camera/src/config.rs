//! Chargement et structures de configuration (fichier `camera-config.toml`).

use serde::Deserialize;

/// Dossier des enregistrements vidéo par défaut, relatif au répertoire de
/// travail. Surchargeable par `[recording] dir` (voir [`RecordingConfig`]).
pub const DEFAULT_RECORDINGS_DIR: &str = "output_record";

/// Racine de la configuration, telle que lue depuis `camera-config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    pub server: ServerConfig,
    pub camera: CameraConfig,
    pub detection: DetectionConfig,
    pub email: EmailConfig,
    // Section entière optionnelle : un `camera-config.toml` existant sans `[mqtt]`
    // continue de charger tel quel, avec la publication MQTT désactivée.
    #[serde(default)]
    pub mqtt: MqttConfig,
    // Section entière optionnelle : rétention des enregistrements.
    #[serde(default)]
    pub recording: RecordingConfig,
}

/// Paramètres des enregistrements vidéo : durée de conservation et fréquence
/// du nettoyage automatique (voir [`crate::retention`]). Section entièrement
/// optionnelle, pour qu'un `camera-config.toml` existant reste valide tel quel.
#[derive(Debug, Clone, Deserialize)]
pub struct RecordingConfig {
    /// Dossier où sont écrits, listés et purgés les enregistrements.
    ///
    /// Relatif au répertoire de travail par défaut. Le renseigner en ABSOLU
    /// est recommandé pour un déploiement en conteneur ou en service systemd,
    /// où ce répertoire n'est pas celui du dépôt.
    #[serde(default = "default_recordings_dir")]
    pub dir: String,

    /// Durée de conservation des enregistrements, en jours. Au-delà, ils sont
    /// supprimés automatiquement par la tâche de nettoyage.
    ///
    /// **`0` désactive entièrement la suppression automatique** : aucun
    /// fichier n'est alors jamais effacé. C'est la valeur à mettre pour gérer
    /// la rétention soi-même (script externe, politique de sauvegarde), et
    /// c'est aussi le garde-fou qui évite qu'une valeur mal saisie soit
    /// interprétée comme « tout supprimer immédiatement ».
    #[serde(default = "default_retention_days")]
    pub retention_days: u64,

    /// Intervalle entre deux passages de nettoyage, en secondes. Un passage a
    /// aussi lieu au démarrage de l'application, pour que les fichiers
    /// périmés accumulés pendant un arrêt prolongé soient traités sans
    /// attendre le premier intervalle.
    #[serde(default = "default_cleanup_interval_secs")]
    pub cleanup_interval_secs: u64,
}

fn default_recordings_dir() -> String {
    DEFAULT_RECORDINGS_DIR.to_string()
}

impl Default for RecordingConfig {
    fn default() -> Self {
        Self {
            dir: default_recordings_dir(),
            retention_days: default_retention_days(),
            cleanup_interval_secs: default_cleanup_interval_secs(),
        }
    }
}

fn default_retention_days() -> u64 {
    7
}

fn default_cleanup_interval_secs() -> u64 {
    // Une heure : la rétention se compte en jours, inutile de balayer le
    // dossier plus souvent.
    3600
}

/// Paramètres du serveur HTTP / WebSocket (Axum).
#[derive(Debug, Clone, Deserialize)]
pub struct ServerConfig {
    // Adresse d'écoute, ex : "0.0.0.0"
    pub host: String,
    // Port d'écoute, ex : 8080
    pub port: u16,
    // Jeton exigé en paramètre `?token=` pour se connecter au WebSocket
    pub api_token: String,
}

/// Paramètres de la caméra V4L2.
#[derive(Debug, Clone, Deserialize)]
pub struct CameraConfig {
    // Index du périphérique vidéo (ex : 0 pour /dev/video0)
    #[serde(default)]
    pub device_index: usize,
    // Nom de la caméra, inclus dans les événements MQTT (voir `[mqtt]` et
    // `crate::mqtt`) pour distinguer plusieurs installations FoxGuard.
    #[serde(default = "default_camera_name")]
    pub name: String,
}

fn default_camera_name() -> String {
    "foxguard".to_string()
}

/// Paramètres de détection (YOLO, YuNet, ArcFace) et des alertes associées.
#[derive(Debug, Clone, Deserialize)]
pub struct DetectionConfig {
    // Surveillance IA activée ou non au démarrage (bascule aussi via l'UI web)
    #[serde(default)]
    pub enabled: bool,
    // Chemin du modèle ONNX YOLOv8 (détection personne/chat/chien)
    pub model_path: String,
    // Chemin du modèle ONNX YuNet (détection de visage)
    pub model_detect_face_path: String,
    // Chemin du modèle ONNX ArcFace / MobileFaceNet (empreinte faciale)
    pub model_face_path: String,
    // Taille d'entrée (carrée) du modèle YOLO, en pixels
    pub input_size: u32,
    // Taille d'entrée (carrée) du modèle ArcFace, en pixels (112 attendu)
    pub input_face_size: u32,
    // Score de confiance minimal pour retenir une détection YOLO
    pub confidence_threshold: f32,
    // Délai minimal entre deux e-mails d'alerte, en secondes
    pub email_cooldown_secs: u64,
}

/// Paramètres d'envoi des e-mails d'alerte (SMTP).
#[derive(Debug, Clone, Deserialize)]
pub struct EmailConfig {
    // Envoi d'e-mails d'alerte activé ou non
    #[serde(default)]
    pub enabled: bool,
    pub smtp_server: String,
    pub smtp_user: String,
    pub smtp_password: String,
    pub from_address: String,
    pub to_address: String,
}

/// Paramètres de publication des événements de détection sur un broker MQTT
/// (voir `crate::mqtt`) : fonctionnalité optionnelle, désactivée par défaut.
/// Tous les champs ont une valeur par défaut, y compris la section `[mqtt]`
/// elle-même (voir [`Config::mqtt`]), pour qu'un `camera-config.toml` existant
/// n'ait pas besoin d'être modifié pour rester valide.
#[derive(Debug, Clone, Deserialize)]
pub struct MqttConfig {
    #[serde(default)]
    pub enabled: bool,
    // Hôte du broker MQTT (ex : "192.168.1.50"). Requis si `enabled = true`
    // (sinon la connexion échoue simplement, journalisée en boucle par le
    // client MQTT en tâche de fond, voir `crate::mqtt::MqttPublisher`).
    #[serde(default)]
    pub broker_host: String,
    #[serde(default = "default_mqtt_port")]
    pub broker_port: u16,
    // Identifiants optionnels : laissés vides si le broker n'exige pas
    // d'authentification.
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    // Topic sur lequel publier chaque événement de détection.
    #[serde(default = "default_mqtt_topic")]
    pub topic: String,
}

impl Default for MqttConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            broker_host: String::new(),
            broker_port: default_mqtt_port(),
            username: String::new(),
            password: String::new(),
            topic: default_mqtt_topic(),
        }
    }
}

fn default_mqtt_port() -> u16 {
    1883
}

fn default_mqtt_topic() -> String {
    "foxguard/detections".to_string()
}

impl Config {
    /// Charge et parse `camera-config.toml` (ou un autre chemin TOML) en [`Config`].
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&content)?;
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    // TOML minimal mais complet : couvre les 4 sections requises, y
    // compris les champs sans `#[serde(default)]` (server.*, detection.*
    // hors `enabled`, email.* hors `enabled`).
    const VALID_TOML: &str = r#"
        [server]
        host = "0.0.0.0"
        port = 8080
        api_token = "secret"

        [camera]
        device_index = 2

        [detection]
        enabled = true
        model_path = "src/vision/models/yolov8n.onnx"
        model_detect_face_path = "src/vision/models/face_detection_yunet_2023mar.onnx"
        model_face_path = "src/vision/models/arcface-mobilefacenet.onnx"
        input_size = 640
        input_face_size = 112
        confidence_threshold = 0.5
        email_cooldown_secs = 60

        [email]
        enabled = false
        smtp_server = "smtp.example.com"
        smtp_user = "user@example.com"
        smtp_password = "hunter2"
        from_address = "foxguard@example.com"
        to_address = "me@example.com"
    "#;

    /// Écrit `content` dans un fichier temporaire et retourne son handle
    /// (à garder vivant tant que le chemin est utilisé : le fichier est
    /// supprimé au `Drop`).
    fn write_temp_toml(content: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().expect("création du fichier temporaire");
        file.write_all(content.as_bytes())
            .expect("écriture du TOML temporaire");
        file
    }

    #[test]
    fn recording_section_is_optional_and_defaults_to_seven_days() {
        // Un `camera-config.toml` antérieur à l'ajout de `[recording]` doit rester
        // valide et bénéficier de la rétention par défaut.
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.recording.retention_days, 7);
        assert_eq!(config.recording.cleanup_interval_secs, 3600);
    }

    #[test]
    fn recording_retention_can_be_overridden() {
        let toml = format!("{VALID_TOML}\n[recording]\nretention_days = 30\n");
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.recording.retention_days, 30);
        // Le champ non renseigné garde son défaut.
        assert_eq!(config.recording.cleanup_interval_secs, 3600);
    }

    #[test]
    fn a_retention_of_zero_is_accepted_and_means_disabled() {
        // `0` est une valeur VALIDE et significative : elle désactive la
        // suppression automatique (voir `crate::retention`). Elle ne doit
        // donc pas être rejetée ni remplacée par le défaut.
        let toml = format!("{VALID_TOML}\n[recording]\nretention_days = 0\n");
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.recording.retention_days, 0);
    }

    #[test]
    fn load_parses_a_valid_config() {
        let file = write_temp_toml(VALID_TOML);

        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.server.host, "0.0.0.0");
        assert_eq!(config.server.port, 8080);
        assert_eq!(config.server.api_token, "secret");
        assert_eq!(config.camera.device_index, 2);
        assert!(config.detection.enabled);
        assert_eq!(config.detection.input_size, 640);
        assert_eq!(config.detection.input_face_size, 112);
        assert!((config.detection.confidence_threshold - 0.5).abs() < f32::EPSILON);
        assert_eq!(config.detection.email_cooldown_secs, 60);
        assert!(!config.email.enabled);
        assert_eq!(config.email.to_address, "me@example.com");

        // `VALID_TOML` n'a ni `camera.name` ni de section `[mqtt]` : les
        // deux doivent tomber sur leurs valeurs par défaut plutôt que de
        // faire échouer le chargement (voir les deux tests dédiés
        // ci-dessous pour le détail de ces valeurs par défaut).
        assert_eq!(config.camera.name, "foxguard");
        assert!(!config.mqtt.enabled);
    }

    #[test]
    fn load_applies_serde_defaults_when_optional_fields_are_absent() {
        // `camera.device_index`, `detection.enabled` et `email.enabled` sont
        // tous `#[serde(default)]` : ils doivent tomber à leur valeur par
        // défaut (0 / false) quand ils sont omis, plutôt que de faire
        // échouer le parsing.
        let toml_without_defaults = r#"
            [server]
            host = "127.0.0.1"
            port = 1234
            api_token = "t"

            [camera]

            [detection]
            model_path = "a"
            model_detect_face_path = "b"
            model_face_path = "c"
            input_size = 320
            input_face_size = 112
            confidence_threshold = 0.4
            email_cooldown_secs = 30

            [email]
            smtp_server = "s"
            smtp_user = "u"
            smtp_password = "p"
            from_address = "f"
            to_address = "t"
        "#;
        let file = write_temp_toml(toml_without_defaults);

        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.camera.device_index, 0);
        assert!(!config.detection.enabled);
        assert!(!config.email.enabled);
    }

    #[test]
    fn load_fails_on_missing_file() {
        let result = Config::load("/chemin/qui/nexiste/vraiment/pas/config.toml");
        assert!(result.is_err());
    }

    #[test]
    fn load_fails_on_malformed_toml() {
        let file = write_temp_toml("ceci n'est pas === du toml valide [[[");
        let result = Config::load(file.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn load_fails_when_a_required_field_is_missing() {
        // `[server]` sans `api_token`, qui n'a pas de valeur par défaut.
        let incomplete = r#"
            [server]
            host = "0.0.0.0"
            port = 8080

            [camera]

            [detection]
            model_path = "a"
            model_detect_face_path = "b"
            model_face_path = "c"
            input_size = 320
            input_face_size = 112
            confidence_threshold = 0.4
            email_cooldown_secs = 30

            [email]
            smtp_server = "s"
            smtp_user = "u"
            smtp_password = "p"
            from_address = "f"
            to_address = "t"
        "#;
        let file = write_temp_toml(incomplete);
        let result = Config::load(file.path().to_str().unwrap());
        assert!(result.is_err());
    }

    #[test]
    fn mqtt_section_is_entirely_optional_and_defaults_to_disabled() {
        // Un `camera-config.toml` d'avant l'ajout de MQTT, sans section `[mqtt]`
        // du tout, doit continuer à charger tel quel.
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(!config.mqtt.enabled);
        assert_eq!(config.mqtt.broker_port, 1883);
        assert_eq!(config.mqtt.topic, "foxguard/detections");
        assert!(config.mqtt.broker_host.is_empty());
    }

    #[test]
    fn mqtt_section_parses_explicit_values() {
        let toml_with_mqtt = format!(
            r#"
                {VALID_TOML}

                [mqtt]
                enabled = true
                broker_host = "192.168.1.50"
                broker_port = 8883
                username = "foxguard"
                password = "hunter2"
                topic = "maison/foxguard/detections"
            "#
        );
        let file = write_temp_toml(&toml_with_mqtt);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(config.mqtt.enabled);
        assert_eq!(config.mqtt.broker_host, "192.168.1.50");
        assert_eq!(config.mqtt.broker_port, 8883);
        assert_eq!(config.mqtt.username, "foxguard");
        assert_eq!(config.mqtt.password, "hunter2");
        assert_eq!(config.mqtt.topic, "maison/foxguard/detections");
    }

    #[test]
    fn camera_name_can_be_set_explicitly() {
        let toml_with_name = r#"
            [server]
            host = "0.0.0.0"
            port = 8080
            api_token = "secret"

            [camera]
            name = "salon"

            [detection]
            model_path = "a"
            model_detect_face_path = "b"
            model_face_path = "c"
            input_size = 320
            input_face_size = 112
            confidence_threshold = 0.4
            email_cooldown_secs = 30

            [email]
            smtp_server = "s"
            smtp_user = "u"
            smtp_password = "p"
            from_address = "f"
            to_address = "t"
        "#;
        let file = write_temp_toml(toml_with_name);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.camera.name, "salon");
    }
}
