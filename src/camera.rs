use anyhow::Result;
use chrono::Local;
use font8x8::UnicodeFonts;
use image::{ImageFormat, Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut};
use imageproc::rect::Rect;
use rayon::prelude::*;
use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::config::Config;
use crate::mail::Mailer;
use crate::vision::{BoundingBox, FaceDetectorYuNet, FaceEmbedder, KnownPerson, ObjectDetector};

/// État partagé de la caméra pour la gestion par le WebSocket
pub struct SharedState {
    pub detection_enabled: AtomicBool,
    pub recording_enabled: AtomicBool,
    pub api_token: String,
    pub tx: broadcast::Sender<Vec<u8>>,
}

// ============================================================
// TRACKING DES PERSONNES
//
// Le tracker est basé sur les bounding-box YOLO.
// La reconnaissance faciale est simplement un état associé
// à chaque personne suivie.
//
// Il n'y a PAS de FaceTracker séparé :
// YOLO détecte une personne -> PersonTrack la suit.
// YuNet/ArcFace enrichissent ensuite ce track.
// ============================================================

struct PersonTrack {
    id: u64,

    // Bounding box YOLO
    bbox: (u32, u32, u32, u32),

    // Résultat de reconnaissance
    name: Option<String>,
    similarity: f32,

    // Embedding de la dernière reconnaissance
    embedding: Option<Vec<f32>>,

    // Dernière tentative YuNet + ArcFace
    last_face_recognition: Instant,

    // Dernière fois où YOLO a vu cette personne
    last_seen: Instant,

    // Nombre de frames vues
    frames_seen: u32,

    // bbox lors de la dernière reconnaissance faciale
    last_recognition_bbox: (u32, u32, u32, u32),

    // Nombre de frames pendant lesquelles le track
    // n'a pas été vu par YOLO.
    missed_frames: u32,
}

impl PersonTrack {
    fn needs_recognition(&self) -> bool {
        let now = Instant::now();

        // Nouvelle personne
        if self.name.is_none() {
            return true;
        }

        // La personne a suffisamment bougé
        if Self::has_moved_significantly(self.last_recognition_bbox, self.bbox) {
            return true;
        }

        // Reconnaissance périodique de sécurité
        if now.duration_since(self.last_face_recognition) >= Duration::from_secs(3) {
            return true;
        }

        false
    }

    fn has_moved_significantly(
        old_bbox: (u32, u32, u32, u32),
        new_bbox: (u32, u32, u32, u32),
    ) -> bool {
        let (ox, oy, ow, oh) = old_bbox;
        let (nx, ny, nw, nh) = new_bbox;

        let old_cx = ox as f32 + ow as f32 / 2.0;
        let old_cy = oy as f32 + oh as f32 / 2.0;

        let new_cx = nx as f32 + nw as f32 / 2.0;
        let new_cy = ny as f32 + nh as f32 / 2.0;

        let dx = new_cx - old_cx;
        let dy = new_cy - old_cy;

        let distance = (dx * dx + dy * dy).sqrt();

        // 10% de la taille moyenne de la personne
        let reference_size = ((ow + oh + nw + nh) as f32 / 4.0).max(1.0);

        distance > reference_size * 0.15
    }
}

struct PersonTracker {
    tracks: Vec<PersonTrack>,
    next_id: u64,
}

impl PersonTracker {
    fn new() -> Self {
        Self {
            tracks: Vec::new(),
            next_id: 1,
        }
    }

    // --------------------------------------------------------
    // Calcul IoU
    // --------------------------------------------------------

    fn calculate_iou(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> f32 {
        let (ax, ay, aw, ah) = a;
        let (bx, by, bw, bh) = b;

        let ax2 = ax.saturating_add(aw);
        let ay2 = ay.saturating_add(ah);

        let bx2 = bx.saturating_add(bw);
        let by2 = by.saturating_add(bh);

        let x1 = ax.max(bx);
        let y1 = ay.max(by);

        let x2 = ax2.min(bx2);
        let y2 = ay2.min(by2);

        if x2 <= x1 || y2 <= y1 {
            return 0.0;
        }

        let intersection = (x2 - x1) as f32 * (y2 - y1) as f32;

        let area_a = aw as f32 * ah as f32;

        let area_b = bw as f32 * bh as f32;

        let union = area_a + area_b - intersection;

        if union <= 0.0 {
            return 0.0;
        }

        intersection / union
    }

    // --------------------------------------------------------
    // Mise à jour du tracking
    //
    // IMPORTANT :
    // On retourne les IDs et non les indices du Vec.
    //
    // Cela évite le bug provoqué par retain().
    // --------------------------------------------------------

    fn update(&mut self, detected_boxes: &[(u32, u32, u32, u32)]) -> Vec<u64> {
        let now = Instant::now();

        // Supprimer les tracks trop anciens AVANT
        // de faire les associations.

        self.tracks
            .retain(|track| now.duration_since(track.last_seen) < Duration::from_millis(1500));

        let mut matched = vec![false; self.tracks.len()];

        let mut result = Vec::with_capacity(detected_boxes.len());

        for &bbox in detected_boxes {
            let mut best_index = None;
            let mut best_iou = 0.15f32;

            for (index, track) in self.tracks.iter().enumerate() {
                if matched[index] {
                    continue;
                }

                let iou = Self::calculate_iou(track.bbox, bbox);

                if iou > best_iou {
                    best_iou = iou;
                    best_index = Some(index);
                }
            }

            // ------------------------------------------------
            // Track existant
            // ------------------------------------------------

            if let Some(index) = best_index {
                matched[index] = true;

                let track = &mut self.tracks[index];

                track.bbox = bbox;
                track.last_seen = now;
                track.frames_seen += 1;

                result.push(track.id);
            }
            // ------------------------------------------------
            // Nouvelle personne
            // ------------------------------------------------
            else {
                let id = self.next_id;

                self.next_id += 1;

                self.tracks.push(PersonTrack {
                    id,
                    bbox,
                    name: None,
                    similarity: 0.0,
                    embedding: None,

                    last_face_recognition: now - Duration::from_secs(60),
                    last_seen: now,

                    frames_seen: 1,

                    last_recognition_bbox: bbox,
                    missed_frames: 0,
                });

                result.push(id);
            }
        }

        result
    }

    // --------------------------------------------------------
    // Recherche d'un track par ID
    // --------------------------------------------------------

    fn find_by_id(&self, id: u64) -> Option<usize> {
        self.tracks.iter().position(|track| track.id == id)
    }

    // --------------------------------------------------------
    // Recherche du track correspondant à une bbox
    // --------------------------------------------------------

    fn find_by_bbox(&self, bbox: (u32, u32, u32, u32)) -> Option<usize> {
        let mut best_index = None;
        let mut best_iou = 0.15f32;

        for (index, track) in self.tracks.iter().enumerate() {
            let iou = Self::calculate_iou(track.bbox, bbox);

            if iou > best_iou {
                best_iou = iou;
                best_index = Some(index);
            }
        }

        best_index
    }
}

/// Helper pour dessiner du texte ASCII avec la police bitmap 8x8 native
fn draw_text_8x8(img: &mut RgbImage, text: &str, start_x: i32, start_y: i32, color: Rgb<u8>) {
    for (i, c) in text.chars().enumerate() {
        if let Some(glyph) = font8x8::BASIC_FONTS.get(c) {
            let char_offset_x = start_x + (i as i32 * 8);

            for (row, byte) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if (byte & (1 << col)) != 0 {
                        let px = char_offset_x + col as i32;
                        let py = start_y + row as i32;

                        if px >= 0 && px < img.width() as i32 && py >= 0 && py < img.height() as i32
                        {
                            img.put_pixel(px as u32, py as u32, color);
                        }
                    }
                }
            }
        }
    }
}

/// Charge les photos du dossier `known_faces/` et pré-calcule leurs empreintes faciales
fn load_known_faces(
    face_detector: &FaceDetectorYuNet,
    embedder: &FaceEmbedder,
    folder: &str,
) -> Vec<KnownPerson> {
    let mut known = Vec::new();

    let path = Path::new(folder);

    // ---------------------------------------------------------
    // Création du dossier s'il n'existe pas
    // ---------------------------------------------------------
    if !path.exists() {
        if let Err(e) = std::fs::create_dir_all(path) {
            eprintln!("❌ Impossible de créer le dossier {} : {:?}", folder, e);
        }

        return known;
    }

    // ---------------------------------------------------------
    // Lecture du dossier
    // ---------------------------------------------------------
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("❌ Impossible de lire le dossier {} : {:?}", folder, e);
            return known;
        }
    };

    for entry in entries.flatten() {
        let entry_path = entry.path();

        // On ignore les dossiers
        if !entry_path.is_file() {
            continue;
        }

        // -----------------------------------------------------
        // Vérification de l'extension
        // -----------------------------------------------------
        let extension = match entry_path.extension().and_then(|s| s.to_str()) {
            Some(ext) => ext.to_lowercase(),
            None => continue,
        };

        if !matches!(extension.as_str(), "jpg" | "jpeg" | "png") {
            continue;
        }

        // -----------------------------------------------------
        // Nom de la personne = nom du fichier
        //
        // known_faces/jerome.jpg
        //             ↓
        //          "jerome"
        // -----------------------------------------------------
        let name = entry_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Inconnu")
            .to_string();

        println!("🔄 Chargement du visage connu : {}", entry_path.display());

        // -----------------------------------------------------
        // 1. Chargement de la photo
        // -----------------------------------------------------
        let img = match image::open(&entry_path) {
            Ok(img) => img.to_rgb8(),

            Err(e) => {
                eprintln!(
                    "❌ Impossible de charger {} : {:?}",
                    entry_path.display(),
                    e
                );
                continue;
            }
        };

        // -----------------------------------------------------
        // 2. Détection + alignement YuNet
        //
        // IMPORTANT :
        // detect_face_crop() retourne maintenant directement
        // un visage aligné en 112x112.
        // -----------------------------------------------------
        let face_crop = match face_detector.detect_face_crop(&img) {
            Ok(Some(face)) => face,

            Ok(None) => {
                eprintln!("   ⚠️ Aucun visage détecté dans {}", entry_path.display());

                continue;
            }

            Err(e) => {
                eprintln!("   ❌ Erreur YuNet sur {} : {:?}", entry_path.display(), e);

                continue;
            }
        };

        // -----------------------------------------------------
        // 4. Extraction de l'embedding ArcFace
        // -----------------------------------------------------
        let embedding = match embedder.extract_embedding(&face_crop) {
            Ok(embedding) => embedding,

            Err(e) => {
                eprintln!("   ❌ Erreur ArcFace pour {} : {:?}", name, e);

                continue;
            }
        };

        // -----------------------------------------------------
        // 5. Vérification de l'embedding
        // -----------------------------------------------------
        if embedding.is_empty() {
            eprintln!("   ❌ Embedding vide pour {}", name);

            continue;
        }

        let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

        // -----------------------------------------------------
        // 6. Vérification de la norme
        // -----------------------------------------------------
        if !norm.is_finite() || norm < 1e-6 {
            eprintln!(
                "   ❌ Embedding invalide pour {} (norme = {:.6})",
                name, norm
            );

            continue;
        }

        // -----------------------------------------------------
        // 7. Ajout à la base des personnes connues
        // -----------------------------------------------------
        known.push(KnownPerson {
            name: name.clone(),
            embedding,
        });

        println!("   ✅ Visage connu chargé : {}", name);
    }

    println!("👥 Base de reconnaissance : {} personne(s)", known.len());

    known
}

fn process_persons_parallel(
    img: &RgbImage,
    person_boxes: &[(u32, u32, u32, u32)],
    tracker: &mut PersonTracker,
    face_detector: &FaceDetectorYuNet,
    face_embedder: &FaceEmbedder,
    known_people: &[KnownPerson],
    threshold: f32,
) {
    // ------------------------------------------------------------
    // 1. Mise à jour du tracking
    // ------------------------------------------------------------
    let track_ids = tracker.update(person_boxes);

    // ------------------------------------------------------------
    // 2. Chercher les personnes qui doivent être reconnues
    // ------------------------------------------------------------
    let mut recognition_jobs = Vec::new();

    for track_id in track_ids {
        let Some(track_index) = tracker.find_by_id(track_id) else {
            continue;
        };

        let track = &tracker.tracks[track_index];

        if track.needs_recognition() {
            recognition_jobs.push((track_id, track.bbox));
        }
    }

    if recognition_jobs.is_empty() {
        return;
    }

    // ------------------------------------------------------------
    // 3. Reconnaissance parallèle
    // ------------------------------------------------------------
    let results: Vec<(u64, Option<(String, f32)>)> = recognition_jobs
        .par_iter()
        .filter_map(|(track_id, bbox)| {
            let (x, y, w, h) = *bbox;

            // Taille minimale de la personne
            if w < 40 || h < 40 {
                return Some((*track_id, None));
            }

            // Vérification des coordonnées
            if x >= img.width() || y >= img.height() {
                return Some((*track_id, None));
            }

            let max_w = img.width().saturating_sub(x);
            let max_h = img.height().saturating_sub(y);

            let crop_w = w.min(max_w);
            let crop_h = h.min(max_h);

            if crop_w < 40 || crop_h < 40 {
                return Some((*track_id, None));
            }

            // ----------------------------------------------------
            // Crop personne
            // ----------------------------------------------------
            let person_crop = image::imageops::crop_imm(img, x, y, crop_w, crop_h).to_image();

            // ----------------------------------------------------
            // YuNet
            // ----------------------------------------------------
            let face_crop = match face_detector.detect_face_crop(&person_crop) {
                Ok(Some(face)) => face,

                Ok(None) => {
                    println!("🙂 Track #{} : aucun visage exploitable", track_id);

                    return Some((*track_id, None));
                }

                Err(e) => {
                    eprintln!("⚠️ YuNet erreur Track #{} : {:?}", track_id, e);

                    return Some((*track_id, None));
                }
            };

            // ----------------------------------------------------
            // ArcFace
            // ----------------------------------------------------
            let embedding = match face_embedder.extract_embedding(&face_crop) {
                Ok(embedding) => embedding,

                Err(e) => {
                    eprintln!("⚠️ ArcFace erreur Track #{} : {:?}", track_id, e);

                    return Some((*track_id, None));
                }
            };

            // ----------------------------------------------------
            // Identification
            // ----------------------------------------------------
            let identity = FaceEmbedder::identify_person(&embedding, known_people, threshold);

            match &identity {
                Some((name, similarity)) => {
                    println!("🎯 Track #{} → {} ({:.3})", track_id, name, similarity);
                }

                None => {
                    println!("❓ Track #{} → inconnu", track_id);
                }
            }

            Some((*track_id, identity))
        })
        .collect();

    // ------------------------------------------------------------
    // 4. Mise à jour des tracks
    // ------------------------------------------------------------
    let now = Instant::now();

    for (track_id, identity) in results {
        let Some(track) = tracker.find_by_id(track_id) else {
            continue;
        };

        let track = &mut tracker.tracks[track];

        // Une tentative de reconnaissance vient d'avoir lieu
        track.last_face_recognition = now;

        // Bbox utilisée pour cette reconnaissance
        track.last_recognition_bbox = track.bbox;

        match identity {
            Some((name, similarity)) => {
                track.name = Some(name.clone());
                track.similarity = similarity;

                println!(
                    "✅ Track #{} identité mémorisée : {} ({:.3})",
                    track_id, name, similarity
                );
            }

            None => {
                // On conserve volontairement l'identité précédente.
                //
                // Exemple :
                // Jérôme est reconnu puis YuNet rate temporairement
                // son visage -> on garde "jerome".
            }
        }
    }
}

pub fn start_camera_loop(config: Config, state: Arc<SharedState>) -> Result<()> {
    // Initialisation dynamique du périphérique V4L2
    let dev = Device::new(config.camera.device_index)?;
    let fmt = dev.format()?;
    println!(
        "🎥 Caméra détectée : {}x{} ({:?})",
        fmt.width, fmt.height, fmt.fourcc
    );

    // Allocation de 2 buffers MMAP
    let mut stream = MmapStream::with_buffers(&dev, Type::VideoCapture, 2)?;

    // Initialisation de l'IA (ObjectDetector) et du Mailer
    let detector = ObjectDetector::new(config.detection.clone())?;
    let mailer = Mailer::new(config.email.clone());

    // Chargement explicite de YuNet (détecteur de visages) avec gestion d'erreur
    let face_detector = match FaceDetectorYuNet::new(config.detection.clone()) {
        Ok(detector) => {
            println!("✅ Modèle YuNet chargé avec succès.");
            Arc::new(Some(detector))
        }
        Err(e) => {
            eprintln!("❌ Erreur d'initialisation YuNet : {:?}", e);
            Arc::new(None)
        }
    };

    let face_embedder = Arc::new(FaceEmbedder::new(config.detection.clone()).ok());

    let known_people = if let (Some(detector), Some(embedder)) = (&*face_detector, &*face_embedder)
    {
        Arc::new(load_known_faces(detector, embedder, "known_faces"))
    } else {
        eprintln!("⚠️ YuNet ou ArcFace indisponible.");
        Arc::new(Vec::new())
    };

    // Canal non-bloquant pour la détection IA en arrière-plan
    let (detect_tx, detect_rx) = mpsc::sync_channel::<RgbImage>(1);
    let last_boxes: Arc<Mutex<Vec<BoundingBox>>> = Arc::new(Mutex::new(Vec::new()));

    let last_boxes_worker = Arc::clone(&last_boxes);
    let face_detector_worker = Arc::clone(&face_detector);
    let face_embedder_worker = Arc::clone(&face_embedder);
    let known_people_worker = Arc::clone(&known_people);

    // ============================================================
    // Worker IA
    //
    // YOLO
    //   ↓
    // PersonTracker
    //   ↓
    // YuNet + ArcFace
    //   ↓
    // Rayon
    //   ↓
    // résultat mis en cache
    // ============================================================

    tokio::task::spawn_blocking(move || {
        let mut tracker = PersonTracker::new();

        let mut frame_counter: u32 = 0;

        // Caméra à ~25 FPS :
        // YOLO sera exécuté environ 4 fois/seconde.
        const YOLO_INTERVAL: u32 = 6;

        while let Ok(img) = detect_rx.recv() {
            frame_counter = frame_counter.wrapping_add(1);

            // =====================================================
            // YOLO UNIQUEMENT TOUTES LES N FRAMES
            // =====================================================

            let run_yolo = frame_counter % YOLO_INTERVAL == 0;

            if !run_yolo {
                // -------------------------------------------------
                // Pas de nouveau YOLO.
                //
                // On republie simplement les dernières détections.
                // La reconnaissance faciale n'est donc PAS relancée.
                // -------------------------------------------------

                if let Ok(guard) = last_boxes_worker.lock() {
                    if !guard.is_empty() {

                        // Rien à recalculer ici.
                        // Les BoundingBox déjà calculées restent valides.
                    }
                }

                continue;
            }

            // =====================================================
            // YOLO
            // =====================================================

            let mut boxes = match detector.detect(&img) {
                Ok(boxes) => boxes,

                Err(e) => {
                    eprintln!("❌ Erreur YOLO : {:?}", e);
                    continue;
                }
            };

            // =====================================================
            // Extraire uniquement les personnes
            // =====================================================

            let person_boxes: Vec<(u32, u32, u32, u32)> = boxes
                .iter()
                .filter(|bbox| bbox.label == "person" && bbox.width > 20 && bbox.height > 20)
                .map(|bbox| (bbox.x, bbox.y, bbox.width, bbox.height))
                .collect();

            // =====================================================
            // Tracking + reconnaissance faciale
            // =====================================================

            if !person_boxes.is_empty() {
                if let (Some(face_detector), Some(face_embedder)) =
                    (face_detector_worker.as_ref(), face_embedder_worker.as_ref())
                {
                    process_persons_parallel(
                        &img,
                        &person_boxes,
                        &mut tracker,
                        face_detector,
                        face_embedder,
                        known_people_worker.as_slice(),
                        0.55,
                    );
                }
            } else {
                // -------------------------------------------------
                // Aucune personne détectée.
                //
                // Le tracker va supprimer progressivement les
                // anciennes personnes selon son timeout.
                // -------------------------------------------------

                tracker.update(&[]);
            }

            // =====================================================
            // Appliquer le résultat du tracking aux BoundingBox
            // =====================================================

            for bbox in &mut boxes {
                if bbox.label != "person" {
                    continue;
                }

                let coords = (bbox.x, bbox.y, bbox.width, bbox.height);

                // Recherche du track correspondant
                let Some(track_index) = tracker.find_by_bbox(coords) else {
                    continue;
                };

                if track_index >= tracker.tracks.len() {
                    continue;
                }

                let track = &tracker.tracks[track_index];

                // -------------------------------------------------
                // Identité connue
                // -------------------------------------------------

                if let Some(name) = &track.name {
                    bbox.label = name.clone();

                    // La similarité devient la confiance affichée
                    bbox.confidence = track.similarity;
                }
            }

            // =====================================================
            // Publication des nouvelles BoundingBox
            // =====================================================

            if let Ok(mut guard) = last_boxes_worker.lock() {
                *guard = boxes;
            }
        }
    });

    let mut video_file: Option<File> = None;
    let mut last_email_time =
        Instant::now() - Duration::from_secs(config.detection.email_cooldown_secs);

    println!("📹 Boucle Caméra démarrée avec succès.");

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
                decode_yuyv_to_rgb(buf, fmt.width, fmt.height)
            };

            if let Some(mut img) = decoded_img {
                let _ = detect_tx.try_send(img.clone());

                let current_boxes = last_boxes.lock().unwrap_or_else(|e| e.into_inner()).clone();

                if !current_boxes.is_empty() {
                    for bbox in &current_boxes {
                        let is_known =
                            bbox.label != "person" && bbox.label != "cat" && bbox.label != "dog";

                        let box_color = if is_known {
                            Rgb([0u8, 255u8, 0u8])
                        } else {
                            Rgb([255u8, 0u8, 0u8])
                        };
                        let white = Rgb([255u8, 255u8, 255u8]);

                        let rect =
                            Rect::at(bbox.x as i32, bbox.y as i32).of_size(bbox.width, bbox.height);
                        draw_hollow_rect_mut(&mut img, rect, box_color);

                        let caption = if is_known {
                            format!("{}", bbox.label)
                        } else {
                            format!("{} {:.0}%", bbox.label, bbox.confidence * 100.0)
                        };

                        let text_bg_height = 10u32;
                        let text_bg_width = (caption.len() * 8) as u32 + 2;
                        let text_bg_y = if bbox.y >= text_bg_height {
                            bbox.y - text_bg_height
                        } else {
                            bbox.y
                        };

                        let bg_rect = Rect::at(bbox.x as i32, text_bg_y as i32)
                            .of_size(text_bg_width, text_bg_height);
                        draw_filled_rect_mut(&mut img, bg_rect, box_color);

                        draw_text_8x8(
                            &mut img,
                            &caption,
                            bbox.x as i32 + 1,
                            text_bg_y as i32 + 1,
                            white,
                        );
                    }

                    if last_email_time.elapsed()
                        >= Duration::from_secs(config.detection.email_cooldown_secs)
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
            if let Ok(mut guard) = last_boxes.lock() {
                guard.clear();
            }

            if is_jpeg {
                buf.to_vec()
            } else {
                if let Some(img) = decode_yuyv_to_rgb(buf, fmt.width, fmt.height) {
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
            if video_file.is_none() {
                std::fs::create_dir_all("output_record")?;

                let filename = format!(
                    "output_record/rec_{}.mjpeg",
                    Local::now().format("%Y%m%d_%H%M%S")
                );
                video_file = Some(File::create(&filename)?);
                println!("💾 Début d'enregistrement : {}", filename);
            }
            if let Some(ref mut file) = video_file {
                let _ = file.write_all(&jpeg_bytes);
            }
        } else if video_file.is_some() {
            println!("💾 Arrêt de l'enregistrement.");
            video_file = None;
        }

        let _ = state.tx.send(jpeg_bytes);
    }
}

/// Décode un flux brut YUYV (YUY2) en RgbImage de manière totalement parallélisée avec Rayon
fn decode_yuyv_to_rgb(buf: &[u8], width: u32, height: u32) -> Option<RgbImage> {
    let total_pixels = (width * height) as usize;
    if buf.len() < total_pixels * 2 {
        return None;
    }

    let mut raw_rgb = vec![0u8; total_pixels * 3];

    // Utilisation de Rayon ici pour paralléliser la conversion par paquets
    raw_rgb
        .par_chunks_exact_mut(6)
        .zip(buf.par_chunks_exact(4))
        .for_each(|(rgb_out, chunk)| {
            let y0 = chunk[0] as f32;
            let u = chunk[1] as f32 - 128.0;
            let y1 = chunk[2] as f32;
            let v = chunk[3] as f32 - 128.0;

            // Pixel 1
            rgb_out[0] = (y0 + 1.402 * v).clamp(0.0, 255.0) as u8;
            rgb_out[1] = (y0 - 0.34414 * u - 0.71414 * v).clamp(0.0, 255.0) as u8;
            rgb_out[2] = (y0 + 1.772 * u).clamp(0.0, 255.0) as u8;

            // Pixel 2
            rgb_out[3] = (y1 + 1.402 * v).clamp(0.0, 255.0) as u8;
            rgb_out[4] = (y1 - 0.34414 * u - 0.71414 * v).clamp(0.0, 255.0) as u8;
            rgb_out[5] = (y1 + 1.772 * u).clamp(0.0, 255.0) as u8;
        });

    RgbImage::from_raw(width, height, raw_rgb)
}
