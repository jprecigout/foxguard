//! Écriture des enregistrements vidéo (`output_record/`) en **MP4 fragmenté**
//! (voir [`crate::mp4`]).
//!
//! # Un seul format
//!
//! Tout ce qui est enregistré l'est en `.mp4` contenant le flux H.264 de la
//! caméra. Il n'y a plus de second format : le H.264 est le seul chemin vidéo,
//! donc le seul qu'il y ait à écrire.
//!
//! Le format d'images JPEG horodatées (`.mjpeg`) qui a précédé était un format
//! MAISON — des JPEG concaténés, chacun préfixé de son horodatage et de sa
//! longueur. Il avait une qualité, celle de porter la durée réelle de chaque
//! image, et deux défauts qui l'ont emporté : une vingtaine de fois plus
//! lourd, et illisible par tout autre logiciel que la page qui savait le
//! décoder. Le MP4 garde la qualité et perd les défauts.
//!
//! La purge, elle, continue de reconnaître l'extension `.mjpeg` (voir
//! [`crate::retention`]) : sans ça, les fichiers d'une caméra mise à jour
//! s'accumuleraient sans jamais expirer.
//!
//! # Les durées restent réelles
//!
//! La cadence d'une caméra n'est pas constante — une webcam UVC la réduit en
//! basse luminosité. Chaque image porte donc sa durée MESURÉE, et non une
//! cadence supposée : c'est [`RecordingWriter::write_frame`] qui l'horodate à
//! l'écriture, ou [`RecordingWriter::write_frame_at`] quand l'appelant connaît
//! mieux que l'horloge le moment d'une image (voir [`super::clips`]).

use std::io;
use std::path::Path;
use std::time::Instant;

use chrono::Local;
use tracing::info;

use crate::h264::{AccessUnit, NAL_PPS, NAL_SPS, nal_type};
use crate::mp4::{Fmp4Writer, TIMESCALE};

/// Extension des enregistrements produits.
const EXTENSION: &str = "mp4";

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

/// Une frame à enregistrer : une unité d'accès H.264, NAL sans préfixe de
/// délimitation.
#[derive(Debug, Clone, Copy)]
pub(super) struct Frame<'a> {
    pub(super) nals: &'a [Vec<u8>],
    pub(super) keyframe: bool,
}

/// Ce qu'il faut connaître pour OUVRIR un enregistrement.
///
/// Les jeux de paramètres H.264 en font partie : le segment d'initialisation
/// d'un MP4 les contient, et il est écrit AVANT la première image. Un
/// enregistrement ne peut donc commencer qu'une fois une image clé produite —
/// ce qui tombe bien, puisque c'est de toute façon la seule frame sur laquelle
/// un décodeur peut démarrer.
#[derive(Debug, Clone)]
pub struct RecordingFormat {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

impl RecordingFormat {
    /// Déduit le format d'une image clé, ou `None` si elle ne porte pas ses
    /// jeux de paramètres.
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

        Some(Self {
            width,
            height,
            fps,
            sps: sps.clone(),
            pps: pps.clone(),
        })
    }
}

/// Écrivain d'un fichier d'enregistrement
/// `output_record/<préfixe>_<horodatage>.mp4`.
pub(super) struct RecordingWriter {
    /// Début de l'enregistrement, origine de tous ses horodatages.
    ///
    /// Avoir laissé le format MP4 sans horloge propre a coûté un bogue : il
    /// datait toutes ses images à zéro, et une heure d'enregistrement s'y
    /// déclarait comme une seconde, sans que rien ne le signale.
    started_at: Instant,
    writer: Fmp4Writer,
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
                format!("{stem}.{EXTENSION}")
            } else {
                format!("{stem}-{attempt}.{EXTENSION}")
            };

            let filename = dir.join(&name);

            let created = Fmp4Writer::create(
                &filename,
                name.clone(),
                format.width,
                format.height,
                format.fps,
                &format.sps,
                &format.pps,
            );

            match created {
                Ok(writer) => {
                    if announce {
                        info!("💾 Début d'enregistrement : {}", filename.display());
                    }

                    return Ok(Self {
                        started_at: Instant::now(),
                        writer,
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
        self.writer.name()
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
        // Millisecondes vers l'horloge du fichier (90 kHz).
        let timestamp = u64::from(timestamp_ms) * u64::from(TIMESCALE / 1_000);

        self.writer
            .write_frame(frame.nals, timestamp, frame.keyframe)
    }

    /// Referme l'enregistrement.
    ///
    /// À appeler explicitement plutôt que de laisser tomber la valeur : le
    /// fichier doit encore écrire son dernier fragment et sa durée totale, et
    /// `Drop` ne peut pas signaler l'échec de ces écritures.
    pub(super) fn finish(self) -> io::Result<()> {
        self.writer.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const SPS: [u8; 4] = [0x67, 0x42, 0xC0, 0x1E];
    const PPS: [u8; 2] = [0x68, 0xCE];

    fn format() -> RecordingFormat {
        RecordingFormat {
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

    fn frame_of(unit: &AccessUnit) -> Frame<'_> {
        Frame {
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
    fn a_recording_is_named_with_the_mp4_extension() {
        // L'interface choisit son lecteur d'après l'extension, et la purge
        // décide d'après elle ce qui est un enregistrement.
        let dir = tempfile::tempdir().expect("dossier temporaire");

        let writer = RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, &format(), false)
            .expect("création");

        assert!(writer.name().ends_with(".mp4"), "{}", writer.name());
    }

    #[test]
    fn an_event_clip_is_named_distinctly_from_a_continuous_recording() {
        // Les deux cohabitent avec le même format et la même rétention : le
        // préfixe est le seul moyen de les distinguer.
        let dir = tempfile::tempdir().expect("dossier temporaire");

        let clip = RecordingWriter::create_event_clip(dir.path().to_str().unwrap(), &format())
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
                RecordingWriter::create_event_clip(path, &format())
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

        let mut first = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &format(), false)
            .expect("premier enregistrement");
        let key = access_unit(true);
        first.write_frame(frame_of(&key)).expect("écriture");
        let first_path = dir.path().join(first.name());
        first.finish().expect("fermeture");

        let before = std::fs::metadata(&first_path).expect("métadonnées").len();

        let _second = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &format(), false)
            .expect("second enregistrement");

        let after = std::fs::metadata(&first_path).expect("métadonnées").len();
        assert_eq!(before, after, "le premier fichier a été tronqué");
    }

    // --- Contenu ---

    #[test]
    fn a_recording_is_a_real_mp4() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, &format(), false)
                .expect("création");

        let key = access_unit(true);
        let delta = access_unit(false);
        writer.write_frame_at(0, frame_of(&key)).expect("image clé");
        writer
            .write_frame_at(80, frame_of(&delta))
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
        let mut writer = RecordingWriter::create_in(dir.path(), EVENT_PREFIX, &format(), false)
            .expect("création");

        let key = access_unit(true);
        let delta = access_unit(false);
        writer.write_frame_at(0, frame_of(&key)).unwrap();
        // 100 ms plus tard.
        writer.write_frame_at(100, frame_of(&delta)).unwrap();
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
    fn write_frame_advances_the_timeline_of_a_recording() {
        // RÉGRESSION : le format MP4 n'avait pas d'horloge propre et datait
        // toutes ses images à zéro. Les durées se repliaient sur une unité par
        // image, et quarante-huit secondes d'enregistrement se déclaraient
        // comme une seconde — sans qu'aucun test ne le voie, puisqu'ils
        // passaient tous par `write_frame_at`.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut writer =
            RecordingWriter::create_in(dir.path(), CONTINUOUS_PREFIX, &format(), false)
                .expect("création");

        let key = access_unit(true);
        let delta = access_unit(false);

        writer.write_frame(frame_of(&key)).expect("image clé");
        std::thread::sleep(Duration::from_millis(60));
        writer.write_frame(frame_of(&delta)).expect("intermédiaire");
        std::thread::sleep(Duration::from_millis(60));
        writer.write_frame(frame_of(&delta)).expect("intermédiaire");
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

    // --- Détection du format depuis une image clé ---

    #[test]
    fn the_format_is_deduced_from_a_keyframe() {
        let unit = access_unit(true);
        let format = RecordingFormat::from_keyframe(&unit, 640, 480, 12).expect("format");

        assert_eq!((format.width, format.height, format.fps), (640, 480, 12));
        assert_eq!(format.sps, SPS);
        assert_eq!(format.pps, PPS);
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
