//! Tampon d'image I420 (YUV 4:2:0 planaire), le format d'ENTRÉE attendu par
//! l'encodeur H.264 (voir [`super::encoder`]).
//!
//! Le tampon est alloué UNE FOIS et rempli en place à chaque frame
//! ([`I420Buffer::fill_from_yuyv`], [`I420Buffer::fill_from_rgb`]) : à 25
//! im/s, une allocation de 460 Ko par frame tiendrait l'allocateur du
//! Raspberry Pi occupé pour rien.
//!
//! # Deux chemins d'entrée, et pourquoi
//!
//! La caméra ne fournit pas toujours la même chose (voir
//! `crate::capture::capture_loop`) :
//!
//! - sur le Raspberry Pi, le flux brut est en **YUYV** (YUV 4:2:2 entrelacé).
//!   C'est déjà du YUV : la conversion vers I420 n'est qu'un
//!   sous-échantillonnage vertical de la chrominance, sans une seule
//!   multiplication. C'est le chemin de loin le moins coûteux, et celui qui
//!   évite un aller-retour YUV → RGB → YUV ;
//! - sur un PC (webcam MJPEG) ou quand l'incrustation des boîtes a réécrit
//!   l'image, on part d'une **RgbImage**, et il faut alors la vraie
//!   conversion colorimétrique.
//!
//! # Plage de valeurs (« range »)
//!
//! Les deux chemins produisent du YUV en plage **télévision** (luminance 16
//! à 235), parce que c'est ce qu'un décodeur H.264 suppose par défaut en
//! l'absence d'indication contraire dans le flux :
//!
//! - le chemin YUYV n'a rien à faire pour cela : une webcam UVC émet déjà
//!   dans cette plage, on recopie ses octets tels quels ;
//! - le chemin RGB utilise donc les coefficients BT.601 en plage télévision,
//!   et NON les coefficients pleine plage de `crate::capture::codec`. Ce
//!   n'est pas une incohérence : ce module-là décode du YUV de caméra vers du
//!   RGB destiné à l'affichage et à l'inférence, celui-ci encode vers un flux
//!   vidéo destiné à un lecteur. Utiliser la pleine plage ici donnerait une
//!   image aux noirs bouchés et aux blancs brûlés dans VLC.

use image::RgbImage;
use openh264::formats::YUVSource;
use rayon::prelude::*;

/// Une image en I420 : plan de luminance pleine résolution, suivi des deux
/// plans de chrominance en demi-résolution horizontale ET verticale.
pub struct I420Buffer {
    /// Les trois plans à la suite : `Y` (w×h), puis `U` et `V` (w/2 × h/2).
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl I420Buffer {
    /// Alloue un tampon pour des images `width`×`height`.
    ///
    /// Les deux dimensions sont arrondies à l'entier PAIR inférieur : le
    /// sous-échantillonnage 4:2:0 travaille par blocs de 2×2 pixels, et
    /// H.264 lui-même ne sait pas coder de dimension impaire. Mieux vaut
    /// perdre une ligne ou une colonne que refuser une résolution de caméra.
    pub fn new(width: u32, height: u32) -> Self {
        let width = (width as usize) & !1;
        let height = (height as usize) & !1;

        let luma = width * height;
        let chroma = (width / 2) * (height / 2);

        Self {
            // Gris neutre : si une frame n'était jamais écrite, le flux
            // montrerait une image grise plutôt que du bruit mémoire.
            data: {
                let mut data = vec![128u8; luma + 2 * chroma];
                data[..luma].fill(16);
                data
            },
            width,
            height,
        }
    }

    /// Dimensions effectivement encodées (voir [`Self::new`] pour l'arrondi).
    pub fn dimensions(&self) -> (u32, u32) {
        (self.width as u32, self.height as u32)
    }

    /// Découpe le tampon en ses trois plans mutables.
    fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
        let luma = self.width * self.height;
        let chroma = (self.width / 2) * (self.height / 2);

        let (y, rest) = self.data.split_at_mut(luma);
        let (u, v) = rest.split_at_mut(chroma);

        (y, u, v)
    }

    /// Remplit le tampon depuis un buffer brut **YUYV** (YUY2) de
    /// `width`×`height`, tel que le fournit la caméra V4L2 du Raspberry Pi.
    ///
    /// Retourne `false` si le buffer est trop court pour la résolution
    /// annoncée — même garde que `crate::capture::codec::decode_yuyv_to_rgb`,
    /// pour la même raison : une frame tronquée ne doit pas faire paniquer la
    /// boucle de capture.
    pub fn fill_from_yuyv(&mut self, buf: &[u8], source_width: u32) -> bool {
        let (width, height) = (self.width, self.height);
        // Une ligne source peut être plus large que ce qu'on encode (arrondi
        // à l'entier pair) : on lit la ligne entière mais n'en garde que le
        // début.
        let source_stride = (source_width as usize) * 2;

        if source_stride < width * 2 || buf.len() < source_stride * height {
            return false;
        }

        let chroma_width = width / 2;
        let (y_plane, u_plane, v_plane) = self.planes_mut();

        // LUMINANCE : une ligne de sortie par ligne d'entrée, un octet sur
        // deux. Aucune arithmétique de couleur, c'est une simple extraction.
        y_plane
            .par_chunks_exact_mut(width)
            .zip(buf.par_chunks_exact(source_stride))
            .for_each(|(out, row)| {
                for (dst, pair) in out.iter_mut().zip(row.chunks_exact(2)) {
                    *dst = pair[0];
                }
            });

        // CHROMINANCE : YUYV est en 4:2:2 (chrominance à pleine résolution
        // VERTICALE), I420 en 4:2:0. Chaque ligne de sortie est donc la
        // moyenne de DEUX lignes d'entrée consécutives.
        u_plane
            .par_chunks_exact_mut(chroma_width)
            .zip(v_plane.par_chunks_exact_mut(chroma_width))
            .zip(buf.par_chunks_exact(source_stride * 2))
            .for_each(|((u_out, v_out), two_rows)| {
                let (top, bottom) = two_rows.split_at(source_stride);

                for (index, (u, v)) in u_out.iter_mut().zip(v_out.iter_mut()).enumerate() {
                    let offset = index * 4;
                    // Y0 U Y1 V : la chrominance est aux offsets 1 et 3.
                    *u = (u16::from(top[offset + 1]) + u16::from(bottom[offset + 1])).div_ceil(2)
                        as u8;
                    *v = (u16::from(top[offset + 3]) + u16::from(bottom[offset + 3])).div_ceil(2)
                        as u8;
                }
            });

        true
    }

    /// Remplit le tampon depuis une image RGB — le flux d'une webcam MJPEG
    /// décodée, ou la frame sur laquelle les boîtes de détection ont déjà été
    /// incrustées (c'est ce qui fait que le flux RTSP montre les mêmes boîtes
    /// que le flux WebSocket).
    ///
    /// Retourne `false` si l'image est plus petite que le tampon.
    pub fn fill_from_rgb(&mut self, image: &RgbImage) -> bool {
        let (width, height) = (self.width, self.height);

        if (image.width() as usize) < width || (image.height() as usize) < height {
            return false;
        }

        let source_stride = image.width() as usize * 3;
        let source = image.as_raw();
        let chroma_width = width / 2;
        let (y_plane, u_plane, v_plane) = self.planes_mut();

        y_plane
            .par_chunks_exact_mut(width)
            .enumerate()
            .for_each(|(row, out)| {
                let line = &source[row * source_stride..][..width * 3];
                for (dst, px) in out.iter_mut().zip(line.chunks_exact(3)) {
                    *dst = luma_from_rgb(px[0], px[1], px[2]);
                }
            });

        // Chrominance moyennée sur le bloc de 2×2 pixels qu'elle couvre :
        // ne la prendre que sur un pixel sur quatre ferait scintiller les
        // bords colorés d'une frame à l'autre.
        u_plane
            .par_chunks_exact_mut(chroma_width)
            .zip(v_plane.par_chunks_exact_mut(chroma_width))
            .enumerate()
            .for_each(|(chroma_row, (u_out, v_out))| {
                let top = &source[(chroma_row * 2) * source_stride..];
                let bottom = &source[(chroma_row * 2 + 1) * source_stride..];

                for (index, (u, v)) in u_out.iter_mut().zip(v_out.iter_mut()).enumerate() {
                    let left = index * 6;

                    let mut red = 0u32;
                    let mut green = 0u32;
                    let mut blue = 0u32;

                    for line in [top, bottom] {
                        for px in line[left..left + 6].chunks_exact(3) {
                            red += u32::from(px[0]);
                            green += u32::from(px[1]);
                            blue += u32::from(px[2]);
                        }
                    }

                    let (red, green, blue) = ((red / 4) as u8, (green / 4) as u8, (blue / 4) as u8);

                    *u = chroma_u_from_rgb(red, green, blue);
                    *v = chroma_v_from_rgb(red, green, blue);
                }
            });

        true
    }
}

// Coefficients BT.601 en plage TÉLÉVISION (voir la note en tête de module) :
// la luminance vit entre 16 et 235, la chrominance entre 16 et 240 autour de
// 128.

fn luma_from_rgb(red: u8, green: u8, blue: u8) -> u8 {
    let value = 16.0
        + 0.256_788 * f32::from(red)
        + 0.504_129 * f32::from(green)
        + 0.097_906 * f32::from(blue);
    value.clamp(16.0, 235.0) as u8
}

fn chroma_u_from_rgb(red: u8, green: u8, blue: u8) -> u8 {
    let value = 128.0 - 0.148_223 * f32::from(red) - 0.290_993 * f32::from(green)
        + 0.439_216 * f32::from(blue);
    value.clamp(16.0, 240.0) as u8
}

fn chroma_v_from_rgb(red: u8, green: u8, blue: u8) -> u8 {
    let value = 128.0 + 0.439_216 * f32::from(red)
        - 0.367_788 * f32::from(green)
        - 0.071_427 * f32::from(blue);
    value.clamp(16.0, 240.0) as u8
}

/// Implémentation attendue par `openh264` : il lit les trois plans tels
/// quels, sans copie.
impl YUVSource for I420Buffer {
    fn dimensions(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    fn strides(&self) -> (usize, usize, usize) {
        (self.width, self.width / 2, self.width / 2)
    }

    fn y(&self) -> &[u8] {
        &self.data[..self.width * self.height]
    }

    fn u(&self) -> &[u8] {
        let luma = self.width * self.height;
        let chroma = (self.width / 2) * (self.height / 2);
        &self.data[luma..luma + chroma]
    }

    fn v(&self) -> &[u8] {
        let luma = self.width * self.height;
        let chroma = (self.width / 2) * (self.height / 2);
        &self.data[luma + chroma..luma + 2 * chroma]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_dimensions_are_rounded_down_to_even() {
        // H.264 ne code pas de dimension impaire, et le 4:2:0 travaille par
        // blocs de 2×2 : mieux vaut perdre une ligne que refuser la caméra.
        let buffer = I420Buffer::new(641, 481);
        assert_eq!(buffer.dimensions(), (640, 480));
    }

    #[test]
    fn the_buffer_has_the_i420_plane_sizes() {
        let buffer = I420Buffer::new(640, 480);

        assert_eq!(buffer.y().len(), 640 * 480);
        assert_eq!(buffer.u().len(), 320 * 240);
        assert_eq!(buffer.v().len(), 320 * 240);
        assert_eq!(buffer.strides(), (640, 320, 320));
    }

    #[test]
    fn a_truncated_yuyv_buffer_is_refused_rather_than_panicking() {
        let mut buffer = I420Buffer::new(4, 4);
        // 4x4 en YUYV demande 32 octets, on n'en donne que 8.
        assert!(!buffer.fill_from_yuyv(&[0u8; 8], 4));
    }

    #[test]
    fn yuyv_luma_is_copied_through_untouched() {
        // Le chemin YUYV ne doit faire AUCUNE conversion de plage : les
        // octets de luminance de la caméra se retrouvent tels quels.
        let mut buffer = I420Buffer::new(2, 2);
        // Deux lignes de 2 pixels : Y0 U Y1 V.
        let yuyv = [
            30u8, 128, 90, 128, // ligne 0 : Y = 30, 90
            60, 128, 120, 128, // ligne 1 : Y = 60, 120
        ];

        assert!(buffer.fill_from_yuyv(&yuyv, 2));
        assert_eq!(buffer.y(), &[30, 90, 60, 120]);
    }

    #[test]
    fn yuyv_chroma_is_averaged_over_the_two_source_rows() {
        // 4:2:2 -> 4:2:0 : la chrominance perd sa résolution verticale.
        let mut buffer = I420Buffer::new(2, 2);
        let yuyv = [
            16u8, 100, 16, 200, // ligne 0 : U = 100, V = 200
            16, 140, 16, 240, // ligne 1 : U = 140, V = 240
        ];

        assert!(buffer.fill_from_yuyv(&yuyv, 2));
        assert_eq!(buffer.u(), &[120]);
        assert_eq!(buffer.v(), &[220]);
    }

    #[test]
    fn a_source_wider_than_the_encoded_size_is_cropped_not_misread() {
        // Caméra en 5 pixels de large, encodage en 4 : si le pas de ligne
        // était confondu avec la largeur encodée, l'image serait cisaillée.
        let mut buffer = I420Buffer::new(5, 2);
        assert_eq!(buffer.dimensions(), (4, 2));

        let mut yuyv = vec![0u8; 5 * 2 * 2];
        // Ligne 0 : luminance 10, 20, 30, 40, 50 — on n'attend que les 4
        // premières.
        for (index, chunk) in yuyv[..20].chunks_exact_mut(2).enumerate() {
            chunk[0] = 10 * (index as u8 + 1);
            chunk[1] = 128;
        }

        assert!(buffer.fill_from_yuyv(&yuyv, 5));
        assert_eq!(&buffer.y()[..4], &[10, 20, 30, 40]);
    }

    #[test]
    fn rgb_black_and_white_land_on_the_television_range() {
        let mut buffer = I420Buffer::new(2, 2);

        let black = RgbImage::from_pixel(2, 2, image::Rgb([0, 0, 0]));
        assert!(buffer.fill_from_rgb(&black));
        assert_eq!(buffer.y(), &[16, 16, 16, 16]);

        let white = RgbImage::from_pixel(2, 2, image::Rgb([255, 255, 255]));
        assert!(buffer.fill_from_rgb(&white));
        assert!(
            buffer.y().iter().all(|&y| (234..=235).contains(&y)),
            "luminance du blanc : {:?}",
            buffer.y()
        );
    }

    #[test]
    fn a_neutral_gray_has_neutral_chroma() {
        let mut buffer = I420Buffer::new(2, 2);
        let gray = RgbImage::from_pixel(2, 2, image::Rgb([128, 128, 128]));

        assert!(buffer.fill_from_rgb(&gray));

        assert_eq!(buffer.u(), &[128]);
        assert_eq!(buffer.v(), &[128]);
    }

    #[test]
    fn pure_red_pushes_v_up_and_u_down() {
        // Le signe des deux coefficients de chrominance : une inversion U/V
        // donnerait une image aux couleurs permutées, ce qu'aucun test de
        // dimension ne verrait.
        let mut buffer = I420Buffer::new(2, 2);
        let red = RgbImage::from_pixel(2, 2, image::Rgb([255, 0, 0]));

        assert!(buffer.fill_from_rgb(&red));

        assert!(buffer.v()[0] > 200, "V du rouge : {}", buffer.v()[0]);
        assert!(buffer.u()[0] < 100, "U du rouge : {}", buffer.u()[0]);
    }

    #[test]
    fn an_image_smaller_than_the_buffer_is_refused() {
        let mut buffer = I420Buffer::new(640, 480);
        let small = RgbImage::from_pixel(320, 240, image::Rgb([0, 0, 0]));

        assert!(!buffer.fill_from_rgb(&small));
    }
}
