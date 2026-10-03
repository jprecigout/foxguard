//! Écriture des enregistrements vidéo (`output_record/`).
//!
//! # Deux formats, et pourquoi
//!
//! - **MP4 fragmenté** (`.mp4`) dès que l'encodage H.264 est actif. C'est le
//!   format par défaut d'une installation moderne : dix fois plus léger que
//!   l'autre, lisible par un `<video>` de navigateur, par VLC, par ffmpeg,
//!   par n'importe quel outil d'archivage. Voir [`crate::mp4`].
//!
//! - **Images JPEG horodatées** (`.mjpeg`) sinon. C'est le format
//!   historique, et il reste le seul possible quand `[h264] enabled = false` :
//!   sans encodeur, il n'y a pas de flux H.264 à écrire.
//!
//! Le format d'un fichier se lit donc à son extension, et les deux cohabitent
//! dans le même dossier — une caméra mise à jour continue de servir et de
//! purger les enregistrements qu'elle a écrits avant.
//!
//! # Le format historique
//!
//! Chaque frame JPEG y est précédée d'un horodatage et de sa longueur, ce qui
//! permet à la lecture de restituer l'enregistrement à la vitesse RÉELLE de
//! capture plutôt qu'à un débit fixe supposé (celui-ci varie, par exemple en
//! fonction de la luminosité pour une webcam UVC).
//!
//! Format binaire par frame (little-endian) :
//! `[u32 timestamp_ms][u32 frame_len][frame_len octets de JPEG]`
//! `timestamp_ms` est le temps écoulé depuis le début de CET enregistrement
//! (horloge relative, pas un horodatage absolu).
//!
//! Le MP4, lui, porte ses durées par image : la même fidélité, dans un
//! format que tout le monde sait lire.

use std::fs::File;
use std::io::{self, Write};
use std::path::Path;
use std::time::Instant;

use chrono::Local;
use tracing::info;

use crate::h264::{AccessUnit, NAL_PPS, NAL_SPS, nal_type};
use crate::mp4::{Fmp4Writer, TIMESCALE};

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

/// Une frame à enregistrer.
///
/// Les deux variantes ne sont pas interchangeables : un fichier a un format,
/// et lui soumettre l'autre est une erreur de programmation que
/// [`RecordingWriter::write_frame_at`] signale plutôt que d'écrire un fichier
/// corrompu.
#[derive(Debug, Clone, Copy)]
pub(super) enum Frame<'a> {
    /// Une image JPEG complète.
    Jpeg(&'a [u8]),
    /// Une unité d'accès H.264, NAL sans préfixe de délimitation.
    H264 { nals: &'a [Vec<u8>], keyframe: bool },
}

/// Le format dans lequel ouvrir un enregistrement, et ce qu'il faut pour
/// l'ouvrir.
///
/// Les jeux de paramètres H.264 en font partie : le segment d'initialisation
/// d'un MP4 les contient, et il est écrit AVANT la première image. Un
/// enregistrement en MP4 ne peut donc commencer qu'une fois une image clé
/// produite — ce qui tombe bien, puisque c'est de toute façon la seule frame
/// sur laquelle un décodeur peut démarrer.
#[derive(Debug, Clone)]
pub enum RecordingFormat {
    /// Images JPEG horodatées (format historique).
    Mjpeg,
    /// MP4 fragmenté contenant le flux H.264.
    Mp4 {
        width: u32,
        height: u32,
        fps: u32,
        sps: Vec<u8>,
        pps: Vec<u8>,
    },
}

impl RecordingFormat {
    /// Déduit le format MP4 d'une image clé, ou `None` si elle ne porte pas
    /// ses jeux de paramètres.
    pub fn from_keyframe(unit: &AccessUnit, width: u32, height: u32, fps: u32) -> Option<Self> {
        if !unit.keyframe {
            return None;
        }

        let sps = unit
            .nals
            .iter()
            .find(|nal| nal_type(nal) == Some(NAL_SPS))?;
        let pps = unit
            .nals
            .iter()
            .find(|nal| nal_type(nal) == Some(NAL_PPS))?;

        Some(Self::Mp4 {
            width,
            height,
            fps,
            sps: sps.clone(),
            pps: pps.clone(),
        })
    }

    /// Extension des fichiers de ce format.
    fn extension(&self) -> &'static str {
        match self {
            Self::Mjpeg => "mjpeg",
            Self::Mp4 { .. } => "mp4",
        }
    }
}

/// Écrivain d'un fichier d'enregistrement
/// `output_record/<préfixe>_<horodatage>.<extension>`.
pub(super) struct RecordingWriter {
    /// Début de l'enregistrement, origine de tous ses horodatages.
    ///
    /// Porté ICI et non dans chaque format : les deux comptent le temps de la
    /// même façon, et c'est cette horloge unique qui garantit qu'ils le
    /// comptent vraiment. L'avoir dupliquée a déjà coûté un bogue — le format
    /// MP4 n'avait pas la sienne, et datait toutes ses images à zéro : une
    /// heure d'enregistrement s'y déclarait comme une seconde, sans que rien
    /// ne le signale.
    started_at: Instant,
    sink: Sink,
}

/// Le format d'un enregistrement ouvert.
enum Sink {
    Mjpeg(MjpegWriter),
    Mp4(Fmp4Writer),
}

impl RecordingWriter {
    /// Crée un nouvel enregistrement continu dans `dir` (créé si besoin).
    ///
    /// Le dossier vient de la configuration (`[recording] dir`) et non d'une
    /// constante : c'est ce qui permet de le placer ailleurs qu'à côté du
    /// binaire, et aux tests de travailler dans un dossier temporaire.
    pub(super) fn create(dir: &str, format: &RecordingFormat) -> io::Result<Self> {
        Self::create_in(Path::new(dir), CONTINUOUS_PREFIX, format, true)
    }

    /// Crée un clip d'événement dans `dir`.
    ///
    /// Silencieux, contrairement à [`Self::create`] : les clips sont
    /// déclenchés par les détections, et une ligne de journal par détection
    /// doublerait inutilement celle de l'événement lui-même.
    pub(super) fn create_event_clip(dir: &str, format: &RecordingFormat) -> io::Result<Self> {
        Self::create_in(Path::new(dir), EVENT_PREFIX, format, false)
    }

    /// Crée un enregistrement à partir d'un [`Path`] : sert aussi aux tests,
    /// qui travaillent dans un dossier temporaire.
    fn create_in(
        dir: &Path,
        prefix: &str,
        format: &RecordingFormat,
        announce: bool,
    ) -> io::Result<Self> {
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
                format!("{stem}.{}", format.extension())
            } else {
                format!("{stem}-{attempt}.{}", format.extension())
            };

            let filename = dir.join(&name);

            let sink = match format {
                RecordingFormat::Mjpeg => File::options()
                    .write(true)
                    .create_new(true)
                    .open(&filename)
                    .map(|file| {
                        Sink::Mjpeg(MjpegWriter {
                            file,
                            name: name.clone(),
                        })
                    }),

                RecordingFormat::Mp4 {
                    width,
                    height,
                    fps,
                    sps,
                    pps,
                } => Fmp4Writer::create(&filename, name.clone(), *width, *height, *fps, sps, pps)
                    .map(Sink::Mp4),
            };

            match sink {
                Ok(sink) => {
                    if announce {
                        info!("💾 Début d'enregistrement : {}", filename.display());
                    }

                    return Ok(Self {
                        started_at: Instant::now(),
                        sink,
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
        match &self.sink {
            Sink::Mjpeg(writer) => &writer.name,
            Sink::Mp4(writer) => writer.name(),
        }
    }

    /// Ajoute une frame, horodatée au moment de l'écriture.
    ///
    /// C'est le chemin de l'enregistrement CONTINU, où les frames arrivent au
    /// fil de la capture : leur écart réel est donc celui de l'horloge.
    pub(super) fn write_frame(&mut self, frame: Frame<'_>) -> io::Result<()> {
        // Saturating : au-delà de ~49 jours (u32::MAX ms) d'enregistrement
        // continu, l'horodatage plafonne plutôt que de déborder silencieusement.
        let elapsed = u32::try_from(self.started_at.elapsed().as_millis()).unwrap_or(u32::MAX);

        self.write_frame_at(elapsed, frame)
    }

    /// Ajoute une frame avec un horodatage FOURNI, en millisecondes depuis le
    /// début de l'enregistrement.
    ///
    /// Indispensable aux clips d'événement : leurs premières frames viennent
    /// d'un tampon circulaire (voir [`super::clips`]) et sont donc écrites
    /// d'un bloc, en quelques millisecondes, alors qu'elles couvrent
    /// plusieurs secondes de vidéo. Mesurées à l'écriture, elles
    /// porteraient toutes un horodatage proche de zéro et la relecture
    /// avalerait tout le pré-enregistrement d'un coup.
    pub(super) fn write_frame_at(&mut self, timestamp_ms: u32, frame: Frame<'_>) -> io::Result<()> {
        match (&mut self.sink, frame) {
            (Sink::Mjpeg(writer), Frame::Jpeg(jpeg)) => writer.write_frame(timestamp_ms, jpeg),

            (Sink::Mp4(writer), Frame::H264 { nals, keyframe }) => {
                // Millisecondes vers l'horloge du fichier (90 kHz).
                let timestamp = u64::from(timestamp_ms) * u64::from(TIMESCALE / 1_000);
                writer.write_frame(nals, timestamp, keyframe)
            }

            // Un fichier a UN format : lui soumettre l'autre produirait des
            // octets que rien ne saurait relire. Mieux vaut une erreur visible
            // qu'un enregistrement silencieusement corrompu.
            (sink, _) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "frame d'un format incompatible avec l'enregistrement « {} »",
                    match sink {
                        Sink::Mjpeg(writer) => writer.name.as_str(),
                        Sink::Mp4(writer) => writer.name(),
                    }
                ),
            )),
        }
    }

    /// Referme l'enregistrement.
    ///
    /// À appeler explicitement plutôt que de laisser tomber la valeur : le
    /// MP4 doit encore écrire son dernier fragment et sa durée totale, et
    /// `Drop` ne peut pas signaler l'échec de ces écritures.
    pub(super) fn finish(self) -> io::Result<()> {
        match self.sink {
            Sink::Mjpeg(mut writer) => writer.file.flush(),
            Sink::Mp4(writer) => writer.finish(),
        }
    }
}

/// Écrivain du format historique : des images JPEG horodatées.
pub(super) struct MjpegWriter {
    file: File,
    /// Nom du fichier (sans son dossier), tel qu'il sera demandé par la route
    /// `GET /recordings/{filename}`.
    name: String,
}

impl MjpegWriter {
    /// Ajoute une frame JPEG, précédée de son horodatage et de sa longueur
    /// (voir le format en tête de module).
    fn write_frame(&mut self, timestamp_ms: u32, jpeg_bytes: &[u8]) -> io::Result<()> {
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
    use std::time::Duration;

    const SPS: [u8; 4] = [0x67, 0x42, 0xC0, 0x1E];
    const PPS: [u8; 2] = [0x68, 0xCE];

    fn mp4_format() -> RecordingFormat {
        RecordingFormat::Mp4 {
            width: 64,
            height: 48,
            fps: 12,
            sps: SPS.to_vec(),
            pps: PPS.to_vec(),
        }
    }

    /// Une unité d'accès H.264 plausible.
    fn access_unit(keyframe: bool) -> AccessUnit {
        let mut nals = Vec::new();

        if keyframe {
            nals.push(SPS.to_vec());
            nals.push(PPS.to_vec());
        }

        nals.push(vec![if keyframe { 0x65 } else { 0x41 }, 1, 2, 3]);

        AccessUnit {
            nals,
            keyframe,
            rtp_timestamp: 0,
        }
    }

    fn h264_frame(unit: &AccessUnit) -> Frame<'_> {
        Frame::H264 {
            nals: &unit.nals,
            keyframe: unit.keyframe,
        }
    }

    fn only_file(dir: &Path) -> std::path::PathBuf {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("lecture du dossier")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 1, "{entries:?}");
        entries.pop().unwrap()
    }

    // --- Nommage et création ---

    #[test]
    fn the_extension_announces_the_format() {
        // C'est à son extension qu'on reconnaît le format d'un
        // enregistrement : les deux cohabitent dans le même dossier, et
        // l'interface choisit son lecteur d'après elle.
        let dir = tempfile::tempdir().expect("dossier temporaire");

        let mjpeg = RecordingWriter::create_in(
            dir.path(),
            CONTINUOUS_PREFIX,
            &RecordingFormat::Mjpeg,
            false,
        )
        .expect("mjpeg");
        assert!(mjpeg.name().ends_with(".mjpeg"), "{}", mjpeg.name());

        let mp4 = RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, &mp4_format(), false)
            .expect("mp4");
        assert!(mp4.name().ends_with(".mp4"), "{}", mp4.name());
    }

    #[test]
    fn an_event_clip_is_named_distinctly_from_a_continuous_recording() {
        // Les deux cohabitent avec le même format et la même rétention : le
        // préfixe est le seul moyen de les distinguer.
        let dir = tempfile::tempdir().expect("dossier temporaire");

        let clip = RecordingWriter::create_event_clip(
            dir.path().to_str().unwrap(),
            &RecordingFormat::Mjpeg,
        )
        .expect("création du clip");

        assert!(clip.name().starts_with("evt_"), "{}", clip.name());
    }

    #[test]
    fn clips_created_back_to_back_never_share_a_file() {
        // Deux détections coup sur coup. `File::create` aurait TRONQUÉ le
        // premier fichier, et la première vidéo serait perdue sans un mot.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let path = dir.path().to_str().unwrap();

        let names: Vec<String> = (0..5)
            .map(|_| {
                RecordingWriter::create_event_clip(path, &RecordingFormat::Mjpeg)
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

        let mut first =
            RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &RecordingFormat::Mjpeg, false)
                .expect("premier enregistrement");
        first
            .write_frame(Frame::Jpeg(&[1, 2, 3, 4]))
            .expect("écriture");
        let first_path = dir.path().join(first.name());
        first.finish().expect("fermeture");

        let before = std::fs::metadata(&first_path).expect("métadonnées").len();

        let _second =
            RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &RecordingFormat::Mjpeg, false)
                .expect("second enregistrement");

        let after = std::fs::metadata(&first_path).expect("métadonnées").len();
        assert_eq!(before, after, "le premier fichier a été tronqué");
    }

    // --- Format historique ---

    #[test]
    fn write_frame_uses_the_documented_binary_format() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer = RecordingWriter::create_in(
            dir.path(),
            CONTINUOUS_PREFIX,
            &RecordingFormat::Mjpeg,
            false,
        )
        .expect("création");

        let frame = vec![0xFFu8, 0xD8, 0xAA, 0xBB, 0xCC];
        writer.write_frame(Frame::Jpeg(&frame)).expect("écriture");
        writer.finish().expect("fermeture");

        let raw = std::fs::read(only_file(dir.path())).expect("lecture");

        // [u32 timestamp_ms][u32 frame_len][frame_len octets de JPEG]
        assert_eq!(raw.len(), 4 + 4 + frame.len());
        assert_eq!(
            u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize,
            frame.len()
        );
        assert_eq!(&raw[8..], frame.as_slice());
    }

    #[test]
    fn write_frame_at_records_the_timestamp_it_is_given() {
        // C'est ce qui permet au pré-enregistrement d'un clip de garder son
        // cadencement réel bien qu'il soit écrit d'un bloc.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &RecordingFormat::Mjpeg, false)
                .expect("création");

        writer
            .write_frame_at(4_321, Frame::Jpeg(&[1, 2, 3]))
            .expect("écriture");
        writer.finish().expect("fermeture");

        let raw = std::fs::read(only_file(dir.path())).expect("lecture");
        assert_eq!(u32::from_le_bytes(raw[0..4].try_into().unwrap()), 4_321);
    }

    #[test]
    fn write_frame_appends_successive_frames_sequentially() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer = RecordingWriter::create_in(
            dir.path(),
            CONTINUOUS_PREFIX,
            &RecordingFormat::Mjpeg,
            false,
        )
        .expect("création");

        writer.write_frame(Frame::Jpeg(&[1, 2, 3])).unwrap();
        writer.write_frame(Frame::Jpeg(&[4, 5])).unwrap();
        writer.finish().expect("fermeture");

        let raw = std::fs::read(only_file(dir.path())).expect("lecture");

        assert_eq!(raw.len(), (8 + 3) + (8 + 2));
        assert_eq!(u32::from_le_bytes(raw[4..8].try_into().unwrap()), 3);
        assert_eq!(&raw[8..11], &[1, 2, 3]);
        assert_eq!(u32::from_le_bytes(raw[15..19].try_into().unwrap()), 2);
        assert_eq!(&raw[19..21], &[4, 5]);
    }

    // --- MP4 ---

    #[test]
    fn an_mp4_recording_is_a_real_mp4() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, &mp4_format(), false)
                .expect("création");

        let key = access_unit(true);
        let delta = access_unit(false);
        writer
            .write_frame_at(0, h264_frame(&key))
            .expect("image clé");
        writer
            .write_frame_at(80, h264_frame(&delta))
            .expect("image intermédiaire");
        writer.finish().expect("fermeture");

        let data = std::fs::read(only_file(dir.path())).expect("lecture");

        assert_eq!(&data[4..8], b"ftyp");
        assert!(data.windows(4).any(|w| w == b"moov"));
        assert!(data.windows(4).any(|w| w == b"moof"));
    }

    #[test]
    fn milliseconds_are_converted_to_the_file_clock() {
        // Le fichier compte en 90 kHz, l'appelant en millisecondes : une
        // conversion manquante donnerait une vidéo jouée 90 fois trop vite.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &mp4_format(), false)
            .expect("création");

        let key = access_unit(true);
        let delta = access_unit(false);
        writer.write_frame_at(0, h264_frame(&key)).unwrap();
        // 100 ms plus tard.
        writer.write_frame_at(100, h264_frame(&delta)).unwrap();
        writer.finish().expect("fermeture");

        let data = std::fs::read(only_file(dir.path())).expect("lecture");

        // La durée de la première image doit valoir 100 ms en horloge du
        // fichier, soit 9000 unités.
        let expected = (100 * TIMESCALE / 1_000).to_be_bytes();
        assert!(
            data.windows(4).any(|w| w == expected),
            "aucune durée de {} unités dans le fichier",
            100 * TIMESCALE / 1_000
        );
    }

    #[test]
    fn write_frame_advances_the_timeline_of_an_mp4_recording() {
        // RÉGRESSION : le format MP4 n'avait pas d'horloge propre et datait
        // toutes ses images à zéro. Les durées se repliaient sur une unité par
        // image, et quarante-huit secondes d'enregistrement se déclaraient
        // comme une seconde — sans qu'aucun test ne le voie, puisqu'ils
        // passaient tous par `write_frame_at`.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, &mp4_format(), false)
                .expect("création");

        let key = access_unit(true);
        let delta = access_unit(false);

        writer.write_frame(h264_frame(&key)).expect("image clé");
        std::thread::sleep(Duration::from_millis(60));
        writer
            .write_frame(h264_frame(&delta))
            .expect("intermédiaire");
        std::thread::sleep(Duration::from_millis(60));
        writer
            .write_frame(h264_frame(&delta))
            .expect("intermédiaire");
        writer.finish().expect("fermeture");

        let data = std::fs::read(only_file(dir.path())).expect("lecture");
        let duration = movie_duration(&data);

        // Deux intervalles d'au moins 60 ms, plus la durée nominale de la
        // dernière image : largement au-dessus des trois unités que donnait
        // l'horloge figée.
        assert!(
            duration > u64::from(TIMESCALE) / 10,
            "durée déclarée de {duration} unités pour ~120 ms d'enregistrement"
        );
    }

    #[test]
    fn write_frame_advances_the_timeline_of_a_legacy_recording() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer = RecordingWriter::create_in(
            dir.path(),
            CONTINUOUS_PREFIX,
            &RecordingFormat::Mjpeg,
            false,
        )
        .expect("création");

        writer.write_frame(Frame::Jpeg(&[1])).expect("écriture");
        std::thread::sleep(Duration::from_millis(60));
        writer.write_frame(Frame::Jpeg(&[2])).expect("écriture");
        writer.finish().expect("fermeture");

        let raw = std::fs::read(only_file(dir.path())).expect("lecture");
        let second = u32::from_le_bytes(raw[9..13].try_into().unwrap());

        assert!(second >= 50, "horodatage de la seconde image : {second} ms");
    }

    /// Durée du film déclarée par le `mvhd` d'un MP4.
    ///
    /// Version 1 : après l'en-tête (8), version et drapeaux (4), les dates
    /// (16) et l'échelle de temps (4).
    fn movie_duration(data: &[u8]) -> u64 {
        let at = data
            .windows(4)
            .position(|w| w == b"mvhd")
            .expect("boîte mvhd")
            - 4
            + 8
            + 4
            + 16
            + 4;

        u64::from_be_bytes(data[at..at + 8].try_into().unwrap())
    }

    // --- Cohérence des formats ---

    #[test]
    fn a_jpeg_frame_is_refused_by_an_mp4_recording() {
        // Un fichier a UN format : lui soumettre l'autre produirait des
        // octets que rien ne saurait relire. Mieux vaut une erreur visible
        // qu'un enregistrement silencieusement corrompu.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &mp4_format(), false)
            .expect("création");

        let error = writer
            .write_frame(Frame::Jpeg(&[0xFF, 0xD8]))
            .expect_err("un JPEG n'a rien à faire dans un MP4");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn an_h264_frame_is_refused_by_a_legacy_recording() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &RecordingFormat::Mjpeg, false)
                .expect("création");

        let unit = access_unit(true);
        let error = writer
            .write_frame(h264_frame(&unit))
            .expect_err("du H.264 n'a rien à faire dans un fichier d'images JPEG");

        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    // --- Détection du format depuis une image clé ---

    #[test]
    fn the_format_is_deduced_from_a_keyframe() {
        let unit = access_unit(true);
        let format = RecordingFormat::from_keyframe(&unit, 640, 480, 12).expect("format");

        match format {
            RecordingFormat::Mp4 {
                width,
                height,
                fps,
                sps,
                pps,
            } => {
                assert_eq!((width, height, fps), (640, 480, 12));
                assert_eq!(sps, SPS);
                assert_eq!(pps, PPS);
            }
            RecordingFormat::Mjpeg => panic!("format MP4 attendu"),
        }
    }

    #[test]
    fn an_intermediate_frame_does_not_define_a_format() {
        // Elle ne porte pas de jeux de paramètres : un MP4 ouvert sur elle
        // serait illisible.
        let unit = access_unit(false);
        assert!(RecordingFormat::from_keyframe(&unit, 640, 480, 12).is_none());
    }

    #[test]
    fn a_keyframe_without_parameter_sets_does_not_define_a_format() {
        let unit = AccessUnit {
            nals: vec![vec![0x65, 1, 2, 3]],
            keyframe: true,
            rtp_timestamp: 0,
        };

        assert!(RecordingFormat::from_keyframe(&unit, 640, 480, 12).is_none());
    }
}
