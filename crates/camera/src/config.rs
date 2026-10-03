//! Chargement et structures de configuration (fichier `camera-config.toml`).

use serde::Deserialize;

/// Dossier des enregistrements vidéo par défaut, relatif au répertoire de
/// travail. Surchargeable par `[recording] dir` (voir [`RecordingConfig`]).
pub const DEFAULT_RECORDINGS_DIR: &str = "output_record";

/// Dossier des photos de référence de la reconnaissance faciale par défaut,
/// relatif au répertoire de travail. Surchargeable par
/// `[detection] known_faces_dir` (voir [`DetectionConfig`]).
pub const DEFAULT_KNOWN_FACES_DIR: &str = "known_faces";

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
    // Section entière optionnelle : pré-filtre de mouvement devant YOLO.
    #[serde(default)]
    pub motion: MotionConfig,
    // Section entière optionnelle : encodage H.264 du flux.
    #[serde(default)]
    pub h264: H264Config,
    // Section entière optionnelle : serveur RTSP.
    #[serde(default)]
    pub rtsp: RtspConfig,
}

impl Config {
    /// Vrai si le flux doit être encodé en H.264.
    ///
    /// `[rtsp] enabled` l'implique : un serveur RTSP sans flux à servir n'a
    /// aucun sens. C'est aussi ce qui garde valide un `camera-config.toml`
    /// antérieur à l'apparition de la section `[h264]`, où le flux RTSP
    /// s'activait à lui seul.
    pub fn h264_enabled(&self) -> bool {
        self.h264.enabled || self.rtsp.enabled
    }
}

/// Pré-filtre de mouvement placé DEVANT l'inférence YOLO (voir
/// [`crate::capture::motion`]).
///
/// Une caméra de surveillance regarde une scène immobile l'immense majorité
/// du temps. Faire tourner YOLO sur chacune de ces images identiques, c'est
/// payer en permanence le prix fort — sur un Raspberry Pi, l'inférence est de
/// loin le poste de dépense dominant — pour apprendre à chaque fois que rien
/// n'a changé. Comparer deux images miniatures coûte, lui, quelques
/// microsecondes.
///
/// Section entièrement optionnelle, et ACTIVE par défaut : c'est une
/// économie sans contrepartie fonctionnelle (voir les garde-fous de
/// [`Self::hold_secs`] et [`Self::max_idle_secs`]).
#[derive(Debug, Clone, Deserialize)]
pub struct MotionConfig {
    /// Pré-filtre actif ou non. `false` rétablit le comportement antérieur :
    /// YOLO tourne à intervalle fixe, que l'image bouge ou non.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Écart de luminance, sur 0-255, à partir duquel un pixel est considéré
    /// comme ayant changé.
    ///
    /// Trop bas, le bruit du capteur en basse lumière suffit à déclencher ;
    /// trop haut, une personne habillée dans les tons du décor passe
    /// inaperçue.
    #[serde(default = "default_motion_pixel_threshold")]
    pub pixel_threshold: u8,

    /// Proportion de pixels changés, de 0 à 1, à partir de laquelle on
    /// considère qu'il y a mouvement.
    ///
    /// La comparaison se fait sur une miniature (voir
    /// [`crate::capture::motion`]) : une personne au loin n'y occupe que
    /// quelques pixels, d'où une valeur par défaut volontairement basse.
    #[serde(default = "default_motion_min_changed_ratio")]
    pub min_changed_ratio: f32,

    /// Durée, en secondes, pendant laquelle YOLO continue de tourner après
    /// la dernière image jugée en mouvement.
    ///
    /// GARDE-FOU ESSENTIEL : quelqu'un qui s'arrête devant la caméra ne
    /// produit plus de mouvement, mais est toujours là. Sans cette
    /// rémanence, le suivi le perdrait dès son premier instant d'immobilité
    /// et le redécouvrirait au moindre geste, en republiant un événement à
    /// chaque fois.
    #[serde(default = "default_motion_hold_secs")]
    pub hold_secs: u64,

    /// Délai maximal, en secondes, entre deux passages de YOLO même en
    /// l'absence totale de mouvement.
    ///
    /// SECOND GARDE-FOU : la détection de mouvement compare deux images
    /// successives, elle est donc aveugle à une présence parfaitement
    /// immobile. Ce passage périodique garantit qu'une personne figée finit
    /// toujours par être vue. `0` le désactive — à n'utiliser que si
    /// l'économie de CPU primait sur tout le reste.
    #[serde(default = "default_motion_max_idle_secs")]
    pub max_idle_secs: u64,
}

impl Default for MotionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pixel_threshold: default_motion_pixel_threshold(),
            min_changed_ratio: default_motion_min_changed_ratio(),
            hold_secs: default_motion_hold_secs(),
            max_idle_secs: default_motion_max_idle_secs(),
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_motion_pixel_threshold() -> u8 {
    // Au-dessus du bruit de lecture d'un capteur correctement exposé, en
    // dessous d'un changement de contenu réel.
    20
}

fn default_motion_min_changed_ratio() -> f32 {
    // 0,6 % d'une miniature de 64x48, soit une vingtaine de pixels : l'ordre
    // de grandeur d'une silhouette au fond du champ.
    0.006
}

fn default_motion_hold_secs() -> u64 {
    3
}

fn default_motion_max_idle_secs() -> u64 {
    20
}

/// Encodage H.264 du flux caméra (voir `crate::h264`).
///
/// Ces réglages ne sont PAS dans `[rtsp]`, bien qu'ils y aient commencé : le
/// flux encodé sert aujourd'hui trois consommateurs — les lecteurs RTSP du
/// réseau, le direct des interfaces web (décodé par le navigateur), et les
/// enregistrements. Les laisser sous `[rtsp]` laisserait croire qu'ils ne
/// concernent que le premier.
///
/// Section entièrement optionnelle et DÉSACTIVÉE par défaut : l'encodage est
/// logiciel, donc coûteux en CPU, et une installation qui se contente du flux
/// MJPEG historique n'a aucune raison de le payer. Rien n'est encodé tant
/// qu'aucun consommateur n'est effectivement abonné, mais le réglage reste
/// explicite.
#[derive(Debug, Clone, Deserialize)]
pub struct H264Config {
    /// Encodage disponible ou non.
    ///
    /// `[rtsp] enabled = true` l'implique (voir [`Config::h264_enabled`]) :
    /// il n'est donc à activer explicitement que pour servir les interfaces
    /// web sans ouvrir de port RTSP sur le réseau.
    #[serde(default)]
    pub enabled: bool,

    /// Cadence cible du flux encodé, en images par seconde.
    ///
    /// Volontairement INFÉRIEURE à la cadence de capture par défaut : c'est
    /// le réglage qui pèse le plus sur le coût de l'encodage, et 12 im/s
    /// suffisent largement à une scène de surveillance. Les frames en trop
    /// sont écartées avant l'encodeur (voir `crate::capture::capture_loop`).
    #[serde(default = "default_h264_fps")]
    pub fps: u32,

    /// Débit cible, en kilobits par seconde.
    #[serde(default = "default_h264_bitrate_kbps")]
    pub bitrate_kbps: u32,

    /// Intervalle entre deux images clés, en secondes.
    ///
    /// Borne le temps qu'un consommateur qui vient de s'abonner passe devant
    /// un écran noir : une image clé est le seul point d'entrée d'un
    /// décodeur. Courte, les images clés mangent le débit ; longue, le
    /// démarrage traîne. Une image clé est de toute façon produite à la
    /// demande dès qu'un consommateur arrive.
    #[serde(default = "default_h264_keyframe_interval_secs")]
    pub keyframe_interval_secs: u32,
}

impl Default for H264Config {
    fn default() -> Self {
        Self {
            enabled: false,
            fps: default_h264_fps(),
            bitrate_kbps: default_h264_bitrate_kbps(),
            keyframe_interval_secs: default_h264_keyframe_interval_secs(),
        }
    }
}

fn default_h264_fps() -> u32 {
    12
}

fn default_h264_bitrate_kbps() -> u32 {
    1500
}

fn default_h264_keyframe_interval_secs() -> u32 {
    2
}

/// Mise à disposition du flux encodé en RTSP (voir `crate::rtsp`), pour les
/// lecteurs vidéo et enregistreurs du réseau.
///
/// Section entièrement optionnelle et désactivée par défaut. L'activer active
/// aussi l'encodage (voir [`Config::h264_enabled`]) : les réglages de
/// l'encodeur, eux, sont dans `[h264]`.
#[derive(Debug, Clone, Deserialize)]
pub struct RtspConfig {
    #[serde(default)]
    pub enabled: bool,

    /// Adresse d'écoute du serveur RTSP.
    #[serde(default = "default_rtsp_host")]
    pub host: String,

    /// Port d'écoute. 554 est le port officiel de RTSP, mais il est
    /// privilégié (inférieur à 1024) : la caméra tournant sous un utilisateur
    /// sans privilèges (voir `deploy/camera/Dockerfile`), le défaut est 8554,
    /// la convention pour un RTSP non privilégié.
    #[serde(default = "default_rtsp_port")]
    pub port: u16,

    /// Chemin du flux dans l'URL (`rtsp://hôte:8554/<path>`). Les barres
    /// obliques superflues sont tolérées.
    #[serde(default = "default_rtsp_path")]
    pub path: String,

    /// Exiger le jeton de `[server] api_token` dans la chaîne de requête de
    /// l'URL (`rtsp://hôte:8554/stream?token=…`).
    ///
    /// ACTIF par défaut : ce flux montre exactement la même image que le
    /// WebSocket, qui est lui authentifié. L'ouvrir sans contrôle serait une
    /// régression de confidentialité, pas une simplification.
    #[serde(default = "default_true")]
    pub require_token: bool,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: default_rtsp_host(),
            port: default_rtsp_port(),
            path: default_rtsp_path(),
            require_token: true,
        }
    }
}

fn default_rtsp_host() -> String {
    "0.0.0.0".to_string()
}

fn default_rtsp_port() -> u16 {
    8554
}

fn default_rtsp_path() -> String {
    "stream".to_string()
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

    /// Écriture d'un clip par détection, pour la timeline de l'interface du
    /// manager (voir [`crate::capture::clips`]).
    ///
    /// Indépendant de l'enregistrement continu, qui lui est piloté à la main
    /// depuis l'interface de la caméra : ces clips-ci sont déclenchés par les
    /// détections, et c'est précisément ce qui les rend consultables — une
    /// timeline dont chaque entrée renvoie vers un enregistrement de plusieurs
    /// heures n'aiderait personne.
    #[serde(default = "default_true")]
    pub clips_enabled: bool,

    /// Durée conservée AVANT la détection, en secondes.
    ///
    /// C'est l'intérêt principal du dispositif : l'événement n'est publié
    /// qu'une fois la personne reconnue (ou constatée inconnue), soit déjà
    /// une seconde ou deux après son arrivée dans le champ. Sans ce
    /// pré-enregistrement, le clip commencerait au milieu de l'action et ne
    /// montrerait jamais par où la personne est entrée.
    #[serde(default = "default_clip_pre_secs")]
    pub clip_pre_secs: u64,

    /// Durée enregistrée APRÈS la détection, en secondes.
    #[serde(default = "default_clip_post_secs")]
    pub clip_post_secs: u64,
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
            clips_enabled: true,
            clip_pre_secs: default_clip_pre_secs(),
            clip_post_secs: default_clip_post_secs(),
        }
    }
}

fn default_clip_pre_secs() -> u64 {
    4
}

fn default_clip_post_secs() -> u64 {
    8
}

fn default_known_faces_dir() -> String {
    DEFAULT_KNOWN_FACES_DIR.to_string()
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

    /// URL de base par laquelle cette caméra est joignable depuis un
    /// navigateur, ex. `http://192.168.1.42:8080`.
    ///
    /// Publiée dans les événements MQTT pour que la timeline de l'interface
    /// du manager puisse offrir un lien direct vers le clip d'une détection
    /// ET vers la vue en direct de la caméra (voir
    /// `foxguard_protocol::DetectionEvent::clip_url` et `::live_url`).
    ///
    /// La caméra NE PEUT PAS la deviner : elle écoute en général sur
    /// `0.0.0.0`, et son adresse vue du navigateur dépend du réseau et d'un
    /// éventuel proxy. Laissée vide (le défaut), les événements ne portent
    /// ni lien de lecture ni lien de direct — la vignette, elle, voyage dans
    /// l'événement et reste visible dans tous les cas.
    #[serde(default)]
    pub public_url: String,
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
    // Dossier des photos de référence de la reconnaissance faciale, où sont
    // aussi écrites les captures déclenchées depuis l'interface web.
    //
    // Relatif au répertoire de travail par défaut. Le renseigner en ABSOLU
    // est recommandé pour un déploiement en conteneur ou en service systemd,
    // où ce répertoire n'est pas celui du dépôt.
    #[serde(default = "default_known_faces_dir")]
    pub known_faces_dir: String,
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
    fn data_directories_fall_back_to_their_historical_defaults() {
        // Un `camera-config.toml` antérieur à l'ajout de ces réglages doit
        // continuer d'écrire exactement où il écrivait avant.
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.detection.known_faces_dir, "known_faces");
        assert_eq!(config.recording.dir, "output_record");
    }

    #[test]
    fn data_directories_can_be_pointed_elsewhere() {
        // Cas d'un déploiement où les données vivent hors du dossier du
        // binaire (conteneur, service systemd).
        let toml = VALID_TOML.replace(
            "email_cooldown_secs = 60",
            "email_cooldown_secs = 60\n        known_faces_dir = \"/var/lib/foxguard/visages\"",
        ) + "\n[recording]\ndir = \"/var/lib/foxguard/videos\"\n";
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(
            config.detection.known_faces_dir,
            "/var/lib/foxguard/visages"
        );
        assert_eq!(config.recording.dir, "/var/lib/foxguard/videos");
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

    // --- `[motion]` : pré-filtre de mouvement ---

    #[test]
    fn the_motion_section_is_optional_and_active_by_default() {
        // Un `camera-config.toml` antérieur à l'ajout du pré-filtre doit
        // continuer de charger, et BÉNÉFICIER de l'économie : c'est un gain
        // sans contrepartie fonctionnelle (voir `MotionConfig`).
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(config.motion.enabled);
        assert_eq!(config.motion.pixel_threshold, 20);
        assert_eq!(config.motion.hold_secs, 3);
        assert_eq!(config.motion.max_idle_secs, 20);
    }

    #[test]
    fn the_motion_prefilter_can_be_turned_off() {
        // Rétablit le comportement antérieur : YOLO à intervalle fixe.
        let toml = format!("{VALID_TOML}\n[motion]\nenabled = false\n");
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(!config.motion.enabled);
        // Les autres réglages gardent leur défaut.
        assert_eq!(config.motion.pixel_threshold, 20);
    }

    #[test]
    fn the_motion_thresholds_can_be_tuned() {
        let toml = format!(
            "{VALID_TOML}\n[motion]\npixel_threshold = 35\nmin_changed_ratio = 0.02\nhold_secs = 10\n"
        );
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.motion.pixel_threshold, 35);
        assert!((config.motion.min_changed_ratio - 0.02).abs() < f32::EPSILON);
        assert_eq!(config.motion.hold_secs, 10);
    }

    #[test]
    fn a_max_idle_of_zero_is_accepted_and_means_never() {
        // Comme `retention_days = 0`, c'est une valeur SIGNIFICATIVE et non
        // une erreur : elle supprime le passage périodique de sécurité.
        let toml = format!("{VALID_TOML}\n[motion]\nmax_idle_secs = 0\n");
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.motion.max_idle_secs, 0);
    }

    // --- `[rtsp]` : encodage H.264 et flux RTSP ---

    #[test]
    fn the_rtsp_section_is_optional_and_disabled_by_default() {
        // L'encodage H.264 est logiciel : on ne l'impose pas à une
        // installation qui ne l'a pas demandé.
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(!config.rtsp.enabled);
        assert!(!config.h264.enabled);
        assert_eq!(config.rtsp.port, 8554);
        assert_eq!(config.rtsp.path, "stream");
        assert_eq!(config.h264.fps, 12);
    }

    #[test]
    fn enabling_rtsp_implies_enabling_the_encoder() {
        // Un serveur RTSP sans flux à servir n'a aucun sens — et c'est ce qui
        // garde valide un `camera-config.toml` antérieur à la section
        // `[h264]`, où le flux RTSP s'activait à lui seul.
        let toml = format!("{VALID_TOML}\n[rtsp]\nenabled = true\n");
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(!config.h264.enabled, "la section [h264] reste à son défaut");
        assert!(config.h264_enabled(), "mais l'encodage doit être actif");
    }

    #[test]
    fn the_encoder_can_be_enabled_without_opening_an_rtsp_port() {
        // Le cas d'une installation qui veut le H.264 dans ses interfaces web
        // sans exposer de flux sur le réseau.
        let toml = format!("{VALID_TOML}\n[h264]\nenabled = true\n");
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(config.h264_enabled());
        assert!(!config.rtsp.enabled);
    }

    #[test]
    fn the_encoder_settings_can_be_tuned() {
        let toml = format!(
            "{VALID_TOML}\n[h264]\nenabled = true\nfps = 25\nbitrate_kbps = 3000\nkeyframe_interval_secs = 1\n"
        );
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.h264.fps, 25);
        assert_eq!(config.h264.bitrate_kbps, 3000);
        assert_eq!(config.h264.keyframe_interval_secs, 1);
    }

    #[test]
    fn the_rtsp_stream_requires_a_token_by_default() {
        // Le flux RTSP montre la même image que le WebSocket, qui est
        // authentifié : l'ouvrir sans contrôle serait une régression.
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(config.rtsp.require_token);
    }

    #[test]
    fn the_rtsp_section_parses_explicit_values() {
        let toml = format!(
            r#"
                {VALID_TOML}

                [rtsp]
                enabled = true
                host = "127.0.0.1"
                port = 554
                path = "/salon/live/"
                require_token = false
            "#
        );
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(config.rtsp.enabled);
        assert_eq!(config.rtsp.host, "127.0.0.1");
        assert_eq!(config.rtsp.port, 554);
        assert_eq!(config.rtsp.path, "/salon/live/");
        assert!(!config.rtsp.require_token);
    }

    // --- Clips d'événement et URL publique ---

    #[test]
    fn event_clips_are_enabled_by_default_with_a_pre_roll() {
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(config.recording.clips_enabled);
        // Le pré-enregistrement est la raison d'être du dispositif : un clip
        // qui commence à l'instant de la détection a déjà raté l'arrivée.
        assert!(config.recording.clip_pre_secs > 0);
        assert_eq!(config.recording.clip_post_secs, 8);
    }

    #[test]
    fn event_clips_can_be_turned_off_and_their_durations_tuned() {
        let toml = format!(
            "{VALID_TOML}\n[recording]\nclips_enabled = false\nclip_pre_secs = 2\nclip_post_secs = 15\n"
        );
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(!config.recording.clips_enabled);
        assert_eq!(config.recording.clip_pre_secs, 2);
        assert_eq!(config.recording.clip_post_secs, 15);
        // La rétention, dans la même section, garde son défaut.
        assert_eq!(config.recording.retention_days, 7);
    }

    #[test]
    fn the_public_url_is_empty_unless_configured() {
        // La caméra ne peut pas la deviner : mieux vaut aucun lien qu'un lien
        // mort dans la timeline.
        let file = write_temp_toml(VALID_TOML);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert!(config.server.public_url.is_empty());
    }

    #[test]
    fn the_public_url_can_be_declared() {
        let toml = VALID_TOML.replace(
            "api_token = \"secret\"",
            "api_token = \"secret\"\n        public_url = \"http://192.168.1.42:8080\"",
        );
        let file = write_temp_toml(&toml);
        let config = Config::load(file.path().to_str().unwrap()).expect("config valide");

        assert_eq!(config.server.public_url, "http://192.168.1.42:8080");
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
