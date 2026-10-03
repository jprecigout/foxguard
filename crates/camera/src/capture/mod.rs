//! Capture caméra (V4L2) et boucle de traitement principale : décodage des
//! frames, pipeline de détection/reconnaissance (YOLO -> tracking ->
//! YuNet -> ArcFace), incrustation des boîtes, enregistrement et diffusion
//! du flux vidéo aux clients WebSocket. Voir [`start_camera_loop`].
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
pub use state::SharedState;

use anyhow::Result;
use image::RgbImage;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;
use tracing::{info, warn};
use v4l::buffer::Type;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::config::Config;
use crate::h264::{H264Encoder, H264Stream};
use crate::vision::BoundingBox;

use capture_loop::{H264Output, Pipeline};
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
    rtsp: Option<Arc<H264Stream>>,
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

    // Worker IA : mouvement -> YOLO -> PersonTracker -> YuNet + ArcFace (voir
    // worker::spawn_recognition_worker)
    worker::spawn_recognition_worker(
        detect_rx,
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

    // Encodage H.264, seulement si un flux RTSP a été demandé. L'échec
    // d'initialisation de l'encodeur ne fait PAS tomber la caméra : elle doit
    // continuer à surveiller, enregistrer et diffuser son flux WebSocket même
    // privée de RTSP.
    let h264 = rtsp.and_then(|stream| {
        build_h264_output(&config, fmt.width, fmt.height, stream).or_else(|| {
            warn!("⚠️ Flux RTSP indisponible : l'encodeur H.264 n'a pas pu démarrer.");
            None
        })
    });

    capture_loop::run(
        state,
        stream,
        fmt.width,
        fmt.height,
        Pipeline {
            mailer: models.mailer,
            email_cooldown_secs: config.detection.email_cooldown_secs,
            detect_tx,
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

/// Construit l'encodeur H.264 du flux RTSP, ou `None` s'il n'a pas pu être
/// initialisé.
fn build_h264_output(
    config: &Config,
    width: u32,
    height: u32,
    stream: Arc<H264Stream>,
) -> Option<H264Output> {
    let fps = config.h264.fps.clamp(1, 120);

    let encoder = H264Encoder::new(
        width,
        height,
        fps,
        config.h264.bitrate_kbps,
        config.h264.keyframe_interval_secs,
    )
    .inspect_err(|e| warn!("⚠️ Encodeur H.264 non initialisé : {e:#}"))
    .ok()?;

    let (encoded_width, encoded_height) = encoder.dimensions();

    info!(
        "🎞️ Encodage H.264 prêt : {}x{} à {} im/s, {} kb/s (aucune frame n'est encodée tant que personne ne regarde).",
        encoded_width, encoded_height, fps, config.h264.bitrate_kbps
    );

    Some(H264Output {
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
