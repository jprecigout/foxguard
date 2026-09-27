//! Thread worker dédié au pipeline de reconnaissance : YOLO (toutes les N
//! frames) -> tracking (voir `super::tracking`) -> YuNet + ArcFace en
//! parallèle. Le résultat (bounding-box avec identité) est publié dans
//! `last_boxes`, lu par la boucle de capture (voir `super::capture_loop`)
//! pour l'incrustation sur le flux vidéo.

use std::sync::{Arc, Mutex, mpsc};

use image::RgbImage;
use tracing::error;

use crate::mqtt::MqttPublisher;
use crate::util::MutexExt;
use crate::vision::{BoundingBox, FaceDetectorYuNet, FaceEmbedder, KnownPerson, ObjectDetector};

use super::tracking::{self, PersonTracker};

/// Démarre le thread worker de reconnaissance (bloquant, voir
/// `tokio::task::spawn_blocking`) qui consomme les frames envoyées par
/// `super::capture_loop::run` via `detect_rx`.
///
/// Pipeline : YOLO -> `PersonTracker` -> YuNet + ArcFace (en parallèle via
/// Rayon) -> résultat mis en cache dans `last_boxes`. Les changements d'état
/// de reconnaissance retournés par `tracking::process_persons_parallel` sont
/// publiés sur MQTT si `mqtt` est renseigné (voir `crate::mqtt`).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_recognition_worker(
    detect_rx: mpsc::Receiver<RgbImage>,
    last_boxes: Arc<Mutex<Vec<BoundingBox>>>,
    detector: ObjectDetector,
    face_detector: Arc<Option<FaceDetectorYuNet>>,
    face_embedder: Arc<Option<FaceEmbedder>>,
    known_people: Arc<Mutex<Vec<KnownPerson>>>,
    mqtt: Option<MqttPublisher>,
    camera_name: String,
) {
    tokio::task::spawn_blocking(move || {
        let mut tracker = PersonTracker::new();

        let mut frame_counter: u32 = 0;

        // Caméra à ~25 FPS :
        // YOLO sera exécuté environ 4 fois/seconde.
        const YOLO_INTERVAL: u32 = 6;

        while let Ok(img) = detect_rx.recv() {
            frame_counter = frame_counter.wrapping_add(1);

            // YOLO UNIQUEMENT TOUTES LES N FRAMES

            let run_yolo = frame_counter.is_multiple_of(YOLO_INTERVAL);

            if !run_yolo {
                // Pas de nouveau YOLO.
                //
                // On republie simplement les dernières détections
                // (déjà dans last_boxes, lues côté flux vidéo).
                // La reconnaissance faciale n'est donc PAS relancée.
                continue;
            }

            // YOLO

            let mut boxes = match detector.detect(&img) {
                Ok(boxes) => boxes,

                Err(e) => {
                    error!("❌ Erreur YOLO : {:?}", e);
                    continue;
                }
            };

            // Extraire uniquement les personnes

            let person_boxes: Vec<(u32, u32, u32, u32)> = boxes
                .iter()
                .filter(|bbox| bbox.label == "person" && bbox.width > 20 && bbox.height > 20)
                .map(|bbox| (bbox.x, bbox.y, bbox.width, bbox.height))
                .collect();

            // Tracking + reconnaissance faciale

            if !person_boxes.is_empty() {
                if let (Some(face_detector), Some(face_embedder)) =
                    (face_detector.as_ref(), face_embedder.as_ref())
                {
                    let known_people_guard = known_people.lock_or_recover();
                    let status_changes = tracking::process_persons_parallel(
                        &img,
                        &person_boxes,
                        &mut tracker,
                        face_detector,
                        face_embedder,
                        known_people_guard.as_slice(),
                        0.55,
                    );
                    drop(known_people_guard);

                    if let Some(mqtt) = &mqtt {
                        for status in &status_changes {
                            mqtt.publish_status(&camera_name, status);
                        }
                    }
                }
            } else {
                // Aucune personne détectée.
                //
                // Le tracker va supprimer progressivement les
                // anciennes personnes selon son timeout.

                tracker.update(&[]);
            }

            // Appliquer le résultat du tracking aux BoundingBox

            tracking::apply_track_identities(&tracker, &mut boxes);

            // Publication des nouvelles BoundingBox

            *last_boxes.lock_or_recover() = boxes;
        }
    });
}
