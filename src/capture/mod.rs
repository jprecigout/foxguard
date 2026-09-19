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
//! - [`overlay`] : incrustation des bounding-box sur la frame vidéo.
//! - [`codec`] : décodage YUYV -> RGB.
//! - [`recording`] : écriture des enregistrements disque (`output_record/`).
//! - [`capture_loop`] : boucle principale de lecture V4L2 et de diffusion.

mod capture_loop;
mod codec;
mod known_faces;
mod models;
mod overlay;
mod recording;
mod state;
mod tracking;
mod worker;

pub use state::SharedState;

use anyhow::Result;
use image::RgbImage;
use std::sync::{Arc, Mutex, mpsc};
use v4l::buffer::Type;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::config::Config;
use crate::vision::BoundingBox;

use models::Models;

/// Point d'entrée de la caméra, appelé dans un thread bloquant dédié (voir
/// `main.rs`). Ouvre le périphérique V4L2, charge les modèles IA et la base
/// de visages connus ([`models::Models::load`]), démarre le thread worker de
/// reconnaissance ([`worker::spawn_recognition_worker`]), puis boucle
/// indéfiniment sur la capture des frames ([`capture_loop::run`]).
///
/// Ne retourne qu'en cas d'erreur fatale à l'initialisation ou à la capture
/// (périphérique caméra indisponible, etc.).
pub fn start_camera_loop(config: Config, state: Arc<SharedState>) -> Result<()> {
    // Initialisation dynamique du périphérique V4L2
    //
    // `stream` emprunte `dev` (v4l::MmapStream<'_>) : les deux doivent donc
    // rester vivants ensemble jusqu'à la fin de la capture (capture_loop::run),
    // ce qui empêche de factoriser cette étape dans une fonction séparée
    // sans `unsafe` (type self-référentiel).
    let dev = Device::new(config.camera.device_index)?;
    let fmt = dev.format()?;
    println!(
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

    // Worker IA : YOLO -> PersonTracker -> YuNet + ArcFace (voir worker::spawn_recognition_worker)
    worker::spawn_recognition_worker(
        detect_rx,
        Arc::clone(&last_boxes),
        models.detector,
        Arc::clone(&models.face_detector),
        Arc::clone(&models.face_embedder),
        Arc::clone(&models.known_people),
        models.mqtt.clone(),
        config.camera.name.clone(),
    );

    capture_loop::run(
        state,
        config.detection.email_cooldown_secs,
        stream,
        fmt.width,
        fmt.height,
        models.mailer,
        detect_tx,
        last_boxes,
        models.face_detector,
        models.face_embedder,
        models.known_people,
    )
}
