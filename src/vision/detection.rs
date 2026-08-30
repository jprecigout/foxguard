use crate::config::DetectionConfig;
use anyhow::Result;
use image::{RgbImage, imageops::FilterType};
use std::sync::Arc;
use tract_onnx::prelude::*;

#[derive(Debug, Clone)]
pub struct BoundingBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    pub label: String,
    pub confidence: f32,
}

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

pub struct ObjectDetector {
    model: Arc<TypedSimplePlan>,
    config: DetectionConfig,
}

impl ObjectDetector {
    pub fn new(config: DetectionConfig) -> Result<Self> {
        let size = config.input_size as usize;
        let model = tract_onnx::onnx()
            .model_for_path(&config.model_path)?
            .with_input_fact(
                0,
                InferenceFact::dt_shape(f32::datum_type(), &[1, 3, size as usize, size as usize]),
            )?
            .into_optimized()?
            .into_runnable()?;

        Ok(Self { model, config })
    }

    /// Détection ultra-rapide et vectorisée avec ndarray
    pub fn detect(&self, img: &RgbImage) -> Result<Vec<BoundingBox>> {
        let size = self.config.input_size;
        let resized = image::imageops::resize(img, size, size, FilterType::Nearest);

        // Conversion et normalisation SIMD via ndarray
        let raw_u8 = resized.as_raw();
        let nd_u8 =
            tract_ndarray::ArrayView::from_shape((size as usize, size as usize, 3), raw_u8)?;
        let nd_f32 = nd_u8.mapv(|x| x as f32 / 255.0);

        // Réorganisation (H, W, C) -> (C, H,W) sans copie
        let nd_chw = nd_f32.permuted_axes([2, 0, 1]);
        let tensor: Tensor = nd_chw.insert_axis(tract_ndarray::Axis(0)).into();

        // Exécution de l'inférence
        let outputs = self.model.run(tvec!(tensor.into()))?;
        let output = outputs[0].to_plain_array_view::<f32>()?;

        let mut detections = Vec::new();
        let shape = output.shape();
        if shape.len() < 3 {
            return Ok(detections);
        }

        let num_anchors = shape[2];
        let img_width = img.width() as f32;
        let img_height = img.height() as f32;

        // Définition des classes autorisées pour la détection (personne, chat, chien)
        let allowed_classes = ["person", "cat", "dog"];

        for col in 0..num_anchors {
            let cx = output[[0, 0, col]];
            let cy = output[[0, 1, col]];
            let w = output[[0, 2, col]];
            let h = output[[0, 3, col]];

            let mut max_score = 0.0f32;
            let mut class_id = 0;

            for c in 0..80 {
                let score = output[[0, 4 + c, col]];
                if score > max_score {
                    max_score = score;
                    class_id = c;
                }
            }

            if max_score >= self.config.confidence_threshold {
                let label = COCO_CLASSES.get(class_id).unwrap_or(&"inconnu").to_string();

                // FILTRE : On vérifie si la classe détectée fait partie de notre liste
                if allowed_classes.contains(&label.as_str()) {
                    let scale_x = img_width / size as f32;
                    let scale_y = img_height / size as f32;

                    let x = ((cx - w / 2.0) * scale_x).max(0.0) as u32;
                    let y = ((cy - h / 2.0) * scale_y).max(0.0) as u32;
                    let width = (w * scale_x) as u32;
                    let height = (h * scale_y) as u32;

                    let label = COCO_CLASSES.get(class_id).unwrap_or(&"inconnu").to_string();

                    detections.push(BoundingBox {
                        x,
                        y,
                        width,
                        height,
                        label,
                        confidence: max_score,
                    });
                }
            }
        }

        // Tri décroissant par score de confiance
        detections.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // Filtrage NMS (seuil IoU fixé a 0.45)
        let mut final_detections = non_maximum_suppression(detections, 0.45);

        // Conservation des 10 meilleures détections pour éviter les surcharges
        final_detections.truncate(10);

        Ok(final_detections)
    }
}

// Les modèles YOLO génèrent des milliers de boîtes candidates (environ 2 100 pour une entrée en 320×320).
// Sans NMS, plusieurs ancres voisines détectent la même personne avec un score supérieur au seuil de confiance, le code dessine alors un cadre pour chacune d'entre elles.
// Pour ne conserver qu'une seule boîte par objet détecté, on applique la suppression non maximale (NMS) après le tri par confiance.

// Calcule le chevauchement (Intersection over Union) entre deux boîtes
fn calculate_iou(box1: &BoundingBox, box2: &BoundingBox) -> f32 {
    let x1 = box1.x.max(box2.x);
    let y1 = box1.y.max(box2.y);
    let x2 = (box1.x + box1.width).min(box2.x + box2.width);
    let y2 = (box1.y + box1.height).min(box2.y + box2.height);

    if x2 <= x1 || y2 <= y1 {
        return 0.0;
    }

    let intersection = ((x2 - x1) * (y2 - y1)) as f32;
    let area1 = (box1.width * box1.height) as f32;
    let area2 = (box2.width * box2.height) as f32;
    let union = area1 + area2 - intersection;

    if union <= 0.0 {
        0.0
    } else {
        intersection / union
    }
}

// Filtre les détections doublons pour un même objet
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
