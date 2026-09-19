//! Incrustation des bounding-box (et de leur légende) sur la frame vidéo.

use font8x8::UnicodeFonts;
use image::{Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut};
use imageproc::rect::Rect;

use crate::vision::BoundingBox;

/// Helper pour dessiner du texte ASCII avec la police bitmap 8x8 native
fn draw_text_8x8(img: &mut RgbImage, text: &str, start_x: i32, start_y: i32, color: Rgb<u8>) {
    for (i, c) in text.chars().enumerate() {
        if let Some(glyph) = font8x8::BASIC_FONTS.get(c) {
            let char_offset_x = start_x + (i as i32 * 8);

            for (row, byte) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if (byte & (1 << col)) != 0 {
                        let px = char_offset_x + col;
                        let py = start_y + row as i32;

                        if px >= 0 && px < img.width() as i32 && py >= 0 && py < img.height() as i32
                        {
                            img.put_pixel(px as u32, py as u32, color);
                        }
                    }
                }
            }
        }
    }
}

/// Dessine les bounding-box de la frame courante (rectangle + légende) et
/// indique si au moins une détection non reconnue (personne inconnue, chat
/// ou chien) est présente, auquel cas la frame doit déclencher une alerte
/// e-mail (voir `super::capture_loop`).
///
/// Une personne reconnue (visage identifié) n'est pas une intrusion : elle
/// est dessinée en vert et ne déclenche pas d'alerte.
pub(super) fn draw_detections(img: &mut RgbImage, boxes: &[BoundingBox]) -> bool {
    let mut should_alert = false;

    for bbox in boxes {
        let is_known = bbox.label != "person" && bbox.label != "cat" && bbox.label != "dog";

        if !is_known {
            should_alert = true;
        }

        let box_color = if is_known {
            Rgb([0u8, 255u8, 0u8])
        } else {
            Rgb([255u8, 0u8, 0u8])
        };
        let white = Rgb([255u8, 255u8, 255u8]);

        let rect = Rect::at(bbox.x as i32, bbox.y as i32).of_size(bbox.width, bbox.height);
        draw_hollow_rect_mut(img, rect, box_color);

        let caption = if is_known {
            bbox.label.to_string()
        } else {
            format!("{} {:.0}%", bbox.label, bbox.confidence * 100.0)
        };

        let text_bg_height = 10u32;
        let text_bg_width = (caption.len() * 8) as u32 + 2;
        let text_bg_y = if bbox.y >= text_bg_height {
            bbox.y - text_bg_height
        } else {
            bbox.y
        };

        let bg_rect =
            Rect::at(bbox.x as i32, text_bg_y as i32).of_size(text_bg_width, text_bg_height);
        draw_filled_rect_mut(img, bg_rect, box_color);

        draw_text_8x8(
            img,
            &caption,
            bbox.x as i32 + 1,
            text_bg_y as i32 + 1,
            white,
        );
    }

    should_alert
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bbox(x: u32, y: u32, label: &str) -> BoundingBox {
        BoundingBox {
            x,
            y,
            width: 40,
            height: 40,
            label: label.to_string(),
            confidence: 0.9,
        }
    }

    fn blank_image() -> RgbImage {
        RgbImage::from_pixel(200, 200, Rgb([10, 10, 10]))
    }

    #[test]
    fn no_detections_does_not_alert() {
        let mut img = blank_image();
        assert!(!draw_detections(&mut img, &[]));
    }

    #[test]
    fn unrecognized_person_triggers_an_alert() {
        let mut img = blank_image();
        let boxes = vec![bbox(20, 20, "person")];
        assert!(draw_detections(&mut img, &boxes));
    }

    #[test]
    fn cat_and_dog_labels_trigger_an_alert() {
        let mut img = blank_image();
        assert!(draw_detections(&mut img, &[bbox(20, 20, "cat")]));

        let mut img = blank_image();
        assert!(draw_detections(&mut img, &[bbox(20, 20, "dog")]));
    }

    #[test]
    fn a_recognized_face_does_not_trigger_an_alert() {
        // Une bbox dont le label est un nom (résultat de la reconnaissance
        // faciale, voir `tracking::apply_track_identities`) n'est ni
        // "person", ni "cat", ni "dog" : elle est considérée comme connue.
        let mut img = blank_image();
        let boxes = vec![bbox(20, 20, "jerome")];
        assert!(!draw_detections(&mut img, &boxes));
    }

    #[test]
    fn a_single_unrecognized_detection_among_known_ones_still_alerts() {
        let mut img = blank_image();
        let boxes = vec![bbox(20, 20, "jerome"), bbox(80, 80, "person")];
        assert!(draw_detections(&mut img, &boxes));
    }

    #[test]
    fn draw_detections_actually_modifies_the_image() {
        let mut img = blank_image();
        let original = img.clone();
        draw_detections(&mut img, &[bbox(20, 20, "person")]);
        assert_ne!(img, original);
    }

    #[test]
    fn known_detection_is_drawn_in_green_and_unknown_in_red() {
        let mut img = blank_image();
        // Contour supérieur du rectangle : bbox.y == 20, x de 20 à 59.
        draw_detections(&mut img, &[bbox(20, 20, "jerome")]);
        assert_eq!(*img.get_pixel(20, 20), Rgb([0, 255, 0]));

        let mut img = blank_image();
        draw_detections(&mut img, &[bbox(20, 20, "person")]);
        assert_eq!(*img.get_pixel(20, 20), Rgb([255, 0, 0]));
    }

    #[test]
    fn drawing_near_the_top_edge_does_not_panic() {
        // bbox.y (0) < text_bg_height (10) : la légende doit être repliée
        // sous le rectangle plutôt que de provoquer un débordement/soustraction
        // négative sur un u32.
        let mut img = blank_image();
        let boxes = vec![bbox(0, 0, "person")];
        draw_detections(&mut img, &boxes);
    }
}
