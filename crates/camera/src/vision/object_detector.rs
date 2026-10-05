//! Détection d'objets ONNX (YOLO26), restreinte aux classes personne / chat /
//! chien.
//!
//! Le modèle est un export « end-to-end » : il rend directement une ligne par
//! objet, déjà dédoublonnée, et aucune suppression des non-maxima (NMS) n'est
//! à faire ici.
//!
//! L'entrée du modèle est un CARRÉ, alors que la caméra filme en 4:3. La
//! frame y est donc mise en lettres plutôt qu'écrasée (voir [`letterbox`]),
//! et les coordonnées prédites sont ramenées ensuite dans l'image d'origine
//! (voir [`Letterboxed::to_source`]).

use anyhow::{Context, Result};
use image::{Rgb, RgbImage, imageops::FilterType};
use std::sync::Arc;
use tract_onnx::prelude::*;

use crate::config::DetectionConfig;

use super::model::load_onnx_model;
use super::types::BoundingBox;

/// Noms des 80 classes COCO, dans l'ordre des identifiants rendus par le modèle.
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
/// 114 sur les trois canaux : c'est la valeur que la chaîne d'entraînement
/// d'Ultralytics utilise pour ses propres bandes. Un noir franc créerait aux bords
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
/// vers 640x640, toute la scène est étirée d'un tiers en hauteur. Or YOLO a été
/// entraîné sur des images letterboxées — il n'a jamais vu de silhouettes
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
    /// Ramène une boîte prédite par le modèle (coins `x1, y1, x2, y2`, dans
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
    fn to_source(&self, x1: f32, y1: f32, x2: f32, y2: f32) -> Option<(u32, u32, u32, u32)> {
        let to_source_x = |x: f32| ((x - self.pad_x) / self.scale).clamp(0.0, self.source_width);
        let to_source_y = |y: f32| ((y - self.pad_y) / self.scale).clamp(0.0, self.source_height);

        let left = to_source_x(x1);
        let right = to_source_x(x2);
        let top = to_source_y(y1);
        let bottom = to_source_y(y2);

        let width = (right - left) as u32;
        let height = (bottom - top) as u32;

        (width > 0 && height > 0).then_some((left as u32, top as u32, width, height))
    }
}

/// Classes retenues parmi les 80 du jeu COCO.
const ALLOWED_CLASSES: &[&str] = &["person", "cat", "dog"];

/// Nombre maximal de détections rendues par image.
const MAX_DETECTIONS: usize = 10;

/// Détecteur d'objets ONNX (YOLO26), restreint aux classes personne/chat/chien.
pub struct ObjectDetector {
    model: Arc<TypedSimplePlan>,
    config: DetectionConfig,
}

impl ObjectDetector {
    /// Charge le modèle YOLO26 à la taille d'entrée fixée par
    /// `config.input_size`.
    ///
    /// L'export ONNX fige cette taille (les couches d'attention sont
    /// dimensionnées pour elle) : toute autre valeur est refusée, et l'erreur
    /// le rappelle plutôt que de laisser l'opérateur face au seul message de
    /// tract.
    pub fn new(config: DetectionConfig) -> Result<Self> {
        let size = config.input_size;
        let model = load_onnx_model(&config.model_path, size, size).with_context(|| {
            format!(
                "chargement de {} en {size}x{size} (l'export ONNX fige la taille \
                 d'entrée : `input_size` doit être celle de l'export)",
                config.model_path
            )
        })?;

        Ok(Self { model, config })
    }

    /// Détecte personnes, chats et chiens sur `img`, par confiance
    /// décroissante.
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

        decode(&output, &letterboxed, self.config.confidence_threshold)
    }
}

/// Décode la sortie du modèle, `[1, détections, 6]` : une ligne par objet —
/// coins `x1, y1, x2, y2` dans l'espace du canevas, score, classe.
///
/// Ne retient que les classes de [`ALLOWED_CLASSES`] au-dessus de `threshold`,
/// ramenées dans l'image d'origine, et au plus [`MAX_DETECTIONS`], par
/// confiance décroissante.
fn decode(
    output: &tract_ndarray::ArrayViewD<f32>,
    letterboxed: &Letterboxed,
    threshold: f32,
) -> Result<Vec<BoundingBox>> {
    let rows = match output.shape() {
        [1, rows, 6] => *rows,
        shape => anyhow::bail!(
            "sortie de modèle inattendue {shape:?} : un export YOLO26 end-to-end \
             rend [1, détections, 6]"
        ),
    };

    let mut detections: Vec<BoundingBox> = (0..rows)
        .filter_map(|row| {
            let at = |col: usize| output[[0, row, col]];

            let confidence = at(4);
            if confidence < threshold {
                return None;
            }

            let label = *COCO_CLASSES.get(at(5) as usize)?;
            if !ALLOWED_CLASSES.contains(&label) {
                return None;
            }

            // Espace du canevas -> image d'origine : bandes retirées,
            // échelle défaite, débordements rognés.
            let (x, y, width, height) = letterboxed.to_source(at(0), at(1), at(2), at(3))?;

            Some(BoundingBox {
                x,
                y,
                width,
                height,
                label: label.to_string(),
                confidence,
            })
        })
        .collect();

    // Le modèle trie déjà ses lignes, mais rien dans le format ne le
    // garantit : le tri, sur une poignée de boîtes, ne coûte rien.
    detections.sort_unstable_by(|a, b| b.confidence.total_cmp(&a.confidence));
    detections.truncate(MAX_DETECTIONS);

    Ok(detections)
}

#[cfg(test)]
mod tests {
    use super::*;

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
            boxed.to_source(0.0, 80.0, 640.0, 560.0),
            Some((0, 0, 640, 480))
        );
    }

    #[test]
    fn the_bands_are_removed_from_the_vertical_coordinates() {
        let boxed = letterbox(&filled(640, 480, [0, 0, 0]), 640);

        // Boîte de 100x100 centrée sur le canevas : horizontalement
        // inchangée, verticalement remontée des 80 lignes de la bande haute.
        assert_eq!(
            boxed.to_source(270.0, 270.0, 370.0, 370.0),
            Some((270, 190, 100, 100))
        );
    }

    #[test]
    fn the_scale_is_undone_on_a_frame_larger_than_the_model() {
        // 1280x720 réduit de moitié : une boîte de 100x100 dans le canevas
        // vaut 200x200 dans l'image d'origine.
        let boxed = letterbox(&filled(1280, 720, [0, 0, 0]), 640);

        assert_eq!(
            boxed.to_source(270.0, 270.0, 370.0, 370.0),
            Some((540, 260, 200, 200))
        );
    }

    #[test]
    fn a_box_overflowing_the_frame_is_cropped_to_its_edges() {
        // Une personne coupée par le bord gauche : le modèle prédit volontiers
        // une boîte qui sort du cadre. Elle ne doit pas sortir de l'image.
        let boxed = letterbox(&filled(640, 480, [0, 0, 0]), 640);

        let (x, y, width, height) = boxed
            .to_source(-100.0, 220.0, 100.0, 420.0)
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

        assert_eq!(boxed.to_source(300.0, 0.0, 340.0, 40.0), None);
    }

    // --- decode ---

    /// Sortie de modèle `[1, lignes, 6]` faite des `rows` donnés.
    fn output(rows: &[[f32; 6]]) -> tract_ndarray::ArrayD<f32> {
        tract_ndarray::Array3::from_shape_fn((1, rows.len(), 6), |(_, r, c)| rows[r][c]).into_dyn()
    }

    /// Canevas sans bande ni mise à l'échelle : les coordonnées du modèle
    /// sont celles de l'image.
    fn square() -> Letterboxed {
        letterbox(&filled(640, 640, [0, 0, 0]), 640)
    }

    #[test]
    fn a_row_becomes_a_box_from_its_corners() {
        // Coins (100, 200)-(300, 600), score 0.9, classe 15 (chat).
        let out = output(&[[100.0, 200.0, 300.0, 600.0, 0.9, 15.0]]);

        let boxes = decode(&out.view(), &square(), 0.4).unwrap();

        assert_eq!(boxes.len(), 1);
        let b = &boxes[0];
        assert_eq!((b.x, b.y, b.width, b.height), (100, 200, 200, 400));
        assert_eq!(b.label, "cat");
        assert_eq!(b.confidence, 0.9);
    }

    #[test]
    fn rows_under_the_threshold_or_of_another_class_are_dropped() {
        let out = output(&[
            [0.0, 0.0, 10.0, 10.0, 0.39, 0.0],  // personne, sous le seuil
            [0.0, 0.0, 10.0, 10.0, 0.95, 2.0],  // voiture
            [0.0, 0.0, 10.0, 10.0, 0.95, 99.0], // classe hors COCO
            [0.0, 0.0, 10.0, 10.0, 0.41, 16.0], // chien, retenu
        ]);

        let boxes = decode(&out.view(), &square(), 0.4).unwrap();

        assert_eq!(boxes.len(), 1);
        assert_eq!(boxes[0].label, "dog");
    }

    #[test]
    fn boxes_come_out_by_decreasing_confidence_and_capped() {
        let rows: Vec<[f32; 6]> = (0..15)
            .map(|i| [0.0, 0.0, 10.0, 10.0, 0.5 + i as f32 * 0.01, 0.0])
            .collect();

        let boxes = decode(&output(&rows).view(), &square(), 0.4).unwrap();

        assert_eq!(boxes.len(), MAX_DETECTIONS);
        assert!(boxes.windows(2).all(|w| w[0].confidence >= w[1].confidence));
        assert!((boxes[0].confidence - 0.64).abs() < 1e-6);
    }

    #[test]
    fn an_output_of_another_shape_is_an_error() {
        // Sortie à ancres d'un export non end-to-end : refusée plutôt que
        // décodée de travers.
        let out = tract_ndarray::Array3::<f32>::zeros((1, 84, 2100)).into_dyn();

        assert!(decode(&out.view(), &square(), 0.4).is_err());
    }
}
