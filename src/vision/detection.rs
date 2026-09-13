use anyhow::Result;
use image::{RgbImage, imageops::FilterType};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tract_ndarray::prelude::*;
use tract_onnx::prelude::*;

use crate::config::DetectionConfig;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnownPerson {
    pub name: String,
    pub embedding: Vec<f32>,
}

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
                InferenceFact::dt_shape(f32::datum_type(), &[1, 3, size, size]),
            )?
            .into_optimized()?
            .into_runnable()?;

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

pub struct FaceDetectorYuNet {
    model: Arc<TypedSimplePlan>,
    config: DetectionConfig,
}

#[derive(Debug, Clone)]
struct FaceDetectionInternal {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    landmarks: [[f32; 2]; 5],
    score: f32,
}

impl FaceDetectorYuNet {
    pub fn new(config: DetectionConfig) -> Result<Self> {
        let model = tract_onnx::onnx()
            .model_for_path(&config.model_detect_face_path)?
            .with_input_fact(
                0,
                InferenceFact::dt_shape(f32::datum_type(), &[1, 3, 640, 640]),
            )?
            .into_optimized()?
            .into_runnable()?;

        Ok(Self { model, config })
    }

    pub fn detect_face_crop(&self, person_crop: &RgbImage) -> Result<Option<RgbImage>> {
        const INPUT_SIZE: usize = 640;
        const SCORE_THRESHOLD: f32 = 0.65;
        const NMS_THRESHOLD: f32 = 0.30;

        let orig_w = person_crop.width();
        let orig_h = person_crop.height();

        if orig_w < 20 || orig_h < 20 {
            return Ok(None);
        }

        // ============================================================
        // 1. LETTERBOX
        // ============================================================

        let scale = (INPUT_SIZE as f32 / orig_w as f32).min(INPUT_SIZE as f32 / orig_h as f32);

        let resized_w = ((orig_w as f32 * scale).round() as u32).max(1);
        let resized_h = ((orig_h as f32 * scale).round() as u32).max(1);

        let resized =
            image::imageops::resize(person_crop, resized_w, resized_h, FilterType::Triangle);

        let mut letterboxed =
            RgbImage::from_pixel(INPUT_SIZE as u32, INPUT_SIZE as u32, image::Rgb([0, 0, 0]));

        let pad_x = (INPUT_SIZE as u32 - resized_w) / 2;
        let pad_y = (INPUT_SIZE as u32 - resized_h) / 2;

        image::imageops::overlay(&mut letterboxed, &resized, pad_x as i64, pad_y as i64);

        // ============================================================
        // 2. TENSOR BGR
        // ============================================================

        let mut tensor_data = Array4::<f32>::zeros((1, 3, INPUT_SIZE, INPUT_SIZE));

        for (x, y, pixel) in letterboxed.enumerate_pixels() {
            let r = pixel[0] as f32;
            let g = pixel[1] as f32;
            let b = pixel[2] as f32;

            tensor_data[[0, 0, y as usize, x as usize]] = b;
            tensor_data[[0, 1, y as usize, x as usize]] = g;
            tensor_data[[0, 2, y as usize, x as usize]] = r;
        }

        let tensor: Tensor = tensor_data.into();

        // ============================================================
        // 3. INFÉRENCE
        // ============================================================

        let outputs = self.model.run(tvec![tensor.into()])?;

        if outputs.len() != 12 {
            eprintln!("❌ YuNet : {} sorties reçues, 12 attendues", outputs.len());
            return Ok(None);
        }

        // ============================================================
        // 4. DEBUG DES SORTIES
        // ============================================================

        let cls_views = [
            outputs[0].to_plain_array_view::<f32>()?,
            outputs[1].to_plain_array_view::<f32>()?,
            outputs[2].to_plain_array_view::<f32>()?,
        ];

        let obj_views = [
            outputs[3].to_plain_array_view::<f32>()?,
            outputs[4].to_plain_array_view::<f32>()?,
            outputs[5].to_plain_array_view::<f32>()?,
        ];

        let bbox_views = [
            outputs[6].to_plain_array_view::<f32>()?,
            outputs[7].to_plain_array_view::<f32>()?,
            outputs[8].to_plain_array_view::<f32>()?,
        ];

        // Affiche les dimensions une seule fois pour vérifier le modèle.
        for i in 0..3 {
            eprintln!(
                "YuNet scale {} : cls={:?} obj={:?} bbox={:?}",
                i,
                cls_views[i].shape(),
                obj_views[i].shape(),
                bbox_views[i].shape()
            );
        }

        // ============================================================
        // 5. STRIDES
        // ============================================================

        const STRIDES: [usize; 3] = [8, 16, 32];

        #[derive(Clone)]
        struct Detection {
            bbox: (f32, f32, f32, f32),
            score: f32,
        }

        let mut detections = Vec::<Detection>::new();

        // ============================================================
        // 6. DÉCODAGE
        // ============================================================

        for scale_index in 0..3 {
            let stride = STRIDES[scale_index];

            let grid_w = INPUT_SIZE / stride;
            let grid_h = INPUT_SIZE / stride;

            let expected = grid_w * grid_h;

            let cls = &cls_views[scale_index];
            let obj = &obj_views[scale_index];
            let bbox = &bbox_views[scale_index];

            if cls.shape() != [1, expected, 1]
                || obj.shape() != [1, expected, 1]
                || bbox.shape() != [1, expected, 4]
            {
                eprintln!(
                    "⚠️ Dimensions invalides scale={} cls={:?} obj={:?} bbox={:?}",
                    scale_index,
                    cls.shape(),
                    obj.shape(),
                    bbox.shape()
                );

                continue;
            }

            for index in 0..expected {
                let cls_score = cls[[0, index, 0]];
                let obj_score = obj[[0, index, 0]];

                if !cls_score.is_finite() || !obj_score.is_finite() {
                    continue;
                }

                let cls_score = cls_score.clamp(0.0, 1.0);
                let obj_score = obj_score.clamp(0.0, 1.0);

                let score = (cls_score * obj_score).sqrt();

                if score < SCORE_THRESHOLD {
                    continue;
                }

                let gx = index % grid_w;
                let gy = index / grid_w;

                // ====================================================
                // IMPORTANT :
                //
                // YuNet utilise les prior centers :
                //
                // prior_x = gx * stride
                // prior_y = gy * stride
                // ====================================================

                let prior_x = gx as f32 * stride as f32;
                let prior_y = gy as f32 * stride as f32;

                let bx = bbox[[0, index, 0]];
                let by = bbox[[0, index, 1]];
                let bw = bbox[[0, index, 2]];
                let bh = bbox[[0, index, 3]];

                if !bx.is_finite() || !by.is_finite() || !bw.is_finite() || !bh.is_finite() {
                    continue;
                }

                // ====================================================
                // YuNet bbox
                // ====================================================

                let cx = prior_x + bx * stride as f32;

                let cy = prior_y + by * stride as f32;

                let width = bw.exp() * stride as f32;

                let height = bh.exp() * stride as f32;

                if !cx.is_finite() || !cy.is_finite() || !width.is_finite() || !height.is_finite() {
                    continue;
                }

                // ====================================================
                // REJET DES VALEURS ABERRANTES
                // ====================================================

                if width < 5.0 || height < 5.0 || width > 300.0 || height > 300.0 {
                    continue;
                }

                let ratio = width.max(height) / width.min(height);

                if ratio > 2.5 {
                    continue;
                }

                // ====================================================
                // XYWH
                // ====================================================

                let mut x = cx - width * 0.5;
                let mut y = cy - height * 0.5;

                let mut w = width;
                let mut h = height;

                // ====================================================
                // CLAMP
                // ====================================================

                x = x.clamp(0.0, INPUT_SIZE as f32 - 1.0);

                y = y.clamp(0.0, INPUT_SIZE as f32 - 1.0);

                w = w.min(INPUT_SIZE as f32 - x);

                h = h.min(INPUT_SIZE as f32 - y);

                if w < 5.0 || h < 5.0 {
                    continue;
                }

                detections.push(Detection {
                    bbox: (x, y, w, h),
                    score,
                });
            }
        }

        // ============================================================
        // 7. CANDIDATS
        // ============================================================

        eprintln!("🔍 YuNet : {} visage(s) candidat(s)", detections.len());

        if detections.is_empty() {
            return Ok(None);
        }

        // ============================================================
        // 8. TRI
        // ============================================================

        detections.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // ============================================================
        // 9. NMS
        // ============================================================

        fn iou(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> f32 {
            let (ax, ay, aw, ah) = a;
            let (bx, by, bw, bh) = b;

            let ax2 = ax + aw;
            let ay2 = ay + ah;

            let bx2 = bx + bw;
            let by2 = by + bh;

            let ix1 = ax.max(bx);
            let iy1 = ay.max(by);

            let ix2 = ax2.min(bx2);
            let iy2 = ay2.min(by2);

            if ix2 <= ix1 || iy2 <= iy1 {
                return 0.0;
            }

            let intersection = (ix2 - ix1) * (iy2 - iy1);

            let union = aw * ah + bw * bh - intersection;

            if union <= 0.0 {
                0.0
            } else {
                intersection / union
            }
        }

        let mut selected = Vec::<Detection>::new();

        for detection in detections {
            if selected
                .iter()
                .all(|existing| iou(existing.bbox, detection.bbox) < NMS_THRESHOLD)
            {
                selected.push(detection);
            }
        }

        if selected.is_empty() {
            return Ok(None);
        }

        // ============================================================
        // 10. MEILLEUR VISAGE
        // ============================================================

        let best = selected.into_iter().next().unwrap();

        let (x640, y640, w640, h640) = best.bbox;

        // ============================================================
        // 11. 640x640 -> IMAGE ORIGINALE
        // ============================================================

        let x_resized = (x640 - pad_x as f32).max(0.0);

        let y_resized = (y640 - pad_y as f32).max(0.0);

        let right_resized = (x640 + w640 - pad_x as f32).max(0.0);

        let bottom_resized = (y640 + h640 - pad_y as f32).max(0.0);

        let x_original = (x_resized / scale).clamp(0.0, orig_w as f32);

        let y_original = (y_resized / scale).clamp(0.0, orig_h as f32);

        let right_original = (right_resized / scale).clamp(0.0, orig_w as f32);

        let bottom_original = (bottom_resized / scale).clamp(0.0, orig_h as f32);

        let x = x_original as u32;
        let y = y_original as u32;

        let mut w = (right_original - x_original).max(1.0) as u32;

        let mut h = (bottom_original - y_original).max(1.0) as u32;

        w = w.min(orig_w.saturating_sub(x));

        h = h.min(orig_h.saturating_sub(y));

        if w < 10 || h < 10 {
            return Ok(None);
        }

        // ============================================================
        // 12. PROTECTION CONTRE LES BBOX ABSURDES
        // ============================================================

        if w > orig_w * 8 / 10 || h > orig_h * 8 / 10 {
            eprintln!(
                "⚠️ YuNet bbox rejeté : {}x{} dans crop {}x{}",
                w, h, orig_w, orig_h
            );

            return Ok(None);
        }

        // ============================================================
        // 13. MARGE ARCFACE
        // ============================================================

        let margin_x = (w as f32 * 0.30) as u32;

        let margin_y = (h as f32 * 0.35) as u32;

        let crop_x = x.saturating_sub(margin_x);

        let crop_y = y.saturating_sub(margin_y);

        let crop_right = x.saturating_add(w).saturating_add(margin_x).min(orig_w);

        let crop_bottom = y.saturating_add(h).saturating_add(margin_y).min(orig_h);

        let crop_w = crop_right.saturating_sub(crop_x);

        let crop_h = crop_bottom.saturating_sub(crop_y);

        if crop_w < 10 || crop_h < 10 {
            return Ok(None);
        }

        // ============================================================
        // 14. CROP
        // ============================================================

        let face_crop =
            image::imageops::crop_imm(person_crop, crop_x, crop_y, crop_w, crop_h).to_image();

        // ============================================================
        // 15. ARCFACE 112x112
        // ============================================================

        let aligned = image::imageops::resize(&face_crop, 112, 112, FilterType::Triangle);

        println!(
            "🙂 YuNet : visage {:.1}% | bbox {}x{} | crop {}x{}",
            best.score * 100.0,
            w,
            h,
            crop_w,
            crop_h
        );

        Ok(Some(aligned))
    }
}

/// Moteur d'extraction d'empreintes faciales ONNX (ArcFace / MobileFaceNet 112x112)
pub struct FaceEmbedder {
    model: Arc<TypedSimplePlan>,
    config: DetectionConfig,
}

impl FaceEmbedder {
    pub fn new(config: DetectionConfig) -> Result<Self> {
        let size = config.input_face_size as usize;
        let model = tract_onnx::onnx()
            .model_for_path(&config.model_face_path)?
            .with_input_fact(
                0,
                InferenceFact::dt_shape(f32::datum_type(), &[1, 3, size, size]),
            )?
            .into_optimized()?
            .into_runnable()?;

        Ok(Self { model, config })
    }

    /// Extrait le vecteur d'empreinte (512 float) d'un découpage de visage
    pub fn extract_embedding(&self, face_crop: &RgbImage) -> Result<Vec<f32>> {
        let size = self.config.input_face_size as usize;
        let resized = image::imageops::resize(
            face_crop,
            self.config.input_face_size,
            self.config.input_face_size,
            FilterType::Triangle,
        );

        // Préparation du tenseur au format NCHW [1, 3, size, size]
        let mut tensor_data = Array4::<f32>::zeros((1, 3, size, size));

        for (x, y, pixel) in resized.enumerate_pixels() {
            let r = pixel[0] as f32;
            let g = pixel[1] as f32;
            let b = pixel[2] as f32;

            // Conversion RGB -> BGR + Normalisation InsightFace/ArcFace : (x - 127.5) / 127.5
            tensor_data[[0, 0, y as usize, x as usize]] = (b - 127.5) / 127.5; // B
            tensor_data[[0, 1, y as usize, x as usize]] = (g - 127.5) / 127.5; // G
            tensor_data[[0, 2, y as usize, x as usize]] = (r - 127.5) / 127.5; // R
        }

        let tensor: Tensor = tensor_data.into();
        let outputs = self.model.run(tvec!(tensor.into()))?;

        if outputs.is_empty() {
            return Err(anyhow::anyhow!("ArcFace n'a produit aucune sortie"));
        }

        let embedding_view = outputs[0].to_plain_array_view::<f32>()?;

        let embedding_slice = embedding_view.as_slice().ok_or_else(|| {
            anyhow::anyhow!("Impossible de lire la sortie ArcFace sous forme de slice")
        })?;

        if embedding_slice.is_empty() {
            return Err(anyhow::anyhow!("ArcFace a produit un embedding vide"));
        }

        let embedding = embedding_slice.to_vec();

        if embedding.len() != 512 {
            return Err(anyhow::anyhow!(
                "Embedding ArcFace inattendu : {} dimensions (attendu 512)",
                embedding.len()
            ));
        }

        // ---------------------------------------------------------
        // Normalisation L2
        // ---------------------------------------------------------

        let norm = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();

        if !norm.is_finite() || norm < 1e-6 {
            return Err(anyhow::anyhow!(
                "Embedding ArcFace invalide : norme={:.6}",
                norm
            ));
        }

        let normalized: Vec<f32> = embedding.iter().map(|x| x / norm).collect();

        Ok(normalized)
    }

    /// Compare le vecteur extrait avec la base des personnes connues en parallèle via Rayon
    pub fn identify_person(
        detected_embedding: &[f32],
        known_people: &[KnownPerson],
        threshold: f32, // Ex: 0.60
    ) -> Option<(String, f32)> {
        known_people
            .par_iter()
            .map(|person| {
                let sim = cosine_similarity(detected_embedding, &person.embedding);
                (person.name.clone(), sim)
            })
            .filter(|(_, sim)| *sim >= threshold)
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }
}

/// Calcule la similarité cosinus entre deux vecteurs d'empreinte
pub fn cosine_similarity(v1: &[f32], v2: &[f32]) -> f32 {
    if v1.len() != v2.len() || v1.is_empty() {
        return 0.0;
    }

    let similarity: f32 = v1.iter().zip(v2.iter()).map(|(a, b)| a * b).sum();

    similarity.clamp(-1.0, 1.0)
}

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
