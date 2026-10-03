//! Analyse et formatage des messages RTSP (RFC 2326).
//!
//! RTSP est un protocole texte calqué sur HTTP/1.1 : une ligne de requête,
//! des en-têtes `Clé: valeur`, une ligne vide, puis un corps optionnel. Cette
//! ressemblance est trompeuse sur un point : **une connexion RTSP transporte
//! aussi des données binaires**, les paquets RTP « entrelacés » (voir
//! [`Incoming::Interleaved`]). Un analyseur HTTP ordinaire s'y perdrait.

use std::collections::HashMap;

/// Taille maximale d'un message RTSP accepté.
///
/// Un `DESCRIBE` ou un `SETUP` pèse quelques centaines d'octets. Ce plafond
/// est là pour qu'une connexion qui n'enverrait jamais de fin de message ne
/// fasse pas grossir indéfiniment le tampon de réception — c'est le port
/// ouvert sur le réseau, pas un fichier de configuration.
pub const MAX_MESSAGE_SIZE: usize = 16 * 1024;

/// Ce qu'une connexion RTSP peut livrer.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming {
    /// Une requête RTSP à traiter.
    Request(Request),
    /// Un paquet binaire entrelacé reçu du CLIENT.
    ///
    /// Les lecteurs en transport TCP y renvoient leurs rapports de réception
    /// RTCP. Ce serveur ne s'en sert pas — il n'adapte pas son débit — mais
    /// il doit savoir les reconnaître pour ne pas les prendre pour une
    /// requête malformée et fermer la connexion.
    Interleaved { channel: u8, length: usize },
}

/// Une requête RTSP analysée.
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    /// `OPTIONS`, `DESCRIBE`, `SETUP`, `PLAY`, `TEARDOWN`, ...
    pub method: String,
    /// L'URI demandée, telle quelle (`rtsp://hôte:8554/stream?token=...`).
    pub uri: String,
    /// Numéro de séquence, à renvoyer à l'identique : c'est lui qui permet
    /// au client d'apparier réponse et requête.
    pub cseq: String,
    /// En-têtes, clés normalisées en minuscules (RFC 2326 : elles sont
    /// insensibles à la casse, et les lecteurs ne s'accordent pas sur la
    /// graphie de `Session` ou `Transport`).
    pub headers: HashMap<String, String>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).map(String::as_str)
    }

    /// Valeur d'un paramètre de la chaîne de requête de l'URI.
    ///
    /// Sert à l'authentification par jeton (`?token=...`) : c'est le seul
    /// endroit où un lecteur vidéo ordinaire sait transporter un secret sans
    /// configuration particulière.
    pub fn query_parameter(&self, name: &str) -> Option<&str> {
        let query = self.uri.split_once('?')?.1;

        query.split('&').find_map(|pair| {
            let (key, value) = pair.split_once('=')?;
            (key == name).then_some(value)
        })
    }

    /// Chemin de l'URI, sans schéma, hôte ni chaîne de requête.
    ///
    /// `rtsp://192.168.1.42:8554/stream?token=x` donne `/stream`.
    pub fn path(&self) -> &str {
        let without_query = self.uri.split('?').next().unwrap_or(&self.uri);

        // On saute `rtsp://hôte:port` s'il est présent : RFC 2326 impose
        // l'URI absolue, mais tous les lecteurs ne la respectent pas.
        let after_scheme = without_query
            .split_once("://")
            .map_or(without_query, |(_, rest)| rest);

        match after_scheme.find('/') {
            Some(index) => &after_scheme[index..],
            // `rtsp://hôte:8554` sans chemin, ou une URI `*`.
            None => "/",
        }
    }
}

/// Résultat de l'analyse d'un tampon de réception.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    /// Un message complet a été lu ; `consumed` octets sont à retirer du
    /// tampon.
    Message { incoming: Incoming, consumed: usize },
    /// Le tampon ne contient pas encore un message complet.
    Incomplete,
    /// Le tampon ne contient pas du RTSP (ou dépasse [`MAX_MESSAGE_SIZE`]) :
    /// la connexion doit être fermée, il n'y a pas de resynchronisation
    /// possible sur un flux texte dont on a perdu le fil.
    Invalid(&'static str),
}

/// Tente d'extraire un message du début de `buffer`.
///
/// Ne consomme rien elle-même : c'est l'appelant qui retire `consumed`
/// octets, ce qui lui laisse la possibilité de ne rien faire (et donc de
/// réessayer plus tard avec davantage de données).
pub fn parse(buffer: &[u8]) -> ParseOutcome {
    let Some(&first) = buffer.first() else {
        return ParseOutcome::Incomplete;
    };

    // Paquet entrelacé : `$` + canal + longueur sur 16 bits + charge utile
    // (RFC 2326 §10.12). À distinguer AVANT toute tentative d'analyse
    // texte — le contenu binaire n'a aucune raison d'être du RTSP valide.
    if first == b'$' {
        if buffer.len() < 4 {
            return ParseOutcome::Incomplete;
        }

        let length = usize::from(u16::from_be_bytes([buffer[2], buffer[3]]));

        if buffer.len() < 4 + length {
            return ParseOutcome::Incomplete;
        }

        return ParseOutcome::Message {
            incoming: Incoming::Interleaved {
                channel: buffer[1],
                length,
            },
            consumed: 4 + length,
        };
    }

    let Some(header_end) = find_double_crlf(buffer) else {
        return if buffer.len() > MAX_MESSAGE_SIZE {
            ParseOutcome::Invalid("message RTSP trop long")
        } else {
            ParseOutcome::Incomplete
        };
    };

    let Ok(head) = std::str::from_utf8(&buffer[..header_end]) else {
        return ParseOutcome::Invalid("message RTSP non UTF-8");
    };

    let mut lines = head.split("\r\n");

    let Some(request_line) = lines.next() else {
        return ParseOutcome::Invalid("ligne de requête absente");
    };

    let mut parts = request_line.split_whitespace();
    let (Some(method), Some(uri), Some(version)) = (parts.next(), parts.next(), parts.next())
    else {
        return ParseOutcome::Invalid("ligne de requête malformée");
    };

    if !version.starts_with("RTSP/") {
        return ParseOutcome::Invalid("version de protocole inattendue");
    }

    let mut headers = HashMap::new();

    for line in lines {
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    // Un corps n'est annoncé que par `Content-Length` (il n'y a pas de
    // transfert fragmenté en RTSP). On attend qu'il soit entièrement arrivé
    // avant de livrer la requête, sinon ses octets seraient relus comme le
    // début du message suivant.
    let body_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);

    if body_length > MAX_MESSAGE_SIZE {
        return ParseOutcome::Invalid("corps de message RTSP trop long");
    }

    if buffer.len() < header_end + 4 + body_length {
        return ParseOutcome::Incomplete;
    }

    ParseOutcome::Message {
        incoming: Incoming::Request(Request {
            method: method.to_ascii_uppercase(),
            uri: uri.to_string(),
            // Absent, `CSeq` est renvoyé vide : la réponse part quand même,
            // plutôt que de laisser le client attendre pour un en-tête qu'il
            // a lui-même oublié.
            cseq: headers.get("cseq").cloned().unwrap_or_default(),
            headers,
        }),
        consumed: header_end + 4 + body_length,
    }
}

/// Position de la ligne vide qui termine les en-têtes.
fn find_double_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

/// Construit une réponse RTSP.
///
/// `status` est la ligne d'état sans la version (`"200 OK"`), `headers` les
/// en-têtes propres à la réponse. `CSeq` et `Server` sont ajoutés
/// systématiquement : le premier est obligatoire, le second aide au
/// diagnostic côté client.
pub fn response(status: &str, cseq: &str, headers: &[(&str, &str)], body: &str) -> Vec<u8> {
    let mut message = format!("RTSP/1.0 {status}\r\nCSeq: {cseq}\r\nServer: FoxGuard\r\n");

    for (name, value) in headers {
        message.push_str(&format!("{name}: {value}\r\n"));
    }

    if !body.is_empty() {
        message.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }

    message.push_str("\r\n");
    message.push_str(body);

    message.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_bytes(extra: &str) -> Vec<u8> {
        format!("DESCRIBE rtsp://cam:8554/stream RTSP/1.0\r\nCSeq: 2\r\n{extra}\r\n").into_bytes()
    }

    fn parse_request(buffer: &[u8]) -> (Request, usize) {
        match parse(buffer) {
            ParseOutcome::Message {
                incoming: Incoming::Request(request),
                consumed,
            } => (request, consumed),
            other => panic!("requête attendue, obtenu {other:?}"),
        }
    }

    #[test]
    fn a_complete_request_is_parsed_with_its_method_uri_and_cseq() {
        let (request, consumed) = parse_request(&request_bytes(""));

        assert_eq!(request.method, "DESCRIBE");
        assert_eq!(request.uri, "rtsp://cam:8554/stream");
        assert_eq!(request.cseq, "2");
        assert_eq!(consumed, request_bytes("").len());
    }

    #[test]
    fn header_names_are_matched_regardless_of_case() {
        // Les lecteurs ne s'accordent pas sur la graphie : VLC écrit
        // `Transport`, d'autres `transport`.
        let (request, _) = parse_request(&request_bytes("TrAnSpOrT: RTP/AVP/TCP\r\n"));

        assert_eq!(request.header("transport"), Some("RTP/AVP/TCP"));
    }

    #[test]
    fn a_lowercase_method_is_normalized() {
        let buffer = b"describe rtsp://cam/stream RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let (request, _) = parse_request(buffer);

        assert_eq!(request.method, "DESCRIBE");
    }

    #[test]
    fn a_partial_request_is_reported_incomplete_rather_than_rejected() {
        // Un message RTSP peut arriver en plusieurs segments TCP.
        let full = request_bytes("");

        for cut in 1..full.len() - 1 {
            assert_eq!(
                parse(&full[..cut]),
                ParseOutcome::Incomplete,
                "coupure après {cut} octets"
            );
        }
    }

    #[test]
    fn two_pipelined_requests_are_delivered_one_at_a_time() {
        // Les clients enchaînent parfois OPTIONS et DESCRIBE sans attendre.
        let mut buffer = request_bytes("");
        buffer.extend(b"OPTIONS * RTSP/1.0\r\nCSeq: 3\r\n\r\n");

        let (first, consumed) = parse_request(&buffer);
        assert_eq!(first.method, "DESCRIBE");

        let (second, _) = parse_request(&buffer[consumed..]);
        assert_eq!(second.method, "OPTIONS");
        assert_eq!(second.cseq, "3");
    }

    #[test]
    fn a_body_is_waited_for_before_the_request_is_delivered() {
        // Sans cette attente, les octets du corps seraient relus comme le
        // début du message suivant.
        let head =
            b"SET_PARAMETER rtsp://cam/stream RTSP/1.0\r\nCSeq: 4\r\nContent-Length: 5\r\n\r\n";

        assert_eq!(parse(head), ParseOutcome::Incomplete);

        let mut full = head.to_vec();
        full.extend(b"abcde");
        let (request, consumed) = parse_request(&full);

        assert_eq!(request.method, "SET_PARAMETER");
        assert_eq!(consumed, full.len());
    }

    #[test]
    fn an_interleaved_packet_is_recognized_and_skipped_whole() {
        // Les rapports RTCP du client arrivent sur la même connexion : les
        // prendre pour du texte fermerait la session en pleine lecture.
        let mut buffer = vec![b'$', 1, 0, 4];
        buffer.extend([0xDE, 0xAD, 0xBE, 0xEF]);

        assert_eq!(
            parse(&buffer),
            ParseOutcome::Message {
                incoming: Incoming::Interleaved {
                    channel: 1,
                    length: 4
                },
                consumed: 8,
            }
        );
    }

    #[test]
    fn a_partial_interleaved_packet_waits_for_the_rest() {
        let buffer = vec![b'$', 1, 0, 8, 0xDE, 0xAD];
        assert_eq!(parse(&buffer), ParseOutcome::Incomplete);
    }

    #[test]
    fn a_request_following_an_interleaved_packet_is_found() {
        let mut buffer = vec![b'$', 0, 0, 2, 0xAA, 0xBB];
        buffer.extend(request_bytes(""));

        let consumed = match parse(&buffer) {
            ParseOutcome::Message { consumed, .. } => consumed,
            other => panic!("{other:?}"),
        };

        let (request, _) = parse_request(&buffer[consumed..]);
        assert_eq!(request.method, "DESCRIBE");
    }

    #[test]
    fn garbage_is_rejected_rather_than_buffered_forever() {
        // Un scan de port ou un client qui parle un autre protocole : la
        // connexion doit tomber, pas grossir.
        let buffer = vec![b'x'; MAX_MESSAGE_SIZE + 1];
        assert!(matches!(parse(&buffer), ParseOutcome::Invalid(_)));
    }

    #[test]
    fn a_wrong_protocol_version_is_rejected() {
        let buffer = b"GET /stream HTTP/1.1\r\nHost: cam\r\n\r\n";
        assert!(matches!(parse(buffer), ParseOutcome::Invalid(_)));
    }

    #[test]
    fn an_absurd_content_length_is_rejected_rather_than_reserved() {
        let buffer =
            b"PLAY rtsp://cam/stream RTSP/1.0\r\nCSeq: 1\r\nContent-Length: 99999999\r\n\r\n";
        assert!(matches!(parse(buffer), ParseOutcome::Invalid(_)));
    }

    #[test]
    fn a_missing_cseq_does_not_prevent_answering() {
        let buffer = b"OPTIONS * RTSP/1.0\r\n\r\n";
        let (request, _) = parse_request(buffer);

        assert_eq!(request.cseq, "");
    }

    // --- Chemin et paramètres de l'URI ---

    #[test]
    fn the_path_is_extracted_from_an_absolute_uri() {
        let (request, _) = parse_request(&request_bytes(""));
        assert_eq!(request.path(), "/stream");
    }

    #[test]
    fn the_query_string_is_not_part_of_the_path() {
        let buffer = b"SETUP rtsp://cam:8554/stream?token=secret RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let (request, _) = parse_request(buffer);

        assert_eq!(request.path(), "/stream");
        assert_eq!(request.query_parameter("token"), Some("secret"));
    }

    #[test]
    fn a_relative_uri_is_tolerated() {
        // RFC 2326 impose l'URI absolue, mais tous les lecteurs ne la
        // respectent pas.
        let buffer = b"PLAY /stream RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let (request, _) = parse_request(buffer);

        assert_eq!(request.path(), "/stream");
    }

    #[test]
    fn a_uri_without_a_path_falls_back_to_the_root() {
        let buffer = b"OPTIONS rtsp://cam:8554 RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let (request, _) = parse_request(buffer);

        assert_eq!(request.path(), "/");
    }

    #[test]
    fn a_missing_query_parameter_is_absent_not_empty() {
        let (request, _) = parse_request(&request_bytes(""));
        assert_eq!(request.query_parameter("token"), None);
    }

    #[test]
    fn a_token_among_several_query_parameters_is_found() {
        let buffer = b"SETUP rtsp://cam/stream?a=1&token=secret&b=2 RTSP/1.0\r\nCSeq: 1\r\n\r\n";
        let (request, _) = parse_request(buffer);

        assert_eq!(request.query_parameter("token"), Some("secret"));
    }

    // --- Formatage des réponses ---

    #[test]
    fn a_response_carries_the_status_line_and_echoes_the_cseq() {
        let bytes = response("200 OK", "7", &[], "");
        let text = String::from_utf8(bytes).expect("UTF-8");

        assert!(text.starts_with("RTSP/1.0 200 OK\r\n"), "{text}");
        assert!(text.contains("CSeq: 7\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\n"), "{text:?}");
    }

    #[test]
    fn a_response_with_a_body_announces_its_length() {
        let bytes = response(
            "200 OK",
            "2",
            &[("Content-Type", "application/sdp")],
            "v=0\r\n",
        );
        let text = String::from_utf8(bytes).expect("UTF-8");

        assert!(text.contains("Content-Length: 5\r\n"), "{text}");
        assert!(text.contains("Content-Type: application/sdp\r\n"), "{text}");
        assert!(text.ends_with("\r\n\r\nv=0\r\n"), "{text:?}");
    }

    #[test]
    fn a_response_without_a_body_omits_the_content_length() {
        // Annoncer `Content-Length: 0` est légal mais déroute certains
        // lecteurs sur une réponse à OPTIONS.
        let bytes = response("200 OK", "1", &[("Public", "DESCRIBE")], "");
        let text = String::from_utf8(bytes).expect("UTF-8");

        assert!(!text.contains("Content-Length"), "{text}");
    }
}
