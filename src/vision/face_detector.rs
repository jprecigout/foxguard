//! Détection et alignement de visage ONNX (YuNet).

use anyhow::Result;
use image::{RgbImage, imageops::FilterType};
use std::sync::Arc;
use tract_ndarray::prelude::*;
use tract_onnx::prelude::*;

use crate::config::DetectionConfig;

use super::model::load_onnx_model;

/// Détecteur de visage ONNX (YuNet). Voir [`Self::new`] pour les contraintes
/// de résolution d'entrée de ce modèle.
pub struct FaceDetectorYuNet {
    model: Arc<TypedSimplePlan>,
    input_w: u32,
    input_h: u32,
}

impl FaceDetectorYuNet {
    /// IMPORTANT : ce fichier ONNX contient des constantes internes figées à
    /// l'export (génération des priors) qui ne fonctionnent qu'avec une
    /// entrée exactement 640x640 : `with_input_fact` échoue ("Impossible to
    /// unify...") pour toute autre taille. `input_w`/`input_h` restent des
    /// paramètres explicites pour la clarté de l'appelant, mais doivent
    /// valoir 640x640 en pratique avec ce modèle.
    ///
    /// Par ailleurs (diagnostiqué expérimentalement), la tête de régression
    /// bbox de ce modèle ne régresse correctement que sur des pixels à
    /// résolution NATIVE : lui donner une image RÉDUITE pour tenir dans
    /// 640x640 fausse largement la largeur/hauteur détectée (jusqu'à 3-4x
    /// trop grand), même si le visage occupe alors une grande partie du
    /// canevas. Pour les photos de `known_faces/`, plus grandes que 640x640,
    /// voir `detect_face_native_res` dans `camera.rs` qui scanne l'image par
    /// fenêtres 640x640 à résolution native plutôt que de la réduire.
    pub fn new(config: DetectionConfig, input_w: u32, input_h: u32) -> Result<Self> {
        let model = load_onnx_model(&config.model_detect_face_path, input_w, input_h)?;

        Ok(Self {
            model,
            input_w,
            input_h,
        })
    }

    /// Détecte et aligne (112x112) le visage le plus probable dans un crop
    /// de personne, avec le seuil de score utilisé pour le flux caméra live
    /// (0.65). Raccourci sur [`Self::detect_face_crop_with_threshold`] qui
    /// jette le score de confiance retourné.
    pub fn detect_face_crop(&self, person_crop: &RgbImage) -> Result<Option<RgbImage>> {
        Ok(self
            .detect_face_crop_with_threshold(person_crop, 0.65)?
            .map(|(img, _score)| img))
    }

    /// Comme `detect_face_crop`, avec un seuil de score personnalisable, et
    /// renvoie en plus le score de confiance de la détection retenue (utile
    /// pour comparer plusieurs fenêtres dans `detect_face_native_res` et
    /// garder la meilleure plutôt que la première trouvée).
    ///
    /// Utile pour `detect_face_native_res` (camera.rs) : quand on scanne une
    /// grande photo par fenêtres 640x640, une fenêtre donnée ne contient
    /// souvent qu'une partie du visage ou le visage décentré, ce qui abaisse
    /// mécaniquement le score par rapport à un visage bien cadré (vérifié
    /// empiriquement : un visage bien centré dans une fenêtre native donne
    /// déjà un score proche de 0.63, sous le seuil de 0.65 utilisé pour le
    /// flux caméra live). Le flux live garde 0.65 via `detect_face_crop`.
    pub fn detect_face_crop_with_threshold(
        &self,
        person_crop: &RgbImage,
        score_threshold: f32,
    ) -> Result<Option<(RgbImage, f32)>> {
        let input_w = self.input_w as usize;
        let input_h = self.input_h as usize;
        const NMS_THRESHOLD: f32 = 0.30;

        let orig_w = person_crop.width();
        let orig_h = person_crop.height();

        if orig_w < 20 || orig_h < 20 {
            return Ok(None);
        }

        // LETTERBOX

        let scale = (input_w as f32 / orig_w as f32).min(input_h as f32 / orig_h as f32);

        let resized_w = ((orig_w as f32 * scale).round() as u32).max(1);
        let resized_h = ((orig_h as f32 * scale).round() as u32).max(1);

        let resized =
            image::imageops::resize(person_crop, resized_w, resized_h, FilterType::Triangle);

        let mut letterboxed =
            RgbImage::from_pixel(input_w as u32, input_h as u32, image::Rgb([0, 0, 0]));

        let pad_x = (input_w as u32 - resized_w) / 2;
        let pad_y = (input_h as u32 - resized_h) / 2;

        image::imageops::overlay(&mut letterboxed, &resized, pad_x as i64, pad_y as i64);

        // TENSOR BGR
        //
        // YuNet est un modèle OpenCV natif entraîné en BGR : contrairement
        // à ArcFace (voir FaceEmbedder::extract_embedding, qui lui attend du
        // RGB), on garde ici l'ordre B, G, R sans le permuter.

        let mut tensor_data = Array4::<f32>::zeros((1, 3, input_h, input_w));

        for (x, y, pixel) in letterboxed.enumerate_pixels() {
            let r = pixel[0] as f32;
            let g = pixel[1] as f32;
            let b = pixel[2] as f32;

            tensor_data[[0, 0, y as usize, x as usize]] = b;
            tensor_data[[0, 1, y as usize, x as usize]] = g;
            tensor_data[[0, 2, y as usize, x as usize]] = r;
        }

        let tensor: Tensor = tensor_data.into();

        // INFÉRENCE

        let outputs = self.model.run(tvec![tensor.into()])?;

        if outputs.len() != 12 {
            eprintln!("❌ YuNet : {} sorties reçues, 12 attendues", outputs.len());
            return Ok(None);
        }

        // EXTRACTION DES SORTIES
        //
        // 12 sorties attendues : cls/obj/bbox pour chacune des 3 échelles
        // (strides 8, 16, 32). Les points-clés (kps, sorties 9-11) ne sont
        // pas exploités : la reconnaissance actuelle recadre puis redimen-
        // sionne le visage plutôt que de l'aligner par les landmarks.

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

        // STRIDES

        const STRIDES: [usize; 3] = [8, 16, 32];

        #[derive(Clone)]
        struct Detection {
            bbox: (f32, f32, f32, f32),
            score: f32,
        }

        let mut detections = Vec::<Detection>::new();

        // DÉCODAGE

        for scale_index in 0..3 {
            let stride = STRIDES[scale_index];

            let grid_w = input_w / stride;
            let grid_h = input_h / stride;

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

                if score < score_threshold {
                    continue;
                }

                let gx = index % grid_w;
                let gy = index / grid_w;

                // IMPORTANT :
                //
                // YuNet utilise les prior centers :
                //
                // prior_x = gx * stride
                // prior_y = gy * stride

                let prior_x = gx as f32 * stride as f32;
                let prior_y = gy as f32 * stride as f32;

                let bx = bbox[[0, index, 0]];
                let by = bbox[[0, index, 1]];
                let bw = bbox[[0, index, 2]];
                let bh = bbox[[0, index, 3]];

                if !bx.is_finite() || !by.is_finite() || !bw.is_finite() || !bh.is_finite() {
                    continue;
                }

                // YuNet bbox

                let cx = prior_x + bx * stride as f32;

                let cy = prior_y + by * stride as f32;

                let width = bw.exp() * stride as f32;

                let height = bh.exp() * stride as f32;

                if !cx.is_finite() || !cy.is_finite() || !width.is_finite() || !height.is_finite() {
                    continue;
                }

                // REJET DES VALEURS ABERRANTES

                // Plafond relatif à la taille d'entrée plutôt qu'une valeur fixe
                // de 300px : un plafond fixe rejette à tort les visages en gros
                // plan (ex: photo de référence dans known_faces/) où le visage
                // occupe une grande partie du cadre.
                let max_face_w = input_w as f32 * 0.95;
                let max_face_h = input_h as f32 * 0.95;

                if width < 5.0 || height < 5.0 || width > max_face_w || height > max_face_h {
                    continue;
                }

                let ratio = width.max(height) / width.min(height);

                if ratio > 2.5 {
                    continue;
                }

                // XYWH

                let mut x = cx - width * 0.5;
                let mut y = cy - height * 0.5;

                let mut w = width;
                let mut h = height;

                // CLAMP

                x = x.clamp(0.0, input_w as f32 - 1.0);

                y = y.clamp(0.0, input_h as f32 - 1.0);

                w = w.min(input_w as f32 - x);

                h = h.min(input_h as f32 - y);

                if w < 5.0 || h < 5.0 {
                    continue;
                }

                detections.push(Detection {
                    bbox: (x, y, w, h),
                    score,
                });
            }
        }

        // CANDIDATS

        eprintln!("🔍 YuNet : {} visage(s) candidat(s)", detections.len());

        if detections.is_empty() {
            return Ok(None);
        }

        // TRI

        detections.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // NMS

        let mut selected = Vec::<Detection>::new();

        for detection in detections {
            if selected
                .iter()
                .all(|existing| crate::geometry::iou(existing.bbox, detection.bbox) < NMS_THRESHOLD)
            {
                selected.push(detection);
            }
        }

        // MEILLEUR VISAGE

        let Some(best) = selected.into_iter().next() else {
            return Ok(None);
        };

        let (x640, y640, w640, h640) = best.bbox;

        // ESPACE D'ENTRÉE (input_w x input_h) -> IMAGE ORIGINALE

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

        // PROTECTION CONTRE LES BBOX ABSURDES

        // Seuil relevé à 97% : un simple crop de personne (caméra) a rarement
        // un visage occupant tout le cadre, mais une photo de référence en
        // gros plan (known_faces/) si, et ne doit pas être rejetée à tort.
        if w > orig_w * 97 / 100 || h > orig_h * 97 / 100 {
            eprintln!(
                "⚠️ YuNet bbox rejeté : {}x{} dans crop {}x{}",
                w, h, orig_w, orig_h
            );

            return Ok(None);
        }

        // MARGE ARCFACE

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

        // CROP

        let face_crop =
            image::imageops::crop_imm(person_crop, crop_x, crop_y, crop_w, crop_h).to_image();

        // ARCFACE 112x112

        let aligned = image::imageops::resize(&face_crop, 112, 112, FilterType::Triangle);

        println!(
            "🙂 YuNet : visage {:.1}% | bbox {}x{} | crop {}x{}",
            best.score * 100.0,
            w,
            h,
            crop_w,
            crop_h
        );

        Ok(Some((aligned, best.score)))
    }
}
