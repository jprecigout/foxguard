//! Dialogue RTSP de bout en bout, sur une VRAIE connexion TCP.
//!
//! Les tests unitaires de `src/rtsp/` vérifient chaque pièce isolément
//! (analyse des messages, négociation du transport, empaquetage RTP). Ils ne
//! disent rien de leur enchaînement, qui est précisément là où un serveur
//! RTSP se casse : un en-tête `Session` oublié, un `SETUP` accepté avant le
//! `DESCRIBE`, une réponse envoyée sans `CSeq`, et le lecteur abandonne sans
//! que rien n'ait l'air anormal côté serveur.
//!
//! Ces tests-ci parlent donc au serveur comme le ferait VLC : ils ouvrent une
//! chaussette, enchaînent `OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, puis
//! vérifient que des paquets RTP arrivent réellement.
//!
//! Le client est volontairement SYNCHRONE (`std::net::TcpStream`) : un test
//! qui attend bêtement sur `read` décrit bien mieux la séquence attendue
//! qu'un enchaînement de `await`. D'où le
//! `#[tokio::test(flavor = "multi_thread")]` sur chacun : sur l'exécuteur
//! mono-thread par défaut, le test bloquerait le seul thread disponible et le
//! serveur n'aurait jamais la main pour répondre.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use foxguard_camera::config::RtspConfig;
use foxguard_camera::h264::{AccessUnit, H264Stream, ParameterSets};
use foxguard_camera::rtsp;

const TOKEN: &str = "jeton-de-test";

/// Un port libre sur la boucle locale.
///
/// Obtenu en ouvrant puis refermant une chaussette : le serveur RTSP ne
/// retourne pas le port sur lequel il a écouté (il est lancé en tâche de
/// fond et ne doit pas faire échouer le démarrage de la caméra), il faut donc
/// lui en imposer un.
fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("port libre");
    listener.local_addr().expect("adresse locale").port()
}

/// Démarre un serveur RTSP de test et retourne son flux et son port.
async fn start_server(require_token: bool) -> (Arc<H264Stream>, u16) {
    let port = free_port();
    let stream = Arc::new(H264Stream::new());

    rtsp::spawn(
        RtspConfig {
            enabled: true,
            host: "127.0.0.1".to_string(),
            port,
            path: "stream".to_string(),
            require_token,
        },
        TOKEN.to_string(),
        "salon".to_string(),
        Arc::clone(&stream),
    );

    // Laisse le temps à la tâche de fond d'ouvrir son écoute.
    for _ in 0..100 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    (stream, port)
}

/// Un client RTSP minimal, qui sait aussi reconnaître les paquets
/// entrelacés.
struct Client {
    socket: TcpStream,
    buffer: Vec<u8>,
}

impl Client {
    fn connect(port: u16) -> Self {
        let socket = TcpStream::connect(("127.0.0.1", port)).expect("connexion RTSP");
        socket
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("délai de lecture");

        Self {
            socket,
            buffer: Vec::new(),
        }
    }

    fn send(&mut self, request: &str) {
        self.socket
            .write_all(request.as_bytes())
            .expect("envoi de la requête");
    }

    fn fill(&mut self) {
        let mut chunk = [0u8; 8192];
        let read = self.socket.read(&mut chunk).expect("lecture de la réponse");
        assert_ne!(read, 0, "connexion fermée par le serveur");
        self.buffer.extend_from_slice(&chunk[..read]);
    }

    /// Lit la prochaine réponse RTSP, en sautant les paquets entrelacés qui
    /// pourraient la précéder.
    fn read_response(&mut self) -> String {
        loop {
            if self.buffer.first() == Some(&b'$') {
                if self.buffer.len() < 4 {
                    self.fill();
                    continue;
                }

                let length = usize::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]]));

                if self.buffer.len() < 4 + length {
                    self.fill();
                    continue;
                }

                self.buffer.drain(..4 + length);
                continue;
            }

            if let Some(end) = self
                .buffer
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
            {
                let head = String::from_utf8(self.buffer[..end + 4].to_vec()).expect("UTF-8");

                let body_length = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("Content-Length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);

                if self.buffer.len() < end + 4 + body_length {
                    self.fill();
                    continue;
                }

                let message = String::from_utf8(self.buffer[..end + 4 + body_length].to_vec())
                    .expect("UTF-8");
                self.buffer.drain(..end + 4 + body_length);

                return message;
            }

            self.fill();
        }
    }

    /// Lit le prochain paquet RTP, en sautant les rapports RTCP.
    ///
    /// Le serveur émet un rapport par seconde sur le canal apparié : un test
    /// qui porte sur la vidéo doit les ignorer, comme le fait un lecteur.
    fn read_rtp(&mut self) -> Vec<u8> {
        for _ in 0..50 {
            let (channel, packet) = self.read_interleaved();

            if channel == 0 {
                return packet;
            }
        }

        panic!("aucun paquet RTP parmi les paquets entrelacés reçus");
    }

    /// Lit le prochain paquet entrelacé : (canal, charge utile).
    fn read_interleaved(&mut self) -> (u8, Vec<u8>) {
        loop {
            if self.buffer.len() >= 4 && self.buffer[0] == b'$' {
                let length = usize::from(u16::from_be_bytes([self.buffer[2], self.buffer[3]]));

                if self.buffer.len() >= 4 + length {
                    let channel = self.buffer[1];
                    let payload = self.buffer[4..4 + length].to_vec();
                    self.buffer.drain(..4 + length);

                    return (channel, payload);
                }
            }

            self.fill();
        }
    }

    /// Enchaîne le dialogue complet jusqu'au `PLAY`.
    fn play(&mut self, port: u16, token: Option<&str>) {
        let url = match token {
            Some(token) => format!("rtsp://127.0.0.1:{port}/stream?token={token}"),
            None => format!("rtsp://127.0.0.1:{port}/stream"),
        };

        self.send(&format!(
            "DESCRIBE {url} RTSP/1.0\r\nCSeq: 2\r\nAccept: application/sdp\r\n\r\n"
        ));
        assert!(self.read_response().starts_with("RTSP/1.0 200"));

        self.send(&format!(
            "SETUP {url} RTSP/1.0\r\nCSeq: 3\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
        ));
        let setup = self.read_response();
        assert!(setup.starts_with("RTSP/1.0 200"), "{setup}");

        let session = session_of(&setup);

        self.send(&format!(
            "PLAY {url} RTSP/1.0\r\nCSeq: 4\r\nSession: {session}\r\nRange: npt=0.000-\r\n\r\n"
        ));
        let play = self.read_response();
        assert!(play.starts_with("RTSP/1.0 200"), "{play}");
    }
}

/// Valeur de l'en-tête `Session` d'une réponse, sans ses paramètres.
fn session_of(response: &str) -> String {
    response
        .lines()
        .find_map(|line| line.strip_prefix("Session:"))
        .map(|value| {
            value
                .trim()
                .split(';')
                .next()
                .unwrap_or_default()
                .to_string()
        })
        .expect("en-tête Session")
}

fn access_unit(timestamp: u32, keyframe: bool, payload_size: usize) -> AccessUnit {
    let mut slice = vec![if keyframe { 0x65 } else { 0x41 }];
    slice.extend((0..payload_size).map(|i| (i % 251) as u8));

    AccessUnit {
        nals: if keyframe {
            vec![vec![0x67, 0x42, 0xC0, 0x1E], vec![0x68, 0xCE], slice]
        } else {
            vec![slice]
        },
        keyframe,
        rtp_timestamp: timestamp,
    }
}

fn parameters() -> ParameterSets {
    ParameterSets {
        sps: vec![0x67, 0x42, 0xC0, 0x1E],
        pps: vec![0x68, 0xCE],
    }
}

// --- Dialogue ---

#[tokio::test(flavor = "multi_thread")]
async fn options_lists_the_methods_a_player_needs() {
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    let response = client.read_response();

    assert!(response.starts_with("RTSP/1.0 200 OK"), "{response}");
    assert!(response.contains("CSeq: 1"), "{response}");

    for method in ["DESCRIBE", "SETUP", "PLAY", "TEARDOWN"] {
        assert!(response.contains(method), "{method} absent de : {response}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn options_is_answered_without_a_token() {
    // C'est la requête par laquelle un lecteur découvre le serveur, et son
    // URI est souvent `*` — donc sans jeton possible.
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send("OPTIONS * RTSP/1.0\r\nCSeq: 1\r\n\r\n");
    assert!(client.read_response().starts_with("RTSP/1.0 200"));
}

#[tokio::test(flavor = "multi_thread")]
async fn describe_without_a_token_is_refused() {
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "DESCRIBE rtsp://127.0.0.1:{port}/stream RTSP/1.0\r\nCSeq: 2\r\n\r\n"
    ));
    let response = client.read_response();

    assert!(response.starts_with("RTSP/1.0 401"), "{response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn describe_with_a_wrong_token_is_refused() {
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "DESCRIBE rtsp://127.0.0.1:{port}/stream?token=pas-le-bon RTSP/1.0\r\nCSeq: 2\r\n\r\n"
    ));
    assert!(client.read_response().starts_with("RTSP/1.0 401"));
}

#[tokio::test(flavor = "multi_thread")]
async fn describe_returns_an_sdp_describing_an_h264_stream() {
    let (stream, port) = start_server(true).await;
    stream.publish(access_unit(0, true, 100), Some(&parameters()));

    let mut client = Client::connect(port);
    client.send(&format!(
        "DESCRIBE rtsp://127.0.0.1:{port}/stream?token={TOKEN} RTSP/1.0\r\nCSeq: 2\r\n\r\n"
    ));
    let response = client.read_response();

    assert!(
        response.contains("Content-Type: application/sdp"),
        "{response}"
    );
    assert!(response.contains("m=video 0 RTP/AVP 96"), "{response}");
    assert!(response.contains("a=rtpmap:96 H264/90000"), "{response}");
    // Les paramètres connus doivent y figurer : sans eux, le lecteur reste
    // noir jusqu'à la première image clé.
    assert!(response.contains("sprop-parameter-sets="), "{response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_free_server_serves_a_describe_without_one() {
    let (_stream, port) = start_server(false).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "DESCRIBE rtsp://127.0.0.1:{port}/stream RTSP/1.0\r\nCSeq: 2\r\n\r\n"
    ));
    assert!(client.read_response().starts_with("RTSP/1.0 200"));
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unknown_path_is_a_404() {
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "DESCRIBE rtsp://127.0.0.1:{port}/pas-ce-flux?token={TOKEN} RTSP/1.0\r\nCSeq: 2\r\n\r\n"
    ));
    assert!(client.read_response().starts_with("RTSP/1.0 404"));
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_confirms_the_interleaved_transport_and_opens_a_session() {
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "SETUP rtsp://127.0.0.1:{port}/stream?token={TOKEN} RTSP/1.0\r\nCSeq: 3\r\n\
         Transport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
    ));
    let response = client.read_response();

    assert!(response.starts_with("RTSP/1.0 200"), "{response}");
    assert!(response.contains("interleaved=0-1"), "{response}");
    assert!(response.contains("ssrc="), "{response}");
    assert!(!session_of(&response).is_empty(), "{response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn setup_announces_server_ports_for_a_udp_transport() {
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "SETUP rtsp://127.0.0.1:{port}/stream?token={TOKEN} RTSP/1.0\r\nCSeq: 3\r\n\
         Transport: RTP/AVP;unicast;client_port=51000-51001\r\n\r\n"
    ));
    let response = client.read_response();

    assert!(response.starts_with("RTSP/1.0 200"), "{response}");
    assert!(response.contains("client_port=51000-51001"), "{response}");
    assert!(response.contains("server_port="), "{response}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_unsupported_transport_gets_461_so_the_player_can_propose_another() {
    // C'est le mécanisme par lequel un lecteur se rabat sur du TCP après un
    // refus d'UDP : répondre autre chose qu'un 461 le ferait abandonner.
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "SETUP rtsp://127.0.0.1:{port}/stream?token={TOKEN} RTSP/1.0\r\nCSeq: 3\r\n\
         Transport: RTP/AVP;multicast;port=50000-50001\r\n\r\n"
    ));
    assert!(client.read_response().starts_with("RTSP/1.0 461"));
}

#[tokio::test(flavor = "multi_thread")]
async fn playing_without_a_setup_is_refused() {
    let (_stream, port) = start_server(true).await;
    let mut client = Client::connect(port);

    client.send(&format!(
        "PLAY rtsp://127.0.0.1:{port}/stream?token={TOKEN} RTSP/1.0\r\nCSeq: 4\r\n\r\n"
    ));
    assert!(client.read_response().starts_with("RTSP/1.0 455"));
}

// --- Flux ---

#[tokio::test(flavor = "multi_thread")]
async fn a_playing_client_receives_the_published_frames_as_rtp() {
    // LE test qui compte : tout le dialogue ne sert qu'à ça.
    let (stream, port) = start_server(true).await;

    let mut client = Client::connect(port);
    client.play(port, Some(TOKEN));

    // Le flux ne considère un lecteur comme présent qu'à partir de son
    // abonnement, créé au `PLAY`.
    for _ in 0..100 {
        if stream.has_viewers() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(stream.has_viewers(), "le PLAY n'a pas abonné le lecteur");

    stream.publish(access_unit(9000, true, 200), Some(&parameters()));

    // Trois NAL publiées (SPS, PPS, tranche), chacune assez petite pour
    // tenir dans un paquet.
    let mut payload_types = Vec::new();

    for _ in 0..3 {
        let packet = client.read_rtp();

        assert!(packet.len() >= 12, "paquet RTP tronqué");
        assert_eq!(packet[0] >> 6, 2, "version RTP");
        assert_eq!(packet[1] & 0x7F, 96, "type de charge utile dynamique");
        assert_eq!(
            u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
            9000,
            "horodatage RTP"
        );

        payload_types.push(packet[12] & 0x1F);
    }

    // SPS (7), PPS (8) puis la tranche IDR (5) : c'est l'ordre dont un
    // décodeur a besoin.
    assert_eq!(payload_types, vec![7, 8, 5]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_large_keyframe_arrives_fragmented_and_reassembles_intact() {
    // Une image clé réaliste dépasse toujours un datagramme : si la
    // fragmentation FU-A était fausse, aucun lecteur n'afficherait rien.
    let (stream, port) = start_server(true).await;

    let mut client = Client::connect(port);
    client.play(port, Some(TOKEN));

    for _ in 0..100 {
        if stream.has_viewers() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let mut slice = vec![0x65u8];
    slice.extend((0..6000).map(|i| (i % 251) as u8));

    stream.publish(
        AccessUnit {
            nals: vec![slice.clone()],
            keyframe: true,
            rtp_timestamp: 3600,
        },
        None,
    );

    let mut reassembled = vec![slice[0]];
    let mut packets = 0;

    loop {
        let packet = client.read_rtp();
        packets += 1;

        // FU-A : type 28 dans l'indicateur.
        assert_eq!(packet[12] & 0x1F, 28, "fragment attendu");
        reassembled.extend_from_slice(&packet[14..]);

        // Bit E : dernier fragment.
        if packet[13] & 0x40 != 0 {
            // Et le bit marker, qui annonce la frame complète.
            assert!(packet[1] & 0x80 != 0, "bit marker absent du dernier paquet");
            break;
        }

        assert!(packets < 100, "fragmentation sans fin");
    }

    assert!(packets > 1, "l'image clé n'a pas été fragmentée");
    assert_eq!(
        reassembled, slice,
        "la NAL ne se reconstitue pas à l'identique"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_rtcp_sender_report_is_sent_on_the_paired_channel() {
    // Sans rapport, certains lecteurs (dont VLC) finissent par supposer que
    // la source est morte.
    let (stream, port) = start_server(true).await;

    let mut client = Client::connect(port);
    client.play(port, Some(TOKEN));

    for _ in 0..100 {
        if stream.has_viewers() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    stream.publish(access_unit(9000, true, 50), None);

    // On lit jusqu'à tomber sur le canal RTCP (1), parmi les paquets RTP.
    for _ in 0..30 {
        let (channel, packet) = client.read_interleaved();

        if channel == 1 {
            assert_eq!(packet.len(), 28, "taille d'un Sender Report");
            assert_eq!(packet[0] >> 6, 2, "version RTP");
            assert_eq!(packet[1], 200, "type de paquet SR");
            return;
        }
    }

    panic!("aucun rapport RTCP reçu");
}

#[tokio::test(flavor = "multi_thread")]
async fn teardown_ends_the_session_and_releases_the_stream() {
    let (stream, port) = start_server(true).await;

    let mut client = Client::connect(port);
    client.play(port, Some(TOKEN));

    for _ in 0..100 {
        if stream.has_viewers() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(stream.has_viewers());

    client.send(&format!(
        "TEARDOWN rtsp://127.0.0.1:{port}/stream?token={TOKEN} RTSP/1.0\r\nCSeq: 9\r\n\r\n"
    ));

    // Le flux doit cesser de se croire regardé : c'est ce qui arrête
    // l'encodage H.264 dans la boucle de capture.
    for _ in 0..100 {
        if !stream.has_viewers() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    panic!("le flux se croit encore regardé après le TEARDOWN");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disconnected_player_stops_counting_as_a_viewer() {
    // Le cas le plus fréquent dans la vraie vie : on ferme VLC, il ne dit
    // pas au revoir. Si le flux continuait de se croire regardé, le
    // Raspberry Pi encoderait du H.264 pour personne, indéfiniment.
    let (stream, port) = start_server(true).await;

    let mut client = Client::connect(port);
    client.play(port, Some(TOKEN));

    for _ in 0..100 {
        if stream.has_viewers() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(stream.has_viewers());

    drop(client);

    for _ in 0..200 {
        // Les publications font avancer la boucle de la session, qui
        // découvre alors la connexion fermée.
        stream.publish(access_unit(0, false, 50), None);

        if !stream.has_viewers() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    panic!("le lecteur déconnecté compte toujours comme spectateur");
}

#[tokio::test(flavor = "multi_thread")]
async fn several_players_each_get_their_own_rtp_session() {
    // L'encodage a lieu une fois ; c'est l'empaquetage qui est propre à
    // chaque lecteur. Un SSRC partagé ferait confondre les deux flux par un
    // équipement sur le chemin.
    let (stream, port) = start_server(true).await;

    let mut first = Client::connect(port);
    first.play(port, Some(TOKEN));

    let mut second = Client::connect(port);
    second.play(port, Some(TOKEN));

    for _ in 0..100 {
        if stream.has_viewers() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    stream.publish(access_unit(9000, true, 100), None);

    let first_packet = first.read_rtp();
    let second_packet = second.read_rtp();

    let ssrc_of =
        |packet: &[u8]| u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);

    assert_ne!(ssrc_of(&first_packet), ssrc_of(&second_packet));
    // Mais la même base de temps : un lecteur qui se branche tard ne doit
    // pas repartir à zéro.
    assert_eq!(&first_packet[4..8], &second_packet[4..8]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_keepalive_during_playback_is_answered_without_disturbing_the_stream() {
    // Les lecteurs envoient un `GET_PARAMETER` périodique. Le serveur doit y
    // répondre tout en continuant d'émettre, sur la MÊME connexion.
    let (stream, port) = start_server(true).await;

    let mut client = Client::connect(port);
    client.play(port, Some(TOKEN));

    for _ in 0..100 {
        if stream.has_viewers() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    client.send(&format!(
        "GET_PARAMETER rtsp://127.0.0.1:{port}/stream?token={TOKEN} RTSP/1.0\r\nCSeq: 5\r\n\r\n"
    ));

    let response = client.read_response();
    assert!(response.starts_with("RTSP/1.0 200"), "{response}");

    // Et le flux continue.
    stream.publish(access_unit(7200, true, 60), None);
    let packet = client.read_rtp();

    assert_eq!(packet[1] & 0x7F, 96);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_disabled_rtsp_server_does_not_listen_at_all() {
    let port = free_port();
    let stream = Arc::new(H264Stream::new());

    rtsp::spawn(
        RtspConfig {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port,
            ..RtspConfig::default()
        },
        TOKEN.to_string(),
        "salon".to_string(),
        stream,
    );

    tokio::time::sleep(Duration::from_millis(200)).await;

    assert!(
        TcpStream::connect(("127.0.0.1", port)).is_err(),
        "le port est ouvert alors que [rtsp] enabled = false"
    );
}
