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

        detections.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        detections.truncate(10);

        Ok(detections)
    }
}
