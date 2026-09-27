//! Détection d'objets ONNX (YOLOv8), restreinte aux classes personne / chat
//! / chien.

use anyhow::Result;
use image::{RgbImage, imageops::FilterType};
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
        let resized = image::imageops::resize(img, size, size, FilterType::Nearest);

        // Conversion et normalisation SIMD via ndarray
        let raw_u8 = resized.as_raw();
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
        let img_width = img.width() as f32;
        let img_height = img.height() as f32;

        let scale_x = img_width / size as f32;
        let scale_y = img_height / size as f32;

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

                let x = ((cx - w / 2.0) * scale_x).max(0.0) as u32;
                let y = ((cy - h / 2.0) * scale_y).max(0.0) as u32;
                let width = (w * scale_x) as u32;
                let height = (h * scale_y) as u32;

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
