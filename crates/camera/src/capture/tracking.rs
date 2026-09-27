//! Suivi des personnes (tracking) d'une frame à l'autre par IoU, et
//! reconnaissance faciale parallèle (YuNet + ArcFace) pour les tracks qui en
//! ont besoin.
//!
//! Le tracker de personnes est basé sur les bounding-box YOLO : il n'y a pas
//! de FaceTracker séparé. YOLO détecte une personne, [`PersonTracker`] la
//! suit d'une frame à l'autre, puis YuNet/ArcFace enrichissent ce track avec
//! un résultat de reconnaissance faciale (état associé, voir `PersonTrack`).

use rayon::prelude::*;
use std::time::{Duration, Instant};

use image::RgbImage;

use crate::mqtt::PersonStatus;
use crate::vision::{BoundingBox, FaceDetectorYuNet, FaceEmbedder, KnownPerson};

/// Une personne suivie d'une frame à l'autre, avec son dernier résultat de
/// reconnaissance faciale (voir la note de module ci-dessus).
struct PersonTrack {
    id: u64,

    // Bounding box YOLO
    bbox: (u32, u32, u32, u32),

    // Résultat de reconnaissance
    name: Option<String>,
    similarity: f32,

    // Dernière tentative YuNet + ArcFace
    last_face_recognition: Instant,

    // Dernière fois où YOLO a vu cette personne
    last_seen: Instant,

    // Nombre de frames vues
    frames_seen: u32,

    // bbox lors de la dernière reconnaissance faciale
    last_recognition_bbox: (u32, u32, u32, u32),

    // Dernier statut (inconnu / connu-untel) publié sur MQTT pour ce track
    // (voir `crate::mqtt`), pour ne publier qu'aux changements d'état
    // plutôt qu'à chaque tentative de reconnaissance. `None` tant qu'aucune
    // publication n'a encore eu lieu pour ce track.
    last_reported_status: Option<PersonStatus>,
}

/// Statut de reconnaissance courant d'un track, tel que dérivé de son nom
/// (voir [`PersonTrack::name`]).
fn status_from_name(name: &Option<String>) -> PersonStatus {
    match name {
        Some(n) => PersonStatus::Known(n.clone()),
        None => PersonStatus::Unknown,
    }
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

/// Associe les bounding-box YOLO d'une frame à l'autre (par IoU) pour
/// maintenir une identité de tracking stable par personne.
pub(super) struct PersonTracker {
    tracks: Vec<PersonTrack>,
    next_id: u64,
}

impl PersonTracker {
    pub(super) fn new() -> Self {
        Self {
            tracks: Vec::new(),
            next_id: 1,
        }
    }

    /// Calcul de l'intersection sur union (IoU) entre deux bounding-box (voir
    /// [`crate::geometry::iou`], partagé avec la détection de visages)
    fn calculate_iou(a: (u32, u32, u32, u32), b: (u32, u32, u32, u32)) -> f32 {
        let to_rect = |(x, y, w, h): (u32, u32, u32, u32)| (x as f32, y as f32, w as f32, h as f32);

        crate::geometry::iou(to_rect(a), to_rect(b))
    }

    /// Met à jour le tracker avec les bounding-box YOLO détectées sur la
    /// frame courante et retourne l'ID de track associé à chacune, dans le
    /// même ordre.
    ///
    /// Important : on retourne les IDs (et non les indices du `Vec`), ce qui
    /// évite le bug d'indices invalidés par `retain()`.
    pub(super) fn update(&mut self, detected_boxes: &[(u32, u32, u32, u32)]) -> Vec<u64> {
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

            // Track existant

            if let Some(index) = best_index {
                matched[index] = true;

                let track = &mut self.tracks[index];

                track.bbox = bbox;
                track.last_seen = now;
                track.frames_seen += 1;

                result.push(track.id);
            }
            // Nouvelle personne
            else {
                let id = self.next_id;

                self.next_id += 1;

                self.tracks.push(PersonTrack {
                    id,
                    bbox,
                    name: None,
                    similarity: 0.0,

                    last_face_recognition: now - Duration::from_secs(60),
                    last_seen: now,

                    frames_seen: 1,

                    last_recognition_bbox: bbox,

                    last_reported_status: None,
                });

                // IMPORTANT : `matched` doit rester de la même longueur que
                // `self.tracks`, qui vient de grandir. Sans ce push, la
                // détection suivante de CETTE MÊME frame ferait `matched[index]`
                // avec un `index` provenant de `self.tracks.iter().enumerate()`
                // (donc jusqu'à `self.tracks.len() - 1`), hors des bornes de
                // `matched` -> panique dès que 2 nouvelles personnes
                // apparaissent dans la même frame (ex : première frame avec
                // plusieurs personnes déjà dans le champ). Marquer ce
                // nouveau track comme "déjà apparié" est aussi correct
                // sémantiquement : deux bounding-box distinctes de la même
                // frame ne doivent jamais fusionner sur le même track.
                matched.push(true);

                result.push(id);
            }
        }

        result
    }

    /// Recherche d'un track par ID
    pub(super) fn find_by_id(&self, id: u64) -> Option<usize> {
        self.tracks.iter().position(|track| track.id == id)
    }

    /// Recherche du track correspondant à une bbox (par IoU maximal)
    pub(super) fn find_by_bbox(&self, bbox: (u32, u32, u32, u32)) -> Option<usize> {
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

/// Met à jour le tracker avec les bounding-box YOLO de la frame courante, puis
/// lance (en parallèle via Rayon) la reconnaissance faciale YuNet + ArcFace
/// pour chaque track qui en a besoin (voir [`PersonTrack::needs_recognition`]),
/// mémorise l'identité retenue sur le track correspondant, et retourne les
/// changements d'état (inconnu <-> connu-untel) à publier sur MQTT par
/// l'appelant (voir `super::worker` et `crate::mqtt`). Cette fonction ne
/// fait elle-même aucune publication : elle reste, comme le reste de ce
/// module, dépourvue d'effets de bord réseau.
#[allow(clippy::too_many_arguments)]
pub(super) fn process_persons_parallel(
    img: &RgbImage,
    person_boxes: &[(u32, u32, u32, u32)],
    tracker: &mut PersonTracker,
    face_detector: &FaceDetectorYuNet,
    face_embedder: &FaceEmbedder,
    known_people: &[KnownPerson],
    threshold: f32,
) -> Vec<PersonStatus> {
    // Mise à jour du tracking
    let track_ids = tracker.update(person_boxes);

    // Chercher les personnes qui doivent être reconnues
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
        return Vec::new();
    }

    // Reconnaissance parallèle
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

            // Crop personne
            let person_crop = image::imageops::crop_imm(img, x, y, crop_w, crop_h).to_image();

            // YuNet
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

            // ArcFace
            let embedding = match face_embedder.extract_embedding(&face_crop) {
                Ok(embedding) => embedding,

                Err(e) => {
                    eprintln!("⚠️ ArcFace erreur Track #{} : {:?}", track_id, e);

                    return Some((*track_id, None));
                }
            };

            // Identification
            let identity = FaceEmbedder::identify_person(&embedding, known_people, threshold);

            // DEBUG TEMPORAIRE : affiche la meilleure similarité brute même
            // sous le seuil, pour savoir si on est "proche" (embedding correct,
            // seuil/alignement à ajuster) ou "loin" (embedding non exploitable).
            if let Some((closest_name, closest_sim)) = known_people
                .iter()
                .map(|p| {
                    (
                        p.name.as_str(),
                        crate::vision::cosine_similarity(&embedding, &p.embedding),
                    )
                })
                .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            {
                println!(
                    "🔬 Track #{} meilleure similarité brute : {} = {:.3} (seuil={:.2})",
                    track_id, closest_name, closest_sim, threshold
                );
            }

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

    apply_recognition_results(tracker, results)
}

/// Applique les résultats de reconnaissance déjà calculés (potentiellement
/// en parallèle via Rayon, voir [`process_persons_parallel`]) aux tracks
/// correspondants, et retourne les changements d'état à publier sur MQTT.
///
/// Extrait de [`process_persons_parallel`] pour être testable sans dépendre
/// de YuNet/ArcFace (voir les tests en fin de fichier) : cette fonction ne
/// prend que des résultats déjà calculés, jamais d'image ni de modèle ONNX.
fn apply_recognition_results(
    tracker: &mut PersonTracker,
    results: Vec<(u64, Option<(String, f32)>)>,
) -> Vec<PersonStatus> {
    let now = Instant::now();
    let mut status_changes = Vec::new();

    for (track_id, identity) in results {
        let Some(track_index) = tracker.find_by_id(track_id) else {
            continue;
        };

        let track = &mut tracker.tracks[track_index];

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

        // Publication MQTT uniquement sur changement d'état (voir
        // `PersonTrack::last_reported_status`) : la première tentative de
        // reconnaissance d'un track publie toujours (transition depuis
        // "jamais publié"), les tentatives suivantes ne republient que si le
        // statut a réellement changé (ex : inconnu -> "jerome").
        let current_status = status_from_name(&track.name);

        if track.last_reported_status.as_ref() != Some(&current_status) {
            track.last_reported_status = Some(current_status.clone());
            status_changes.push(current_status);
        }
    }

    status_changes
}

/// Applique l'identité mémorisée par le tracker (voir
/// [`process_persons_parallel`]) aux bounding-box `person` de la frame
/// courante : le nom reconnu devient le label affiché, et la similarité
/// devient la confiance affichée.
pub(super) fn apply_track_identities(tracker: &PersonTracker, boxes: &mut [BoundingBox]) {
    for bbox in boxes {
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

        // Identité connue

        if let Some(name) = &track.name {
            bbox.label = name.clone();

            // La similarité devient la confiance affichée
            bbox.confidence = track.similarity;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(
        name: Option<&str>,
        last_face_recognition: Instant,
        bbox: (u32, u32, u32, u32),
    ) -> PersonTrack {
        PersonTrack {
            id: 1,
            bbox,
            name: name.map(str::to_string),
            similarity: if name.is_some() { 0.9 } else { 0.0 },
            last_face_recognition,
            last_seen: Instant::now(),
            frames_seen: 1,
            last_recognition_bbox: bbox,
            last_reported_status: None,
        }
    }

    // --- PersonTrack::needs_recognition ---

    #[test]
    fn needs_recognition_is_true_for_a_brand_new_track() {
        let t = track(None, Instant::now(), (0, 0, 10, 10));
        assert!(t.needs_recognition());
    }

    #[test]
    fn needs_recognition_is_false_right_after_a_successful_recognition() {
        let t = track(Some("jerome"), Instant::now(), (0, 0, 10, 10));
        assert!(!t.needs_recognition());
    }

    #[test]
    fn needs_recognition_is_true_after_the_periodic_safety_timeout() {
        let t = track(
            Some("jerome"),
            Instant::now() - Duration::from_secs(4),
            (0, 0, 10, 10),
        );
        assert!(t.needs_recognition());
    }

    #[test]
    fn needs_recognition_is_true_when_the_track_has_moved_significantly() {
        let mut t = track(Some("jerome"), Instant::now(), (100, 100, 50, 50));
        // La bbox courante s'est beaucoup déplacée depuis la dernière
        // reconnaissance (`last_recognition_bbox`).
        t.bbox = (400, 400, 50, 50);
        assert!(t.needs_recognition());
    }

    // --- PersonTrack::has_moved_significantly ---

    #[test]
    fn has_moved_significantly_is_false_for_a_tiny_shift() {
        let old = (100, 100, 50, 50);
        let new = (101, 101, 50, 50);
        assert!(!PersonTrack::has_moved_significantly(old, new));
    }

    #[test]
    fn has_moved_significantly_is_true_for_a_large_shift() {
        let old = (100, 100, 50, 50);
        let new = (300, 300, 50, 50);
        assert!(PersonTrack::has_moved_significantly(old, new));
    }

    // --- PersonTracker::update ---

    #[test]
    fn update_assigns_sequential_ids_to_new_detections() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(0, 0, 10, 10), (100, 100, 10, 10)]);
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn update_returns_empty_when_there_is_nothing_detected() {
        let mut tracker = PersonTracker::new();
        assert!(tracker.update(&[]).is_empty());
    }

    #[test]
    fn update_keeps_the_same_id_across_a_small_movement() {
        let mut tracker = PersonTracker::new();
        let first = tracker.update(&[(100, 100, 50, 50)]);
        let second = tracker.update(&[(105, 102, 50, 50)]);
        assert_eq!(first, second);
    }

    #[test]
    fn update_assigns_a_new_id_when_there_is_no_meaningful_overlap() {
        let mut tracker = PersonTracker::new();
        let first = tracker.update(&[(0, 0, 10, 10)]);
        let second = tracker.update(&[(900, 900, 10, 10)]);
        assert_ne!(first[0], second[0]);
    }

    #[test]
    fn find_by_id_and_find_by_bbox_agree_on_the_same_track() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);

        let by_id = tracker.find_by_id(ids[0]).expect("track trouvé par id");
        let by_bbox = tracker
            .find_by_bbox((10, 10, 50, 50))
            .expect("track trouvé par bbox");
        assert_eq!(by_id, by_bbox);
    }

    #[test]
    fn find_by_id_returns_none_for_an_unknown_id() {
        let tracker = PersonTracker::new();
        assert_eq!(tracker.find_by_id(12345), None);
    }

    // --- apply_track_identities ---

    #[test]
    fn apply_track_identities_labels_a_matching_person_box_with_the_recognized_name() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);
        let idx = tracker.find_by_id(ids[0]).unwrap();
        tracker.tracks[idx].name = Some("jerome".to_string());
        tracker.tracks[idx].similarity = 0.87;

        let mut boxes = vec![BoundingBox {
            x: 10,
            y: 10,
            width: 50,
            height: 50,
            label: "person".to_string(),
            confidence: 0.95,
        }];

        apply_track_identities(&tracker, &mut boxes);

        assert_eq!(boxes[0].label, "jerome");
        assert!((boxes[0].confidence - 0.87).abs() < 1e-6);
    }

    #[test]
    fn apply_track_identities_leaves_unknown_person_boxes_unchanged() {
        let tracker = PersonTracker::new(); // aucun track

        let mut boxes = vec![BoundingBox {
            x: 10,
            y: 10,
            width: 50,
            height: 50,
            label: "person".to_string(),
            confidence: 0.95,
        }];

        apply_track_identities(&tracker, &mut boxes);

        assert_eq!(boxes[0].label, "person");
        assert!((boxes[0].confidence - 0.95).abs() < 1e-6);
    }

    #[test]
    fn apply_track_identities_ignores_non_person_boxes() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);
        let idx = tracker.find_by_id(ids[0]).unwrap();
        tracker.tracks[idx].name = Some("jerome".to_string());
        tracker.tracks[idx].similarity = 0.87;

        let mut boxes = vec![BoundingBox {
            x: 10,
            y: 10,
            width: 50,
            height: 50,
            label: "cat".to_string(),
            confidence: 0.95,
        }];

        apply_track_identities(&tracker, &mut boxes);

        // "cat" n'est jamais réécrit, seules les boxes "person" le sont.
        assert_eq!(boxes[0].label, "cat");
    }

    // --- apply_recognition_results (changements d'état pour MQTT) ---

    #[test]
    fn first_recognition_attempt_with_no_face_found_reports_unknown_once() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);

        let changes = apply_recognition_results(&mut tracker, vec![(ids[0], None)]);

        assert_eq!(changes, vec![PersonStatus::Unknown]);
        let idx = tracker.find_by_id(ids[0]).unwrap();
        assert_eq!(
            tracker.tracks[idx].last_reported_status,
            Some(PersonStatus::Unknown)
        );
    }

    #[test]
    fn first_recognition_attempt_with_a_match_reports_known() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);

        let changes = apply_recognition_results(
            &mut tracker,
            vec![(ids[0], Some(("jerome".to_string(), 0.9)))],
        );

        assert_eq!(changes, vec![PersonStatus::Known("jerome".to_string())]);
    }

    #[test]
    fn repeated_failed_recognition_does_not_republish_the_same_unknown_status() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);

        let first = apply_recognition_results(&mut tracker, vec![(ids[0], None)]);
        assert_eq!(first, vec![PersonStatus::Unknown]);

        // Deuxième tentative, toujours sans visage exploitable : le statut
        // ("unknown") n'a pas changé, donc rien à republier.
        let second = apply_recognition_results(&mut tracker, vec![(ids[0], None)]);
        assert!(second.is_empty());
    }

    #[test]
    fn transition_from_unknown_to_known_reports_the_new_status() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);

        apply_recognition_results(&mut tracker, vec![(ids[0], None)]);
        let changes = apply_recognition_results(
            &mut tracker,
            vec![(ids[0], Some(("jerome".to_string(), 0.9)))],
        );

        assert_eq!(changes, vec![PersonStatus::Known("jerome".to_string())]);
    }

    #[test]
    fn repeated_successful_recognition_of_the_same_person_does_not_republish() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);

        apply_recognition_results(
            &mut tracker,
            vec![(ids[0], Some(("jerome".to_string(), 0.9)))],
        );
        // Reconnaissance périodique de sécurité, même identité retrouvée.
        let second = apply_recognition_results(
            &mut tracker,
            vec![(ids[0], Some(("jerome".to_string(), 0.95)))],
        );

        assert!(second.is_empty());
    }

    #[test]
    fn a_failed_recognition_after_a_known_status_does_not_revert_to_unknown() {
        // Comportement volontaire (voir le commentaire dans
        // `apply_recognition_results`) : une identité déjà connue est
        // conservée si une tentative ultérieure ne retrouve pas de visage,
        // donc aucun changement d'état (et donc aucune publication MQTT
        // erronée "personne inconnue") ne doit avoir lieu dans ce cas.
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(10, 10, 50, 50)]);

        apply_recognition_results(
            &mut tracker,
            vec![(ids[0], Some(("jerome".to_string(), 0.9)))],
        );
        let second = apply_recognition_results(&mut tracker, vec![(ids[0], None)]);

        assert!(second.is_empty());
        let idx = tracker.find_by_id(ids[0]).unwrap();
        assert_eq!(
            tracker.tracks[idx].last_reported_status,
            Some(PersonStatus::Known("jerome".to_string()))
        );
    }

    #[test]
    fn results_for_an_unknown_track_id_are_silently_ignored() {
        let mut tracker = PersonTracker::new();

        // Aucun track n'existe avec cet id (ex : expiré entre-temps).
        let changes = apply_recognition_results(&mut tracker, vec![(9999, None)]);

        assert!(changes.is_empty());
    }

    #[test]
    fn multiple_tracks_can_each_report_their_own_status_change() {
        let mut tracker = PersonTracker::new();
        let ids = tracker.update(&[(0, 0, 50, 50), (200, 200, 50, 50)]);

        let changes = apply_recognition_results(
            &mut tracker,
            vec![(ids[0], Some(("jerome".to_string(), 0.9))), (ids[1], None)],
        );

        assert_eq!(changes.len(), 2);
        assert!(changes.contains(&PersonStatus::Known("jerome".to_string())));
        assert!(changes.contains(&PersonStatus::Unknown));
    }
}
