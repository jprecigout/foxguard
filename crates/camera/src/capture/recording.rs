//! Écriture des enregistrements vidéo (`output_record/*.mjpeg`).
//!
//! Chaque frame JPEG est précédée d'un horodatage et de sa longueur (voir
//! [`RecordingWriter::write_frame`]), ce qui permet à la lecture côté client
//! (voir `crate::api`, servi tel quel, et `playVideo` dans
//! `static/controller.html`) de restituer l'enregistrement à la vitesse
//! RÉELLE de capture plutôt qu'à un débit fixe arbitraire (30 im/s) qui ne
//! correspond pas forcément au FPS réel de la caméra à l'instant T (celui-ci
//! peut varier, par exemple en fonction de la luminosité pour une webcam
//! UVC) — c'est cette hypothèse d'un débit fixe qui rendait la relecture
//! accélérée et incohérente d'un enregistrement à l'autre.
//!
//! Format binaire par frame (little-endian) :
//! `[u32 timestamp_ms][u32 frame_len][frame_len octets de JPEG]`
//! `timestamp_ms` est le temps écoulé depuis le début de CET enregistrement
//! (horloge relative, pas un horodatage absolu).

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use chrono::Local;

/// Écrivain d'un fichier d'enregistrement `output_record/rec_<horodatage>.mjpeg`.
pub(super) struct RecordingWriter {
    file: File,
    started_at: Instant,
}

impl RecordingWriter {
    /// Crée un nouveau fichier d'enregistrement dans `output_record/`
    /// (créé si besoin) et démarre son horloge interne (voir
    /// [`Self::write_frame`]).
    pub(super) fn create() -> io::Result<Self> {
        Self::create_in(Path::new("output_record"))
    }

    /// Comme [`Self::create`], mais dans un dossier explicite plutôt que le
    /// dossier `output_record/` fixe utilisé en production. Extrait pour
    /// être testable sans dépendre du répertoire courant du processus (voir
    /// les tests en fin de fichier).
    fn create_in(dir: &Path) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;

        let filename = dir.join(format!(
            "rec_{}.mjpeg",
            Local::now().format("%Y%m%d_%H%M%S")
        ));

        println!("💾 Début d'enregistrement : {}", filename.display());

        Ok(Self {
            file: File::create(&filename)?,
            started_at: Instant::now(),
        })
    }

    /// Ajoute une frame JPEG à l'enregistrement, précédée de son horodatage
    /// (ms écoulées depuis le début de l'enregistrement) et de sa longueur
    /// (voir le format en tête de module).
    pub(super) fn write_frame(&mut self, jpeg_bytes: &[u8]) -> io::Result<()> {
        // Saturating : au-delà de ~49 jours (u32::MAX ms) d'enregistrement
        // continu, l'horodatage plafonne plutôt que de déborder silencieusement.
        let timestamp_ms = u32::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u32::MAX);
        let frame_len = jpeg_bytes.len() as u32;

        self.file.write_all(&timestamp_ms.to_le_bytes())?;
        self.file.write_all(&frame_len.to_le_bytes())?;
        self.file.write_all(jpeg_bytes)?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_in_creates_the_directory_and_a_mjpeg_file() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let _writer = RecordingWriter::create_in(dir.path()).expect("création de l'enregistrement");

        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .expect("lecture du dossier")
            .filter_map(|e| e.ok())
            .collect();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].file_name().to_string_lossy().ends_with(".mjpeg"));
    }

    #[test]
    fn write_frame_uses_the_documented_binary_format() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path()).expect("création de l'enregistrement");

        let frame = vec![0xFFu8, 0xD8, 0xAA, 0xBB, 0xCC];
        writer.write_frame(&frame).expect("écriture de la frame");
        drop(writer);

        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let raw = std::fs::read(&path).expect("lecture du fichier d'enregistrement");

        // [u32 timestamp_ms][u32 frame_len][frame_len octets de JPEG]
        assert_eq!(raw.len(), 4 + 4 + frame.len());

        let frame_len = u32::from_le_bytes(raw[4..8].try_into().unwrap());
        assert_eq!(frame_len as usize, frame.len());
        assert_eq!(&raw[8..], frame.as_slice());
    }

    #[test]
    fn write_frame_appends_successive_frames_sequentially() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path()).expect("création de l'enregistrement");

        writer.write_frame(&[1, 2, 3]).unwrap();
        writer.write_frame(&[4, 5]).unwrap();
        drop(writer);

        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let raw = std::fs::read(&path).unwrap();

        // Frame 1 : en-tête (8 octets) + 3 octets de payload.
        // Frame 2 : en-tête (8 octets) + 2 octets de payload, juste après.
        assert_eq!(raw.len(), (8 + 3) + (8 + 2));

        let frame1_len = u32::from_le_bytes(raw[4..8].try_into().unwrap());
        assert_eq!(frame1_len, 3);
        assert_eq!(&raw[8..11], &[1, 2, 3]);

        let frame2_len = u32::from_le_bytes(raw[15..19].try_into().unwrap());
        assert_eq!(frame2_len, 2);
        assert_eq!(&raw[19..21], &[4, 5]);
    }
}
