//! Chargement et rechargement à chaud de la base de visages connus
//! (`known_faces/`), utilisée par la reconnaissance faciale (voir
//! `super::tracking`).

use anyhow::Result;
use chrono::Local;
use image::{ImageFormat, RgbImage};
use rayon::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::util::MutexExt;
use crate::vision::{FaceDetectorYuNet, FaceEmbedder, KnownPerson};

use super::state::SharedState;

/// Détecte un visage dans une image potentiellement bien plus grande que
/// 640x640 (cas des photos de `known_faces/`), sans jamais RÉDUIRE l'image.
///
/// IMPORTANT  :
/// la tête de régression bbox de ce modèle YuNet régresse correctement
/// quand elle reçoit des pixels à résolution NATIVE (même si le visage
/// occupe une grande partie du canevas 640x640), mais devient très
/// imprécise (bbox jusqu'à 3-4x trop grande) dès que l'image entière est
/// réduite pour tenir dans 640x640. Le modèle ONNX lui-même n'accepte que
/// des entrées exactement 640x640 (des constantes internes empêchent de le
/// faire tourner à une autre résolution avec tract).
///
/// On scanne donc l'image par fenêtres de 640x640 prises à résolution
/// native (simple recadrage, jamais de redimensionnement), avec un
/// recouvrement, et on garde la détection au score le PLUS ÉLEVÉ sur
/// l'ensemble des fenêtres (pas la première trouvée : une fenêtre qui coupe
/// le visage près d'un bord donne un score bas et un cadrage moins précis
/// qu'une fenêtre où le visage est bien centré). Un arrêt anticipé
/// (`EARLY_EXIT_SCORE`) limite le nombre de fenêtres testées une fois une
/// bonne détection trouvée. Cette fonction n'est utilisée que par
/// [`load_known_faces`] : au démarrage, et à chaque capture de référence
/// depuis l'UI web (rechargement à chaud, voir [`try_capture_reference`]) —
/// dans les deux cas en dehors du chemin critique du flux caméra live.
fn detect_face_native_res(
    face_detector: &FaceDetectorYuNet,
    img: &RgbImage,
) -> Result<Option<RgbImage>> {
    const WINDOW: u32 = 640;
    // Recouvrement large (60%) : un visage proche de la taille du canevas
    // (jusqu'à ~600px) doit tenir ENTIER dans au moins une fenêtre, sans
    // être coupé par un bord, pour que le score de détection reste élevé.
    const STRIDE: u32 = 256;
    // Empiriquement, un visage bien centré dans une fenêtre 640x640 native
    // obtient déjà un score proche de 0.63 (sous le 0.65 utilisé en live) ;
    // les fenêtres du scan ne sont presque jamais parfaitement centrées,
    // donc on utilise un seuil plus permissif ici.
    const SCAN_SCORE_THRESHOLD: f32 = 0.5;
    // Score à partir duquel on arrête le scan : inutile de continuer à tester
    // des dizaines de fenêtres restantes une fois qu'on a déjà une détection
    // de très bonne qualité (le meilleur score observé empiriquement sur une
    // fenêtre bien centrée tourne autour de 0.65-0.66). Ce seuil accélère
    // nettement le chargement des grandes photos de référence.
    const EARLY_EXIT_SCORE: f32 = 0.6;

    let (w, h) = (img.width(), img.height());

    // Image déjà assez petite : un seul passage (detect_face_crop gère déjà
    // le letterbox sans réduction dans ce cas).
    if w <= WINDOW && h <= WINDOW {
        return Ok(face_detector
            .detect_face_crop_with_threshold(img, SCAN_SCORE_THRESHOLD)?
            .map(|(face, _score)| face));
    }

    let mut best: Option<(f32, RgbImage)> = None;

    let mut y = 0u32;
    'scan: loop {
        let win_h = WINDOW.min(h - y);

        let mut x = 0u32;
        loop {
            let win_w = WINDOW.min(w - x);

            let window = image::imageops::crop_imm(img, x, y, win_w, win_h).to_image();

            if let Ok(Some((face, score))) =
                face_detector.detect_face_crop_with_threshold(&window, SCAN_SCORE_THRESHOLD)
            {
                let is_better = match &best {
                    Some((best_score, _)) => score > *best_score,
                    None => true,
                };
                if is_better {
                    best = Some((score, face));
                }

                if score >= EARLY_EXIT_SCORE {
                    break 'scan;
                }
            }

            if x + win_w >= w {
                break;
            }
            x += STRIDE;
        }

        if y + win_h >= h {
            break;
        }
        y += STRIDE;
    }

    if let Some((score, _)) = &best {
        println!(
            "👁️ Meilleure fenêtre retenue pour la photo de référence : score={:.3}",
            score
        );
    }

    Ok(best.map(|(_, face)| face))
}

/// Charge les photos du dossier `known_faces/` et pré-calcule leurs
/// empreintes faciales. Plusieurs fichiers `<nom>_<horodatage>.jpg` sont
/// regroupés sous une seule identité `<nom>` (plusieurs gabarits pour une
/// même personne, voir la capture depuis l'UI web dans
/// [`try_capture_reference`]) ; `identify_person` retient ensuite le
/// meilleur score parmi tous les gabarits.
///
/// Chaque photo est indépendante des autres (chargement disque + YuNet +
/// ArcFace) : le traitement par fichier ([`load_known_face`]) est donc
/// parallélisé avec Rayon, comme le reste du pipeline de vision (voir
/// `super::tracking::process_persons_parallel`).
pub(super) fn load_known_faces(
    face_detector: &FaceDetectorYuNet,
    embedder: &FaceEmbedder,
    folder: &str,
) -> Vec<KnownPerson> {
    let path = Path::new(folder);

    // Création du dossier s'il n'existe pas
    if !path.exists() {
        if let Err(e) = std::fs::create_dir_all(path) {
            eprintln!("❌ Impossible de créer le dossier {} : {:?}", folder, e);
        }

        return Vec::new();
    }

    // Lecture du dossier
    let entries = match std::fs::read_dir(path) {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("❌ Impossible de lire le dossier {} : {:?}", folder, e);
            return Vec::new();
        }
    };

    // On ne garde que les fichiers image : la liste des chemins est
    // constituée séquentiellement (lecture du dossier), puis chaque photo
    // est traitée en parallèle ci-dessous.
    let image_paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|entry_path| entry_path.is_file())
        .filter(|entry_path| is_supported_image(entry_path))
        .collect();

    // PARALLÉLISATION RAYON : chargement + YuNet + ArcFace pour chaque
    // photo de référence, indépendamment des autres.
    let known: Vec<KnownPerson> = image_paths
        .par_iter()
        .filter_map(|entry_path| load_known_face(face_detector, embedder, entry_path))
        .collect();

    let distinct_people = known
        .iter()
        .map(|p| p.name.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    println!(
        "👥 Base de reconnaissance : {} personne(s) ({} gabarit(s) au total)",
        distinct_people,
        known.len()
    );

    known
}

/// Ne garde que les fichiers dont l'extension correspond à un format image
/// supporté (`jpg`, `jpeg`, `png`, insensible à la casse). Extrait de
/// [`load_known_faces`] pour être testable indépendamment du système de
/// fichiers réel et des modèles ONNX.
fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| matches!(ext.to_lowercase().as_str(), "jpg" | "jpeg" | "png"))
        .unwrap_or(false)
}

/// Déduit le nom d'une personne à partir du "stem" (nom de fichier sans
/// extension) de sa photo de référence, en ignorant un éventuel suffixe
/// numérique qui distingue plusieurs captures d'une même personne (angles /
/// poses différents, voir la capture depuis l'UI web) :
///
/// `"jerome"`               → `"jerome"`
/// `"jerome_20260918093604"` → `"jerome"`
///
/// Extrait de [`load_known_face`] pour être testable sans I/O ni modèle ONNX.
fn person_name_from_stem(stem: &str) -> String {
    match stem.rsplit_once('_') {
        Some((base, suffix))
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            base.to_string()
        }
        _ => stem.to_string(),
    }
}

/// Charge et encode une seule photo de référence de `known_faces/` : nom,
/// chargement disque, détection + alignement YuNet, empreinte ArcFace.
/// Appelée en parallèle via Rayon par [`load_known_faces`] pour chaque
/// fichier du dossier.
fn load_known_face(
    face_detector: &FaceDetectorYuNet,
    embedder: &FaceEmbedder,
    entry_path: &Path,
) -> Option<KnownPerson> {
    let stem = entry_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("Inconnu");

    let name = person_name_from_stem(stem);

    println!("🔄 Chargement du visage connu : {}", entry_path.display());

    // Chargement de la photo
    let img = match image::open(entry_path) {
        Ok(img) => img.to_rgb8(),

        Err(e) => {
            eprintln!(
                "❌ Impossible de charger {} : {:?}",
                entry_path.display(),
                e
            );
            return None;
        }
    };

    // Détection + alignement YuNet
    //
    // detect_face_native_res() scanne l'image par fenêtres 640x640 à
    // résolution native (voir sa doc) plutôt que de réduire toute la
    // photo, pour obtenir une bbox fiable même sur un gros plan haute
    // résolution. Elle retourne directement un visage aligné en 112x112.
    let face_crop = match detect_face_native_res(face_detector, &img) {
        Ok(Some(face)) => face,

        Ok(None) => {
            eprintln!("   ⚠️ Aucun visage détecté dans {}", entry_path.display());

            return None;
        }

        Err(e) => {
            eprintln!("   ❌ Erreur YuNet sur {} : {:?}", entry_path.display(), e);

            return None;
        }
    };

    // Extraction de l'embedding ArcFace
    let embedding = match embedder.extract_embedding(&face_crop) {
        Ok(embedding) => embedding,

        Err(e) => {
            eprintln!("   ❌ Erreur ArcFace pour {} : {:?}", name, e);

            return None;
        }
    };

    // Vérification de l'embedding
    if embedding.is_empty() {
        eprintln!("   ❌ Embedding vide pour {}", name);

        return None;
    }

    let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

    // Vérification de la norme
    if !norm.is_finite() || norm < 1e-6 {
        eprintln!(
            "   ❌ Embedding invalide pour {} (norme = {:.6})",
            name, norm
        );

        return None;
    }

    println!("   ✅ Visage connu chargé : {}", name);

    Some(KnownPerson { name, embedding })
}

/// Capture de photo de référence (enrôlement à chaud).
///
/// Déclenché depuis l'UI web (commande WS "capture_reference", voir
/// `crate::api::ClientCommand::CaptureReference`). On sauvegarde la frame
/// BRUTE (avant dessin des boîtes) telle que vue par la webcam, dans
/// `known_faces/<nom>_<horodatage>.jpg` (chaque capture s'ajoute aux
/// précédentes, voir [`load_known_faces`]), puis on recharge la base de
/// reconnaissance en tâche de fond.
///
/// Intérêt : la photo de référence est ainsi capturée dans le même
/// "domaine" (même caméra, même distance, même qualité) que les visages vus
/// en direct, ce qui donne une similarité ArcFace bien plus fiable qu'avec
/// une photo (ex : selfie téléphone) prise dans des conditions très
/// différentes.
pub(super) fn try_capture_reference(
    state: &SharedState,
    img: &RgbImage,
    face_detector: &Arc<Option<FaceDetectorYuNet>>,
    face_embedder: &Arc<Option<FaceEmbedder>>,
    known_people: &Arc<Mutex<Vec<KnownPerson>>>,
) {
    let Some(name) = state.pending_enrollment.lock_or_recover().take() else {
        return;
    };

    let safe_name: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();

    if safe_name.is_empty() {
        eprintln!(
            "⚠️ Nom de référence invalide reçu pour la capture : {:?}",
            name
        );

        return;
    }

    let _ = std::fs::create_dir_all("known_faces");

    // Suffixe temporel : chaque capture s'ajoute comme un nouveau
    // "gabarit" pour cette personne au lieu d'écraser les
    // précédentes. Plusieurs captures sous des angles / poses
    // différents rendent la reconnaissance bien plus robuste
    // qu'une seule photo (voir load_known_faces, qui regroupe
    // tous les fichiers "<nom>_<horodatage>.jpg" sous <nom>).
    let timestamp = Local::now().format("%Y%m%d%H%M%S%3f");
    let path = format!("known_faces/{}_{}.jpg", safe_name, timestamp);

    let mut encoded = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut encoded);
    if img.write_to(&mut cursor, ImageFormat::Jpeg).is_ok()
        && std::fs::write(&path, &encoded).is_ok()
    {
        println!(
            "📸 Photo de référence capturée depuis la webcam pour '{}' → {}",
            safe_name, path
        );

        // Rechargement en arrière-plan pour ne pas bloquer le flux caméra
        // (le scan par fenêtres 640x640, et désormais le traitement Rayon
        // des différents gabarits, peuvent prendre plusieurs centaines de
        // ms). `tokio::task::spawn_blocking` (plutôt qu'un `std::thread::spawn`
        // brut) confie ce travail bloquant/CPU-bound au pool de threads
        // dédié de Tokio, cohérent avec le reste du pipeline (voir
        // `super::worker::spawn_recognition_worker`).
        let detector_reload = Arc::clone(face_detector);
        let embedder_reload = Arc::clone(face_embedder);
        let known_people_reload = Arc::clone(known_people);
        tokio::task::spawn_blocking(move || {
            if let (Some(det), Some(emb)) = (&*detector_reload, &*embedder_reload) {
                let fresh = load_known_faces(det, emb, "known_faces");
                let distinct = fresh
                    .iter()
                    .map(|p| p.name.as_str())
                    .collect::<std::collections::HashSet<_>>()
                    .len();
                let total = fresh.len();
                *known_people_reload.lock_or_recover() = fresh;
                println!(
                    "🔄 Base de reconnaissance rechargée : {} personne(s) ({} gabarit(s) au total).",
                    distinct, total
                );
            }
        });
    } else {
        eprintln!(
            "❌ Échec de sauvegarde de la photo de référence pour '{}'",
            safe_name
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- person_name_from_stem ---

    #[test]
    fn name_without_timestamp_suffix_is_kept_as_is() {
        assert_eq!(person_name_from_stem("jerome"), "jerome");
    }

    #[test]
    fn numeric_suffix_after_underscore_is_stripped() {
        assert_eq!(person_name_from_stem("jerome_20260918093604"), "jerome");
    }

    #[test]
    fn only_the_last_underscore_group_is_treated_as_a_suffix() {
        // "jean_paul_20260918093604" -> le nom peut lui-même contenir un
        // underscore ; seul le dernier groupe (purement numérique) est
        // retiré.
        assert_eq!(
            person_name_from_stem("jean_paul_20260918093604"),
            "jean_paul"
        );
    }

    #[test]
    fn non_numeric_suffix_is_not_stripped() {
        // Le suffixe après le dernier "_" n'est pas purement numérique : on
        // garde le nom de fichier complet.
        assert_eq!(person_name_from_stem("photo_finale"), "photo_finale");
    }

    #[test]
    fn trailing_underscore_with_empty_suffix_is_kept_as_is() {
        assert_eq!(person_name_from_stem("jerome_"), "jerome_");
    }

    #[test]
    fn name_without_any_underscore_is_kept_as_is() {
        assert_eq!(person_name_from_stem("photo123"), "photo123");
    }

    // --- is_supported_image ---

    #[test]
    fn accepts_common_image_extensions_case_insensitively() {
        assert!(is_supported_image(Path::new("a.jpg")));
        assert!(is_supported_image(Path::new("a.JPG")));
        assert!(is_supported_image(Path::new("a.jpeg")));
        assert!(is_supported_image(Path::new("a.png")));
        assert!(is_supported_image(Path::new("a.PNG")));
    }

    #[test]
    fn rejects_non_image_extensions() {
        assert!(!is_supported_image(Path::new("a.txt")));
        assert!(!is_supported_image(Path::new("a.gif")));
        assert!(!is_supported_image(Path::new("a")));
    }

    // NOTE : `load_known_faces` et `load_known_face` ne sont volontairement
    // pas testées directement ici : elles nécessitent une instance réelle
    // de `FaceDetectorYuNet` / `FaceEmbedder`, donc de charger les modèles
    // ONNX (fichiers volumineux, absents de l'environnement de `cargo
    // test`). Leur logique pure (nommage, filtrage des extensions) est
    // couverte ci-dessus via `person_name_from_stem` et `is_supported_image`.
}
