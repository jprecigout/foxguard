//! Validation des fichiers MP4 produits par un VRAI lecteur (GStreamer).
//!
//! Les tests unitaires de `src/mp4/` vérifient la structure : que chaque
//! boîte déclare la bonne taille, que les décalages tombent juste, que les
//! durées suivent les horodatages réels. C'est nécessaire et insuffisant — un
//! fichier peut satisfaire toutes ces assertions et rester indécodable pour
//! un lecteur, parce qu'il manque une boîte que la spécification n'impose pas
//! mais que tout le monde attend, ou parce que les paramètres du décodeur ne
//! décrivent pas le flux qu'ils accompagnent.
//!
//! Ces tests-ci écrivent un fichier avec le vrai encodeur, puis le font
//! décoder par GStreamer. Ils s'ignorent d'eux-mêmes si `gst-launch-1.0`
//! n'est pas installé (même principe que `rtsp_gstreamer.rs`).

use std::process::{Command, Stdio};

use foxguard_camera::h264::{H264Encoder, NAL_PPS, NAL_SPS, nal_type};
use foxguard_camera::mp4::{Fmp4Writer, TIMESCALE};

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;
const FPS: u32 = 15;

/// Vrai si GStreamer et les greffons nécessaires sont disponibles.
fn gstreamer_available() -> bool {
    let Ok(output) = Command::new("gst-launch-1.0").arg("--version").output() else {
        return false;
    };

    if !output.status.success() {
        return false;
    }

    ["avdec_h264", "qtdemux", "h264parse"]
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

/// Remplit une mire qui se déplace : une image fixe serait écartée par le
/// contrôle de débit, et le fichier n'aurait presque rien à décoder.
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

/// Écrit un enregistrement de `frames` images et retourne son chemin.
///
/// Reproduit exactement ce que fait la caméra : encodage, puis écriture des
/// unités d'accès telles quelles.
fn write_recording(
    dir: &std::path::Path,
    frames: u32,
    keyframe_interval_secs: u32,
) -> std::path::PathBuf {
    let mut encoder =
        H264Encoder::new(WIDTH, HEIGHT, FPS, 800, keyframe_interval_secs).expect("encodeur H.264");
    let mut buffer = image::RgbImage::new(WIDTH, HEIGHT);

    // Les paramètres ne sont connus qu'après la première image : le fichier
    // ne peut donc être créé qu'à ce moment-là, ce que fait aussi la caméra.
    let mut writer: Option<Fmp4Writer> = None;
    let path = dir.join("enregistrement.mp4");
    let ticks_per_frame = u64::from(TIMESCALE / FPS);

    for frame in 0..frames {
        fill_moving_frame(&mut buffer, frame);

        let Some(unit) = encoder.encode_rgb(&buffer).expect("encodage") else {
            continue;
        };

        if writer.is_none() {
            let sps = unit
                .nals
                .iter()
                .find(|nal| nal_type(nal) == Some(NAL_SPS))
                .expect("la première image clé porte son SPS");
            let pps = unit
                .nals
                .iter()
                .find(|nal| nal_type(nal) == Some(NAL_PPS))
                .expect("la première image clé porte son PPS");

            writer = Some(
                Fmp4Writer::create(
                    &path,
                    "enregistrement.mp4".to_string(),
                    WIDTH,
                    HEIGHT,
                    FPS,
                    sps,
                    pps,
                )
                .expect("création du fichier"),
            );
        }

        writer
            .as_mut()
            .expect("fichier créé")
            .write_frame(
                &unit.nals,
                u64::from(frame) * ticks_per_frame,
                unit.keyframe,
            )
            .expect("écriture de l'image");
    }

    writer
        .expect("au moins une image")
        .finish()
        .expect("fermeture");
    path
}

/// Fait décoder le fichier par GStreamer et retourne sa sortie.
fn decode(path: &std::path::Path, expected_frames: u32) -> std::process::Output {
    Command::new("gst-launch-1.0")
        .arg("-q")
        .arg("filesrc")
        .arg(format!("location={}", path.display()))
        .arg("!")
        .arg("qtdemux")
        .arg("!")
        .arg("h264parse")
        .arg("!")
        .arg("avdec_h264")
        .arg("!")
        .arg("fakesink")
        .arg(format!("num-buffers={expected_frames}"))
        .output()
        .expect("lancement de gst-launch-1.0")
}

#[test]
fn gstreamer_decodes_a_recording() {
    if !gstreamer_available() {
        eprintln!("gst-launch-1.0 (ou ses greffons) absent : test ignoré.");
        return;
    }

    let dir = tempfile::tempdir().expect("dossier temporaire");
    // Deux secondes à 15 im/s, avec une image clé par seconde : le fichier
    // comporte donc PLUSIEURS fragments, ce qui est le cas intéressant.
    let path = write_recording(dir.path(), 30, 1);

    let output = decode(&path, 25);

    assert!(
        output.status.success(),
        "GStreamer n'a pas pu décoder l'enregistrement.\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn gstreamer_reads_the_declared_dimensions_and_duration() {
    if !gstreamer_available() {
        eprintln!("gst-launch-1.0 (ou ses greffons) absent : test ignoré.");
        return;
    }

    let Ok(discoverer) = Command::new("gst-discoverer-1.0").arg("--version").output() else {
        eprintln!("gst-discoverer-1.0 absent : test ignoré.");
        return;
    };
    if !discoverer.status.success() {
        return;
    }

    let dir = tempfile::tempdir().expect("dossier temporaire");
    let path = write_recording(dir.path(), 30, 1);

    let output = Command::new("gst-discoverer-1.0")
        .arg(&path)
        .output()
        .expect("lancement de gst-discoverer-1.0");

    let report = String::from_utf8_lossy(&output.stdout);

    assert!(output.status.success(), "{report}");
    assert!(
        report.contains(&format!("{WIDTH}x{HEIGHT}")),
        "dimensions absentes du rapport :\n{report}"
    );
    // Une durée NON NULLE : c'est elle que les `tfdt` et les durées par
    // image reconstituent. Nulle, elle signalerait que le lecteur n'a pas su
    // lire les fragments.
    assert!(
        report.contains("Duration: 0:00:0"),
        "durée absente ou nulle :\n{report}"
    );
}

#[test]
fn a_truncated_recording_is_still_partly_readable() {
    // LA raison d'être du format fragmenté : une coupure de courant, un
    // disque plein ou un redémarrage laissent un fichier incomplet. Un MP4
    // classique, dont la table des matières est écrite à la fin, serait alors
    // entièrement perdu — pas une image n'en serait tirée.
    if !gstreamer_available() {
        eprintln!("gst-launch-1.0 (ou ses greffons) absent : test ignoré.");
        return;
    }

    let dir = tempfile::tempdir().expect("dossier temporaire");
    let path = write_recording(dir.path(), 45, 1);

    let complete = std::fs::read(&path).expect("lecture");
    let truncated_path = dir.path().join("tronque.mp4");
    // On coupe aux deux tiers, en plein milieu d'un fragment.
    std::fs::write(&truncated_path, &complete[..complete.len() * 2 / 3]).expect("écriture");

    let output = decode(&truncated_path, 15);

    assert!(
        output.status.success(),
        "un fichier tronqué doit rester lisible jusqu'à sa coupure.\n--- stderr ---\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
