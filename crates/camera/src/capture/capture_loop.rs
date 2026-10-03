//! Boucle de capture principale : lit les frames du périphérique V4L2,
//! transmet chaque frame au thread worker en arrière-plan (voir
//! `super::worker`), incruste les boîtes (voir `super::overlay`) et les
//! diffuse aux clients WebSocket, alimente les clips d'événement (voir
//! `super::clips`), encode le flux H.264 pour les lecteurs RTSP (voir
//! `crate::h264` et `crate::rtsp`) et gère l'enregistrement disque
//! optionnel.
//!
//! # Un décodage, plusieurs consommateurs
//!
//! Le point sensible de cette boucle est le DÉCODAGE de la frame. Il n'a lieu
//! qu'une fois par frame et seulement si quelqu'un en a l'usage (voir
//! `needs_decode` ci-dessous) : c'est ce qui préserve l'optimisation de type
//! *pass-through* héritée du flux WebSocket — une webcam qui fournit déjà du
//! MJPEG, détection éteinte et sans lecteur RTSP, voit ses octets retransmis
//! tels quels, sans jamais être décodés ni ré-encodés.

use anyhow::Result;
use image::{ImageFormat, RgbImage};
use std::sync::{Arc, Mutex, atomic::Ordering, mpsc};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use v4l::io::traits::CaptureStream;
use v4l::prelude::*;

use crate::h264::H264Encoder;
use crate::h264::H264Stream;
use crate::mail::Mailer;
use crate::util::MutexExt;
use crate::vision::{BoundingBox, FaceDetectorYuNet, FaceEmbedder, KnownPerson};

use super::clips::ClipRecorder;
use super::codec::decode_yuyv_to_rgb;
use super::known_faces::try_capture_reference;
use super::overlay::draw_detections;
use super::recording::{Frame, RecordingFormat, RecordingWriter};
use super::state::SharedState;

/// Encodage H.264 du flux, quand `[rtsp] enabled = true`.
pub(super) struct H264Output {
    pub(super) encoder: H264Encoder,
    pub(super) stream: Arc<H264Stream>,
    /// Intervalle minimal entre deux frames encodées, déduit de
    /// `[rtsp] fps`.
    pub(super) frame_interval: Duration,
    /// Largeur de la frame telle que la fournit la caméra.
    ///
    /// Distincte de celle de l'encodeur, qui arrondit à l'entier pair : c'est
    /// la largeur SOURCE qui donne le pas des lignes d'un buffer YUYV, et les
    /// confondre cisaillerait l'image sur une caméra à résolution impaire.
    pub(super) source_width: u32,
    /// Cadence annoncée aux fichiers MP4 produits.
    pub(super) fps: u32,
}

/// Ce que la boucle de capture doit savoir du reste du système, regroupé
/// pour ne pas prolonger indéfiniment la liste d'arguments de [`run`].
pub(super) struct Pipeline {
    pub(super) mailer: Mailer,
    pub(super) email_cooldown_secs: u64,
    pub(super) detect_tx: mpsc::SyncSender<RgbImage>,
    pub(super) last_boxes: Arc<Mutex<Vec<BoundingBox>>>,
    pub(super) face_detector: Arc<Option<FaceDetectorYuNet>>,
    pub(super) face_embedder: Arc<Option<FaceEmbedder>>,
    pub(super) known_people: Arc<Mutex<Vec<KnownPerson>>>,
    pub(super) known_faces_dir: String,
    /// Enregistreur de clips, partagé avec le thread de reconnaissance qui
    /// les déclenche.
    pub(super) clips: Arc<Mutex<ClipRecorder>>,
    pub(super) h264: Option<H264Output>,
}

/// Boucle infinie de lecture V4L2 et de diffusion (voir
/// `super::start_camera_loop`). Ne retourne qu'en cas d'erreur fatale à la
/// capture.
pub(super) fn run(
    state: Arc<SharedState>,
    mut stream: MmapStream<'_>,
    width: u32,
    height: u32,
    mut pipeline: Pipeline,
) -> Result<()> {
    let email_cooldown = Duration::from_secs(pipeline.email_cooldown_secs);

    let mut recording: Option<RecordingWriter> = None;
    let mut last_email_time = Instant::now() - email_cooldown;
    let mut last_encoded: Option<Instant> = None;

    // Format des enregistrements. Connu d'emblée sans encodage ; en H.264, il
    // attend la première image clé et ses jeux de paramètres.
    let mut recording_format: Option<RecordingFormat> =
        pipeline.h264.is_none().then_some(RecordingFormat::Mjpeg);

    info!("📹 Boucle Caméra démarrée avec succès.");

    loop {
        // Capture du buffer brut depuis V4L2
        let (buf, _) = stream.next()?;

        let is_detection_active = state.detection_enabled.load(Ordering::Relaxed);
        let is_recording_active = state.recording_enabled.load(Ordering::Relaxed);

        // Détection du format (PC en MJPEG vs Raspberry Pi en YUYV)
        let is_jpeg = buf.len() > 2 && buf[0] == 0xFF && buf[1] == 0xD8;

        // Faut-il une frame H.264 ?
        //
        // Deux conditions, et les deux comptent.
        //
        // D'ABORD, quelqu'un doit en avoir l'usage. L'encodage logiciel est
        // la dépense la plus lourde du système : personne ne doit la payer
        // pour un flux que personne ne regarde. Trois usages possibles :
        //
        //  - un lecteur abonné (RTSP, ou un onglet sur une interface web) ;
        //  - un enregistrement continu en cours ;
        //  - la surveillance active, qui peut déclencher un clip à tout
        //    instant — et un clip doit pouvoir montrer ce qui a PRÉCÉDÉ la
        //    détection, donc son pré-enregistrement doit déjà exister.
        //
        // ENSUITE, la cadence configurée doit être échue : les frames en trop
        // sont écartées avant l'encodeur.
        let h264_is_wanted = pipeline.h264.as_ref().is_some_and(|output| {
            output.stream.has_viewers() || is_recording_active || is_detection_active
        });

        let wants_h264 = h264_is_wanted
            && pipeline.h264.as_ref().is_some_and(|output| {
                last_encoded.is_none_or(|last| last.elapsed() >= output.frame_interval)
            });

        // DÉCODAGE, au plus une fois par frame.
        //
        // - détection active : le pipeline de vision travaille sur du RGB ;
        // - source non JPEG : le flux WebSocket, lui, diffuse du JPEG ;
        // - lecteur RTSP sur une source JPEG : l'encodeur H.264 a besoin des
        //   pixels, qui ne sont nulle part ailleurs.
        //
        // Hors de ces cas — webcam MJPEG, détection éteinte, aucun lecteur —
        // les octets de la caméra traversent la boucle sans être touchés.
        let needs_decode = is_detection_active || !is_jpeg || wants_h264;

        let decoded = if needs_decode {
            decode_frame(buf, is_jpeg, width, height)
        } else {
            None
        };

        let (jpeg_bytes, decoded) = if is_detection_active {
            prepare_detected_frame(
                buf,
                decoded,
                &state,
                &mut pipeline,
                &mut last_email_time,
                email_cooldown,
            )
        } else {
            pipeline.last_boxes.lock_or_recover().clear();

            (encode_jpeg_or_passthrough(buf, decoded.as_ref()), decoded)
        };

        // ENCODAGE H.264
        //
        // Avant les clips et l'enregistrement : ce sont eux qui consomment la
        // frame encodée quand l'encodage est actif.
        let encoded = if wants_h264 {
            last_encoded = Some(Instant::now());
            encode_h264_frame(&mut pipeline, buf, decoded.as_ref(), is_jpeg)
        } else {
            None
        };

        // Le format des enregistrements se décide à la première image clé :
        // c'est elle qui porte les jeux de paramètres dont un MP4 a besoin
        // pour s'ouvrir.
        if let (Some(unit), Some(output)) = (&encoded, pipeline.h264.as_ref())
            && recording_format.is_none()
        {
            let (width, height) = output.encoder.dimensions();
            recording_format = RecordingFormat::from_keyframe(unit, width, height, output.fps);

            if let Some(format) = &recording_format {
                pipeline.clips.lock_or_recover().set_format(format.clone());
            }
        }

        // CLIPS D'ÉVÉNEMENT
        //
        // Le tampon de pré-enregistrement doit contenir la frame même si un
        // clip est déclenché par le thread de reconnaissance dans l'instant
        // qui suit (voir `super::clips`).
        //
        // `is_detection_active` arme le pré-enregistrement : surveillance
        // éteinte, aucun événement ne peut survenir, et le tampon est rendu à
        // la mémoire du Raspberry Pi.
        {
            let mut clips = pipeline.clips.lock_or_recover();

            match (&encoded, pipeline.h264.is_some()) {
                (Some(unit), _) => clips.push_h264(unit, is_detection_active),
                // Sans encodage, les clips restent au format historique.
                (None, false) => clips.push_jpeg(&jpeg_bytes, is_detection_active),
                // Encodage actif mais rien à encoder sur cette frame (cadence
                // non échue) : il n'y a rien à mettre au tampon, et surtout
                // pas une image JPEG que le fichier MP4 ne saurait pas
                // accueillir.
                (None, true) => {}
            }
        }

        if is_recording_active {
            recording = write_recording_frame(
                recording,
                &state.recordings_dir,
                recording_format.as_ref(),
                &encoded,
                &jpeg_bytes,
                pipeline.h264.is_some(),
            )?;
        } else if let Some(writer) = recording.take() {
            info!("💾 Arrêt de l'enregistrement.");

            if let Err(e) = writer.finish() {
                warn!("⚠️ Enregistrement mal refermé : {e}");
            }
        }

        let _ = state.tx.send(jpeg_bytes);
    }
}

/// Écrit une frame dans l'enregistrement continu, en l'ouvrant si besoin.
///
/// Retourne l'enregistrement, ouvert ou non : en MP4, il ne peut commencer
/// qu'une fois le format connu (donc à la première image clé), et la
/// surveillance peut très bien être activée avant.
fn write_recording_frame(
    recording: Option<RecordingWriter>,
    dir: &str,
    format: Option<&RecordingFormat>,
    encoded: &Option<crate::h264::AccessUnit>,
    jpeg_bytes: &[u8],
    h264: bool,
) -> Result<Option<RecordingWriter>> {
    // Ce qu'on a à écrire sur CETTE frame. En H.264, une frame non encodée
    // (cadence non échue) n'apporte rien.
    let frame = match (encoded, h264) {
        (Some(unit), _) => Frame::H264 {
            nals: &unit.nals,
            keyframe: unit.keyframe,
        },
        (None, false) => Frame::Jpeg(jpeg_bytes),
        (None, true) => return Ok(recording),
    };

    let Some(format) = format else {
        // Format pas encore connu : l'encodeur n'a pas produit d'image clé.
        // La prochaine arrive au plus tard à l'intervalle configuré.
        return Ok(recording);
    };

    let mut writer = match recording {
        Some(writer) => writer,
        None => {
            // Un enregistrement MP4 doit COMMENCER par une image clé : c'est
            // la seule sur laquelle un décodeur peut démarrer.
            if h264 && !matches!(frame, Frame::H264 { keyframe: true, .. }) {
                return Ok(None);
            }

            RecordingWriter::create(dir, format)?
        }
    };

    if let Err(e) = writer.write_frame(frame) {
        warn!("⚠️ Écriture de l'enregistrement interrompue : {e}");
    }

    Ok(Some(writer))
}

/// Décode une frame caméra en RGB, quel que soit son format d'origine.
fn decode_frame(buf: &[u8], is_jpeg: bool, width: u32, height: u32) -> Option<RgbImage> {
    if is_jpeg {
        image::load_from_memory(buf).ok().map(|i| i.to_rgb8())
    } else {
        decode_yuyv_to_rgb(buf, width, height)
    }
}

/// Traite une frame pendant que la détection est active : enrôlement à
/// chaud, transmission au worker, incrustation des boîtes et alerte e-mail.
///
/// Retourne les octets JPEG à diffuser, et l'image décodée (rendue à
/// l'appelant pour l'encodage H.264, afin que le flux RTSP montre les mêmes
/// boîtes que le flux WebSocket).
fn prepare_detected_frame(
    buf: &[u8],
    decoded: Option<RgbImage>,
    state: &Arc<SharedState>,
    pipeline: &mut Pipeline,
    last_email_time: &mut Instant,
    email_cooldown: Duration,
) -> (Vec<u8>, Option<RgbImage>) {
    let Some(mut img) = decoded else {
        return (buf.to_vec(), None);
    };

    // Capture de photo de référence (enrôlement à chaud), voir
    // `try_capture_reference`.
    try_capture_reference(
        state,
        &img,
        &pipeline.face_detector,
        &pipeline.face_embedder,
        &pipeline.known_people,
        &pipeline.known_faces_dir,
    );

    let _ = pipeline.detect_tx.try_send(img.clone());

    let current_boxes = pipeline.last_boxes.lock_or_recover().clone();

    if !current_boxes.is_empty() {
        // Une personne reconnue (visage identifié) n'est pas une
        // intrusion : on n'alerte par e-mail que s'il reste au
        // moins une détection non reconnue (personne inconnue,
        // chat ou chien) dans la frame (voir `draw_detections`).
        let should_alert = draw_detections(&mut img, &current_boxes);

        if should_alert && last_email_time.elapsed() >= email_cooldown {
            let mut alert_encoded = Vec::new();
            let mut cursor = std::io::Cursor::new(&mut alert_encoded);
            if img.write_to(&mut cursor, ImageFormat::Jpeg).is_ok() {
                pipeline.mailer.send_alert(alert_encoded);
                *last_email_time = Instant::now();
            }
        }
    }

    (encode_jpeg_or_passthrough(buf, Some(&img)), Some(img))
}

/// Encode une image en JPEG, en se rabattant sur les octets d'origine si
/// l'encodage échoue ou s'il n'y a pas d'image décodée.
fn encode_jpeg_or_passthrough(buf: &[u8], decoded: Option<&RgbImage>) -> Vec<u8> {
    let Some(img) = decoded else {
        return buf.to_vec();
    };

    let mut encoded = Vec::new();
    let mut cursor = std::io::Cursor::new(&mut encoded);

    if img.write_to(&mut cursor, ImageFormat::Jpeg).is_ok() {
        encoded
    } else {
        buf.to_vec()
    }
}

/// Encode une frame en H.264 et la publie aux lecteurs RTSP.
///
/// Deux chemins d'entrée, et le plus économique est préféré (voir
/// `crate::h264::I420Buffer`) :
///
/// - **image décodée** quand on en a une. Elle porte alors les boîtes de
///   détection déjà incrustées, et c'est voulu : le flux RTSP montre
///   exactement ce que montre l'interface embarquée ;
/// - **buffer YUYV brut** sinon — détection éteinte sur le Raspberry Pi. La
///   conversion vers le format de l'encodeur n'est alors qu'un
///   sous-échantillonnage, sans la moindre arithmétique de couleur, et il n'y
///   a de toute façon aucune boîte à montrer.
fn encode_h264_frame(
    pipeline: &mut Pipeline,
    buf: &[u8],
    decoded: Option<&RgbImage>,
    is_jpeg: bool,
) -> Option<crate::h264::AccessUnit> {
    let output = pipeline.h264.as_mut()?;

    // Une session RTSP qui démarre, ou un lecteur qui a décroché, réclame une
    // image clé : c'est la seule frame sur laquelle un décodeur peut
    // (re)partir.
    if output.stream.take_keyframe_request() {
        output.encoder.request_keyframe();
    }

    let encoded = match decoded {
        Some(img) => output.encoder.encode_rgb(img),
        None if !is_jpeg => output.encoder.encode_yuyv(buf, output.source_width),
        // Source JPEG qu'on n'a pas su décoder : il n'y a rien à encoder.
        None => Ok(None),
    };

    match encoded {
        Ok(Some(unit)) => {
            let parameters = output.encoder.parameters().cloned();
            // Publié aux abonnés (RTSP, interfaces web) ET rendu à la boucle,
            // qui s'en sert pour les clips et l'enregistrement : l'encodage a
            // lieu une fois, quel que soit le nombre de consommateurs.
            output.stream.publish(unit.clone(), parameters.as_ref());
            Some(unit)
        }
        Ok(None) => None,
        Err(e) => {
            warn!("⚠️ Encodage H.264 interrompu pour cette frame : {e:#}");
            None
        }
    }
}
