//! Détection et alignement de visage ONNX (YuNet).

use anyhow::Result;
use image::{RgbImage, imageops::FilterType};
use imageproc::geometric_transformations::{Interpolation, Projection, warp_into};
use std::sync::Arc;
use tract_ndarray::prelude::*;
use tract_onnx::prelude::*;

use crate::config::DetectionConfig;
use tracing::{debug, error, warn};

use super::model::load_onnx_model;

/// Gabarit ArcFace standard pour l'alignement à 112x112, dans l'ordre de
/// sortie des landmarks YuNet : œil gauche, œil droit, nez, coin de bouche
/// gauche, coin de bouche droit ("gauche"/"droite" = côté de l'IMAGE, pas de
/// la personne). C'est le même gabarit que celui utilisé par InsightFace/
/// ArcFace pour aligner les visages avant extraction de l'empreinte.
const ARCFACE_TEMPLATE_112: [(f32, f32); 5] = [
    (38.2946, 51.6963),
    (73.5318, 51.5014),
    (56.0252, 71.7366),
    (41.5493, 92.3655),
    (70.7299, 92.2041),
];

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
            error!("❌ YuNet : {} sorties reçues, 12 attendues", outputs.len());
            return Ok(None);
        }

        // EXTRACTION DES SORTIES
        //
        // 12 sorties attendues : cls/obj/bbox pour chacune des 3 échelles
        // (strides 8, 16, 32), plus les points-clés (kps, sorties 9-11),
        // utilisés pour aligner le visage par similarité géométrique plutôt
        // que par un simple crop+resize (voir plus bas, et
        // `landmarks_look_plausible`/`similarity_transform`).

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

        let kps_views = [
            outputs[9].to_plain_array_view::<f32>()?,
            outputs[10].to_plain_array_view::<f32>()?,
            outputs[11].to_plain_array_view::<f32>()?,
        ];

        // STRIDES

        const STRIDES: [usize; 3] = [8, 16, 32];

        #[derive(Clone)]
        struct Detection {
            bbox: (f32, f32, f32, f32),
            score: f32,
            // 5 points (œil gauche, œil droit, nez, coin bouche gauche,
            // coin bouche droit), en espace 640x640 comme `bbox`, ou
            // `(NAN, NAN)` répété si leur décodage a échoué pour cette
            // détection précise (voir la boucle ci-dessous) :
            // `landmarks_look_plausible` s'en rend compte et déclenche le
            // repli vers crop+resize plutôt que d'utiliser des landmarks
            // invalides.
            kps: [(f32, f32); 5],
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
            let kps = &kps_views[scale_index];

            if cls.shape() != [1, expected, 1]
                || obj.shape() != [1, expected, 1]
                || bbox.shape() != [1, expected, 4]
                || kps.shape() != [1, expected, 10]
            {
                warn!(
                    "⚠️ Dimensions invalides scale={} cls={:?} obj={:?} bbox={:?} kps={:?}",
                    scale_index,
                    cls.shape(),
                    obj.shape(),
                    bbox.shape(),
                    kps.shape()
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

                // LANDMARKS
                //
                // Même schéma que le centre de la bbox (prior + offset *
                // stride), mais sans exp() : ce sont des positions, pas des
                // dimensions à régresser en échelle logarithmique.
                let mut points = [(f32::NAN, f32::NAN); 5];
                let mut kps_valid = true;

                for (p, point) in points.iter_mut().enumerate() {
                    let ox = kps[[0, index, p * 2]];
                    let oy = kps[[0, index, p * 2 + 1]];

                    if !ox.is_finite() || !oy.is_finite() {
                        kps_valid = false;
                        break;
                    }

                    *point = (prior_x + ox * stride as f32, prior_y + oy * stride as f32);
                }

                if !kps_valid {
                    points = [(f32::NAN, f32::NAN); 5];
                }

                detections.push(Detection {
                    bbox: (x, y, w, h),
                    score,
                    kps: points,
                });
            }
        }

        // CANDIDATS

        debug!("🔍 YuNet : {} visage(s) candidat(s)", detections.len());

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
            debug!(
                "⚠️ YuNet bbox rejeté : {}x{} dans crop {}x{}",
                w, h, orig_w, orig_h
            );

            return Ok(None);
        }

        // ALIGNEMENT PAR LANDMARKS
        //
        // Reprojette les 5 points 640x640 vers l'espace de `person_crop`
        // (même transform que la bbox ci-dessus), puis ajuste par moindres
        // carrés la transformation de similarité (rotation + échelle +
        // translation, sans cisaillement ni réflexion) qui envoie ces points
        // sur le gabarit ArcFace standard en 112x112. Contrairement au simple
        // crop+resize (plus bas), ceci corrige l'inclinaison de la tête, ce
        // qui réduit la variance de l'empreinte ArcFace pour une même
        // personne selon l'angle de vue — le modèle ArcFace est justement
        // entraîné sur des visages alignés de cette façon.
        //
        // Le décodage des landmarks est moins éprouvé empiriquement que celui
        // de la bbox (voir plus haut) : `landmarks_look_plausible` sert de
        // filet de sécurité, et on retombe sur l'ancien crop+resize si les
        // points semblent aberrants ou si la transformation obtenue est
        // dégénérée, plutôt que de produire un visage aligné n'importe
        // comment.
        let kps_original: [(f32, f32); 5] = best.kps.map(|(kx, ky)| {
            let x_resized = kx - pad_x as f32;
            let y_resized = ky - pad_y as f32;

            (
                (x_resized / scale).clamp(0.0, orig_w as f32),
                (y_resized / scale).clamp(0.0, orig_h as f32),
            )
        });

        if landmarks_look_plausible(&kps_original)
            && let Some(matrix) = similarity_transform(&kps_original, &ARCFACE_TEMPLATE_112)
            && let Some(projection) = Projection::from_matrix(matrix)
        {
            let mut aligned = RgbImage::new(112, 112);

            warp_into(
                person_crop,
                &projection,
                Interpolation::Bilinear,
                image::Rgb([0, 0, 0]),
                &mut aligned,
            );

            debug!(
                "🙂 YuNet : visage {:.1}% | aligné par landmarks",
                best.score * 100.0
            );

            return Ok(Some((aligned, best.score)));
        }

        // REPLI : CROP + RESIZE
        //
        // Comportement historique, utilisé si les landmarks sont absents ou
        // ont échoué aux vérifications de plausibilité ci-dessus.

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

        debug!(
            "🙂 YuNet : visage {:.1}% | bbox {}x{} | crop {}x{} | repli crop+resize",
            best.score * 100.0,
            w,
            h,
            crop_w,
            crop_h
        );

        Ok(Some((aligned, best.score)))
    }
}

/// Vérifie grossièrement que 5 landmarks (œil gauche, œil droit, nez, coin
/// bouche gauche, coin bouche droit) forment un visage plausible, avant de
/// leur faire confiance pour l'alignement : tous finis, yeux suffisamment
/// écartés (évite une transformation dégénérée si les points sont quasi
/// confondus), et yeux nettement au-dessus de la bouche (axe Y croissant
/// vers le bas, comme des coordonnées image classiques).
///
/// Sert de filet de sécurité pour `detect_face_crop_with_threshold` : le
/// décodage des landmarks (voir la boucle de décodage) suit le même schéma
/// que celui de la bbox mais n'a pas pu être validé empiriquement de la même
/// façon (pas de caméra/photo de test disponible pour comparer visuellement
/// les points décodés à un visage réel) ; si les points obtenus n'ont pas
/// cette forme, on préfère retomber sur l'ancien crop+resize plutôt que
/// produire un visage aligné n'importe comment.
fn landmarks_look_plausible(points: &[(f32, f32); 5]) -> bool {
    if !points.iter().all(|(x, y)| x.is_finite() && y.is_finite()) {
        return false;
    }

    let [left_eye, right_eye, _nose, left_mouth, right_mouth] = *points;

    let eye_distance =
        ((right_eye.0 - left_eye.0).powi(2) + (right_eye.1 - left_eye.1).powi(2)).sqrt();

    if eye_distance < 4.0 {
        return false;
    }

    let eyes_y = (left_eye.1 + right_eye.1) / 2.0;
    let mouth_y = (left_mouth.1 + right_mouth.1) / 2.0;

    mouth_y - eyes_y > eye_distance * 0.2
}

/// Calcule la transformation de similarité 2D (rotation + échelle uniforme +
/// translation, sans cisaillement ni réflexion) qui envoie `src` sur `dst`
/// au sens des moindres carrés, par la solution fermée classique pour ce
/// problème (cas particulier 2D de l'algorithme de Umeyama sans réflexion,
/// équivalent à ajuster un facteur complexe échelle*rotation entre les deux
/// nuages de points centrés). Retourne une matrice 3x3 ligne par ligne
/// compatible avec [`Projection::from_matrix`], ou `None` si `src` est
/// dégénéré (points confondus) ou si l'échelle résultante est aberrante.
fn similarity_transform(src: &[(f32, f32); 5], dst: &[(f32, f32); 5]) -> Option<[f32; 9]> {
    let n = src.len() as f32;

    let (mut src_cx, mut src_cy, mut dst_cx, mut dst_cy) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);

    for i in 0..src.len() {
        src_cx += src[i].0;
        src_cy += src[i].1;
        dst_cx += dst[i].0;
        dst_cy += dst[i].1;
    }

    src_cx /= n;
    src_cy /= n;
    dst_cx /= n;
    dst_cy /= n;

    let mut numerator_real = 0.0f32;
    let mut numerator_imag = 0.0f32;
    let mut denominator = 0.0f32;

    for i in 0..src.len() {
        let x = src[i].0 - src_cx;
        let y = src[i].1 - src_cy;
        let u = dst[i].0 - dst_cx;
        let v = dst[i].1 - dst_cy;

        numerator_real += u * x + v * y;
        numerator_imag += v * x - u * y;
        denominator += x * x + y * y;
    }

    if denominator < 1e-6 {
        return None;
    }

    let a_real = numerator_real / denominator;
    let a_imag = numerator_imag / denominator;

    let scale = (a_real * a_real + a_imag * a_imag).sqrt();

    if !scale.is_finite() || !(0.05..20.0).contains(&scale) {
        return None;
    }

    let tx = dst_cx - a_real * src_cx + a_imag * src_cy;
    let ty = dst_cy - a_imag * src_cx - a_real * src_cy;

    Some([a_real, -a_imag, tx, a_imag, a_real, ty, 0.0, 0.0, 1.0])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Landmarks synthétiques pour un visage bien droit et bien centré : les
    /// distances/proportions sont directement dérivées du gabarit ArcFace,
    /// translaté et mis à l'échelle pour simuler un visage détecté dans une
    /// image plus grande.
    fn upright_face(offset_x: f32, offset_y: f32, scale: f32) -> [(f32, f32); 5] {
        ARCFACE_TEMPLATE_112.map(|(x, y)| (x * scale + offset_x, y * scale + offset_y))
    }

    #[test]
    fn plausible_upright_face_passes() {
        let points = upright_face(100.0, 50.0, 1.5);
        assert!(landmarks_look_plausible(&points));
    }

    #[test]
    fn non_finite_points_are_rejected() {
        let mut points = upright_face(0.0, 0.0, 1.0);
        points[2] = (f32::NAN, f32::NAN);
        assert!(!landmarks_look_plausible(&points));
    }

    #[test]
    fn coincident_eyes_are_rejected() {
        let mut points = upright_face(0.0, 0.0, 1.0);
        points[1] = points[0];
        assert!(!landmarks_look_plausible(&points));
    }

    #[test]
    fn mouth_above_eyes_is_rejected() {
        // Bouche et yeux inversés verticalement : géométriquement absurde
        // pour un visage droit ou même penché raisonnablement.
        let points = [
            (41.5493, 92.3655),
            (70.7299, 92.2041),
            (56.0252, 71.7366),
            (38.2946, 51.6963),
            (73.5318, 51.5014),
        ];
        assert!(!landmarks_look_plausible(&points));
    }

    #[test]
    fn identity_transform_is_found_for_identical_point_sets() {
        let points = upright_face(10.0, 20.0, 1.0);
        let matrix =
            similarity_transform(&points, &ARCFACE_TEMPLATE_112).expect("transformation attendue");

        // src = ARCFACE_TEMPLATE_112 translaté de (10, 20) -> dst =
        // ARCFACE_TEMPLATE_112 : la transformation doit être une simple
        // translation de (-10, -20), sans rotation ni changement d'échelle.
        assert!((matrix[0] - 1.0).abs() < 1e-3, "a_real={}", matrix[0]);
        assert!(matrix[1].abs() < 1e-3, "a_imag={}", matrix[1]);
        assert!((matrix[2] - (-10.0)).abs() < 1e-2, "tx={}", matrix[2]);
        assert!((matrix[5] - (-20.0)).abs() < 1e-2, "ty={}", matrix[5]);
    }

    #[test]
    fn scaled_point_set_recovers_the_expected_scale() {
        let points = upright_face(0.0, 0.0, 2.0);
        let matrix =
            similarity_transform(&points, &ARCFACE_TEMPLATE_112).expect("transformation attendue");

        let scale = (matrix[0] * matrix[0] + matrix[3] * matrix[3]).sqrt();
        assert!((scale - 0.5).abs() < 1e-3, "scale={}", scale);
    }

    #[test]
    fn transform_maps_source_points_onto_destination_points() {
        // Vérifie directement la propriété qui compte : appliquer la
        // matrice retournée aux points source doit reproduire les points
        // destination (aux erreurs d'arrondi près), quel que soit le détail
        // de la formule fermée utilisée pour l'obtenir.
        let src = upright_face(15.0, -8.0, 0.8);
        let matrix =
            similarity_transform(&src, &ARCFACE_TEMPLATE_112).expect("transformation attendue");

        for i in 0..5 {
            let (x, y) = src[i];
            let mapped_x = matrix[0] * x + matrix[1] * y + matrix[2];
            let mapped_y = matrix[3] * x + matrix[4] * y + matrix[5];

            assert!(
                (mapped_x - ARCFACE_TEMPLATE_112[i].0).abs() < 1e-2,
                "point {i} x: {mapped_x} != {}",
                ARCFACE_TEMPLATE_112[i].0
            );
            assert!(
                (mapped_y - ARCFACE_TEMPLATE_112[i].1).abs() < 1e-2,
                "point {i} y: {mapped_y} != {}",
                ARCFACE_TEMPLATE_112[i].1
            );
        }
    }

    #[test]
    fn degenerate_coincident_points_return_none() {
        let points = [(5.0, 5.0); 5];
        assert!(similarity_transform(&points, &ARCFACE_TEMPLATE_112).is_none());
    }

    #[test]
    fn from_matrix_accepts_the_produced_transform() {
        // `Projection::from_matrix` retourne `None` si la matrice n'est pas
        // inversible : vérifie qu'une transformation de similarité valide
        // (rotation+échelle+translation, jamais singulière pour une échelle
        // non nulle) est bien acceptée par imageproc.
        let src = upright_face(5.0, 5.0, 1.2);
        let matrix =
            similarity_transform(&src, &ARCFACE_TEMPLATE_112).expect("transformation attendue");

        assert!(Projection::from_matrix(matrix).is_some());
    }
}
