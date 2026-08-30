use anyhow::Result;
use chrono::Local;
use font8x8::UnicodeFonts;
use image::{ImageFormat, Rgb, RgbImage};
use imageproc::drawing::{draw_filled_rect_mut, draw_hollow_rect_mut};
use imageproc::rect::Rect;
use std::fs::File;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use v4l::buffer::Type;
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;
use v4l::video::Capture;

use crate::config::Config;
use crate::mail::Mailer;
use crate::vision::{BoundingBox, ObjectDetector};

/// État partagé de la caméra pour la gestion par le WebSocket
pub struct SharedState {
    pub detection_enabled: AtomicBool,
    pub recording_enabled: AtomicBool,
    pub api_token: String,
    pub tx: broadcast::Sender<Vec<u8>>,
}

/// Helper pour dessiner du texte ASCII avec la police bitmap 8x8 native
fn draw_text_8x8(img: &mut RgbImage, text: &str, start_x: i32, start_y: i32, color: Rgb<u8>) {
    for (i, c) in text.chars().enumerate() {
        if let Some(glyph) = font8x8::BASIC_FONTS.get(c) {
            let char_offset_x = start_x + (i as i32 * 8);

            for (row, byte) in glyph.iter().enumerate() {
                for col in 0..8 {
                    if (byte & (1 << col)) != 0 {
                        let px = char_offset_x + col as i32;
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

pub fn start_camera_loop(config: Config, state: Arc<SharedState>) -> Result<()> {
    // Initialisation dynamique du périphérique V4L2
    let dev = Device::new(config.camera.device_index)?;
    let fmt = dev.format()?;
    println!(
        "🎥 Caméra détectée : {}x{} ({:?})",
        fmt.width, fmt.height, fmt.fourcc
    );

    // Allocation de 2 buffers MMAP (plus stable pour le sous-système mémoire du Pi)
    let mut stream = MmapStream::with_buffers(&dev, Type::VideoCapture, 2)?;

    // Initialisation de l'IA (ObjectDetector) et du Mailer
    let detector = ObjectDetector::new(config.detection.clone())?;
    let mailer = Mailer::new(config.email.clone());

    let mut video_file: Option<File> = None;
    let mut last_email_time =
        Instant::now() - Duration::from_secs(config.detection.email_cooldown_secs);
    let mut frame_count: u64 = 0;
    let mut last_boxes: Vec<BoundingBox> = Vec::new();

    println!("📹 Boucle Caméra démarrée avec succès.");

    loop {
        // Capture du buffer brut depuis V4L2
        let (buf, _) = stream.next()?;
        frame_count += 1;

        let is_detection_active = state.detection_enabled.load(Ordering::Relaxed);
        let is_recording_active = state.recording_enabled.load(Ordering::Relaxed);

        // Détection dynamique du format (PC vs PI)
        // Les fichiers JPEG/MJPEG commencent TOUJOURS par les octets magiques 0xFF 0xD8
        let decoded_img = if buf.len() > 2 && buf[0] == 0xFF && buf[1] == 0xD8 {
            // Mode PC : La caméra envoie du MJPEG, on utilise la crate image
            image::load_from_memory(buf).ok().map(|i| i.to_rgb8())
        } else {
            // Mode Raspberry Pi : La caméra envoie du YUYV brut
            decode_yuyv_to_rgb(buf, fmt.width, fmt.height)
        };

        let jpeg_bytes = if is_detection_active {
            // --- MODE DÉTECTION (IA) ---
            if let Some(mut img) = decoded_img {
                // Exécution de l'IA une frame sur 2
                if frame_count % 2 == 0 {
                    if let Ok(boxes) = detector.detect(&img) {
                        last_boxes = boxes;
                    }
                }

                if !last_boxes.is_empty() {
                    for bbox in &last_boxes {
                        let red = Rgb([255u8, 0u8, 0u8]);
                        let white = Rgb([255u8, 255u8, 255u8]);

                        // Boîte de détection rouge
                        let rect =
                            Rect::at(bbox.x as i32, bbox.y as i32).of_size(bbox.width, bbox.height);
                        draw_hollow_rect_mut(&mut img, rect, red);

                        // Formatage du texte
                        let caption = format!("{} {:.0}%", bbox.label, bbox.confidence * 100.0);

                        // Fond rouge sous le texte
                        let text_bg_height = 10u32;
                        let text_bg_width = (caption.len() * 8) as u32 + 2;
                        let text_bg_y = if bbox.y >= text_bg_height {
                            bbox.y - text_bg_height
                        } else {
                            bbox.y
                        };

                        let bg_rect = Rect::at(bbox.x as i32, text_bg_y as i32)
                            .of_size(text_bg_width, text_bg_height);
                        draw_filled_rect_mut(&mut img, bg_rect, red);

                        // Texte blanc avec font8x8
                        draw_text_8x8(
                            &mut img,
                            &caption,
                            bbox.x as i32 + 1,
                            text_bg_y as i32 + 1,
                            white,
                        );
                    }
                }

                // Ré-encodage de la frame modifiée par l'overlay
                let mut encoded = Vec::new();
                let mut cursor = std::io::Cursor::new(&mut encoded);
                let current_bytes = if img.write_to(&mut cursor, ImageFormat::Jpeg).is_ok() {
                    encoded
                } else {
                    buf.to_vec()
                };

                // Envoi d'e-mail avec l'image encodée et gestion du cooldown
                if !last_boxes.is_empty()
                    && last_email_time.elapsed()
                        >= Duration::from_secs(config.detection.email_cooldown_secs)
                {
                    mailer.send_alert(current_bytes.clone());
                    last_email_time = Instant::now();
                }

                current_bytes
            } else {
                buf.to_vec()
            }
        } else {
            // Mode PASS-THROUGH (0% CPU)
            buf.to_vec()
        };

        // Enregistrement fichier dans output_record
        if is_recording_active {
            if video_file.is_none() {
                std::fs::create_dir_all("output_record")?;

                let filename = format!(
                    "output_record/rec_{}.mjpeg",
                    Local::now().format("%Y%m%d_%H%M%S")
                );
                video_file = Some(File::create(&filename)?);
                println!("💾 Début d'enregistrement : {}", filename);
            }
            if let Some(ref mut file) = video_file {
                let _ = file.write_all(&jpeg_bytes);
            }
        } else if video_file.is_some() {
            println!("💾 Arrêt de l'enregistrement.");
            video_file = None;
        }

        // Broadcast frame websocket
        let _ = state.tx.send(jpeg_bytes);
    }
}

// Décode un flux brut YUYV (YUY2) en RgbImage (Caméra Raspberry Pi)
fn decode_yuyv_to_rgb(buf: &[u8], width: u32, height: u32) -> Option<RgbImage> {
    if buf.len() < (width * height * 2) as usize {
        return None;
    }

    let mut rgb_img = RgbImage::new(width, height);

    for (i, chunk) in buf.chunks_exact(4).enumerate() {
        let y0 = chunk[0] as f32;
        let u = chunk[1] as f32 - 128.0;
        let y1 = chunk[2] as f32;
        let v = chunk[3] as f32 - 128.0;

        let x = (i as u32 * 2) % width;
        let y = (i as u32 * 2) / width;

        if y < height {
            // Pixel 1
            let r1 = (y0 + 1.402 * v).clamp(0.0, 255.0) as u8;
            let g1 = (y0 - 0.34414 * u - 0.71414 * v).clamp(0.0, 255.0) as u8;
            let b1 = (y0 + 1.772 * u).clamp(0.0, 255.0) as u8;
            rgb_img.put_pixel(x, y, Rgb([r1, g1, b1]));

            // Pixel 2
            if x + 1 < width {
                let r2 = (y1 + 1.402 * v).clamp(0.0, 255.0) as u8;
                let g2 = (y1 - 0.34414 * u - 0.71414 * v).clamp(0.0, 255.0) as u8;
                let b2 = (y1 + 1.772 * u).clamp(0.0, 255.0) as u8;
                rgb_img.put_pixel(x + 1, y, Rgb([r2, g2, b2]));
            }
        }
    }
    Some(rgb_img)
}
