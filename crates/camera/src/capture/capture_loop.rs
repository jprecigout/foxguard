//! Boucle de capture principale : lit les frames du périphérique V4L2,
//! transmet chaque frame au thread worker en arrière-plan (voir
//! `super::worker`), incruste les boîtes (voir `super::overlay`) et les
//! diffuse aux clients WebSocket, et gère l'enregistrement disque optionnel.

use anyhow::Result;
use image::{ImageFormat, RgbImage};
use std::sync::{Arc, Mutex, atomic::Ordering, mpsc};
use std::time::{Duration, Instant};
use tracing::info;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;

use crate::mail::Mailer;
use crate::util::MutexExt;
use crate::vision::{BoundingBox, FaceDetectorYuNet, FaceEmbedder, KnownPerson};

use super::codec::decode_yuyv_to_rgb;
use super::known_faces::try_capture_reference;
use super::overlay::draw_detections;
use super::recording::RecordingWriter;
use super::state::SharedState;

/// Boucle infinie de lecture V4L2 et de diffusion (voir
/// `super::start_camera_loop`). Ne retourne qu'en cas d'erreur fatale à la
/// capture.
#[allow(clippy::too_many_arguments)]
pub(super) fn run(
    state: Arc<SharedState>,
    email_cooldown_secs: u64,
    mut stream: MmapStream<'_>,
    width: u32,
    height: u32,
    mailer: Mailer,
    detect_tx: mpsc::SyncSender<RgbImage>,
    last_boxes: Arc<Mutex<Vec<BoundingBox>>>,
    face_detector: Arc<Option<FaceDetectorYuNet>>,
    face_embedder: Arc<Option<FaceEmbedder>>,
    known_people: Arc<Mutex<Vec<KnownPerson>>>,
) -> Result<()> {
    let mut recording: Option<RecordingWriter> = None;
    let mut last_email_time = Instant::now() - Duration::from_secs(email_cooldown_secs);

    info!("📹 Boucle Caméra démarrée avec succès.");

    loop {
        // Capture du buffer brut depuis V4L2
        let (buf, _) = stream.next()?;

        let is_detection_active = state.detection_enabled.load(Ordering::Relaxed);
        let is_recording_active = state.recording_enabled.load(Ordering::Relaxed);

        // Détection du format (PC en MJPEG vs Raspberry Pi en YUYV)
        let is_jpeg = buf.len() > 2 && buf[0] == 0xFF && buf[1] == 0xD8;

        let jpeg_bytes = if is_detection_active {
            let decoded_img = if is_jpeg {
                image::load_from_memory(buf).ok().map(|i| i.to_rgb8())
            } else {
                decode_yuyv_to_rgb(buf, width, height)
            };

            if let Some(mut img) = decoded_img {
                // Capture de photo de référence (enrôlement à chaud), voir
                // `try_capture_reference`.
                try_capture_reference(&state, &img, &face_detector, &face_embedder, &known_people);

                let _ = detect_tx.try_send(img.clone());

                let current_boxes = last_boxes.lock_or_recover().clone();

                if !current_boxes.is_empty() {
                    // Une personne reconnue (visage identifié) n'est pas une
                    // intrusion : on n'alerte par e-mail que s'il reste au
                    // moins une détection non reconnue (personne inconnue,
                    // chat ou chien) dans la frame (voir `draw_detections`).
                    let should_alert = draw_detections(&mut img, &current_boxes);

                    if should_alert
                        && last_email_time.elapsed() >= Duration::from_secs(email_cooldown_secs)
                    {
                        let mut alert_encoded = Vec::new();
                        let mut cursor = std::io::Cursor::new(&mut alert_encoded);
                        if img.write_to(&mut cursor, ImageFormat::Jpeg).is_ok() {
                            mailer.send_alert(alert_encoded);
                            last_email_time = Instant::now();
                        }
                    }
                }

                let mut encoded = Vec::new();
                let mut cursor = std::io::Cursor::new(&mut encoded);
                if img.write_to(&mut cursor, ImageFormat::Jpeg).is_ok() {
                    encoded
                } else {
                    buf.to_vec()
                }
            } else {
                buf.to_vec()
            }
        } else {
            last_boxes.lock_or_recover().clear();

            if is_jpeg {
                buf.to_vec()
            } else {
                if let Some(img) = decode_yuyv_to_rgb(buf, width, height) {
                    let mut encoded = Vec::new();
                    let mut cursor = std::io::Cursor::new(&mut encoded);
                    if img.write_to(&mut cursor, ImageFormat::Jpeg).is_ok() {
                        encoded
                    } else {
                        buf.to_vec()
                    }
                } else {
                    buf.to_vec()
                }
            }
        };

        if is_recording_active {
            if recording.is_none() {
                recording = Some(RecordingWriter::create(&state.recordings_dir)?);
            }
            if let Some(ref mut writer) = recording {
                let _ = writer.write_frame(&jpeg_bytes);
            }
        } else if recording.is_some() {
            info!("💾 Arrêt de l'enregistrement.");
            recording = None;
        }

        let _ = state.tx.send(jpeg_bytes);
    }
}
