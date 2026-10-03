//! Encodage H.264 des frames de la caméra, via `openh264` (implémentation
//! Cisco, BSD-2).
//!
//! # Pourquoi `openh264` et pas autre chose
//!
//! Le flux WebSocket existant est du **MJPEG** : une image JPEG complète par
//! frame, sans aucune compression inter-frame. C'est simple et sans latence,
//! mais ça coûte cher en débit — une scène immobile est retransmise
//! intégralement 25 fois par seconde. H.264 ne transmet que ce qui change :
//! sur une scène de vidéosurveillance, immobile l'essentiel du temps, le
//! débit s'effondre d'un ordre de grandeur. Et c'est le format qu'attendent
//! les lecteurs vidéo et les enregistreurs (VLC, ffmpeg, Home Assistant,
//! Frigate), ce que le MJPEG maison n'est pas.
//!
//! Le choix de `openh264` parmi les options possibles tient à la contrainte
//! qui structure tout ce dépôt : **la compilation croisée ARM64 sous QEMU**
//! (voir `crates/camera/Cargo.toml`, où `ring` et `native-tls` ont déjà
//! coûté cher). Le script de compilation de `openh264-sys2` ne cherche
//! d'assembleur que pour x86 et x86_64 ; pour toute autre architecture il
//! retourne explicitement « pas d'assembleur » et construit du C++ portable.
//! Le piège qui a fait échouer `ring` n'existe donc pas ici. Par ailleurs,
//! `OPENH264_NO_ASM=1` désactive l'assembleur même sur x86, ce qui laisse une
//! porte de sortie si un environnement de compilation se montrait récalcitrant.
//!
//! # Ce que ça coûte
//!
//! C'est un encodeur LOGICIEL : il consomme du CPU, là où le Raspberry Pi
//! dispose par ailleurs d'un encodeur matériel (V4L2 M2M, `/dev/video11`).
//! Deux raisons de commencer par le logiciel : il fonctionne à l'identique
//! sur le PC de développement et sur le Pi (donc il est testable), et il ne
//! dépend ni d'une version de noyau ni d'un réglage de `config.txt`. La
//! fonctionnalité est pour cette raison **désactivée par défaut** (`[rtsp]
//! enabled = false`) : on ne prend pas ce coût sur une installation qui n'a
//! pas demandé de flux RTSP.

use anyhow::{Context, Result};
use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType,
};

use super::i420::I420Buffer;

/// Unité d'accès H.264 : les NAL d'UNE frame encodée, prêtes à être
/// empaquetées en RTP (voir `crate::rtsp::rtp`).
///
/// Les NAL sont livrées SANS leur préfixe de délimitation Annex-B
/// (`00 00 00 01`) : RTP s'appuie sur les frontières de paquets, le préfixe
/// n'y a pas sa place (RFC 6184 §5.3).
#[derive(Debug, Clone)]
pub struct AccessUnit {
    /// Les NAL de la frame, dans l'ordre. Pour une image clé, les
    /// paramètres (SPS/PPS) sont inclus en tête.
    pub nals: Vec<Vec<u8>>,

    /// Vrai pour une image clé (IDR) : c'est la seule frame sur laquelle un
    /// nouveau client peut commencer à décoder.
    pub keyframe: bool,

    /// Horodatage de présentation en horloge RTP (90 kHz, la cadence
    /// imposée par RFC 6184 pour H.264).
    ///
    /// Calculé par le PRODUCTEUR, une fois pour tous les clients : deux
    /// lecteurs branchés sur le même flux doivent voir la même base de
    /// temps, sinon un client qui se connecte tard repart à zéro et son
    /// lecteur croit à un saut dans le passé.
    pub rtp_timestamp: u32,
}

/// Jeu de paramètres du flux (SPS + PPS), tel qu'annoncé dans le SDP d'une
/// réponse `DESCRIBE` (voir `crate::rtsp::sdp`).
///
/// Ces NAL ne sont connues qu'APRÈS la première frame encodée : l'encodeur
/// les produit avec la première image clé. Un `DESCRIBE` reçu avant cela est
/// servi sans elles — le client récupère alors les paramètres en ligne, à la
/// première image clé du flux.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParameterSets {
    pub sps: Vec<u8>,
    pub pps: Vec<u8>,
}

/// Encodeur H.264 d'un flux caméra : garde l'état inter-frame de l'encodeur
/// et le tampon I420 réutilisé d'une frame à l'autre.
pub struct H264Encoder {
    encoder: Encoder,
    buffer: I420Buffer,
    /// Compteur de frames, qui sert de base de temps : l'horloge RTP est
    /// dérivée de la CADENCE ANNONCÉE plutôt que de l'heure réelle, pour que
    /// le flux reste régulier même si la boucle de capture hoquette.
    frames: u64,
    /// Incrément d'horodatage RTP par frame, en unités de 90 kHz.
    ticks_per_frame: u32,
    /// Dernier jeu de paramètres vu, mémorisé pour le SDP.
    parameters: Option<ParameterSets>,
}

impl H264Encoder {
    /// Construit un encodeur pour des frames `width`×`height`.
    ///
    /// `fps` et `bitrate_kbps` ne sont pas des garanties mais des CIBLES
    /// données au contrôle de débit : l'encodeur ajuste sa quantification
    /// pour s'en approcher.
    ///
    /// `keyframe_interval_secs` borne le temps d'attente d'un nouveau client :
    /// il ne peut commencer à décoder qu'à une image clé. Trop court, les
    /// images clés (non compressées temporellement) mangent tout le débit ;
    /// trop long, le lecteur reste noir plusieurs secondes.
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        keyframe_interval_secs: u32,
    ) -> Result<Self> {
        let buffer = I420Buffer::new(width, height);
        let (width, height) = buffer.dimensions();

        anyhow::ensure!(
            width >= 16 && height >= 16,
            "résolution {width}x{height} trop petite pour un encodage H.264"
        );

        let fps = fps.clamp(1, 120);

        let config = EncoderConfig::new()
            .bitrate(BitRate::from_bps(bitrate_kbps.max(32) * 1000))
            .max_frame_rate(FrameRate::from_hz(fps as f32))
            // `CameraVideoRealTime` : l'encodeur privilégie la latence et une
            // charge CPU régulière plutôt que le taux de compression. C'est
            // exactement le compromis voulu sur un Raspberry Pi qui fait
            // tourner YOLO en parallèle.
            .usage_type(UsageType::CameraVideoRealTime)
            // Débit plafonné plutôt que qualité constante : sur un réseau
            // domestique, un pic de débit sur une scène agitée se traduit par
            // des saccades chez le client, pas par une image plus belle.
            .rate_control_mode(RateControlMode::Bitrate)
            // Profil de base : pas de trames B, pas de CABAC. C'est le profil
            // que décodent tous les lecteurs, y compris les plus pauvres
            // (navigateurs, enregistreurs bas de gamme), et le moins coûteux
            // à produire.
            .profile(Profile::Baseline)
            .intra_frame_period(IntraFramePeriod::from_num_frames(
                fps * keyframe_interval_secs.clamp(1, 60),
            ))
            // SPS/PPS répétés à chaque image clé, et non une seule fois au
            // début du flux : un client qui se branche en cours de route
            // DOIT les recevoir, sans quoi il ne décodera jamais rien.
            .sps_pps_strategy(SpsPpsStrategy::IncreasingId)
            // Le flux n'est pas journalisé par `openh264` lui-même : ses
            // messages partiraient sur stderr, hors du `tracing` du reste de
            // l'application (voir `init_tracing` dans `main.rs`).
            .debug(false);

        let encoder = Encoder::with_api_config(OpenH264API::from_source(), config)
            .context("initialisation de l'encodeur H.264 impossible")?;

        Ok(Self {
            encoder,
            buffer,
            frames: 0,
            // 90 000 Hz est l'horloge imposée pour H.264 en RTP.
            ticks_per_frame: 90_000 / fps,
            parameters: None,
        })
    }

    /// Dimensions réellement encodées (arrondies à l'entier pair, voir
    /// [`super::i420::I420Buffer::new`]).
    pub fn dimensions(&self) -> (u32, u32) {
        self.buffer.dimensions()
    }

    /// Jeu de paramètres du flux, dès qu'une première image clé a été
    /// produite.
    pub fn parameters(&self) -> Option<&ParameterSets> {
        self.parameters.as_ref()
    }

    /// Demande que la PROCHAINE frame soit une image clé.
    ///
    /// Appelé quand un client vient de démarrer sa lecture : sans cela il
    /// attendrait l'image clé périodique suivante, soit jusqu'à plusieurs
    /// secondes d'écran noir.
    pub fn request_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    /// Encode une frame fournie en RGB (webcam MJPEG décodée, ou frame avec
    /// les boîtes de détection déjà incrustées).
    pub fn encode_rgb(&mut self, image: &image::RgbImage) -> Result<Option<AccessUnit>> {
        if !self.buffer.fill_from_rgb(image) {
            return Ok(None);
        }

        self.encode_current_buffer()
    }

    /// Encode une frame fournie en YUYV brut, telle que sortie de V4L2 —
    /// le chemin le moins coûteux (voir [`super::i420`]).
    pub fn encode_yuyv(&mut self, buf: &[u8], source_width: u32) -> Result<Option<AccessUnit>> {
        if !self.buffer.fill_from_yuyv(buf, source_width) {
            return Ok(None);
        }

        self.encode_current_buffer()
    }

    /// Encode le contenu courant du tampon I420.
    fn encode_current_buffer(&mut self) -> Result<Option<AccessUnit>> {
        let rtp_timestamp =
            (self.frames.wrapping_mul(u64::from(self.ticks_per_frame)) & 0xFFFF_FFFF) as u32;
        self.frames = self.frames.wrapping_add(1);

        let bitstream = self
            .encoder
            .encode(&self.buffer)
            .context("encodage H.264 de la frame impossible")?;

        // `Skip` : le contrôle de débit a volontairement abandonné cette
        // frame pour tenir la cible. `Invalid` : l'encodeur n'a rien produit.
        // Ni l'un ni l'autre n'est une erreur — il n'y a simplement rien à
        // transmettre.
        if matches!(bitstream.frame_type(), FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }

        let keyframe = matches!(bitstream.frame_type(), FrameType::IDR | FrameType::I);

        let mut nals = Vec::new();

        for layer_index in 0..bitstream.num_layers() {
            let Some(layer) = bitstream.layer(layer_index) else {
                continue;
            };

            for nal_index in 0..layer.nal_count() {
                let Some(nal) = layer.nal_unit(nal_index) else {
                    continue;
                };

                let nal = strip_start_code(nal);
                if nal.is_empty() {
                    continue;
                }

                nals.push(nal.to_vec());
            }
        }

        if nals.is_empty() {
            return Ok(None);
        }

        self.remember_parameter_sets(&nals);

        Ok(Some(AccessUnit {
            nals,
            keyframe,
            rtp_timestamp,
        }))
    }

    /// Mémorise les SPS/PPS d'une unité d'accès pour le SDP.
    fn remember_parameter_sets(&mut self, nals: &[Vec<u8>]) {
        let sps = nals.iter().find(|nal| nal_type(nal) == Some(NAL_SPS));
        let pps = nals.iter().find(|nal| nal_type(nal) == Some(NAL_PPS));

        if let (Some(sps), Some(pps)) = (sps, pps) {
            let candidate = ParameterSets {
                sps: sps.clone(),
                pps: pps.clone(),
            };

            // Comparé avant d'écrire : `SpsPpsStrategy::IncreasingId` les
            // réémet à chaque image clé, et remplacer un jeu identique à
            // chaque seconde ne ferait que du travail pour rien.
            if self.parameters.as_ref() != Some(&candidate) {
                self.parameters = Some(candidate);
            }
        }
    }
}

/// Type de NAL « Sequence Parameter Set » (résolution, profil, niveau).
pub const NAL_SPS: u8 = 7;
/// Type de NAL « Picture Parameter Set » (paramètres d'entropie et de
/// découpage en tranches).
pub const NAL_PPS: u8 = 8;

/// Type d'une NAL (les 5 bits de poids faible de son premier octet), ou
/// `None` si la tranche est vide.
pub fn nal_type(nal: &[u8]) -> Option<u8> {
    Some(nal.first()? & 0x1F)
}

/// Retire le préfixe de délimitation Annex-B (`00 00 01` ou `00 00 00 01`)
/// que `openh264` place devant chaque NAL.
///
/// RTP transporte une NAL par charge utile et s'appuie sur les frontières de
/// paquets : le préfixe y serait interprété comme le début de la NAL, et le
/// décodeur n'y comprendrait rien (RFC 6184 §5.3).
fn strip_start_code(nal: &[u8]) -> &[u8] {
    if nal.starts_with(&[0, 0, 0, 1]) {
        &nal[4..]
    } else if nal.starts_with(&[0, 0, 1]) {
        &nal[3..]
    } else {
        nal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::RgbImage;

    #[test]
    fn a_four_byte_start_code_is_stripped() {
        assert_eq!(strip_start_code(&[0, 0, 0, 1, 0x67, 0x42]), &[0x67, 0x42]);
    }

    #[test]
    fn a_three_byte_start_code_is_stripped() {
        assert_eq!(strip_start_code(&[0, 0, 1, 0x68, 0xCE]), &[0x68, 0xCE]);
    }

    #[test]
    fn a_nal_without_a_start_code_is_left_alone() {
        assert_eq!(strip_start_code(&[0x65, 0xB8]), &[0x65, 0xB8]);
    }

    #[test]
    fn nal_types_are_read_from_the_low_five_bits() {
        // 0x67 = SPS, 0x68 = PPS, 0x65 = tranche IDR.
        assert_eq!(nal_type(&[0x67]), Some(NAL_SPS));
        assert_eq!(nal_type(&[0x68]), Some(NAL_PPS));
        assert_eq!(nal_type(&[0x65]), Some(5));
        assert_eq!(nal_type(&[]), None);
    }

    /// Image de test au contenu qui BOUGE d'une frame à l'autre : une mire
    /// immobile serait entièrement « sautée » par le contrôle de débit, et le
    /// test ne vérifierait plus rien.
    fn moving_frame(width: u32, height: u32, frame: u32) -> RgbImage {
        RgbImage::from_fn(width, height, |x, y| {
            let value = ((x + y * 3 + frame * 37) % 255) as u8;
            image::Rgb([value, 255 - value, value / 2])
        })
    }

    #[test]
    fn the_first_encoded_frame_is_a_keyframe_carrying_its_parameter_sets() {
        // C'est l'invariant dont dépend tout le reste : un client ne peut
        // commencer à décoder qu'à une image clé, et seulement s'il a reçu
        // les SPS/PPS.
        let mut encoder = H264Encoder::new(64, 64, 25, 512, 2).expect("encodeur");

        let unit = encoder
            .encode_rgb(&moving_frame(64, 64, 0))
            .expect("encodage")
            .expect("première frame produite");

        assert!(unit.keyframe);
        assert!(unit.nals.iter().any(|nal| nal_type(nal) == Some(NAL_SPS)));
        assert!(unit.nals.iter().any(|nal| nal_type(nal) == Some(NAL_PPS)));
    }

    #[test]
    fn nals_are_delivered_without_their_annex_b_prefix() {
        let mut encoder = H264Encoder::new(64, 64, 25, 512, 2).expect("encodeur");
        let unit = encoder
            .encode_rgb(&moving_frame(64, 64, 0))
            .expect("encodage")
            .expect("frame produite");

        for nal in &unit.nals {
            assert!(
                !nal.starts_with(&[0, 0, 1]) && !nal.starts_with(&[0, 0, 0, 1]),
                "NAL livrée avec son préfixe Annex-B : {:02x?}",
                &nal[..4.min(nal.len())]
            );
        }
    }

    #[test]
    fn parameter_sets_become_available_after_the_first_frame() {
        let mut encoder = H264Encoder::new(64, 64, 25, 512, 2).expect("encodeur");

        // Avant toute frame, l'encodeur ne les connaît pas : un DESCRIBE
        // reçu à cet instant doit se passer de `sprop-parameter-sets`.
        assert!(encoder.parameters().is_none());

        encoder
            .encode_rgb(&moving_frame(64, 64, 0))
            .expect("encodage");

        let parameters = encoder.parameters().expect("paramètres connus");
        assert_eq!(nal_type(&parameters.sps), Some(NAL_SPS));
        assert_eq!(nal_type(&parameters.pps), Some(NAL_PPS));
    }

    #[test]
    fn the_rtp_timestamp_advances_by_one_frame_period() {
        // 25 im/s sur une horloge à 90 kHz : 3600 unités par frame. Une
        // erreur ici donne un flux que les lecteurs jouent trop vite ou trop
        // lentement.
        let mut encoder = H264Encoder::new(64, 64, 25, 512, 2).expect("encodeur");

        let first = encoder
            .encode_rgb(&moving_frame(64, 64, 0))
            .expect("encodage")
            .expect("frame");
        let second = encoder
            .encode_rgb(&moving_frame(64, 64, 1))
            .expect("encodage")
            .expect("frame");

        assert_eq!(first.rtp_timestamp, 0);
        assert_eq!(second.rtp_timestamp - first.rtp_timestamp, 3600);
    }

    #[test]
    fn subsequent_frames_are_much_smaller_than_the_keyframe() {
        // La raison d'être du H.264 ici : ne pas retransmettre toute l'image
        // à chaque frame, contrairement au MJPEG.
        let mut encoder = H264Encoder::new(128, 128, 25, 512, 10).expect("encodeur");

        let keyframe = encoder
            .encode_rgb(&moving_frame(128, 128, 0))
            .expect("encodage")
            .expect("frame");

        // Même image : il n'y a rien à transmettre que « rien n'a changé ».
        let still = encoder
            .encode_rgb(&moving_frame(128, 128, 0))
            .expect("encodage");

        let keyframe_bytes: usize = keyframe.nals.iter().map(Vec::len).sum();
        let still_bytes: usize = still
            .map(|unit| unit.nals.iter().map(Vec::len).sum())
            .unwrap_or(0);

        assert!(
            still_bytes * 4 < keyframe_bytes,
            "frame immobile {still_bytes} octets contre {keyframe_bytes} pour l'image clé"
        );
    }

    #[test]
    fn a_frame_smaller_than_the_configured_size_is_skipped_not_fatal() {
        // Une caméra qui change de résolution en cours de route ne doit pas
        // faire tomber la boucle de capture.
        let mut encoder = H264Encoder::new(64, 64, 25, 512, 2).expect("encodeur");

        let too_small = RgbImage::from_pixel(32, 32, image::Rgb([10, 20, 30]));
        assert!(
            encoder
                .encode_rgb(&too_small)
                .expect("pas d'erreur")
                .is_none()
        );
    }

    #[test]
    fn a_requested_keyframe_is_produced_on_the_next_frame() {
        let mut encoder = H264Encoder::new(64, 64, 25, 512, 60).expect("encodeur");

        encoder
            .encode_rgb(&moving_frame(64, 64, 0))
            .expect("encodage");
        // Sans la demande explicite, la prochaine image clé n'arriverait que
        // 60 secondes plus tard.
        encoder.request_keyframe();

        let unit = encoder
            .encode_rgb(&moving_frame(64, 64, 1))
            .expect("encodage")
            .expect("frame");

        assert!(unit.keyframe);
    }

    #[test]
    fn a_yuyv_frame_encodes_as_well_as_an_rgb_one() {
        // Le chemin du Raspberry Pi : la caméra fournit du YUYV, on ne
        // repasse pas par du RGB.
        let mut encoder = H264Encoder::new(64, 64, 25, 512, 2).expect("encodeur");

        let mut yuyv = vec![0u8; 64 * 64 * 2];
        for (index, chunk) in yuyv.chunks_exact_mut(4).enumerate() {
            chunk[0] = (index % 220 + 16) as u8;
            chunk[1] = 128;
            chunk[2] = (index % 180 + 16) as u8;
            chunk[3] = 128;
        }

        let unit = encoder
            .encode_yuyv(&yuyv, 64)
            .expect("encodage")
            .expect("frame");

        assert!(unit.keyframe);
    }
}
