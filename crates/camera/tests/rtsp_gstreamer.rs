//! Validation du flux RTSP par un VRAI lecteur vidéo (GStreamer).
//!
//! `rtsp_integration.rs` vérifie que les octets émis ont la forme attendue.
//! C'est nécessaire mais pas suffisant : un flux peut être parfaitement
//! conforme à nos propres assertions et rester indécodable pour un lecteur
//! (SDP incomplet, paramètres absents, horloge incohérente, fragmentation
//! subtilement fausse). Seul un décodeur indépendant tranche.
//!
//! Le test est automatiquement IGNORÉ quand `gst-launch-1.0` n'est pas
//! installé — même principe que les tests PostgreSQL du manager : il ne doit
//! pas transformer une machine sans GStreamer en échec de compilation.

use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use foxguard_camera::config::RtspConfig;
use foxguard_camera::h264::{H264Encoder, H264Stream};
use foxguard_camera::rtsp;

const TOKEN: &str = "jeton-de-test";
// Petite résolution et cadence modeste : la conversion vers le format de
// l'encodeur tourne ici en profil de DÉBOGAGE (non optimisé), et le test doit
// rester une poignée de secondes.
const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const FPS: u32 = 15;

/// Nombre de frames que GStreamer doit décoder pour que le test soit
/// concluant.
///
/// Plus d'une : la première est l'image clé, et un flux dont SEULE l'image
/// clé serait décodable passerait inaperçu. Une vingtaine traverse une image
/// clé, des frames P, puis au moins une image clé périodique.
const FRAMES_TO_DECODE: u32 = 20;

/// Arrête l'alimentation du flux à la fin du test.
///
/// Sans elle, la tâche bloquante qui encode les frames tournerait
/// indéfiniment — et l'arrêt d'un runtime Tokio ATTEND ses tâches bloquantes
/// en cours. Le test réussissait alors sans jamais rendre la main.
struct StopFeeding(Arc<AtomicBool>);

impl Drop for StopFeeding {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Vrai si `gst-launch-1.0` et le décodeur H.264 sont disponibles.
fn gstreamer_available() -> bool {
    let Ok(output) = Command::new("gst-launch-1.0").arg("--version").output() else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    // `avdec_h264` et les éléments RTP viennent de paquets séparés
    // (gst-plugins-libav, gst-plugins-good) : leur absence rendrait l'échec
    // du test trompeur.
    ["avdec_h264", "rtph264depay", "rtspsrc"]
        .iter()
        .all(|element| {
            Command::new("gst-inspect-1.0")
                .arg(element)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
        })
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .expect("port libre")
        .local_addr()
        .expect("adresse locale")
        .port()
}

/// Remplit `image` d'une mire qui SE DÉPLACE d'une frame à l'autre.
///
/// Le mouvement n'est pas décoratif : une mire immobile serait écartée par le
/// contrôle de débit de l'encodeur, et le lecteur n'aurait rien à décoder.
///
/// Écrit dans un tampon réutilisé plutôt que de construire une image neuve :
/// en profil de débogage, `RgbImage::from_fn` à chaque frame domine le temps
/// du test.
fn fill_moving_frame(image: &mut image::RgbImage, frame: u32) {
    let width = image.width() as usize;

    for (row, line) in image.as_mut().chunks_exact_mut(width * 3).enumerate() {
        for (column, pixel) in line.chunks_exact_mut(3).enumerate() {
            let band = ((column as u32 + frame * 7) / 12) % 2;
            let value = if band == 0 { 40 } else { 210 };

            pixel[0] = value;
            pixel[1] = (row % 255) as u8;
            pixel[2] = 255 - value;
        }
    }
}

/// Démarre un serveur RTSP alimenté par un véritable encodeur H.264.
///
/// Retourne son port et le garde-fou qui arrête l'alimentation à la fin du
/// test (voir [`StopFeeding`]).
async fn start_encoded_stream() -> (u16, StopFeeding) {
    let port = free_port();
    let stream = Arc::new(H264Stream::new());

    rtsp::spawn(
        RtspConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            path: "stream".to_string(),
            require_token: true,
        },
        TOKEN.to_string(),
        "salon".to_string(),
        Arc::clone(&stream),
    );

    let stop = Arc::new(AtomicBool::new(false));

    let feeder = Arc::clone(&stream);
    let feeder_stop = Arc::clone(&stop);

    tokio::task::spawn_blocking(move || {
        let mut encoder = H264Encoder::new(WIDTH, HEIGHT, FPS, 800, 1).expect("encodeur H.264");
        let mut frame_buffer = image::RgbImage::new(WIDTH, HEIGHT);
        let frame_period = Duration::from_millis(1_000 / u64::from(FPS));

        for frame in 0.. {
            if feeder_stop.load(Ordering::Relaxed) {
                return;
            }

            if feeder.take_keyframe_request() {
                encoder.request_keyframe();
            }

            fill_moving_frame(&mut frame_buffer, frame);

            if let Ok(Some(unit)) = encoder.encode_rgb(&frame_buffer) {
                let parameters = encoder.parameters().cloned();
                feeder.publish(unit, parameters.as_ref());
            }

            std::thread::sleep(frame_period);
        }
    });

    // Laisse l'écoute s'ouvrir.
    for _ in 0..100 {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    (port, StopFeeding(stop))
}

/// Délai au-delà duquel on considère que le décodeur n'y arrivera pas.
///
/// Un flux correct donne ses vingt frames en deux secondes. Ce plafond est là
/// pour qu'un serveur qui n'émet RIEN fasse échouer le test, avec la sortie du
/// décodeur sous les yeux, au lieu de suspendre la suite indéfiniment.
const DECODE_TIMEOUT: Duration = Duration::from_secs(25);

/// Fait décoder le flux par GStreamer et retourne sa sortie.
///
/// `fakesink num-buffers=…` termine le pipeline sur un EOS dès que le nombre
/// de frames DÉCODÉES est atteint : un code de retour nul est donc exactement
/// l'affirmation « un décodeur indépendant a lu ce flux ».
fn decode_with_gstreamer(port: u16, protocol: &str, frames: u32) -> std::process::Output {
    let mut child = Command::new("gst-launch-1.0")
        .arg("-q")
        .arg("rtspsrc")
        .arg(format!(
            "location=rtsp://127.0.0.1:{port}/stream?token={TOKEN}"
        ))
        .arg(format!("protocols={protocol}"))
        .arg("latency=0")
        .arg("timeout=5000000")
        .arg("!")
        .arg("rtph264depay")
        .arg("!")
        .arg("h264parse")
        .arg("!")
        .arg("avdec_h264")
        .arg("!")
        .arg("fakesink")
        .arg(format!("num-buffers={frames}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("lancement de gst-launch-1.0");

    let deadline = std::time::Instant::now() + DECODE_TIMEOUT;

    while std::time::Instant::now() < deadline {
        match child.try_wait().expect("état de gst-launch-1.0") {
            Some(_) => break,
            None => std::thread::sleep(Duration::from_millis(100)),
        }
    }

    // Toujours en vie passé le délai : on le termine, et sa sortie partielle
    // dira pourquoi il attendait.
    if child.try_wait().expect("état de gst-launch-1.0").is_none() {
        let _ = child.kill();
    }

    child.wait_with_output().expect("sortie de gst-launch-1.0")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gstreamer_decodes_the_stream_over_tcp() {
    if !gstreamer_available() {
        eprintln!("gst-launch-1.0 (ou ses greffons H.264) absent : test ignoré.");
        return;
    }

    // `_stop` doit rester vivant jusqu'à la fin du test : c'est sa
    // destruction qui arrête l'alimentation du flux.
    let (port, _stop) = start_encoded_stream().await;

    let output =
        tokio::task::spawn_blocking(move || decode_with_gstreamer(port, "tcp", FRAMES_TO_DECODE))
            .await
            .expect("tâche de décodage");

    assert!(
        output.status.success(),
        "GStreamer n'a pas pu décoder le flux en TCP entrelacé.\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn gstreamer_decodes_the_stream_over_udp() {
    if !gstreamer_available() {
        eprintln!("gst-launch-1.0 (ou ses greffons H.264) absent : test ignoré.");
        return;
    }

    // `_stop` doit rester vivant jusqu'à la fin du test : c'est sa
    // destruction qui arrête l'alimentation du flux.
    let (port, _stop) = start_encoded_stream().await;

    let output =
        tokio::task::spawn_blocking(move || decode_with_gstreamer(port, "udp", FRAMES_TO_DECODE))
            .await
            .expect("tâche de décodage");

    assert!(
        output.status.success(),
        "GStreamer n'a pas pu décoder le flux en UDP.\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
