//! Capture caméra (V4L2) et boucle de traitement principale : décodage des
//! frames, pipeline de détection/reconnaissance (YOLO -> tracking ->
//! YuNet -> ArcFace), incrustation des boîtes, encodage H.264, enregistrement
//! et diffusion du flux aux clients WebSocket. Voir [`start_camera_loop`].
//!
//! Le module est découpé par responsabilité :
//! - [`state`] : état partagé exposé au serveur HTTP/WebSocket.
//! - [`models`] : chargement des modèles IA et de la base de visages connus.
//! - [`tracking`] : suivi des personnes (IoU) et reconnaissance faciale parallèle.
//! - [`worker`] : thread d'arrière-plan qui exécute le pipeline YOLO -> tracking -> reconnaissance.
//! - [`known_faces`] : chargement et rechargement à chaud de `known_faces/`.
//! - [`motion`] : pré-filtre de mouvement qui décide si YOLO doit tourner.
//! - [`overlay`] : incrustation des bounding-box sur la frame vidéo.
//! - [`thumbnail`] : vignette de détection embarquée dans les événements.
//! - [`codec`] : décodage YUYV -> RGB.
//! - [`clips`] : clips vidéo d'événement (pré- et post-enregistrement).
//! - [`recording`] : écriture des enregistrements disque (`output_record/`).
//! - [`capture_loop`] : boucle principale de lecture V4L2 et de diffusion.

mod capture_loop;
mod clips;
mod codec;
mod known_faces;
mod models;
mod motion;
mod overlay;
mod recording;
mod state;
mod thumbnail;
mod tracking;
mod worker;

pub use clips::ClipRecorder;
pub use recording::RecordingFormat;
pub use state::{DEFAULT_MAX_STREAM_CLIENTS, SharedState};

use anyhow::{Context, Result};
use image::RgbImage;
use std::sync::{Arc, Mutex, atomic::AtomicBool, mpsc};
use std::time::Duration;
use tracing::info;
use v4l::buffer::Type;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::config::Config;
use crate::h264::{H264Encoder, H264Stream};
use crate::vision::BoundingBox;

use capture_loop::{DETECTION_INTERVAL, H264Output, Pipeline};
use models::Models;
use worker::EventPublishing;

/// Point d'entrée de la caméra, appelé dans un thread bloquant dédié (voir
/// `main.rs`). Ouvre le périphérique V4L2, charge les modèles IA et la base
/// de visages connus ([`models::Models::load`]), démarre le thread worker de
/// reconnaissance ([`worker::spawn_recognition_worker`]), puis boucle
/// indéfiniment sur la capture des frames ([`capture_loop::run`]).
///
/// Ne retourne qu'en cas d'erreur fatale à l'initialisation ou à la capture
/// (périphérique caméra indisponible, etc.).
pub fn start_camera_loop(
    config: Config,
    state: Arc<SharedState>,
    clips: Arc<Mutex<ClipRecorder>>,
    h264_stream: Arc<H264Stream>,
) -> Result<()> {
    // Initialisation dynamique du périphérique V4L2
    //
    // `stream` emprunte `dev` (v4l::MmapStream<'_>) : les deux doivent donc
    // rester vivants ensemble jusqu'à la fin de la capture (capture_loop::run),
    // ce qui empêche de factoriser cette étape dans une fonction séparée
    // sans `unsafe` (type self-référentiel).
    let dev = Device::new(config.camera.device_index)?;
    let fmt = dev.format()?;
    info!(
        "🎥 Caméra détectée : {}x{} ({:?})",
        fmt.width, fmt.height, fmt.fourcc
    );

    // Allocation de 2 buffers MMAP
    let stream = MmapStream::with_buffers(&dev, Type::VideoCapture, 2)?;

    // Chargement des modèles IA (YOLO, YuNet, ArcFace) et de la base de
    // visages connus.
    let models = Models::load(&config)?;

    // Canal non-bloquant pour la détection IA en arrière-plan
    let (detect_tx, detect_rx) = mpsc::sync_channel::<RgbImage>(1);
    let last_boxes: Arc<Mutex<Vec<BoundingBox>>> = Arc::new(Mutex::new(Vec::new()));

    // Disponibilité du worker, qu'il tient lui-même à jour : c'est ce qui
    // permet à la boucle de capture de ne copier une frame que lorsqu'il y a
    // quelqu'un pour la traiter (voir `capture_loop::Pipeline::detect_idle`).
    // Vrai au départ : le worker démarre les mains libres.
    let detect_idle = Arc::new(AtomicBool::new(true));

    // Worker IA : mouvement -> YOLO -> PersonTracker -> YuNet + ArcFace (voir
    // worker::spawn_recognition_worker)
    worker::spawn_recognition_worker(
        detect_rx,
        Arc::clone(&detect_idle),
        Arc::clone(&last_boxes),
        models.detector,
        Arc::clone(&models.face_detector),
        Arc::clone(&models.face_embedder),
        Arc::clone(&models.known_people),
        config.motion.clone(),
        EventPublishing {
            mqtt: models.mqtt.clone(),
            camera_name: config.camera.name.clone(),
            clips: Arc::clone(&clips),
            public_url: config.server.public_url.clone(),
        },
    );

    // Encodage H.264. Son échec est FATAL, et c'est délibéré : le H.264 est
    // le seul chemin vidéo de la caméra, donc un encodeur absent signifie
    // aucun direct et aucun enregistrement. Une caméra qui démarre
    // normalement mais n'enregistre rien est le pire mode de panne d'un
    // système de surveillance — on ne s'en aperçoit qu'en cherchant
    // l'enregistrement qui aurait servi. Mieux vaut refuser de démarrer, ce
    // qui se voit tout de suite (même raisonnement que la connexion à la base
    // du manager, voir `foxguard-manager/src/main.rs`).
    let h264 = build_h264_output(&config, fmt.width, fmt.height, h264_stream)?;

    capture_loop::run(
        state,
        stream,
        fmt.width,
        fmt.height,
        Pipeline {
            mailer: models.mailer,
            email_cooldown_secs: config.detection.email_cooldown_secs,
            detect_tx,
            detect_interval: DETECTION_INTERVAL,
            detect_idle,
            last_boxes,
            face_detector: models.face_detector,
            face_embedder: models.face_embedder,
            known_people: models.known_people,
            known_faces_dir: config.detection.known_faces_dir.clone(),
            clips,
            h264,
        },
    )
}

/// Construit l'encodeur H.264 du flux de la caméra.
///
/// Échoue si l'encodeur ne peut pas être initialisé — voir l'appelant pour
/// pourquoi cet échec est fatal.
fn build_h264_output(
    config: &Config,
    width: u32,
    height: u32,
    stream: Arc<H264Stream>,
) -> Result<H264Output> {
    let fps = config.h264.fps.clamp(1, 120);

    let encoder = H264Encoder::new(
        width,
        height,
        fps,
        config.h264.bitrate_kbps,
        config.h264.keyframe_interval_secs,
    )
    .context("l'encodeur H.264 n'a pas pu démarrer, la caméra n'aurait aucun flux à produire")?;

    let (encoded_width, encoded_height) = encoder.dimensions();

    info!(
        "🎞️ Encodage H.264 prêt : {}x{} à {} im/s, {} kb/s (aucune frame n'est encodée tant que personne ne regarde).",
        encoded_width, encoded_height, fps, config.h264.bitrate_kbps
    );

    Ok(H264Output {
        encoder,
        stream,
        // `1 / fps` : les frames capturées au-delà de la cadence configurée
        // sont écartées avant l'encodeur, qui est le poste de dépense qu'on
        // cherche à contenir.
        frame_interval: Duration::from_micros(1_000_000 / u64::from(fps)),
        source_width: width,
        fps,
    })
}
