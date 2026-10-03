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
//! D'où le tampon circulaire : les frames JPEG déjà encodées des dernières
//! `clip_pre_secs` secondes sont conservées en mémoire, et versées au début
//! du clip au moment où l'événement survient. Les frames y gardent leur
//! horodatage d'origine (voir [`super::recording::RecordingWriter::write_frame_at`]),
//! faute de quoi la relecture avalerait tout le pré-enregistrement d'un coup.
//!
//! # Format et rétention : ceux des enregistrements existants
//!
//! Un clip est un fichier `.mjpeg` au format déjà en place, écrit dans le
//! même dossier. Il est donc listé, servi, supprimé et purgé par le code
//! existant sans une ligne de plus (voir `crate::api` et
//! `crate::retention`) — et relu par le même décodeur côté interface. C'est
//! ce qui permet d'ajouter la fonctionnalité sans introduire un second
//! format, un second dossier et une seconde politique de rétention.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use crate::config::RecordingConfig;

use super::recording::RecordingWriter;

/// Plafond mémoire du tampon de pré-enregistrement, en octets.
///
/// Le tampon est borné par une DURÉE (`clip_pre_secs`), mais la taille d'une
/// frame JPEG varie beaucoup avec la scène et la résolution : à 1080p sur une
/// scène agitée, quelques secondes peuvent peser bien plus que prévu. Ce
/// second plafond garantit que la fonctionnalité ne peut pas, sur une caméra
/// inhabituelle, épuiser la mémoire du Raspberry Pi. Les frames les plus
/// anciennes sont abandonnées en premier : le clip aura un
/// pré-enregistrement plus court, ce qui est très préférable à un plantage.
const MAX_PREROLL_BYTES: usize = 48 * 1024 * 1024;

/// Une frame en attente dans le tampon de pré-enregistrement.
struct BufferedFrame {
    captured_at: Instant,
    jpeg: Vec<u8>,
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
}

impl ClipRecorder {
    pub fn new(config: &RecordingConfig) -> Self {
        Self {
            enabled: config.clips_enabled,
            dir: config.dir.clone(),
            pre_roll: Duration::from_secs(config.clip_pre_secs),
            post_roll: Duration::from_secs(config.clip_post_secs.max(1)),
            buffer: VecDeque::new(),
            buffered_bytes: 0,
            active: None,
        }
    }

    /// Soumet une frame JPEG à l'enregistreur : elle rejoint le tampon de
    /// pré-enregistrement et, si un clip est en cours, le fichier.
    ///
    /// Appelée à CHAQUE frame par la boucle de capture, donc tenue de rester
    /// bon marché : elle ne fait qu'un `memcpy` et, le cas échéant, une
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
    pub fn push_frame(&mut self, jpeg: &[u8], armed: bool) {
        self.push_frame_at(jpeg, armed, Instant::now());
    }

    /// Comme [`Self::push_frame`], à un instant fourni : c'est ce qui rend
    /// les fenêtres temporelles testables sans faire dormir le test.
    fn push_frame_at(&mut self, jpeg: &[u8], armed: bool, now: Instant) {
        if !self.enabled {
            return;
        }

        if let Some(clip) = &mut self.active {
            if now >= clip.until {
                debug!("🎞️ Clip d'événement terminé : {}", clip.writer.name());
                self.active = None;
            } else {
                // Horodatage relatif à l'origine du clip, qui est la plus
                // ancienne frame de pré-enregistrement et non l'instant de la
                // détection.
                let offset =
                    u32::try_from(now.duration_since(clip.origin).as_millis()).unwrap_or(u32::MAX);

                if let Err(e) = clip.writer.write_frame_at(offset, jpeg) {
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

        self.buffer.push_back(BufferedFrame {
            captured_at: now,
            jpeg: jpeg.to_vec(),
        });
        self.buffered_bytes += jpeg.len();

        self.trim_buffer(now);
    }

    /// Écarte du tampon les frames trop anciennes, et celles qui dépassent le
    /// plafond mémoire.
    fn trim_buffer(&mut self, now: Instant) {
        while let Some(front) = self.buffer.front() {
            let too_old = now.duration_since(front.captured_at) > self.pre_roll;
            let too_big = self.buffered_bytes > MAX_PREROLL_BYTES;

            if !too_old && !too_big {
                break;
            }

            self.buffered_bytes -= front.jpeg.len();
            self.buffer.pop_front();
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
    /// Retourne `None` si les clips sont désactivés, ou si le fichier n'a pas
    /// pu être créé — l'événement part alors sans clip, ce qui vaut mieux que
    /// de ne pas partir.
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

        let mut writer = match RecordingWriter::create_event_clip(&self.dir) {
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

            if let Err(e) = writer.write_frame_at(offset, &frame.jpeg) {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    impl ClipRecorder {
        /// Raccourci des tests : soumettre une frame avec la surveillance
        /// active, le cas de loin le plus courant.
        fn push_frame_at_armed(&mut self, jpeg: &[u8], now: Instant) {
            self.push_frame_at(jpeg, true, now);
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

    /// Lit les horodatages des frames d'un fichier d'enregistrement.
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

    #[test]
    fn no_clip_is_written_until_an_event_happens() {
        // Le tampon de pré-enregistrement vit en mémoire : au repos, la
        // fonctionnalité ne touche pas au disque.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        for index in 0..50 {
            recorder
                .push_frame_at_armed(&[index], start + Duration::from_millis(index as u64 * 40));
        }

        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_event_clip_starts_with_the_pre_roll_already_buffered() {
        // C'EST LA RAISON D'ÊTRE DU MODULE : le clip doit montrer ce qui a
        // précédé la détection.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        // Deux secondes de pré-enregistrement à 25 im/s.
        for index in 0..50u32 {
            recorder
                .push_frame_at_armed(&[1, 2, 3], start + Duration::from_millis(index as u64 * 40));
        }

        let name = recorder
            .start_or_extend_at(start + Duration::from_millis(2_000))
            .expect("clip créé");

        assert!(name.starts_with("evt_"));

        let timestamps = frame_timestamps(&only_file(dir.path()));
        assert_eq!(timestamps.len(), 50, "les 50 frames du tampon");
    }

    #[test]
    fn the_pre_roll_keeps_its_real_pacing_in_the_file() {
        // Les frames sont écrites d'un bloc, en quelques millisecondes, mais
        // couvrent deux secondes de vidéo. Mesurées à l'écriture, elles
        // porteraient toutes un horodatage proche de zéro et la relecture
        // avalerait tout le pré-enregistrement d'un coup.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        for index in 0..50u32 {
            recorder.push_frame_at_armed(&[7], start + Duration::from_millis(index as u64 * 40));
        }

        recorder.start_or_extend_at(start + Duration::from_millis(2_000));

        let timestamps = frame_timestamps(&only_file(dir.path()));

        assert_eq!(timestamps.first(), Some(&0));
        assert_eq!(timestamps.last(), Some(&1_960), "49 × 40 ms");
        // Strictement croissants : un lecteur qui recadence sur ces
        // horodatages a besoin qu'ils avancent.
        assert!(timestamps.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[test]
    fn frames_after_the_event_continue_the_same_timeline() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        recorder.push_frame_at_armed(&[1], start);
        recorder.push_frame_at_armed(&[2], start + Duration::from_millis(500));

        recorder.start_or_extend_at(start + Duration::from_millis(1_000));

        recorder.push_frame_at_armed(&[3], start + Duration::from_millis(1_500));

        let timestamps = frame_timestamps(&only_file(dir.path()));
        assert_eq!(timestamps, vec![0, 500, 1_500]);
    }

    #[test]
    fn the_pre_roll_buffer_forgets_frames_older_than_configured() {
        // Sans cet oubli, le tampon grossirait indéfiniment.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        // `clip_pre_secs = 4` dans la configuration de test.
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        recorder.push_frame_at_armed(&[1], start);
        recorder.push_frame_at_armed(&[2], start + Duration::from_secs(1));
        // Cette frame-ci est dix secondes plus tard : les deux premières
        // sortent de la fenêtre.
        recorder.push_frame_at_armed(&[3], start + Duration::from_secs(10));

        recorder.start_or_extend_at(start + Duration::from_secs(10));

        let timestamps = frame_timestamps(&only_file(dir.path()));
        assert_eq!(timestamps.len(), 1, "seule la frame récente est conservée");
    }

    #[test]
    fn a_second_event_during_a_clip_extends_it_instead_of_opening_another() {
        // Une même scène produit plusieurs événements (inconnu puis
        // identifié, ou deux personnes) : leur ouvrir un fichier chacun
        // donnerait une poignée de clips qui se chevauchent.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        recorder.push_frame_at_armed(&[1], start);

        let first = recorder.start_or_extend_at(start).expect("clip");
        let second = recorder
            .start_or_extend_at(start + Duration::from_secs(2))
            .expect("clip");

        assert_eq!(first, second);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn an_event_after_a_clip_has_ended_opens_a_new_one() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        recorder.push_frame_at_armed(&[1], start);
        let first = recorder.start_or_extend_at(start).expect("clip");

        // `clip_post_secs = 8` : à +20 s, le premier clip est refermé.
        let second = recorder
            .start_or_extend_at(start + Duration::from_secs(20))
            .expect("clip");

        assert_ne!(first, second);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
    }

    #[test]
    fn a_clip_stops_being_written_after_its_post_roll() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        recorder.push_frame_at_armed(&[1], start);
        recorder.start_or_extend_at(start);

        // Dans la fenêtre : écrite.
        recorder.push_frame_at_armed(&[2], start + Duration::from_secs(4));
        // Au-delà des 8 secondes de post-enregistrement : plus écrite.
        recorder.push_frame_at_armed(&[3], start + Duration::from_secs(30));
        recorder.push_frame_at_armed(&[4], start + Duration::from_secs(31));

        let timestamps = frame_timestamps(&only_file(dir.path()));
        assert_eq!(timestamps, vec![0, 4_000]);
    }

    #[test]
    fn disabled_clips_write_nothing_and_reference_nothing() {
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&RecordingConfig {
            clips_enabled: false,
            ..config(dir.path())
        });

        recorder.push_frame(&[1, 2, 3], true);

        assert_eq!(recorder.start_or_extend(), None);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn an_event_with_an_empty_buffer_still_produces_a_clip() {
        // Les clips peuvent être déclenchés avant qu'une seule frame ne soit
        // passée (démarrage) : le clip doit exister, simplement sans
        // pré-enregistrement.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));

        let name = recorder.start_or_extend().expect("clip créé");

        assert!(name.starts_with("evt_"));
        assert_eq!(frame_timestamps(&only_file(dir.path())).len(), 0);
    }

    #[test]
    fn an_unwritable_directory_does_not_bring_down_the_detection() {
        // Un disque plein ou un dossier en lecture seule doit coûter le clip,
        // pas l'événement.
        let mut recorder = ClipRecorder::new(&RecordingConfig {
            dir: "/proc/foxguard-ne-peut-pas-ecrire-ici".to_string(),
            clips_enabled: true,
            ..RecordingConfig::default()
        });

        recorder.push_frame(&[1, 2, 3], true);

        assert_eq!(recorder.start_or_extend(), None);
    }

    #[test]
    fn a_disarmed_recorder_releases_its_pre_roll_buffer() {
        // Surveillance éteinte : aucun événement ne peut survenir, donc aucun
        // clip. Garder des secondes de vidéo en mémoire pour une
        // fonctionnalité en sommeil serait du gaspillage pur.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        for index in 0..20u64 {
            recorder.push_frame_at_armed(&[1, 2, 3], start + Duration::from_millis(index * 40));
        }
        assert!(!recorder.buffer.is_empty());

        recorder.push_frame_at(&[1, 2, 3], false, start + Duration::from_millis(900));

        assert!(recorder.buffer.is_empty());
        assert_eq!(recorder.buffered_bytes, 0);
    }

    #[test]
    fn a_clip_already_under_way_is_finished_even_after_disarming() {
        // Couper la vidéo en plein milieu parce que quelqu'un a éteint la
        // surveillance laisserait un fichier tronqué sans que rien ne le dise.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&config(dir.path()));
        let start = Instant::now();

        recorder.push_frame_at_armed(&[1], start);
        recorder.start_or_extend_at(start);

        recorder.push_frame_at(&[2], false, start + Duration::from_secs(2));
        recorder.push_frame_at(&[3], false, start + Duration::from_secs(4));

        let timestamps = frame_timestamps(&only_file(dir.path()));
        assert_eq!(timestamps, vec![0, 2_000, 4_000]);
    }

    #[test]
    fn the_buffer_respects_its_memory_ceiling() {
        // Deuxième plafond, en octets : une caméra en haute résolution sur
        // une scène agitée ne doit pas pouvoir épuiser la mémoire du
        // Raspberry Pi.
        let dir = tempfile::tempdir().expect("dossier temporaire");
        let mut recorder = ClipRecorder::new(&RecordingConfig {
            // Une heure de pré-enregistrement : seule la borne mémoire peut
            // arrêter le tampon.
            clip_pre_secs: 3_600,
            ..config(dir.path())
        });
        let start = Instant::now();

        let frame = vec![0u8; 4 * 1024 * 1024];
        for index in 0..20u64 {
            recorder.push_frame_at_armed(&frame, start + Duration::from_millis(index * 40));
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
}
