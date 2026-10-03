//! Génération du descripteur SDP (RFC 4566) renvoyé en réponse à un
//! `DESCRIBE` RTSP.
//!
//! C'est la carte de visite du flux : elle dit au lecteur qu'il s'agit de
//! vidéo H.264, sur quelle horloge, et — crucialement — lui donne les
//! paramètres de décodage (SPS/PPS) avant même la première image, ce qui lui
//! évite d'attendre la prochaine image clé pour afficher quelque chose.

use base64::Engine as _;

use crate::h264::ParameterSets;

use super::rtp::{H264_CLOCK_RATE, H264_PAYLOAD_TYPE};

/// Construit le SDP d'un flux H.264.
///
/// `parameters` est `None` tant que l'encodeur n'a pas produit sa première
/// frame : le SDP est alors servi SANS `sprop-parameter-sets`. Ce n'est pas
/// bloquant — les paramètres sont aussi transmis en ligne avec chaque image
/// clé (voir `SpsPpsStrategy::IncreasingId` dans `crate::h264`) — mais le
/// lecteur restera noir jusqu'à la première d'entre elles.
pub fn describe_stream(session_name: &str, parameters: Option<&ParameterSets>) -> String {
    let mut sdp = String::new();

    // v= : version du format SDP, toujours 0.
    sdp.push_str("v=0\r\n");
    // o= : origine. L'identifiant de session et sa version sont arbitraires ;
    // `IN IP4 0.0.0.0` dit « l'adresse réelle, c'est celle par laquelle tu
    // m'as joint », ce qui est le seul choix correct pour une caméra qui
    // écoute sur toutes ses interfaces et ignore par quelle adresse on
    // l'atteint.
    sdp.push_str("o=- 0 0 IN IP4 0.0.0.0\r\n");
    sdp.push_str(&format!("s={}\r\n", sanitize_line(session_name)));
    sdp.push_str("c=IN IP4 0.0.0.0\r\n");
    // t= : bornes temporelles de la session. 0 0 = permanente, ce qu'est un
    // flux de vidéosurveillance.
    sdp.push_str("t=0 0\r\n");
    // a=recvonly : le client reçoit et n'émet rien. Sans cela certains
    // lecteurs réservent des ressources d'émission pour rien.
    sdp.push_str("a=recvonly\r\n");
    // a=control:* : l'URL de contrôle de la session est celle du DESCRIBE.
    sdp.push_str("a=control:*\r\n");

    // m= : la piste média. `RTP/AVP` (et non `RTP/AVPF`) : pas de retour
    // d'information immédiat du client, ce que ce serveur ne gère pas.
    sdp.push_str(&format!("m=video 0 RTP/AVP {H264_PAYLOAD_TYPE}\r\n"));
    sdp.push_str(&format!(
        "a=rtpmap:{H264_PAYLOAD_TYPE} H264/{H264_CLOCK_RATE}\r\n"
    ));

    // packetization-mode=1 : mode « non entrelacé », celui qui autorise la
    // fragmentation FU-A (voir `super::rtp`). Le mode 0 interdirait les NAL
    // dépassant un datagramme, donc toute image clé un peu détaillée.
    let mut fmtp = format!("a=fmtp:{H264_PAYLOAD_TYPE} packetization-mode=1");

    if let Some(parameters) = parameters {
        let engine = base64::engine::general_purpose::STANDARD;
        fmtp.push_str(&format!(
            ";sprop-parameter-sets={},{}",
            engine.encode(&parameters.sps),
            engine.encode(&parameters.pps)
        ));

        // profile-level-id : les trois premiers octets utiles du SPS
        // (profil, contraintes, niveau) en hexadécimal. Certains lecteurs
        // s'en servent pour décider s'ils savent décoder AVANT de recevoir
        // le moindre paquet.
        if let Some(profile_level) = profile_level_id(&parameters.sps) {
            fmtp.push_str(&format!(";profile-level-id={profile_level}"));
        }
    }

    fmtp.push_str("\r\n");
    sdp.push_str(&fmtp);
    // La piste porte le nom que le client reprendra dans son SETUP.
    sdp.push_str("a=control:trackID=0\r\n");

    sdp
}

/// `profile-level-id` tel qu'attendu par RFC 6184 §8.1 : les octets
/// `profile_idc`, `constraint_flags` et `level_idc` du SPS, en hexadécimal.
///
/// Ils suivent immédiatement l'octet d'en-tête de la NAL.
fn profile_level_id(sps: &[u8]) -> Option<String> {
    let bytes = sps.get(1..4)?;
    Some(format!("{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2]))
}

/// Neutralise les retours à la ligne d'une valeur insérée dans le SDP.
///
/// Le nom de la caméra vient de sa configuration : un `\r\n` malencontreux y
/// injecterait des lignes SDP arbitraires, et un nom choisi pour ça pourrait
/// détourner la description du flux. Les lignes SDP étant délimitées par
/// CRLF, c'est tout ce qu'il y a à neutraliser.
fn sanitize_line(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();

    let cleaned = cleaned.trim();

    if cleaned.is_empty() {
        "FoxGuard".to_string()
    } else {
        cleaned.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SPS minimal plausible : en-tête 0x67, puis profil (66 = Baseline),
    /// contraintes, niveau (30 = 3.0).
    fn parameters() -> ParameterSets {
        ParameterSets {
            sps: vec![0x67, 0x42, 0xC0, 0x1E, 0xAB, 0xCD],
            pps: vec![0x68, 0xCE, 0x3C, 0x80],
        }
    }

    #[test]
    fn the_sdp_announces_an_h264_video_track_on_the_dynamic_payload_type() {
        let sdp = describe_stream("salon", Some(&parameters()));

        assert!(sdp.contains("m=video 0 RTP/AVP 96\r\n"), "{sdp}");
        assert!(sdp.contains("a=rtpmap:96 H264/90000\r\n"), "{sdp}");
    }

    #[test]
    fn every_sdp_line_ends_with_crlf() {
        // RFC 4566 : les lignes SDP sont terminées par CRLF. Un simple LF
        // fait échouer les lecteurs les plus stricts.
        let sdp = describe_stream("salon", Some(&parameters()));

        for line in sdp.split_inclusive('\n') {
            assert!(line.ends_with("\r\n"), "ligne sans CRLF : {line:?}");
        }
    }

    #[test]
    fn the_packetization_mode_allows_fragmentation() {
        // Mode 1 : indispensable, les images clés dépassent toujours un
        // datagramme (voir `super::rtp::MAX_PAYLOAD`).
        let sdp = describe_stream("salon", Some(&parameters()));
        assert!(sdp.contains("packetization-mode=1"), "{sdp}");
    }

    #[test]
    fn the_parameter_sets_are_carried_base64_encoded() {
        let sdp = describe_stream("salon", Some(&parameters()));

        let engine = base64::engine::general_purpose::STANDARD;
        let expected = format!(
            "sprop-parameter-sets={},{}",
            engine.encode(&parameters().sps),
            engine.encode(&parameters().pps)
        );

        assert!(sdp.contains(&expected), "{sdp}");
    }

    #[test]
    fn the_profile_level_id_is_read_from_the_sps() {
        // 0x42 0xC0 0x1E : Baseline, contrainte 1, niveau 3.0.
        let sdp = describe_stream("salon", Some(&parameters()));
        assert!(sdp.contains("profile-level-id=42c01e"), "{sdp}");
    }

    #[test]
    fn a_truncated_sps_yields_no_profile_level_id_rather_than_a_panic() {
        assert_eq!(profile_level_id(&[0x67, 0x42]), None);
    }

    #[test]
    fn a_describe_before_the_first_frame_omits_the_parameter_sets() {
        // L'encodeur ne les connaît qu'après sa première frame : le SDP doit
        // rester valide sans elles.
        let sdp = describe_stream("salon", None);

        assert!(!sdp.contains("sprop-parameter-sets"), "{sdp}");
        assert!(sdp.contains("packetization-mode=1"), "{sdp}");
        assert!(sdp.contains("m=video"), "{sdp}");
    }

    #[test]
    fn the_session_name_appears_in_the_description() {
        let sdp = describe_stream("entrée", Some(&parameters()));
        assert!(sdp.contains("s=entrée\r\n"), "{sdp}");
    }

    #[test]
    fn a_camera_name_cannot_inject_extra_sdp_lines() {
        // Le nom vient de la configuration : un CRLF y ferait passer des
        // lignes SDP arbitraires dans la description du flux.
        let sdp = describe_stream("salon\r\na=control:pirate", Some(&parameters()));

        // La charge utile ne devient pas une ligne à elle seule...
        assert!(!sdp.contains("\r\na=control:pirate\r\n"), "{sdp}");
        // ...et reste confinée dans le champ `s=`.
        let session_line = sdp
            .lines()
            .find(|line| line.starts_with("s="))
            .expect("ligne s=");
        assert!(session_line.contains("a=control:pirate"), "{session_line}");
    }

    #[test]
    fn an_empty_camera_name_falls_back_rather_than_producing_an_empty_field() {
        let sdp = describe_stream("   ", Some(&parameters()));
        assert!(sdp.contains("s=FoxGuard\r\n"), "{sdp}");
    }
}
