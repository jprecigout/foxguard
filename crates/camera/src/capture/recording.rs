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
use tracing::info;

/// Préfixe des enregistrements continus, déclenchés à la main depuis
/// l'interface de la caméra.
const CONTINUOUS_PREFIX: &str = "rec";

/// Préfixe des clips d'événement, déclenchés par une détection (voir
/// [`super::clips`]).
///
/// Un préfixe distinct pour que les deux soient reconnaissables d'un coup
/// d'œil dans `output_record/` : ils ont le même format et la même rétention,
/// mais pas du tout la même raison d'être.
pub(super) const EVENT_PREFIX: &str = "evt";

/// Écrivain d'un fichier d'enregistrement `output_record/<préfixe>_<horodatage>.mjpeg`.
pub(super) struct RecordingWriter {
    file: File,
    started_at: Instant,
    /// Nom du fichier (sans son dossier), tel qu'il sera demandé par la route
    /// `GET /recordings/{filename}`.
    name: String,
}

impl RecordingWriter {
    /// Crée un nouvel enregistrement continu dans `dir` (créé si besoin) et
    /// démarre son horloge interne (voir [`Self::write_frame`]).
    ///
    /// Le dossier vient de la configuration (`[recording] dir`) et non d'une
    /// constante : c'est ce qui permet de le placer ailleurs qu'à côté du
    /// binaire, et aux tests de travailler dans un dossier temporaire.
    pub(super) fn create(dir: &str) -> io::Result<Self> {
        Self::create_in(Path::new(dir), CONTINUOUS_PREFIX, true)
    }

    /// Crée un clip d'événement dans `dir`.
    ///
    /// Silencieux, contrairement à [`Self::create`] : les clips sont
    /// déclenchés par les détections, et une ligne de journal par détection
    /// doublerait inutilement celle de l'événement lui-même.
    pub(super) fn create_event_clip(dir: &str) -> io::Result<Self> {
        Self::create_in(Path::new(dir), EVENT_PREFIX, false)
    }

    /// Crée un enregistrement à partir d'un [`Path`] : sert aussi aux tests,
    /// qui travaillent dans un dossier temporaire.
    fn create_in(dir: &Path, prefix: &str, announce: bool) -> io::Result<Self> {
        std::fs::create_dir_all(dir)?;

        // Les millisecondes font partie du nom : deux clips déclenchés dans
        // la même seconde (deux personnes détectées coup sur coup)
        // écriraient sinon dans le même fichier.
        let stem = format!("{prefix}_{}", Local::now().format("%Y%m%d_%H%M%S%3f"));

        // `create_new` et non `create` : ce dernier TRONQUE un fichier
        // existant. Deux enregistrements dont le nom coïncide — même
        // milliseconde, ou horloge système qui a reculé — verraient le
        // second effacer le premier, silencieusement. Mieux vaut décaler le
        // nom que perdre une vidéo.
        for attempt in 0..100u32 {
            let name = if attempt == 0 {
                format!("{stem}.mjpeg")
            } else {
                format!("{stem}-{attempt}.mjpeg")
            };

            let filename = dir.join(&name);

            match File::options().write(true).create_new(true).open(&filename) {
                Ok(file) => {
                    if announce {
                        info!("💾 Début d'enregistrement : {}", filename.display());
                    }

                    return Ok(Self {
                        file,
                        started_at: Instant::now(),
                        name,
                    });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("aucun nom libre pour un enregistrement « {stem} »"),
        ))
    }

    /// Nom du fichier, tel qu'attendu par la route `GET /recordings/{file}`.
    pub(super) fn name(&self) -> &str {
        &self.name
    }

    /// Ajoute une frame JPEG à l'enregistrement, précédée de son horodatage
    /// (ms écoulées depuis le début de l'enregistrement) et de sa longueur
    /// (voir le format en tête de module).
    pub(super) fn write_frame(&mut self, jpeg_bytes: &[u8]) -> io::Result<()> {
        // Saturating : au-delà de ~49 jours (u32::MAX ms) d'enregistrement
        // continu, l'horodatage plafonne plutôt que de déborder silencieusement.
        let timestamp_ms = u32::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u32::MAX);

        self.write_frame_at(timestamp_ms, jpeg_bytes)
    }

    /// Ajoute une frame avec un horodatage FOURNI, et non mesuré à l'instant
    /// de l'écriture.
    ///
    /// Indispensable aux clips d'événement : leurs premières frames viennent
    /// d'un tampon circulaire (voir [`super::clips`]) et sont donc écrites
    /// d'un bloc, en quelques millisecondes, alors qu'elles couvrent
    /// plusieurs secondes de vidéo. Mesurées à l'écriture, elles
    /// porteraient toutes un horodatage proche de zéro et la relecture
    /// avalerait tout le pré-enregistrement d'un coup.
    pub(super) fn write_frame_at(
        &mut self,
        timestamp_ms: u32,
        jpeg_bytes: &[u8],
    ) -> io::Result<()> {
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
        let _writer = RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, false)
            .expect("création de l'enregistrement");

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
        let mut writer = RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, false)
            .expect("création de l'enregistrement");

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
    fn an_event_clip_is_named_distinctly_from_a_continuous_recording() {
        // Les deux cohabitent dans `output_record/` avec le même format et la
        // même rétention : le préfixe est le seul moyen de les distinguer.
        let dir = tempfile::tempdir().expect("dossier temporaire");

        let clip = RecordingWriter::create_event_clip(dir.path().to_str().unwrap())
            .expect("création du clip");

        assert!(clip.name().starts_with("evt_"), "{}", clip.name());
        assert!(clip.name().ends_with(".mjpeg"), "{}", clip.name());
    }

    #[test]
    fn clips_created_back_to_back_never_share_a_file() {
        // Deux détections coup sur coup. `File::create` aurait TRONQUÉ le
        // premier fichier, et la première vidéo serait perdue sans un mot.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let path = dir.path().to_str().unwrap();

        let names: Vec<String> = (0..5)
            .map(|_| {
                RecordingWriter::create_event_clip(path)
                    .expect("clip")
                    .name()
                    .to_string()
            })
            .collect();

        let distinct: std::collections::HashSet<&String> = names.iter().collect();
        assert_eq!(distinct.len(), names.len(), "{names:?}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), names.len());
    }

    #[test]
    fn an_existing_recording_is_never_truncated() {
        let dir = tempfile::tempdir().expect("dossier temporaire");

        let mut first = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, false)
            .expect("premier enregistrement");
        first.write_frame(&[1, 2, 3, 4]).expect("écriture");
        let first_path = dir.path().join(first.name());
        drop(first);

        let before = std::fs::metadata(&first_path).expect("métadonnées").len();

        let _second = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, false)
            .expect("second enregistrement");

        let after = std::fs::metadata(&first_path).expect("métadonnées").len();
        assert_eq!(before, after, "le premier fichier a été tronqué");
    }

    #[test]
    fn write_frame_at_records_the_timestamp_it_is_given() {
        // C'est ce qui permet au pré-enregistrement d'un clip de garder son
        // cadencement réel bien qu'il soit écrit d'un bloc.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, false)
            .expect("création de l'enregistrement");

        writer.write_frame_at(4_321, &[1, 2, 3]).expect("écriture");
        drop(writer);

        let path = std::fs::read_dir(dir.path())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let raw = std::fs::read(&path).unwrap();

        assert_eq!(u32::from_le_bytes(raw[0..4].try_into().unwrap()), 4_321);
    }

    #[test]
    fn write_frame_appends_successive_frames_sequentially() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer = RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, false)
            .expect("création de l'enregistrement");

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
