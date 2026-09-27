//! Chargement des modèles IA (YOLO, YuNet, ArcFace) et de la base de
//! visages connus, effectué une seule fois au démarrage de la caméra (voir
//! `super::start_camera_loop`).

use anyhow::Result;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::mail::Mailer;
use crate::mqtt::MqttPublisher;
use crate::vision::{FaceDetectorYuNet, FaceEmbedder, KnownPerson, ObjectDetector};
use tracing::{error, info, warn};

use super::known_faces::load_known_faces;

/// Modèles IA et base de visages connus chargés au démarrage de la caméra.
pub(super) struct Models {
    pub(super) detector: ObjectDetector,
    pub(super) mailer: Mailer,
    pub(super) face_detector: Arc<Option<FaceDetectorYuNet>>,
    pub(super) face_embedder: Arc<Option<FaceEmbedder>>,
    pub(super) known_people: Arc<Mutex<Vec<KnownPerson>>>,
    // `None` si `[mqtt] enabled = false` (par défaut) : aucune connexion au
    // broker n'est alors tentée (voir `crate::mqtt::MqttPublisher::connect`).
    pub(super) mqtt: Option<MqttPublisher>,
}

impl Models {
    /// Charge l'`ObjectDetector` (YOLO) et le `Mailer`, puis YuNet
    /// (détection de visage) et ArcFace/MobileFaceNet (empreintes
    /// faciales), et enfin la base de visages connus (`known_faces/`) à
    /// partir de ces deux derniers.
    pub(super) fn load(config: &Config) -> Result<Self> {
        // Initialisation de l'IA (ObjectDetector) et du Mailer
        let detector = ObjectDetector::new(config.detection.clone())?;
        let mailer = Mailer::new(config.email.clone());

        // Chargement explicite de YuNet (détecteur de visages) avec gestion d'erreur
        // Ce détecteur est fixé à 640x640 : c'est le format utilisé pour le flux
        // caméra live (crops de personnes déjà petits, performance critique).
        let face_detector = match FaceDetectorYuNet::new(config.detection.clone(), 640, 640) {
            Ok(detector) => {
                info!("✅ Modèle YuNet chargé avec succès.");
                Arc::new(Some(detector))
            }
            Err(e) => {
                error!("❌ Erreur d'initialisation YuNet : {:?}", e);
                Arc::new(None)
            }
        };

        let face_embedder = Arc::new(FaceEmbedder::new(config.detection.clone()).ok());

        // Le chargement des visages connus réutilise le détecteur YuNet 640x640
        // partagé, mais via detect_face_native_res() qui scanne les photos plus
        // grandes par fenêtres 640x640 à résolution native (voir sa doc).
        // Enveloppé dans un Mutex pour pouvoir être rechargé à chaud après une
        // capture de référence déclenchée depuis l'interface web (voir
        // `super::known_faces::try_capture_reference`).
        let known_people: Arc<Mutex<Vec<KnownPerson>>> =
            if let (Some(detector), Some(embedder)) = (&*face_detector, &*face_embedder) {
                Arc::new(Mutex::new(load_known_faces(
                    detector,
                    embedder,
                    "known_faces",
                )))
            } else {
                warn!("⚠️ YuNet ou ArcFace indisponible.");
                Arc::new(Mutex::new(Vec::new()))
            };

        // Connexion MQTT optionnelle (voir `[mqtt]` dans camera-config.toml). La
        // connexion réelle est paresseuse et se reconnecte automatiquement
        // en tâche de fond (voir `MqttPublisher::connect`) : un broker
        // momentanément injoignable au démarrage n'empêche pas FoxGuard de
        // démarrer.
        let mqtt = if config.mqtt.enabled {
            info!(
                "📡 Connexion MQTT activée : {}:{} (topic \"{}\")",
                config.mqtt.broker_host, config.mqtt.broker_port, config.mqtt.topic
            );
            Some(MqttPublisher::connect(&config.mqtt))
        } else {
            None
        };

        Ok(Self {
            detector,
            mailer,
            face_detector,
            face_embedder,
            known_people,
            mqtt,
        })
    }
}
