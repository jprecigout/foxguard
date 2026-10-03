//! Vignettes des détections : la petite image qui accompagne chaque
//! événement publié, et qui donne à la timeline de l'interface du manager de
//! quoi être lue d'un coup d'œil.
//!
//! # Recadrée sur la personne, pas sur la scène
//!
//! Une vignette de toute la scène n'apprend rien : à 160 pixels de large, une
//! personne au milieu du champ y occupe une dizaine de pixels. La vignette est
//! donc recadrée autour de la bounding-box du suivi, avec une marge — assez
//! pour reconnaître qui c'est, assez pour voir où il est.
//!
//! # Pourquoi si petite
//!
//! Elle voyage DANS l'événement MQTT puis dans la base du manager (voir
//! `foxguard_protocol::DetectionEvent::thumbnail`). Les événements n'étant
//! publiés qu'aux changements d'état, et non à chaque frame, quelques
//! kilo-octets par détection sont un coût négligeable — mais seulement si on
//! s'y tient. D'où les bornes de ce module plutôt qu'un simple
//! ré-encodage de la frame.

use image::codecs::jpeg::JpegEncoder;
use image::{RgbImage, imageops};

/// Largeur de la vignette, en pixels.
///
/// 192 px : lisible sur une timeline, y compris sur un écran à forte densité
/// où elle sera affichée à 96 px logiques.
pub const WIDTH: u32 = 192;

/// Hauteur de la vignette, en pixels.
///
/// Rapport 4:3, indépendant de celui de la caméra : la timeline aligne des
/// vignettes, et une rangée de formats différents serait illisible. Le
/// recadrage s'adapte donc à ce rapport plutôt que l'inverse.
pub const HEIGHT: u32 = 144;

/// Qualité JPEG.
///
/// 72 : à cette taille, l'écart visible avec 90 est nul, pour un fichier
/// deux fois plus petit — et c'est la taille qui compte, puisque la vignette
/// traverse le broker MQTT.
const QUALITY: u8 = 72;

/// Marge ajoutée autour de la bounding-box, en proportion de sa plus grande
/// dimension.
///
/// Sans marge, la vignette est un gros plan serré dont on ne sait pas dire
/// où il a été pris ; trop large, la personne redevient un détail.
const PADDING_RATIO: f32 = 0.25;

/// Produit la vignette JPEG d'une détection.
///
/// `focus` est la bounding-box de la personne concernée. `None` cadre la
/// scène entière — le cas d'un événement sans boîte exploitable.
///
/// Retourne `None` si l'encodage échoue (image vide, mémoire) : une détection
/// sans vignette est publiée quand même, c'est l'événement qui porte
/// l'information.
pub fn encode(image: &RgbImage, focus: Option<(u32, u32, u32, u32)>) -> Option<Vec<u8>> {
    if image.width() == 0 || image.height() == 0 {
        return None;
    }

    let (x, y, width, height) = match focus {
        Some(bbox) => crop_window(image.width(), image.height(), bbox),
        None => (0, 0, image.width(), image.height()),
    };

    let cropped = imageops::crop_imm(image, x, y, width, height).to_image();

    // `Triangle` (interpolation bilinéaire) : à ce facteur de réduction,
    // `Lanczos3` ne se distingue pas à l'œil et coûte plusieurs fois plus
    // cher — or ceci tourne sur le thread de reconnaissance d'un
    // Raspberry Pi, juste après une inférence YOLO.
    let resized = imageops::resize(&cropped, WIDTH, HEIGHT, imageops::FilterType::Triangle);

    let mut jpeg = Vec::new();
    JpegEncoder::new_with_quality(&mut jpeg, QUALITY)
        .encode_image(&resized)
        .ok()?;

    Some(jpeg)
}

/// Fenêtre de recadrage autour d'une bounding-box : marge ajoutée, rapport
/// d'aspect de la vignette respecté, et le tout ramené dans les bornes de
/// l'image.
///
/// Extraite pour être testable : c'est le seul calcul non trivial du module,
/// et une erreur de bornes y provoquerait une panique dans
/// `imageops::crop_imm`.
fn crop_window(
    image_width: u32,
    image_height: u32,
    (box_x, box_y, box_width, box_height): (u32, u32, u32, u32),
) -> (u32, u32, u32, u32) {
    // Une boîte dégénérée ou hors champ (caméra qui a changé de résolution
    // entre la détection et la vignette) : on se rabat sur la scène entière
    // plutôt que de calculer sur des coordonnées qui n'ont plus de sens.
    if box_width == 0 || box_height == 0 || box_x >= image_width || box_y >= image_height {
        return (0, 0, image_width, image_height);
    }

    let center_x = box_x as f32 + box_width as f32 / 2.0;
    let center_y = box_y as f32 + box_height as f32 / 2.0;

    let padding = box_width.max(box_height) as f32 * PADDING_RATIO;

    // On part de la boîte élargie, puis on l'étend sur l'axe qui manque pour
    // atteindre le rapport de la vignette. ÉTENDRE et non rogner : rogner
    // couperait la tête ou les pieds de la personne qu'on veut montrer.
    let padded_width = box_width as f32 + 2.0 * padding;
    let padded_height = box_height as f32 + 2.0 * padding;

    let target_ratio = WIDTH as f32 / HEIGHT as f32;

    let (mut window_width, mut window_height) = if padded_width / padded_height > target_ratio {
        (padded_width, padded_width / target_ratio)
    } else {
        (padded_height * target_ratio, padded_height)
    };

    // La fenêtre ne peut pas être plus grande que l'image.
    window_width = window_width.min(image_width as f32);
    window_height = window_height.min(image_height as f32);

    // Centrée sur la boîte, puis glissée à l'intérieur de l'image : glisser
    // garde la taille (donc le rapport d'aspect) de la fenêtre, là où un
    // simple écrêtage des bords la déformerait pour les détections proches
    // d'un bord — c'est-à-dire la plupart, puisqu'on y entre et on en sort.
    let x = (center_x - window_width / 2.0).clamp(0.0, image_width as f32 - window_width);
    let y = (center_y - window_height / 2.0).clamp(0.0, image_height as f32 - window_height);

    (
        x.max(0.0) as u32,
        y.max(0.0) as u32,
        (window_width as u32).max(1).min(image_width),
        (window_height as u32).max(1).min(image_height),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scene() -> RgbImage {
        RgbImage::from_fn(640, 480, |x, y| {
            image::Rgb([(x % 255) as u8, (y % 255) as u8, 120])
        })
    }

    /// Vérifie qu'une fenêtre tient bien dans l'image : c'est exactement la
    /// condition que `imageops::crop_imm` exige, sans quoi il panique ou
    /// tronque.
    fn assert_inside(window: (u32, u32, u32, u32), width: u32, height: u32) {
        let (x, y, w, h) = window;
        assert!(w > 0 && h > 0, "fenêtre vide : {window:?}");
        assert!(
            x + w <= width && y + h <= height,
            "fenêtre {window:?} hors d'une image {width}x{height}"
        );
    }

    #[test]
    fn a_thumbnail_is_produced_at_the_documented_size() {
        let jpeg = encode(&scene(), Some((200, 150, 80, 200))).expect("vignette");

        let decoded = image::load_from_memory(&jpeg).expect("JPEG valide");
        assert_eq!((decoded.width(), decoded.height()), (WIDTH, HEIGHT));
    }

    #[test]
    fn a_thumbnail_stays_small_enough_to_travel_in_an_mqtt_event() {
        // Elle traverse le broker puis la base du manager : c'est tout
        // l'intérêt de la borner ici.
        let jpeg = encode(&scene(), Some((200, 150, 80, 200))).expect("vignette");

        assert!(jpeg.len() < 20 * 1024, "vignette de {} octets", jpeg.len());
    }

    #[test]
    fn the_thumbnail_is_framed_on_the_detection_and_not_on_the_whole_scene() {
        // La fenêtre doit rester bien plus petite que l'image, sinon la
        // personne y occupe quelques pixels et la vignette n'apprend rien.
        let window = crop_window(640, 480, (280, 180, 60, 120));

        assert!(window.2 < 640 / 2, "fenêtre trop large : {window:?}");
        assert_inside(window, 640, 480);
    }

    #[test]
    fn the_window_contains_the_detection_it_frames() {
        let bbox = (280, 180, 60, 120);
        let (x, y, w, h) = crop_window(640, 480, bbox);

        assert!(x <= bbox.0, "bord gauche coupé");
        assert!(y <= bbox.1, "bord haut coupé");
        assert!(x + w >= bbox.0 + bbox.2, "bord droit coupé");
        assert!(y + h >= bbox.1 + bbox.3, "bord bas coupé");
    }

    #[test]
    fn the_window_keeps_the_thumbnail_aspect_ratio() {
        // Les vignettes sont alignées dans la timeline : une rangée de
        // rapports différents serait illisible, et redimensionner de force
        // écraserait l'image.
        let (_, _, width, height) = crop_window(640, 480, (280, 180, 60, 200));

        let ratio = width as f32 / height as f32;
        let expected = WIDTH as f32 / HEIGHT as f32;

        assert!(
            (ratio - expected).abs() < 0.05,
            "rapport {ratio} au lieu de {expected}"
        );
    }

    #[test]
    fn a_detection_at_the_edge_slides_inward_instead_of_being_squashed() {
        // Le cas le plus courant : on entre et on sort du champ par un bord.
        for bbox in [
            (0, 0, 60, 120),
            (580, 0, 60, 120),
            (0, 360, 60, 120),
            (580, 360, 60, 120),
        ] {
            let window = crop_window(640, 480, bbox);
            assert_inside(window, 640, 480);

            let ratio = window.2 as f32 / window.3 as f32;
            assert!(
                (ratio - WIDTH as f32 / HEIGHT as f32).abs() < 0.05,
                "détection {bbox:?} : rapport déformé ({ratio})"
            );
        }
    }

    #[test]
    fn a_detection_larger_than_the_image_is_clamped_to_it() {
        let window = crop_window(640, 480, (0, 0, 2000, 2000));
        assert_inside(window, 640, 480);
    }

    #[test]
    fn a_degenerate_box_falls_back_to_the_whole_scene() {
        // Boîte de largeur nulle, ou coordonnées héritées d'une résolution
        // différente : on cadre la scène plutôt que de calculer sur du vide.
        assert_eq!(crop_window(640, 480, (10, 10, 0, 50)), (0, 0, 640, 480));
        assert_eq!(crop_window(640, 480, (10, 10, 50, 0)), (0, 0, 640, 480));
        assert_eq!(crop_window(640, 480, (9000, 10, 50, 50)), (0, 0, 640, 480));
        assert_eq!(crop_window(640, 480, (10, 9000, 50, 50)), (0, 0, 640, 480));
    }

    #[test]
    fn a_thumbnail_can_be_produced_without_any_focus() {
        let jpeg = encode(&scene(), None).expect("vignette");
        let decoded = image::load_from_memory(&jpeg).expect("JPEG valide");

        assert_eq!((decoded.width(), decoded.height()), (WIDTH, HEIGHT));
    }

    #[test]
    fn an_empty_image_yields_no_thumbnail_rather_than_a_panic() {
        let empty = RgbImage::new(0, 0);
        assert_eq!(encode(&empty, None), None);
    }

    #[test]
    fn a_tiny_image_still_yields_a_thumbnail() {
        // Une caméra en très basse résolution : la vignette est simplement
        // agrandie, elle ne doit pas échouer.
        let tiny = RgbImage::from_pixel(8, 6, image::Rgb([30, 60, 90]));
        let jpeg = encode(&tiny, Some((1, 1, 4, 4))).expect("vignette");

        let decoded = image::load_from_memory(&jpeg).expect("JPEG valide");
        assert_eq!((decoded.width(), decoded.height()), (WIDTH, HEIGHT));
    }

    #[test]
    fn every_window_of_a_sweep_across_the_image_stays_in_bounds() {
        // Garde-fou général : `crop_imm` panique sur une fenêtre qui dépasse,
        // et une détection peut se trouver n'importe où.
        for x in (0..640).step_by(37) {
            for y in (0..480).step_by(29) {
                for (width, height) in [(1, 1), (20, 60), (300, 50), (600, 460)] {
                    let window = crop_window(640, 480, (x, y, width, height));
                    assert_inside(window, 640, 480);
                }
            }
        }
    }
}
