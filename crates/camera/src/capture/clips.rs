//! Clips vidéo d'événement : quelques secondes de vidéo autour de chaque
//! détection, pour que la timeline de l'interface du manager puisse offrir un
//! accès direct à ce qui s'est passé.
//!
//! # Pourquoi un clip, et pas l'enregistrement continu
//!
//! La caméra sait déjà enregistrer en continu (voir [`super::recording`]),
//! mais c'est un dispositif manuel, piloté depuis son interface, qui produit
//! des fichiers de plusieurs heures. Une timeline dont chaque entrée renvoie
//! vers un tel fichier, sans position de départ, n'aide personne. Les clips
//! d'événement, eux, sont déclenchés par les détections et bornés : ce sont
//! eux qui rendent la timeline exploitable.
//!
//! # Le pré-enregistrement est l'essentiel
//!
//! Un événement n'est publié qu'une fois la personne reconnue, ou constatée
//! inconnue — soit déjà une à deux secondes après son entrée dans le champ.
//! Un clip qui commencerait à cet instant montrerait quelqu'un déjà au milieu
//! de la scène, et jamais par où il est arrivé.
//!
//! D'où le tampon circulaire : les frames déjà encodées des dernières
//! `clip_pre_secs` secondes sont conservées en mémoire, et versées au début
//! du clip au moment où l'événement survient. Elles y gardent leur
//! horodatage d'origine, faute de quoi la relecture avalerait tout le
//! pré-enregistrement d'un coup.
//!
//! # Le tampon suit le format
//!
//! En **MP4** (H.264), le tampon ne peut pas être coupé n'importe où : une
//! image intermédiaire n'a aucun sens sans l'image clé dont elle décrit les
//! différences. Il est donc tronqué par GROUPES D'IMAGES, à la dernière image
//! clé antérieure à la fenêtre — ce qui donne un pré-enregistrement un peu
//! plus long que demandé, jamais plus court, et toujours décodable.
//!
//! En **MJPEG**, chaque image est autonome : la troncature est exacte.
//!
//! # Format et rétention : ceux des enregistrements existants
//!
//! Un clip est un fichier ordinaire du dossier des enregistrements, dans le
//! même format qu'eux. Il est donc listé, servi, supprimé et purgé par le
//! code existant sans une ligne de plus (voir `crate::api` et
//! `crate::retention`), et relu par le même lecteur côté interface.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::config::RecordingConfig;
use crate::h264::AccessUnit;

use super::recording::{Frame, RecordingFormat, RecordingWriter};

/// Plafond mémoire du tampon de pré-enregistrement, en octets.
///
/// Le tampon est borné par une DURÉE (`clip_pre_secs`), mais le poids d'une
/// frame varie beaucoup avec la scène et la résolution. Ce second plafond
/// garantit que la fonctionnalité ne peut pas, sur une caméra inhabituelle,
/// épuiser la mémoire du Raspberry Pi. Les frames les plus anciennes sont
/// abandonnées en premier : le clip aura un pré-enregistrement plus court, ce
/// qui est très préférable à un plantage.
const MAX_PREROLL_BYTES: usize = 48 * 1024 * 1024;

/// Le contenu d'une frame en attente, selon le format de l'enregistrement.
enum BufferedPayload {
    Jpeg(Vec<u8>),
    H264 { nals: Vec<Vec<u8>>, keyframe: bool },
}

impl BufferedPayload {
    fn len(&self) -> usize {
        match self {
            Self::Jpeg(jpeg) => jpeg.len(),
            Self::H264 { nals, .. } => nals.iter().map(Vec::len).sum(),
        }
    }

    fn is_keyframe(&self) -> bool {
        match self {
            // Une image JPEG est toujours autonome : le tampon peut être
            // coupé devant n'importe laquelle.
            Self::Jpeg(_) => true,
            Self::H264 { keyframe, .. } => *keyframe,
        }
    }

    fn as_frame(&self) -> Frame<'_> {
        match self {
            Self::Jpeg(jpeg) => Frame::Jpeg(jpeg),
            Self::H264 { nals, keyframe } => Frame::H264 {
                nals,
                keyframe: *keyframe,
            },
        }
    }
}

/// Une frame en attente dans le tampon de pré-enregistrement.
struct BufferedFrame {
    captured_at: Instant,
    payload: BufferedPayload,
}

/// Le clip en cours d'écriture.
struct ActiveClip {
    writer: RecordingWriter,
    /// Instant de la PREMIÈRE frame écrite, origine des horodatages du
    /// fichier.
    origin: Instant,
    /// Instant au-delà duquel le clip est refermé.
    until: Instant,
}

/// Enregistreur de clips d'événement.
///
/// Partagé entre la boucle de capture, qui l'alimente à chaque frame
/// ([`Self::push_frame`]), et le thread de reconnaissance, qui déclenche les
/// clips ([`Self::start_or_extend`]).
pub struct ClipRecorder {
    enabled: bool,
    dir: String,
    pre_roll: Duration,
    post_roll: Duration,
    buffer: VecDeque<BufferedFrame>,
    buffered_bytes: usize,
    active: Option<ActiveClip>,
    /// Format des clips à écrire, connu dès que l'encodeur a produit une
    /// image clé (voir [`Self::set_format`]). `None` avant cela en mode
    /// H.264 : on ne peut pas ouvrir de MP4 sans ses jeux de paramètres.
    format: Option<RecordingFormat>,
}

impl ClipRecorder {
    /// Construit l'enregistreur. Le format reste à préciser en H.264 (voir
    /// [`Self::set_format`]) ; sans encodage, il est connu d'emblée.
    pub fn new(config: &RecordingConfig, h264: bool) -> Self {
        Self {
            enabled: config.clips_enabled,
            dir: config.dir.clone(),
            pre_roll: Duration::from_secs(config.clip_pre_secs),
            post_roll: Duration::from_secs(config.clip_post_secs.max(1)),
            buffer: VecDeque::new(),
            buffered_bytes: 0,
            active: None,
            format: (!h264).then_some(RecordingFormat::Mjpeg),
        }
    }

    /// Déclare le format des clips à venir.
    ///
    /// Appelée par la boucle de capture à la première image clé : c'est elle
    /// qui porte les jeux de paramètres dont un MP4 a besoin pour s'ouvrir.
    /// Sans effet si le format est déjà connu — les jeux de paramètres ne
    /// changent pas en cours de route (voir `crate::h264::encoder`).
    pub fn set_format(&mut self, format: RecordingFormat) {
        if self.format.is_none() {
            self.format = Some(format);
        }
    }

    /// Soumet une frame à l'enregistreur : elle rejoint le tampon de
    /// pré-enregistrement et, si un clip est en cours, le fichier.
    ///
    /// Appelée à CHAQUE frame par la boucle de capture, donc tenue de rester
    /// bon marché : elle ne fait qu'une copie et, le cas échéant, une
    /// écriture séquentielle.
    ///
    /// `armed` dit si une détection peut survenir, c'est-à-dire si la
    /// surveillance est active. Quand elle ne l'est pas, aucun événement ne
    /// peut être publié, donc aucun clip ne peut être déclenché : garder
    /// malgré tout plusieurs secondes de frames en mémoire reviendrait à
    /// payer en permanence pour une fonctionnalité en sommeil. Le tampon est
    /// alors vidé — quelques mégaoctets rendus au Raspberry Pi.
    ///
    /// Un clip DÉJÀ en cours continue d'être écrit même après un
    /// désarmement : couper sa vidéo en plein milieu parce que quelqu'un a
    /// éteint la surveillance laisserait un fichier tronqué sans que rien ne
    /// le dise.
    pub fn push_jpeg(&mut self, jpeg: &[u8], armed: bool) {
        self.push_payload(BufferedPayload::Jpeg(jpeg.to_vec()), armed, Instant::now());
    }

    /// Comme [`Self::push_jpeg`], pour une frame encodée en H.264.
    pub fn push_h264(&mut self, unit: &AccessUnit, armed: bool) {
        self.push_payload(
            BufferedPayload::H264 {
                nals: unit.nals.clone(),
                keyframe: unit.keyframe,
            },
            armed,
            Instant::now(),
        );
    }

    /// Cœur commun aux deux, à un instant fourni : c'est ce qui rend les
    /// fenêtres temporelles testables sans faire dormir le test.
    fn push_payload(&mut self, payload: BufferedPayload, armed: bool, now: Instant) {
        if !self.enabled {
            return;
        }

        if let Some(clip) = &mut self.active {
            if now >= clip.until {
                let name = clip.writer.name().to_string();
                debug!("🎞️ Clip d'événement terminé : {name}");

                if let Some(clip) = self.active.take()
                    && let Err(e) = clip.writer.finish()
                {
                    warn!("⚠️ Clip d'événement « {name} » mal refermé : {e}");
                }
            } else {
                // Horodatage relatif à l'origine du clip, qui est la plus
                // ancienne frame de pré-enregistrement et non l'instant de la
                // détection.
                let offset =
                    u32::try_from(now.duration_since(clip.origin).as_millis()).unwrap_or(u32::MAX);

                if let Err(e) = clip.writer.write_frame_at(offset, payload.as_frame()) {
                    warn!("⚠️ Écriture du clip d'événement interrompue : {e}");
                    self.active = None;
                }
            }
        }

        if !armed {
            // Surveillance éteinte : plus aucun clip ne peut être déclenché,
            // le pré-enregistrement n'a donc personne à servir.
            self.buffer.clear();
            self.buffered_bytes = 0;
            return;
        }

        self.buffered_bytes += payload.len();
        self.buffer.push_back(BufferedFrame {
            captured_at: now,
            payload,
        });

        self.trim_buffer(now);
    }

    /// Écarte du tampon les frames trop anciennes, et celles qui dépassent le
    /// plafond mémoire.
    ///
    /// La troncature s'arrête à une IMAGE CLÉ : en H.264, une image
    /// intermédiaire n'a aucun sens sans celle dont elle décrit les
    /// différences, et commencer un clip par l'une d'elles donnerait des
    /// premières secondes en bouillie. Le pré-enregistrement est donc parfois
    /// un peu plus long que demandé — jamais plus court, et toujours
    /// décodable.
    fn trim_buffer(&mut self, now: Instant) {
        while self.buffer.len() > 1 {
            let front = &self.buffer[0];

            let too_old = now.duration_since(front.captured_at) > self.pre_roll;
            let too_big = self.buffered_bytes > MAX_PREROLL_BYTES;

            if !too_old && !too_big {
                break;
            }

            // On ne peut retirer la frame de tête que si celle qui la suit
            // peut à son tour ouvrir le tampon.
            if !self.buffer[1].payload.is_keyframe() {
                // Sauf si le plafond mémoire est dépassé : là, il faut
                // vraiment faire de la place, quitte à perdre le groupe
                // d'images en cours.
                if !too_big {
                    break;
                }

                self.drop_front();
                continue;
            }

            self.drop_front();
        }
    }

    /// Retire la frame la plus ancienne du tampon.
    fn drop_front(&mut self) {
        if let Some(frame) = self.buffer.pop_front() {
            self.buffered_bytes -= frame.payload.len();
        }
    }

    /// Déclenche un clip pour un événement qui vient de se produire, et
    /// retourne le nom du fichier à référencer dans l'événement.
    ///
    /// Si un clip est DÉJÀ en cours, il est simplement prolongé et son nom
    /// réutilisé. C'est volontaire : deux personnes détectées coup sur coup,
    /// ou une même personne qui passe d'inconnue à identifiée, produisent
    /// plusieurs événements à quelques secondes d'intervalle. Leur ouvrir un
    /// fichier chacun donnerait une poignée de clips quasi identiques, qui se
    /// chevauchent, pour une seule scène.
    ///
    /// Retourne `None` si les clips sont désactivés, si le format n'est pas
    /// encore connu (aucune image clé produite), ou si le fichier n'a pas pu
    /// être créé — l'événement part alors sans clip, ce qui vaut mieux que de
    /// ne pas partir.
    pub fn start_or_extend(&mut self) -> Option<String> {
        self.start_or_extend_at(Instant::now())
    }

    fn start_or_extend_at(&mut self, now: Instant) -> Option<String> {
        if !self.enabled {
            return None;
        }

        if let Some(clip) = &mut self.active
            && now < clip.until
        {
            clip.until = now + self.post_roll;
            return Some(clip.writer.name().to_string());
        }

        let format = self.format.clone()?;

        let mut writer = match RecordingWriter::create_event_clip(&self.dir, &format) {
            Ok(writer) => writer,
            Err(e) => {
                warn!("⚠️ Clip d'événement non créé : {e}");
                return None;
            }
        };

        let name = writer.name().to_string();

        // Origine des horodatages : la plus ancienne frame disponible, pour
        // que le pré-enregistrement conserve son cadencement réel.
        let origin = self.buffer.front().map_or(now, |frame| frame.captured_at);

        for frame in &self.buffer {
            let offset = u32::try_from(frame.captured_at.duration_since(origin).as_millis())
                .unwrap_or(u32::MAX);

            if let Err(e) = writer.write_frame_at(offset, frame.payload.as_frame()) {
                warn!("⚠️ Pré-enregistrement du clip incomplet : {e}");
                break;
            }
        }

        debug!(
            "🎞️ Clip d'événement démarré : {name} ({} frame(s) de pré-enregistrement)",
            self.buffer.len()
        );

        self.active = Some(ActiveClip {
            writer,
            origin,
            until: now + self.post_roll,
        });

        Some(name)
    }

    /// Referme proprement un clip en cours, à l'arrêt de la caméra.
    pub fn finish(&mut self) {
        if let Some(clip) = self.active.take() {
            let name = clip.writer.name().to_string();

            if let Err(e) = clip.writer.finish() {
                warn!("⚠️ Clip d'événement « {name} » mal refermé : {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ClipRecorder {
        /// Raccourci des tests : soumettre une frame JPEG avec la
        /// surveillance active, le cas de loin le plus courant.
        fn push_at(&mut self, jpeg: &[u8], now: Instant) {
            self.push_payload(BufferedPayload::Jpeg(jpeg.to_vec()), true, now);
        }

        /// Soumettre une frame H.264.
        fn push_h264_at(&mut self, keyframe: bool, size: usize, now: Instant) {
            self.push_payload(
                BufferedPayload::H264 {
                    nals: vec![vec![if keyframe { 0x65 } else { 0x41 }; size.max(1)]],
                    keyframe,
                },
                true,
                now,
            );
        }
    }

    fn config(dir: &std::path::Path) -> RecordingConfig {
        RecordingConfig {
            dir: dir.to_string_lossy().to_string(),
            clips_enabled: true,
            clip_pre_secs: 4,
            clip_post_secs: 8,
            ..RecordingConfig::default()
        }
    }

    /// Enregistreur au format historique : chaque frame est autonome.
    fn mjpeg_recorder(dir: &std::path::Path) -> ClipRecorder {
        ClipRecorder::new(&config(dir), false)
    }

    /// Enregistreur en MP4, dont le format est déjà connu.
    fn mp4_recorder(dir: &std::path::Path) -> ClipRecorder {
        let mut recorder = ClipRecorder::new(&config(dir), true);
        recorder.set_format(RecordingFormat::Mp4 {
            width: 64,
            height: 48,
            fps: 12,
            sps: vec![0x67, 0x42, 0xC0, 0x1E],
            pps: vec![0x68, 0xCE],
        });
        recorder
    }

    /// Lit les horodatages des frames d'un enregistrement au format
    /// historique.
    fn frame_timestamps(path: &std::path::Path) -> Vec<u32> {
        let raw = std::fs::read(path).expect("lecture du clip");
        let mut timestamps = Vec::new();
        let mut offset = 0;

        while offset + 8 <= raw.len() {
            let timestamp = u32::from_le_bytes(raw[offset..offset + 4].try_into().unwrap());
            let length =
                u32::from_le_bytes(raw[offset + 4..offset + 8].try_into().unwrap()) as usize;

            timestamps.push(timestamp);
            offset += 8 + length;
        }

        timestamps
    }

    fn only_file(dir: &std::path::Path) -> std::path::PathBuf {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .expect("lecture du dossier")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .collect();
        entries.sort();
        assert_eq!(entries.len(), 1, "un seul fichier attendu : {entries:?}");
        entries.pop().unwrap()
    }

    // --- Pré-enregistrement ---

    #[test]
    fn no_clip_is_written_until_an_event_happens() {
        // Le tampon de pré-enregistrement vit en mémoire : au repos, la
        // fonctionnalité ne touche pas au disque.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        for index in 0..50 {
            recorder.push_at(&[index], start + Duration::from_millis(index as u64 * 40));
        }

        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_event_clip_starts_with_the_pre_roll_already_buffered() {
        // C'EST LA RAISON D'ÊTRE DU MODULE : le clip doit montrer ce qui a
        // précédé la détection.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        for index in 0..50u32 {
            recorder.push_at(&[1, 2, 3], start + Duration::from_millis(index as u64 * 40));
        }

        let name = recorder
            .start_or_extend_at(start + Duration::from_millis(2_000))
            .expect("clip créé");
        recorder.finish();

        assert!(name.starts_with("evt_"));
        assert!(name.ends_with(".mjpeg"));
        assert_eq!(frame_timestamps(&only_file(dir.path())).len(), 50);
    }

    #[test]
    fn the_pre_roll_keeps_its_real_pacing_in_the_file() {
        // Les frames sont écrites d'un bloc, en quelques millisecondes, mais
        // couvrent deux secondes de vidéo. Mesurées à l'écriture, elles
        // porteraient toutes un horodatage proche de zéro et la relecture
        // avalerait tout le pré-enregistrement d'un coup.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        for index in 0..50u32 {
            recorder.push_at(&[7], start + Duration::from_millis(index as u64 * 40));
        }

        recorder.start_or_extend_at(start + Duration::from_millis(2_000));
        recorder.finish();

        let timestamps = frame_timestamps(&only_file(dir.path()));

        assert_eq!(timestamps.first(), Some(&0));
        assert_eq!(timestamps.last(), Some(&1_960), "49 × 40 ms");
        assert!(timestamps.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn frames_after_the_event_continue_the_same_timeline() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        recorder.push_at(&[1], start);
        recorder.push_at(&[2], start + Duration::from_millis(500));

        recorder.start_or_extend_at(start + Duration::from_millis(1_000));

        recorder.push_at(&[3], start + Duration::from_millis(1_500));
        recorder.finish();

        assert_eq!(
            frame_timestamps(&only_file(dir.path())),
            vec![0, 500, 1_500]
        );
    }

    #[test]
    fn the_pre_roll_buffer_forgets_frames_older_than_configured() {
        // Sans cet oubli, le tampon grossirait indéfiniment.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        recorder.push_at(&[1], start);
        recorder.push_at(&[2], start + Duration::from_secs(1));
        // Dix secondes plus tard : les deux premières sortent de la fenêtre.
        recorder.push_at(&[3], start + Duration::from_secs(10));

        recorder.start_or_extend_at(start + Duration::from_secs(10));
        recorder.finish();

        assert_eq!(frame_timestamps(&only_file(dir.path())).len(), 1);
    }

    // --- Troncature par groupe d'images (H.264) ---

    #[test]
    fn an_h264_buffer_is_only_cut_at_a_keyframe() {
        // Une image intermédiaire n'a aucun sens sans l'image clé dont elle
        // décrit les différences : couper devant elle donnerait un clip dont
        // les premières secondes sont en bouillie.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mp4_recorder(dir.path());
        let start = Instant::now();

        // Une image clé, puis des intermédiaires, largement au-delà de la
        // fenêtre de pré-enregistrement de 4 secondes.
        recorder.push_h264_at(true, 100, start);
        for index in 1..200u64 {
            recorder.push_h264_at(false, 20, start + Duration::from_millis(index * 80));
        }

        assert!(
            recorder
                .buffer
                .front()
                .is_some_and(|f| f.payload.is_keyframe()),
            "le tampon doit toujours commencer par une image clé"
        );
    }

    #[test]
    fn an_h264_buffer_drops_whole_groups_of_pictures() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mp4_recorder(dir.path());
        let start = Instant::now();

        // Trois groupes d'images d'une seconde chacun, espacés de 5 secondes
        // au total : seuls les derniers tiennent dans la fenêtre de 4 s.
        for group in 0..6u64 {
            recorder.push_h264_at(true, 100, start + Duration::from_secs(group));
            for index in 1..5u64 {
                recorder.push_h264_at(
                    false,
                    20,
                    start + Duration::from_secs(group) + Duration::from_millis(index * 200),
                );
            }
        }

        assert!(recorder.buffer.front().unwrap().payload.is_keyframe());
        // Le pré-enregistrement reste au moins aussi long que demandé.
        let span = recorder.buffer.back().unwrap().captured_at
            - recorder.buffer.front().unwrap().captured_at;
        assert!(
            span >= Duration::from_secs(4),
            "pré-enregistrement raccourci à {span:?}"
        );
    }

    #[test]
    fn an_h264_clip_is_a_playable_mp4() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mp4_recorder(dir.path());
        let start = Instant::now();

        recorder.push_h264_at(true, 200, start);
        recorder.push_h264_at(false, 40, start + Duration::from_millis(80));

        let name = recorder
            .start_or_extend_at(start + Duration::from_millis(160))
            .expect("clip créé");

        recorder.push_h264_at(false, 40, start + Duration::from_millis(240));
        recorder.finish();

        assert!(name.ends_with(".mp4"), "{name}");

        let data = std::fs::read(only_file(dir.path())).expect("lecture");
        assert_eq!(&data[4..8], b"ftyp", "le fichier doit être un MP4");
        assert!(
            data.windows(4).any(|w| w == b"moof"),
            "le clip doit contenir au moins un fragment"
        );
    }

    #[test]
    fn no_clip_is_written_before_the_encoder_has_produced_a_keyframe() {
        // Sans jeux de paramètres, un MP4 ne peut pas s'ouvrir : l'événement
        // part sans clip plutôt qu'avec un fichier illisible.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()), true);

        recorder.push_h264_at(true, 100, Instant::now());

        assert_eq!(recorder.start_or_extend(), None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    // --- Déclenchement et fenêtre ---

    #[test]
    fn a_second_event_during_a_clip_extends_it_instead_of_opening_another() {
        // Une même scène produit plusieurs événements (inconnu puis
        // identifié, ou deux personnes) : leur ouvrir un fichier chacun
        // donnerait une poignée de clips qui se chevauchent.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        recorder.push_at(&[1], start);

        let first = recorder.start_or_extend_at(start).expect("clip");
        let second = recorder
            .start_or_extend_at(start + Duration::from_secs(2))
            .expect("clip");
        recorder.finish();

        assert_eq!(first, second);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn an_event_after_a_clip_has_ended_opens_a_new_one() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        recorder.push_at(&[1], start);
        let first = recorder.start_or_extend_at(start).expect("clip");

        // `clip_post_secs = 8` : à +20 s, le premier clip est refermé.
        let second = recorder
            .start_or_extend_at(start + Duration::from_secs(20))
            .expect("clip");
        recorder.finish();

        assert_ne!(first, second);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_clip_stops_being_written_after_its_post_roll() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        recorder.push_at(&[1], start);
        recorder.start_or_extend_at(start);

        // Dans la fenêtre : écrite.
        recorder.push_at(&[2], start + Duration::from_secs(4));
        // Au-delà des 8 secondes de post-enregistrement : plus écrite.
        recorder.push_at(&[3], start + Duration::from_secs(30));
        recorder.push_at(&[4], start + Duration::from_secs(31));
        recorder.finish();

        assert_eq!(frame_timestamps(&only_file(dir.path())), vec![0, 4_000]);
    }

    #[test]
    fn disabled_clips_write_nothing_and_reference_nothing() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(
            &RecordingConfig {
                clips_enabled: false,
                ..config(dir.path())
            },
            false,
        );

        recorder.push_jpeg(&[1, 2, 3], true);

        assert_eq!(recorder.start_or_extend(), None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_event_with_an_empty_buffer_still_produces_a_clip() {
        // Les clips peuvent être déclenchés avant qu'une seule frame ne soit
        // passée (démarrage) : le clip doit exister, simplement sans
        // pré-enregistrement.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());

        let name = recorder.start_or_extend().expect("clip créé");
        recorder.finish();

        assert!(name.starts_with("evt_"));
        assert_eq!(frame_timestamps(&only_file(dir.path())).len(), 0);
    }

    #[test]
    fn an_unwritable_directory_does_not_bring_down_the_detection() {
        // Un disque plein ou un dossier en lecture seule doit coûter le clip,
        // pas l'événement.
        let mut recorder = ClipRecorder::new(
            &RecordingConfig {
                dir: "/proc/foxguard-ne-peut-pas-ecrire-ici".to_string(),
                clips_enabled: true,
                ..RecordingConfig::default()
            },
            false,
        );

        recorder.push_jpeg(&[1, 2, 3], true);

        assert_eq!(recorder.start_or_extend(), None);
    }

    // --- Armement et mémoire ---

    #[test]
    fn a_disarmed_recorder_releases_its_pre_roll_buffer() {
        // Surveillance éteinte : aucun événement ne peut survenir, donc aucun
        // clip. Garder des secondes de vidéo en mémoire pour une
        // fonctionnalité en sommeil serait du gaspillage pur.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        for index in 0..20u64 {
            recorder.push_at(&[1, 2, 3], start + Duration::from_millis(index * 40));
        }
        assert!(!recorder.buffer.is_empty());

        recorder.push_payload(
            BufferedPayload::Jpeg(vec![1, 2, 3]),
            false,
            start + Duration::from_millis(900),
        );

        assert!(recorder.buffer.is_empty());
        assert_eq!(recorder.buffered_bytes, 0);
    }

    #[test]
    fn a_clip_already_under_way_is_finished_even_after_disarming() {
        // Couper la vidéo en plein milieu parce que quelqu'un a éteint la
        // surveillance laisserait un fichier tronqué sans que rien ne le dise.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = mjpeg_recorder(dir.path());
        let start = Instant::now();

        recorder.push_at(&[1], start);
        recorder.start_or_extend_at(start);

        recorder.push_payload(
            BufferedPayload::Jpeg(vec![2]),
            false,
            start + Duration::from_secs(2),
        );
        recorder.push_payload(
            BufferedPayload::Jpeg(vec![3]),
            false,
            start + Duration::from_secs(4),
        );
        recorder.finish();

        assert_eq!(
            frame_timestamps(&only_file(dir.path())),
            vec![0, 2_000, 4_000]
        );
    }

    #[test]
    fn the_buffer_respects_its_memory_ceiling() {
        // Deuxième plafond, en octets : une caméra en haute résolution sur
        // une scène agitée ne doit pas pouvoir épuiser la mémoire du
        // Raspberry Pi.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(
            &RecordingConfig {
                // Une heure de pré-enregistrement : seule la borne mémoire
                // peut arrêter le tampon.
                clip_pre_secs: 3_600,
                ..config(dir.path())
            },
            false,
        );
        let start = Instant::now();

        let frame = vec![0u8; 4 * 1024 * 1024];
        for index in 0..20u64 {
            recorder.push_at(&frame, start + Duration::from_millis(index * 40));
        }

        assert!(
            recorder.buffered_bytes <= MAX_PREROLL_BYTES,
            "{} octets en tampon",
            recorder.buffered_bytes
        );
        assert!(
            recorder.buffer.len() < 20,
            "{} frames",
            recorder.buffer.len()
        );
    }

    #[test]
    fn the_memory_ceiling_wins_over_keeping_a_whole_group_of_pictures() {
        // Un groupe d'images anormalement long ne doit pas pouvoir faire
        // sauter le plafond mémoire : mieux vaut un pré-enregistrement
        // amputé qu'un Raspberry Pi à court de mémoire.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(
            &RecordingConfig {
                clip_pre_secs: 3_600,
                ..config(dir.path())
            },
            true,
        );
        let start = Instant::now();

        recorder.push_h264_at(true, 4 * 1024 * 1024, start);
        for index in 1..20u64 {
            recorder.push_h264_at(
                false,
                4 * 1024 * 1024,
                start + Duration::from_millis(index * 40),
            );
        }

        assert!(
            recorder.buffered_bytes <= MAX_PREROLL_BYTES,
            "{} octets en tampon",
            recorder.buffered_bytes
        );
    }
}
