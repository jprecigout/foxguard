//! Détection d'objets ONNX (YOLOv8), restreinte aux classes personne / chat
//! / chien.
//!
//! L'entrée du modèle est un CARRÉ, alors que la caméra filme en 4:3. La
//! frame y est donc mise en lettres plutôt qu'écrasée (voir [`letterbox`]),
//! et les coordonnées prédites sont ramenées ensuite dans l'image d'origine
//! (voir [`Letterboxed::to_source`]).

use anyhow::Result;
use image::{Rgb, RgbImage, imageops::FilterType};
use rayon::prelude::*;
use std::sync::Arc;
use tract_onnx::prelude::*;

use crate::config::DetectionConfig;

use super::model::load_onnx_model;
use super::types::BoundingBox;

/// Noms des 80 classes COCO, dans l'ordre attendu par la sortie du modèle YOLOv8.
pub const COCO_CLASSES: &[&str] = &[
    "person",
    "bicycle",
    "car",
    "motorcycle",
    "airplane",
    "bus",
    "train",
    "truck",
    "boat",
    "traffic light",
    "fire hydrant",
    "stop sign",
    "parking meter",
    "bench",
    "bird",
    "cat",
    "dog",
    "horse",
    "sheep",
    "cow",
    "elephant",
    "bear",
    "zebra",
    "giraffe",
    "backpack",
    "umbrella",
    "handbag",
    "tie",
    "suitcase",
    "frisbee",
    "skis",
    "snowboard",
    "sports ball",
    "kite",
    "baseball bat",
    "baseball glove",
    "skateboard",
    "surfboard",
    "tennis racket",
    "bottle",
    "wine glass",
    "cup",
    "fork",
    "knife",
    "spoon",
    "bowl",
    "banana",
    "apple",
    "sandwich",
    "orange",
    "broccoli",
    "carrot",
    "hot dog",
    "pizza",
    "donut",
    "cake",
    "chair",
    "couch",
    "potted plant",
    "bed",
    "dining table",
    "toilet",
    "tv",
    "laptop",
    "mouse",
    "remote",
    "keyboard",
    "cell phone",
    "microwave",
    "oven",
    "toaster",
    "sink",
    "refrigerator",
    "book",
    "clock",
    "vase",
    "scissors",
    "teddy bear",
    "hair drier",
    "toothbrush",
];

/// Gris de remplissage des bandes du letterbox.
///
/// 114 sur les trois canaux : c'est la valeur que la chaîne d'entraînement de
/// YOLOv8 utilise pour ses propres bandes. Un noir franc créerait aux bords
/// un contraste que le modèle n'a jamais vu à l'entraînement.
const LETTERBOX_FILL: Rgb<u8> = Rgb([114, 114, 114]);

/// Frame mise à la taille d'entrée du modèle, et de quoi en ramener les
/// prédictions vers l'image d'origine.
struct Letterboxed {
    /// Canevas carré soumis au modèle.
    image: RgbImage,
    /// Facteur appliqué à l'image d'origine pour la faire tenir dans le
    /// canevas.
    scale: f32,
    /// Largeur de la bande gauche, en pixels du canevas.
    pad_x: f32,
    /// Hauteur de la bande haute, en pixels du canevas.
    pad_y: f32,
    source_width: f32,
    source_height: f32,
}

/// Met la frame au format carré attendu par YOLO en CONSERVANT son cadrage :
/// elle est mise à l'échelle par le plus contraignant des deux facteurs, puis
/// centrée dans un carré dont le reste est rempli de [`LETTERBOX_FILL`].
///
/// # Pourquoi pas un simple redimensionnement
///
/// Un `resize` direct vers un carré ÉCRASE le 4:3 de la caméra : en 640x480
/// vers 640x640, toute la scène est étirée d'un tiers en hauteur. Or YOLOv8 a
/// été entraîné sur des images letterboxées — il n'a jamais vu de silhouettes
/// déformées, et une personne allongée verticalement ressemble moins à ce
/// qu'il connaît. Les scores baissent, et les détections les plus fragiles
/// (personne lointaine, animal de dos) passent sous le seuil de confiance.
///
/// Le filtre est `Triangle` et non `Nearest` : un échantillonnage ponctuel
/// jette plus de la moitié des pixels quand l'image est réduite, et c'est
/// précisément sur les petits objets — ceux qui comptent ici — qu'il efface
/// le détail dont le modèle a besoin. Quand aucune mise à l'échelle n'est
/// nécessaire (caméra 640x480 et modèle 640, le cas nominal), les lignes sont
/// recopiées telles quelles et il n'y a aucun rééchantillonnage.
fn letterbox(img: &RgbImage, size: u32) -> Letterboxed {
    let (source_width, source_height) = (img.width(), img.height());

    let scale = (size as f32 / source_width as f32).min(size as f32 / source_height as f32);

    let scaled_width = ((source_width as f32 * scale).round() as u32).clamp(1, size);
    let scaled_height = ((source_height as f32 * scale).round() as u32).clamp(1, size);

    let pad_x = (size - scaled_width) / 2;
    let pad_y = (size - scaled_height) / 2;

    let mut canvas = RgbImage::from_pixel(size, size, LETTERBOX_FILL);

    if (scaled_width, scaled_height) == (source_width, source_height) {
        image::imageops::overlay(&mut canvas, img, pad_x as i64, pad_y as i64);
    } else {
        let scaled =
            image::imageops::resize(img, scaled_width, scaled_height, FilterType::Triangle);
        image::imageops::overlay(&mut canvas, &scaled, pad_x as i64, pad_y as i64);
    }

    Letterboxed {
        image: canvas,
        scale,
        pad_x: pad_x as f32,
        pad_y: pad_y as f32,
        source_width: source_width as f32,
        source_height: source_height as f32,
    }
}

impl Letterboxed {
    /// Ramène une boîte prédite par le modèle (centre et dimensions, dans
    /// l'espace du canevas) vers les coordonnées de l'image d'origine.
    ///
    /// Les bandes sont retirées, l'échelle défaite, et le résultat est rogné
    /// aux bords de l'image : un modèle prédit volontiers une boîte qui
    /// dépasse du cadre pour une personne coupée par un bord, et une
    /// coordonnée hors image ferait dessiner l'incrustation dans le vide
    /// (voir `crate::capture::overlay`) comme elle ferait recadrer la
    /// vignette sur rien.
    ///
    /// Retourne `None` pour une boîte qui ne retombe pas sur au moins un
    /// pixel — celle qui tiendrait entièrement dans une bande, par exemple.
    fn to_source(&self, cx: f32, cy: f32, w: f32, h: f32) -> Option<(u32, u32, u32, u32)> {
        let to_source_x = |x: f32| ((x - self.pad_x) / self.scale).clamp(0.0, self.source_width);
        let to_source_y = |y: f32| ((y - self.pad_y) / self.scale).clamp(0.0, self.source_height);

        let left = to_source_x(cx - w / 2.0);
        let right = to_source_x(cx + w / 2.0);
        let top = to_source_y(cy - h / 2.0);
        let bottom = to_source_y(cy + h / 2.0);

        let width = (right - left) as u32;
        let height = (bottom - top) as u32;

        (width > 0 && height > 0).then_some((left as u32, top as u32, width, height))
    }
}

/// Détecteur d'objets ONNX (YOLOv8), restreint aux classes personne/chat/chien.
pub struct ObjectDetector {
    model: Arc<TypedSimplePlan>,
    config: DetectionConfig,
}

impl ObjectDetector {
    /// Charge le modèle YOLOv8 à la taille d'entrée fixée par `config.input_size`.
    pub fn new(config: DetectionConfig) -> Result<Self> {
        let size = config.input_size;
        let model = load_onnx_model(&config.model_path, size, size)?;

        Ok(Self { model, config })
    }

    /// Détection ultra-rapide et vectorisée avec ndarray et Rayon
    pub fn detect(&self, img: &RgbImage) -> Result<Vec<BoundingBox>> {
        let size = self.config.input_size;

        // Mise en lettres, et non redimensionnement : le modèle attend un
        // carré, la caméra filme en 4:3 (voir [`letterbox`]).
        let letterboxed = letterbox(img, size);

        // Conversion et normalisation SIMD via ndarray
        let raw_u8 = letterboxed.image.as_raw();
        let nd_u8 =
            tract_ndarray::ArrayView::from_shape((size as usize, size as usize, 3), raw_u8)?;
        let nd_f32 = nd_u8.mapv(|x| x as f32 / 255.0);

        // Réorganisation (H, W, C) -> (C, H, W) sans copie
        let nd_chw = nd_f32.permuted_axes([2, 0, 1]);
        let tensor: Tensor = nd_chw.insert_axis(tract_ndarray::Axis(0)).into();

        // Exécution de l'inférence ONNX
        let outputs = self.model.run(tvec!(tensor.into()))?;
        let output = outputs[0].to_plain_array_view::<f32>()?;

        let shape = output.shape();
        if shape.len() < 3 {
            return Ok(Vec::new());
        }

        let num_anchors = shape[2];

        // Définition des classes autorisées pour la détection (personne, chat, chien)
        let allowed_classes = ["person", "cat", "dog"];

        // PARALLÉLISATION RAYON : Décodage et filtrage parallèle de toutes les ancres YOLO
        let mut detections: Vec<BoundingBox> = (0..num_anchors)
            .into_par_iter()
            .filter_map(|col| {
                let cx = output[[0, 0, col]];
                let cy = output[[0, 1, col]];
                let w = output[[0, 2, col]];
                let h = output[[0, 3, col]];

                let mut max_score = 0.0f32;
                let mut class_id = 0;

                // Recherche de la classe à plus fort score pour cette ancre
                for c in 0..80 {
                    let score = output[[0, 4 + c, col]];
                    if score > max_score {
                        max_score = score;
                        class_id = c;
                    }
                }

                if max_score < self.config.confidence_threshold {
                    return None;
                }

                let label = COCO_CLASSES.get(class_id).unwrap_or(&"inconnu");

                // FILTRE : Seules les classes autorisées sont retenues
                if !allowed_classes.contains(label) {
                    return None;
                }

                // Espace du canevas -> image d'origine : bandes retirées,
                // échelle défaite, débordements rognés.
                let (x, y, width, height) = letterboxed.to_source(cx, cy, w, h)?;

                Some(BoundingBox {
                    x,
                    y,
                    width,
                    height,
                    label: label.to_string(),
                    confidence: max_score,
                })
            })
            .collect();

        // PARALLÉLISATION RAYON : Tri rapide décroissant par score de confiance
        detections.par_sort_unstable_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Filtrage NMS (seuil IoU fixé à 0.45)
        let mut final_detections = non_maximum_suppression(detections, 0.45);

        // Conservation des 10 meilleures détections
        final_detections.truncate(10);

        Ok(final_detections)
    }
}

/// Calcule le chevauchement (Intersection over Union) entre deux boîtes (voir
/// [`crate::geometry::iou`], partagé avec le tracking de personnes)
fn calculate_iou(box1: &BoundingBox, box2: &BoundingBox) -> f32 {
    let to_rect = |b: &BoundingBox| (b.x as f32, b.y as f32, b.width as f32, b.height as f32);

    crate::geometry::iou(to_rect(box1), to_rect(box2))
}

/// Filtre les détections doublons pour un même objet
fn non_maximum_suppression(mut boxes: Vec<BoundingBox>, iou_threshold: f32) -> Vec<BoundingBox> {
    let mut kept_boxes = Vec::new();

    while !boxes.is_empty() {
        // La boîte avec la plus haute confiance est extraite
        let current = boxes.remove(0);

        // On élimine les autres boîtes de même classe qui chevauchent trop la boîte courante
        boxes.retain(|b| {
            if b.label == current.label {
                calculate_iou(&current, b) < iou_threshold
            } else {
                true
            }
        });

        kept_boxes.push(current);
    }

    kept_boxes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(x: u32, y: u32, width: u32, height: u32, label: &str, confidence: f32) -> BoundingBox {
        BoundingBox {
            x,
            y,
            width,
            height,
            label: label.to_string(),
            confidence,
        }
    }

    /// Image unie, pour distinguer les pixels d'origine des bandes.
    fn filled(width: u32, height: u32, color: [u8; 3]) -> RgbImage {
        RgbImage::from_pixel(width, height, Rgb(color))
    }

    // --- letterbox ---

    #[test]
    fn letterbox_keeps_the_4_3_framing_and_pads_top_and_bottom() {
        // Le cas nominal : caméra 640x480, modèle 640. L'image tient en
        // largeur, et les 160 lignes manquantes se répartissent en deux
        // bandes de 80.
        let boxed = letterbox(&filled(640, 480, [10, 20, 30]), 640);

        assert_eq!(boxed.image.dimensions(), (640, 640));
        assert_eq!(boxed.scale, 1.0);
        assert_eq!(boxed.pad_x, 0.0);
        assert_eq!(boxed.pad_y, 80.0);
    }

    #[test]
    fn the_bands_carry_the_training_gray_and_the_image_its_own_pixels() {
        let boxed = letterbox(&filled(640, 480, [10, 20, 30]), 640);

        // Bande haute, bande basse : le gris de l'entraînement.
        assert_eq!(*boxed.image.get_pixel(320, 0), LETTERBOX_FILL);
        assert_eq!(*boxed.image.get_pixel(320, 639), LETTERBOX_FILL);

        // Entre les deux, l'image, intacte : à l'échelle 1 il n'y a eu aucun
        // rééchantillonnage.
        assert_eq!(*boxed.image.get_pixel(320, 320), Rgb([10, 20, 30]));
        assert_eq!(*boxed.image.get_pixel(0, 80), Rgb([10, 20, 30]));
    }

    #[test]
    fn a_landscape_frame_larger_than_the_model_is_scaled_down_not_squashed() {
        // 1280x720 vers 640 : le facteur est celui de la LARGEUR (0.5), pas
        // celui de la hauteur, sans quoi l'image déborderait du canevas.
        let boxed = letterbox(&filled(1280, 720, [1, 2, 3]), 640);

        assert_eq!(boxed.scale, 0.5);
        assert_eq!(boxed.pad_x, 0.0);
        assert_eq!(boxed.pad_y, 140.0);
        assert_eq!(boxed.image.dimensions(), (640, 640));
    }

    #[test]
    fn a_portrait_frame_is_padded_left_and_right() {
        let boxed = letterbox(&filled(480, 640, [1, 2, 3]), 640);

        assert_eq!(boxed.pad_x, 80.0);
        assert_eq!(boxed.pad_y, 0.0);
    }

    #[test]
    fn an_already_square_frame_gets_no_band_at_all() {
        let boxed = letterbox(&filled(640, 640, [7, 7, 7]), 640);

        assert_eq!(boxed.pad_x, 0.0);
        assert_eq!(boxed.pad_y, 0.0);
        assert_eq!(*boxed.image.get_pixel(0, 0), Rgb([7, 7, 7]));
    }

    // --- Letterboxed::to_source ---

    #[test]
    fn a_box_over_the_whole_image_maps_back_to_the_whole_image() {
        let boxed = letterbox(&filled(640, 480, [0, 0, 0]), 640);

        // Dans l'espace du canevas, l'image occupe y ∈ [80, 560] : une boîte
        // qui l'épouse exactement doit redonner l'image entière.
        assert_eq!(
            boxed.to_source(320.0, 320.0, 640.0, 480.0),
            Some((0, 0, 640, 480))
        );
    }

    #[test]
    fn the_bands_are_removed_from_the_vertical_coordinates() {
        let boxed = letterbox(&filled(640, 480, [0, 0, 0]), 640);

        // Boîte de 100x100 centrée sur le canevas : horizontalement
        // inchangée, verticalement remontée des 80 lignes de la bande haute.
        assert_eq!(
            boxed.to_source(320.0, 320.0, 100.0, 100.0),
            Some((270, 190, 100, 100))
        );
    }

    #[test]
    fn the_scale_is_undone_on_a_frame_larger_than_the_model() {
        // 1280x720 réduit de moitié : une boîte de 100x100 dans le canevas
        // vaut 200x200 dans l'image d'origine.
        let boxed = letterbox(&filled(1280, 720, [0, 0, 0]), 640);

        assert_eq!(
            boxed.to_source(320.0, 320.0, 100.0, 100.0),
            Some((540, 260, 200, 200))
        );
    }

    #[test]
    fn a_box_overflowing_the_frame_is_cropped_to_its_edges() {
        // Une personne coupée par le bord gauche : le modèle prédit volontiers
        // une boîte qui sort du cadre. Elle ne doit pas sortir de l'image.
        let boxed = letterbox(&filled(640, 480, [0, 0, 0]), 640);

        let (x, y, width, height) = boxed
            .to_source(0.0, 320.0, 200.0, 200.0)
            .expect("la boîte recouvre une partie de l'image");

        assert_eq!((x, y), (0, 140));
        assert_eq!(width, 100, "la moitié hors cadre est rognée");
        assert_eq!(height, 200);
    }

    #[test]
    fn a_box_entirely_inside_a_band_is_dropped() {
        // Une prédiction qui ne tombe que dans le gris ne désigne aucun pixel
        // de la caméra : la retenir ferait une boîte vide sur le flux.
        let boxed = letterbox(&filled(640, 480, [0, 0, 0]), 640);

        assert_eq!(boxed.to_source(320.0, 20.0, 40.0, 40.0), None);
    }

    #[test]
    fn calculate_iou_matches_geometry_iou() {
        let a = bbox(0, 0, 10, 10, "person", 0.9);
        let b = bbox(5, 0, 10, 10, "person", 0.8);
        let expected = crate::geometry::iou((0.0, 0.0, 10.0, 10.0), (5.0, 0.0, 10.0, 10.0));
        assert_eq!(calculate_iou(&a, &b), expected);
    }

    #[test]
    fn nms_on_empty_input_returns_empty_output() {
        assert!(non_maximum_suppression(Vec::new(), 0.45).is_empty());
    }

    #[test]
    fn nms_keeps_a_single_detection() {
        let boxes = vec![bbox(0, 0, 10, 10, "person", 0.9)];
        let kept = non_maximum_suppression(boxes, 0.45);
        assert_eq!(kept.len(), 1);
    }

    #[test]
    fn nms_suppresses_heavily_overlapping_boxes_of_the_same_label() {
        // Deux boîtes quasi identiques pour "person" : seule celle avec la
        // plus haute confiance doit être conservée.
        let boxes = vec![
            bbox(0, 0, 10, 10, "person", 0.95),
            bbox(1, 1, 10, 10, "person", 0.80),
        ];
        let kept = non_maximum_suppression(boxes, 0.45);
        assert_eq!(kept.len(), 1);
        assert!((kept[0].confidence - 0.95).abs() < 1e-6);
    }

    #[test]
    fn nms_keeps_overlapping_boxes_of_different_labels() {
        // Un chat et une personne peuvent légitimement se chevaucher
        // fortement (ex : chat porté dans les bras) : NMS ne compare que les
        // boîtes de même label.
        let boxes = vec![
            bbox(0, 0, 10, 10, "person", 0.9),
            bbox(0, 0, 10, 10, "cat", 0.85),
        ];
        let kept = non_maximum_suppression(boxes, 0.45);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn nms_keeps_distant_boxes_of_the_same_label() {
        let boxes = vec![
            bbox(0, 0, 10, 10, "person", 0.9),
            bbox(500, 500, 10, 10, "person", 0.8),
        ];
        let kept = non_maximum_suppression(boxes, 0.45);
        assert_eq!(kept.len(), 2);
    }
}
