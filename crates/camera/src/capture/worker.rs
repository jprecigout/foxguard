//! Thread worker dédié au pipeline de reconnaissance : pré-filtre de
//! mouvement (voir `super::motion`) -> YOLO -> tracking (voir
//! `super::tracking`) -> YuNet + ArcFace en parallèle. Le résultat
//! (bounding-box avec identité) est publié dans `last_boxes`, lu par la
//! boucle de capture (voir `super::capture_loop`) pour l'incrustation sur le
//! flux vidéo.
//!
//! C'est aussi ici que les événements de détection sont CONSTITUÉS : statut
//! de reconnaissance, vignette recadrée sur la personne (voir
//! `super::thumbnail`) et référence du clip vidéo (voir `super::clips`),
//! avant publication sur MQTT.

use std::sync::{Arc, Mutex, mpsc};

use image::RgbImage;
use tracing::{debug, error};

use crate::config::MotionConfig;
use crate::mqtt::{DetectionEvent, MqttPublisher};
use crate::util::MutexExt;
use crate::vision::{BoundingBox, FaceDetectorYuNet, FaceEmbedder, KnownPerson, ObjectDetector};

use super::clips::ClipRecorder;
use super::motion::MotionGate;
use super::thumbnail;
use super::tracking::{self, PersonTracker, StatusChange};

/// Ce dont le worker a besoin pour transformer un changement d'état en
/// événement publiable.
///
/// Regroupé dans une structure plutôt qu'ajouté à la liste d'arguments de
/// [`spawn_recognition_worker`], qui en compte déjà beaucoup : ces trois
/// éléments ne servent qu'ensemble, et seulement à ça.
pub(super) struct EventPublishing {
    pub(super) mqtt: Option<MqttPublisher>,
    pub(super) camera_name: String,
    /// Enregistreur de clips, partagé avec la boucle de capture qui
    /// l'alimente en frames.
    pub(super) clips: Arc<Mutex<ClipRecorder>>,
    /// URL publique de la caméra, reprise dans la référence du clip. Vide si
    /// elle n'est pas configurée (voir `[server] public_url`).
    pub(super) public_url: String,
}

/// Démarre le thread worker de reconnaissance (bloquant, voir
/// `tokio::task::spawn_blocking`) qui consomme les frames envoyées par
/// `super::capture_loop::run` via `detect_rx`.
///
/// Pipeline : mouvement -> YOLO -> `PersonTracker` -> YuNet + ArcFace (en
/// parallèle via Rayon) -> résultat mis en cache dans `last_boxes`. Les
/// changements d'état de reconnaissance retournés par
/// `tracking::process_persons_parallel` sont publiés sur MQTT si
/// `publishing.mqtt` est renseigné (voir `crate::mqtt`).
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_recognition_worker(
    detect_rx: mpsc::Receiver<RgbImage>,
    last_boxes: Arc<Mutex<Vec<BoundingBox>>>,
    detector: ObjectDetector,
    face_detector: Arc<Option<FaceDetectorYuNet>>,
    face_embedder: Arc<Option<FaceEmbedder>>,
    known_people: Arc<Mutex<Vec<KnownPerson>>>,
    motion_config: MotionConfig,
    publishing: EventPublishing,
) {
    tokio::task::spawn_blocking(move || {
        let mut tracker = PersonTracker::new();
        let mut motion = MotionGate::new(motion_config);

        let mut frame_counter: u32 = 0;

        // Caméra à ~25 FPS :
        // YOLO sera exécuté au plus environ 4 fois/seconde.
        const YOLO_INTERVAL: u32 = 6;

        while let Ok(img) = detect_rx.recv() {
            frame_counter = frame_counter.wrapping_add(1);

            // CADENCE MAXIMALE DE YOLO
            //
            // Première porte, purement périodique : inutile d'analyser deux
            // frames espacées de 40 ms, le suivi n'y gagnerait rien.
            if !frame_counter.is_multiple_of(YOLO_INTERVAL) {
                // Pas de nouveau YOLO.
                //
                // On republie simplement les dernières détections
                // (déjà dans last_boxes, lues côté flux vidéo).
                // La reconnaissance faciale n'est donc PAS relancée.
                continue;
            }

            // PRÉ-FILTRE DE MOUVEMENT
            //
            // Seconde porte : l'image a-t-elle changé ? C'est l'économie
            // décisive — sur une scène immobile, YOLO ne tourne plus du tout
            // (voir `super::motion` pour les deux garde-fous qui évitent que
            // cette économie se paie en détections manquées).
            let verdict = motion.evaluate(&img);

            if !verdict.scan {
                debug!(
                    "💤 Pas de mouvement ({:.1} % de l'image), YOLO non relancé",
                    verdict.changed_ratio * 100.0
                );
                continue;
            }

            debug!(
                "🔍 YOLO relancé ({:?}, {:.1} % de l'image)",
                verdict.reason,
                verdict.changed_ratio * 100.0
            );

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

                    publish_events(&status_changes, &img, &publishing);
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

/// Déclenche le clip d'une salve de détections, et publie un événement par
/// changement d'état.
///
/// # Ce qui est fait, et dans quel ordre
///
/// Le CLIP est déclenché en premier, pour deux raisons : l'événement doit
/// pouvoir en porter le nom, et il l'est **une seule fois pour toute la
/// salve** — deux personnes détectées ensemble relèvent de la même scène, et
/// `ClipRecorder::start_or_extend` leur renverrait de toute façon le même
/// fichier.
///
/// Il est écrit dès que `[recording] clips_enabled` le demande, **même sans
/// broker MQTT configuré**. C'est voulu : le clip est un enregistrement comme
/// un autre, listé et relu par l'interface embarquée de la caméra au même
/// titre que les enregistrements continus, et purgé par la même rétention. Ne
/// l'écrire que pour le manager ferait d'un réglage explicite une option sans
/// effet dans l'installation la plus simple — une caméra seule.
///
/// La VIGNETTE, elle, n'est encodée que s'il y a un broker : elle n'existe
/// que pour voyager dans l'événement, et la produire pour la jeter coûterait
/// un redimensionnement et un encodage JPEG par détection.
fn publish_events(changes: &[StatusChange], image: &RgbImage, publishing: &EventPublishing) {
    if changes.is_empty() {
        return;
    }

    let clip = publishing.clips.lock_or_recover().start_or_extend();

    let Some(mqtt) = &publishing.mqtt else {
        return;
    };

    for change in changes {
        let mut event = DetectionEvent::now(&publishing.camera_name, change.status.clone())
            // Déclarée même sans clip : elle décrit la CAMÉRA, et c'est elle
            // qui permet au manager de proposer aussi son direct.
            .with_base_url(&publishing.public_url);

        if let Some(jpeg) = thumbnail::encode(image, Some(change.bbox)) {
            event = event.with_thumbnail(&jpeg);
        }

        if let Some(clip) = &clip {
            event = event.with_clip(clip.clone());
        }

        mqtt.publish_event(event);
    }
}
