//! Chargement et rechargement à chaud de la base de visages connus
//! (`known_faces/`), utilisée par la reconnaissance faciale (voir
//! `super::tracking`).
//!
//! Le dossier lui-même est tenu par la bibliothèque de visages (voir
//! `super::face_library`) : captures, suppressions et photos reçues des
//! autres caméras passent toutes par elle.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::Result;
use foxguard_protocol::faces::person_name_from_stem;
use image::RgbImage;
use rayon::prelude::*;

use crate::util::MutexExt;
use crate::vision::{FaceDetectorYuNet, FaceEmbedder, KnownPerson};
use tracing::{debug, error, info, warn};

use super::face_library::FaceCommand;
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
/// [`load_known_faces`] (au démarrage, et à chaque rechargement à chaud) et
/// par la vérification d'une capture (voir `super::face_library`) — en
/// dehors du chemin critique du flux caméra live dans tous les cas.
pub(super) fn detect_face_native_res(
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
        debug!(
            "👁️ Meilleure fenêtre retenue pour la photo de référence : score={:.3}",
            score
        );
    }

    Ok(best.map(|(_, face)| face))
}

/// Empreintes faciales déjà calculées, par photo de `known_faces/`.
///
/// Recharger la base ne recalcule que les photos NOUVELLES ou MODIFIÉES. Le
/// rechargement complet était acceptable tant qu'il ne suivait qu'une capture
/// faite devant la caméra ; depuis que les photos sont partagées, chaque
/// capture, sur n'importe quelle caméra, fait recharger TOUTES les caméras —
/// et une photo coûte une détection YuNet par fenêtres plus une inférence
/// ArcFace, plusieurs centaines de millisecondes sur un Raspberry Pi.
#[derive(Default)]
pub(super) struct EmbeddingCache {
    entries: HashMap<PathBuf, CachedFace>,
}

struct CachedFace {
    stamp: FileStamp,
    /// `None` : aucun visage exploitable dans la photo. Mémorisé aussi, pour
    /// ne pas refaire à chaque rechargement une analyse qui échouera encore.
    person: Option<KnownPerson>,
}

/// Ce qui distingue deux versions d'un même fichier : un fichier réécrit
/// (photo remplacée par une caméra) change de date ou de taille.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FileStamp {
    len: u64,
    modified: Option<SystemTime>,
}

impl FileStamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
        })
    }
}

impl EmbeddingCache {
    fn is_fresh(&self, path: &Path, stamp: Option<FileStamp>) -> bool {
        matches!(
            (self.entries.get(path), stamp),
            (Some(cached), Some(stamp)) if cached.stamp == stamp
        )
    }
}

/// Liste les photos de `known_faces/` (fichiers d'image supportés), en
/// créant le dossier s'il n'existe pas encore.
pub(super) fn list_photos(folder: &Path) -> Vec<PathBuf> {
    if !folder.exists() {
        if let Err(e) = std::fs::create_dir_all(folder) {
            error!(
                "❌ Impossible de créer le dossier {} : {:?}",
                folder.display(),
                e
            );
        }

        return Vec::new();
    }

    match std::fs::read_dir(folder) {
        Ok(entries) => entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|entry_path| entry_path.is_file())
            .filter(|entry_path| is_supported_image(entry_path))
            .collect(),
        Err(e) => {
            error!(
                "❌ Impossible de lire le dossier {} : {:?}",
                folder.display(),
                e
            );
            Vec::new()
        }
    }
}

/// Charge les photos du dossier `known_faces/` et en calcule les empreintes
/// faciales — seulement celles que `cache` ne connaît pas encore (voir
/// [`EmbeddingCache`]). Plusieurs fichiers `<nom>_<horodatage>.jpg` sont
/// regroupés sous une seule identité `<nom>` (plusieurs gabarits pour une
/// même personne, voir `foxguard_protocol::faces::person_name_from_stem`) ;
/// `identify_person` retient ensuite le meilleur score parmi tous les
/// gabarits.
///
/// Chaque photo est indépendante des autres (chargement disque + YuNet +
/// ArcFace) : le traitement par fichier ([`load_known_face`]) est donc
/// parallélisé avec Rayon, comme le reste du pipeline de vision (voir
/// `super::tracking::process_persons_parallel`).
pub(super) fn load_known_faces(
    face_detector: &FaceDetectorYuNet,
    embedder: &FaceEmbedder,
    folder: &Path,
    cache: &mut EmbeddingCache,
) -> Vec<KnownPerson> {
    let photos: Vec<(PathBuf, Option<FileStamp>)> = list_photos(folder)
        .into_iter()
        .map(|path| {
            let stamp = FileStamp::of(&path);
            (path, stamp)
        })
        .collect();

    let (cached, missing): (Vec<_>, Vec<_>) = photos
        .into_iter()
        .partition(|(path, stamp)| cache.is_fresh(path, *stamp));

    // PARALLÉLISATION RAYON : chargement + YuNet + ArcFace pour chaque
    // photo à analyser, indépendamment des autres.
    let analysed = missing.len();
    let computed: Vec<(PathBuf, Option<FileStamp>, Option<KnownPerson>)> = missing
        .into_par_iter()
        .map(|(path, stamp)| {
            let person = load_known_face(face_detector, embedder, &path);
            (path, stamp, person)
        })
        .collect();

    // Le cache est RECONSTRUIT à partir des photos présentes : une photo
    // supprimée en sort d'elle-même.
    let mut entries = HashMap::with_capacity(cached.len() + computed.len());
    for (path, _) in cached {
        if let Some(entry) = cache.entries.remove(&path) {
            entries.insert(path, entry);
        }
    }

    // Une photo dont les métadonnées sont illisibles ne peut pas être
    // reconnue au prochain passage : elle compte, mais n'est pas mémorisée.
    let mut uncacheable = Vec::new();
    for (path, stamp, person) in computed {
        match stamp {
            Some(stamp) => {
                entries.insert(path, CachedFace { stamp, person });
            }
            None => uncacheable.extend(person),
        }
    }

    cache.entries = entries;

    let known: Vec<KnownPerson> = cache
        .entries
        .values()
        .filter_map(|entry| entry.person.clone())
        .chain(uncacheable)
        .collect();

    let distinct_people = known
        .iter()
        .map(|p| p.name.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len();
    info!(
        "👥 Base de reconnaissance : {} personne(s) ({} gabarit(s) au total, {} photo(s) analysée(s))",
        distinct_people,
        known.len(),
        analysed
    );

    known
}

/// Ne garde que les fichiers dont l'extension correspond à un format image
/// supporté (`jpg`, `jpeg`, `png`, insensible à la casse). Extrait de
/// [`list_photos`] pour être testable indépendamment du système de fichiers
/// réel et des modèles ONNX.
fn is_supported_image(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .map(|ext| matches!(ext.to_lowercase().as_str(), "jpg" | "jpeg" | "png"))
        .unwrap_or(false)
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

    // Chargement de la photo
    let img = match image::open(entry_path) {
        Ok(img) => img.to_rgb8(),

        Err(e) => {
            error!(
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
            warn!("⚠️ Aucun visage détecté dans {}", entry_path.display());

            return None;
        }

        Err(e) => {
            error!("❌ Erreur YuNet sur {} : {:?}", entry_path.display(), e);

            return None;
        }
    };

    // Extraction de l'embedding ArcFace
    let embedding = match embedder.extract_embedding(&face_crop) {
        Ok(embedding) => embedding,

        Err(e) => {
            error!("❌ Erreur ArcFace pour {} : {:?}", name, e);

            return None;
        }
    };

    // Vérification de l'embedding
    if embedding.is_empty() {
        error!("❌ Embedding vide pour {}", name);

        return None;
    }

    let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

    // Vérification de la norme
    if !norm.is_finite() || norm < 1e-6 {
        error!("❌ Embedding invalide pour {} (norme = {:.6})", name, norm);

        return None;
    }

    // UNE SEULE ligne par photo, portant à la fois le fichier et le nom qui en
    // a été déduit.
    //
    // Le chargement est parallélisé (voir `load_known_faces`) : deux messages
    // distincts — « chargement de <fichier> » puis « chargé : <nom> » —
    // s'entrelaçaient entre threads, et on lisait le fichier d'une photo suivi
    // du nom d'une AUTRE. De quoi croire à une confusion d'identités alors que
    // le chargement était correct.
    info!(
        "✅ Visage de référence chargé : {} → {}",
        entry_path.display(),
        name
    );

    Some(KnownPerson { name, embedding })
}

/// Capture de photo de référence (enrôlement à chaud).
///
/// Déclenché depuis une interface (commande WS "capture_reference", voir
/// `crate::api::ClientCommand::CaptureReference`), celle de la caméra ou
/// celle du manager. On confie la frame BRUTE (avant dessin des boîtes),
/// telle que vue par la caméra, à la bibliothèque de visages (voir
/// `super::face_library`) : c'est elle qui vérifie qu'un visage y figure,
/// l'écrit dans `known_faces/`, la partage avec les autres caméras et
/// recharge la base — sur ses propres threads, pas sur celui de la capture.
///
/// Intérêt : la photo de référence est ainsi capturée dans le même
/// "domaine" (même caméra, même distance, même qualité) que les visages vus
/// en direct, ce qui donne une similarité ArcFace bien plus fiable qu'avec
/// une photo (ex : selfie téléphone) prise dans des conditions très
/// différentes.
pub(super) fn try_capture_reference(state: &SharedState, img: &RgbImage) {
    let Some(request) = state.pending_enrollment.lock_or_recover().take() else {
        return;
    };

    // L'interface a cessé d'attendre (délai dépassé, onglet fermé) :
    // capturer maintenant prendrait une photo que personne n'a demandée À
    // CET INSTANT — peut-être sans personne devant la caméra.
    if request.reply.is_closed() {
        debug!(
            "📸 Capture pour '{}' abandonnée : plus personne n'attend la réponse.",
            request.name
        );
        return;
    }

    // Si la bibliothèque ne tourne pas, la commande (et sa réponse) est
    // perdue : l'interface l'apprend par la fermeture de la réponse.
    let _ = state.faces.send(FaceCommand::Capture {
        name: request.name,
        image: img.clone(),
        reply: request.reply,
    });
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn listing_creates_a_missing_folder_and_keeps_only_images() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("known_faces");

        assert!(list_photos(&folder).is_empty());
        assert!(folder.is_dir(), "le dossier est créé au premier passage");

        std::fs::write(folder.join("lou_1.jpg"), b"x").unwrap();
        std::fs::write(folder.join(".lou_2.jpg.tmp"), b"x").unwrap();
        std::fs::write(folder.join("notes.txt"), b"x").unwrap();

        assert_eq!(list_photos(&folder), vec![folder.join("lou_1.jpg")]);
    }

    #[test]
    fn a_cached_photo_is_fresh_until_it_is_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lou_1.jpg");
        std::fs::write(&path, b"premiere version").unwrap();

        let mut cache = EmbeddingCache::default();
        let stamp = FileStamp::of(&path);
        assert!(!cache.is_fresh(&path, stamp));

        cache.entries.insert(
            path.clone(),
            CachedFace {
                stamp: stamp.unwrap(),
                person: None,
            },
        );
        assert!(cache.is_fresh(&path, stamp));

        // Réécrite par une photo reçue d'une autre caméra : taille
        // différente, donc à réanalyser.
        std::fs::write(&path, b"une seconde version, plus longue").unwrap();
        assert!(!cache.is_fresh(&path, FileStamp::of(&path)));
    }

    // NOTE : `load_known_faces` et `load_known_face` ne sont volontairement
    // pas testées directement ici : elles nécessitent une instance réelle
    // de `FaceDetectorYuNet` / `FaceEmbedder`, donc de charger les modèles
    // ONNX (fichiers volumineux, absents de l'environnement de `cargo
    // test`). Leur logique pure (filtrage des extensions, fraîcheur du
    // cache) est couverte ci-dessus ; le nommage l'est dans
    // `foxguard_protocol::faces`.
}
