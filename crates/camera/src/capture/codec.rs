//! Décodage du flux caméra brut (YUYV / YUY2) en RGB.

use image::RgbImage;
use rayon::prelude::*;

/// Décode un flux brut YUYV (YUY2) en RgbImage de manière totalement parallélisée avec Rayon
pub(super) fn decode_yuyv_to_rgb(buf: &[u8], width: u32, height: u32) -> Option<RgbImage> {
    let total_pixels = (width * height) as usize;
    if buf.len() < total_pixels * 2 {
        return None;
    }

    let mut raw_rgb = vec![0u8; total_pixels * 3];

    // Utilisation de Rayon ici pour paralléliser la conversion par paquets
    raw_rgb
        .par_chunks_exact_mut(6)
        .zip(buf.par_chunks_exact(4))
        .for_each(|(rgb_out, chunk)| {
            let y0 = chunk[0] as f32;
            let u = chunk[1] as f32 - 128.0;
            let y1 = chunk[2] as f32;
            let v = chunk[3] as f32 - 128.0;

            // Pixel 1
            rgb_out[0] = (y0 + 1.402 * v).clamp(0.0, 255.0) as u8;
            rgb_out[1] = (y0 - 0.34414 * u - 0.71414 * v).clamp(0.0, 255.0) as u8;
            rgb_out[2] = (y0 + 1.772 * u).clamp(0.0, 255.0) as u8;

            // Pixel 2
            rgb_out[3] = (y1 + 1.402 * v).clamp(0.0, 255.0) as u8;
            rgb_out[4] = (y1 - 0.34414 * u - 0.71414 * v).clamp(0.0, 255.0) as u8;
            rgb_out[5] = (y1 + 1.772 * u).clamp(0.0, 255.0) as u8;
        });

    RgbImage::from_raw(width, height, raw_rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffer_too_short_returns_none() {
        // 2x2 attend 8 octets (2 pixels YUYV = 4 octets par paire), on n'en
        // fournit qu'un seul.
        let buf = [0u8; 2];
        assert!(decode_yuyv_to_rgb(&buf, 2, 2).is_none());
    }

    #[test]
    fn decodes_a_single_pixel_pair_to_the_expected_dimensions() {
        // Une paire YUYV encode 2 pixels : Y0 U Y1 V.
        let buf = [128u8, 128, 128, 128];
        let img = decode_yuyv_to_rgb(&buf, 2, 1).expect("buffer valide");
        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 1);
    }

    #[test]
    fn mid_gray_yuv_decodes_to_mid_gray_rgb() {
        // Y=128, U=128, V=128 (neutre chromatique) doit redonner un gris
        // proche de (128,128,128) sur les deux pixels, aux arrondis près.
        let buf = [128u8, 128, 128, 128];
        let img = decode_yuyv_to_rgb(&buf, 2, 1).expect("buffer valide");

        for x in 0..2 {
            let px = img.get_pixel(x, 0);
            for channel in px.0 {
                assert!(
                    (channel as i32 - 128).abs() <= 1,
                    "canal {} attendu proche de 128, obtenu {}",
                    x,
                    channel
                );
            }
        }
    }

    #[test]
    fn black_yuv_decodes_to_black_rgb() {
        // Y=0, U=128, V=128 -> noir pur sur les deux pixels.
        let buf = [0u8, 128, 0, 128];
        let img = decode_yuyv_to_rgb(&buf, 2, 1).expect("buffer valide");

        assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0]);
        assert_eq!(img.get_pixel(1, 0).0, [0, 0, 0]);
    }

    #[test]
    fn decodes_multiple_rows() {
        // 2x2 : deux paires YUYV (une par ligne), toutes deux gris neutre.
        let buf = [128u8, 128, 128, 128, 128, 128, 128, 128];
        let img = decode_yuyv_to_rgb(&buf, 2, 2).expect("buffer valide");
        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 2);
    }
}
