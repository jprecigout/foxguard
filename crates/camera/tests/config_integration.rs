//! Test d'intégration de bout en bout du chargement de configuration : lit
//! un vrai fichier TOML sur disque (pas une simple chaîne en mémoire, comme
//! dans les tests unitaires de `src/config.rs`) via `foxguard_camera::config::Config::load`,
//! telle qu'appelée au démarrage réel de l'application (`src/main.rs`).

use std::io::Write;

use foxguard_camera::config::Config;

#[test]
fn load_reads_config_sample_toml_shipped_with_the_repo() {
    // `camera-config-sample.toml` est le modèle fourni aux utilisateurs (voir
    // README.md, section Configuration) : il doit rester chargeable tel quel.
    //
    // Le chemin est résolu depuis le MANIFESTE du crate et non depuis le
    // répertoire courant : Cargo exécute les tests d'intégration avec le
    // répertoire courant positionné sur le paquet (`crates/camera/`), alors
    // que le modèle vit à la racine du workspace — là où l'application est
    // lancée en développement (`cargo run -p foxguard-camera`) et là où le
    // Dockerfile va le chercher.
    let sample =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../camera-config-sample.toml");

    let config = Config::load(sample.to_str().expect("chemin UTF-8"))
        .expect("camera-config-sample.toml doit toujours être un TOML valide et complet");

    assert!(!config.server.host.is_empty());
    assert!(config.server.port > 0);
    assert!(!config.detection.model_path.is_empty());
    assert!(!config.detection.model_detect_face_path.is_empty());
    assert!(!config.detection.model_face_path.is_empty());

    // Le modèle documente aussi les sections optionnelles : les valeurs qu'il
    // montre doivent être celles qui s'appliquent réellement, sinon il
    // enseigne des réglages faux.
    assert!(
        config.motion.enabled,
        "le pré-filtre de mouvement est actif par défaut"
    );
    assert!(
        config.motion.hold_secs > 0 && config.motion.max_idle_secs > 0,
        "les deux garde-fous du pré-filtre doivent être armés dans le modèle"
    );

    assert!(
        !config.h264_enabled(),
        "l'encodage H.264 est désactivé par défaut (il est logiciel)"
    );
    assert!(!config.rtsp.enabled);
    assert!(
        config.rtsp.require_token,
        "le flux RTSP montre la même image que le WebSocket, qui est authentifié"
    );
    assert!(config.rtsp.port > 1024, "port non privilégié");

    assert!(config.recording.clips_enabled);
    assert!(
        config.recording.clip_pre_secs > 0,
        "sans pré-enregistrement, un clip rate l'arrivée de la personne"
    );
}

#[test]
fn load_round_trips_a_full_config_written_to_a_real_file_on_disk() {
    let toml = r#"
        [server]
        host = "0.0.0.0"
        port = 9090
        api_token = "integration-token"

        [camera]
        device_index = 1

        [detection]
        enabled = true
        model_path = "src/vision/models/yolov8n.onnx"
        model_detect_face_path = "src/vision/models/face_detection_yunet_2023mar.onnx"
        model_face_path = "src/vision/models/arcface-mobilefacenet.onnx"
        input_size = 640
        input_face_size = 112
        confidence_threshold = 0.45
        email_cooldown_secs = 120

        [email]
        enabled = true
        smtp_server = "smtp.example.com"
        smtp_user = "alerts@example.com"
        smtp_password = "s3cret"
        from_address = "alerts@example.com"
        to_address = "jerome@example.com"
    "#;

    let mut file = tempfile::NamedTempFile::new().expect("fichier temporaire");
    file.write_all(toml.as_bytes()).expect("écriture du TOML");

    let config = Config::load(file.path().to_str().unwrap()).expect("chargement du TOML");

    assert_eq!(config.server.host, "0.0.0.0");
    assert_eq!(config.server.port, 9090);
    assert_eq!(config.server.api_token, "integration-token");
    assert_eq!(config.camera.device_index, 1);
    assert_eq!(config.detection.email_cooldown_secs, 120);
    assert!(config.email.enabled);
    assert_eq!(config.email.to_address, "jerome@example.com");
}

#[test]
fn load_fails_end_to_end_on_a_nonexistent_path() {
    let result = Config::load("/chemin/inexistant/pour/de/vrai/config.toml");
    assert!(result.is_err());
}
