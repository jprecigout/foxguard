//! Serveur RTSP : accepte les lecteurs, négocie le transport et leur
//! distribue le flux H.264 empaqueté en RTP.
//!
//! # Déroulement d'une lecture
//!
//! ```text
//! OPTIONS   -> la liste des méthodes gérées
//! DESCRIBE  -> le SDP du flux (voir super::sdp)
//! SETUP     -> le transport retenu (voir super::transport) et un identifiant de session
//! PLAY      -> à partir d'ici, les paquets RTP partent
//! TEARDOWN  -> fin
//! ```
//!
//! # Une tâche par connexion, un empaqueteur par client
//!
//! Chaque connexion est servie par sa propre tâche Tokio et s'abonne au canal
//! de diffusion des frames encodées : l'encodage a lieu UNE fois, dans la
//! boucle de capture, quel que soit le nombre de lecteurs. Ce qui est propre
//! à chaque client, c'est l'empaquetage (numéros de séquence, SSRC, compteurs
//! RTCP) — un lecteur qui prend du retard ne décale donc pas les autres.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{broadcast, mpsc};
use tracing::{debug, info, warn};

use crate::config::RtspConfig;
use crate::h264::{AccessUnit, H264Stream};

use super::message::{self, Incoming, ParseOutcome, Request};
use super::rtp::Packetizer;
use super::sdp;
use super::transport::Transport;

/// Méthodes annoncées en réponse à `OPTIONS`.
const SUPPORTED_METHODS: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER";

/// Durée annoncée au client avant expiration d'une session inactive.
///
/// Purement informative : c'est la fermeture de la connexion TCP qui met
/// réellement fin à une session ici. Elle sert à indiquer aux lecteurs à
/// quelle fréquence envoyer leurs `GET_PARAMETER` de maintien.
const SESSION_TIMEOUT_SECS: u32 = 60;

/// Période des rapports d'émetteur RTCP (voir
/// [`super::rtp::Packetizer::sender_report`]).
const RTCP_INTERVAL: Duration = Duration::from_secs(1);

/// Ce qui est partagé par toutes les connexions.
struct ServerContext {
    stream: Arc<H264Stream>,
    /// Jeton attendu dans la chaîne de requête, ou `None` si
    /// l'authentification est désactivée.
    api_token: Option<String>,
    /// Chemin attendu dans l'URI, avec sa barre oblique initiale.
    path: String,
    /// Nom annoncé dans le SDP.
    camera_name: String,
}

/// Démarre le serveur RTSP en tâche de fond.
///
/// Ne retourne pas d'erreur sur un port déjà pris : le serveur RTSP est une
/// fonctionnalité annexe, et la caméra doit continuer à surveiller, à
/// enregistrer et à diffuser son flux WebSocket même si le port 8554 est
/// occupé par autre chose. L'échec est journalisé en `warn!`.
pub fn spawn(config: RtspConfig, api_token: String, camera_name: String, stream: Arc<H264Stream>) {
    if !config.enabled {
        return;
    }

    let context = Arc::new(ServerContext {
        stream,
        api_token: config.require_token.then_some(api_token),
        path: normalize_path(&config.path),
        camera_name,
    });

    let bind = format!("{}:{}", config.host, config.port);

    tokio::spawn(async move {
        match TcpListener::bind(&bind).await {
            Ok(listener) => {
                info!(
                    "🎬 Flux RTSP disponible sur rtsp://{}{}{}",
                    bind,
                    context.path,
                    if context.api_token.is_some() {
                        "?token=…"
                    } else {
                        ""
                    }
                );

                accept_loop(listener, context).await;
            }
            Err(e) => {
                warn!(
                    "⚠️ Serveur RTSP non démarré (écoute sur {bind} impossible : {e}). \
                     Le reste de la caméra fonctionne normalement."
                );
            }
        }
    });
}

/// Boucle d'acceptation : une tâche par lecteur connecté.
async fn accept_loop(listener: TcpListener, context: Arc<ServerContext>) {
    loop {
        let (socket, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                warn!("⚠️ [RTSP] Connexion refusée : {e}");
                // Une erreur d'`accept` est souvent transitoire (table de
                // descripteurs pleine) : on souffle plutôt que de boucler à
                // vide sur la même erreur.
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };

        // Nagle désactivé : il retarderait les petits paquets RTP pour les
        // agréger, ce qui ajoute une latence visible sur un flux en direct.
        let _ = socket.set_nodelay(true);

        let context = Arc::clone(&context);

        tokio::spawn(async move {
            debug!("🔌 [RTSP] Lecteur connecté ({peer})");

            if let Err(e) = serve_connection(socket, peer, context).await {
                debug!("🔌 [RTSP] Session terminée ({peer}) : {e}");
            } else {
                debug!("🔌 [RTSP] Lecteur déconnecté ({peer})");
            }
        });
    }
}

/// État d'une session RTSP, du `SETUP` au `TEARDOWN`.
struct Session {
    identifier: String,
    transport: Option<Transport>,
    playing: bool,
    /// Un jeton valide a déjà été présenté sur CETTE connexion.
    ///
    /// Indispensable à l'interopérabilité : un lecteur ne reprend pas l'URL
    /// du `DESCRIBE` pour son `SETUP`, il la reconstruit à partir du
    /// `Content-Base` de la réponse et de l'attribut `a=control:` du SDP (voir
    /// `super::sdp`). La chaîne de requête, donc le jeton, n'y survit pas —
    /// GStreamer envoie ainsi un `SETUP` sans jeton juste après un `DESCRIBE`
    /// authentifié, et se faisait refuser.
    ///
    /// Le contrôle n'est pas affaibli pour autant : rien n'est servi avant
    /// qu'un jeton valide n'ait été présenté au moins une fois, et la
    /// mémorisation ne vaut QUE pour la durée de la connexion TCP — une
    /// nouvelle connexion doit s'authentifier à nouveau.
    authenticated: bool,
    packetizer: Packetizer,
    /// Chaussettes UDP d'émission, créées au `SETUP` en transport UDP. La
    /// première porte RTP, la seconde RTCP.
    udp: Option<(UdpSocket, UdpSocket)>,
    peer: SocketAddr,
}

/// Ce que le traitement d'une requête demande à la boucle de connexion.
enum Step {
    /// Continuer sans rien changer.
    Continue,
    /// S'abonner au flux et commencer à émettre.
    StartStreaming,
    /// Fermer la connexion.
    Close,
}

async fn serve_connection(
    socket: TcpStream,
    peer: SocketAddr,
    context: Arc<ServerContext>,
) -> Result<()> {
    let (reader, mut writer) = socket.into_split();

    // La lecture vit dans sa propre tâche, et non dans une branche du
    // `select!` ci-dessous : son future détiendrait sinon un emprunt du
    // tampon de réception pendant que les autres branches écrivent dans la
    // même connexion.
    let (incoming_tx, mut incoming_rx) = mpsc::channel::<Incoming>(8);
    tokio::spawn(read_loop(reader, incoming_tx));

    let mut session = Session {
        identifier: new_session_identifier(peer),
        transport: None,
        playing: false,
        authenticated: false,
        packetizer: Packetizer::new(peer.port().into()),
        udp: None,
        peer,
    };

    let mut frames: Option<broadcast::Receiver<Arc<AccessUnit>>> = None;
    let mut rtcp = tokio::time::interval(RTCP_INTERVAL);

    loop {
        let step = tokio::select! {
            incoming = incoming_rx.recv() => {
                match incoming {
                    // La tâche de lecture s'est arrêtée : le client a
                    // raccroché, ou a envoyé quelque chose d'inintelligible.
                    None => Step::Close,

                    // Rapport RTCP du client, déjà écarté du tampon par
                    // l'analyseur. Ce serveur n'adapte pas son débit : il n'y
                    // a rien à en faire, mais il fallait le reconnaître pour
                    // ne pas le prendre pour une requête malformée.
                    Some(Incoming::Interleaved { channel, length }) => {
                        debug!("📨 [RTSP] Paquet entrelacé du lecteur ignoré (canal {channel}, {length} octets)");
                        Step::Continue
                    }

                    Some(Incoming::Request(request)) => {
                        let (reply, step) = handle_request(&request, &mut session, &context);
                        writer.write_all(&reply).await.context("réponse RTSP")?;
                        step
                    }
                }
            }

            unit = next_access_unit(&mut frames) => {
                match unit {
                    Ok(unit) => {
                        send_access_unit(&unit, &mut session, &mut writer).await?;
                        Step::Continue
                    }

                    // Le lecteur n'a pas suivi la cadence et a perdu des
                    // frames. On demande une image clé : sans elle, son
                    // décodeur resterait bloqué sur une image de référence
                    // qu'il n'a jamais reçue, et afficherait des artefacts
                    // jusqu'à l'image clé périodique suivante.
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        debug!("⏭️ [RTSP] {skipped} frame(s) sautée(s) pour {peer}, image clé demandée");
                        context.stream.request_keyframe();
                        Step::Continue
                    }

                    Err(broadcast::error::RecvError::Closed) => Step::Close,
                }
            }

            // `has_sent_packets` : pas de rapport avant la première frame.
            // Le premier `tick` d'un `interval` est immédiat, et un rapport
            // envoyé à cet instant ne contiendrait que des zéros.
            _ = rtcp.tick(), if session.playing && session.packetizer.has_sent_packets() => {
                send_sender_report(&session, &mut writer).await?;
                Step::Continue
            }
        };

        // Appliqué APRÈS le `select!`, pour que l'emprunt de `frames` par
        // `next_access_unit` soit relâché.
        match step {
            Step::Continue => {}
            Step::StartStreaming => {
                frames = Some(context.stream.subscribe());
                // Un lecteur ne peut commencer à décoder qu'à une image clé :
                // sans cette demande, il attendrait la suivante, soit
                // plusieurs secondes d'écran noir.
                context.stream.request_keyframe();
            }
            Step::Close => break,
        }
    }

    Ok(())
}

/// Lit la connexion et transmet les messages complets à la boucle de
/// connexion.
async fn read_loop(mut reader: OwnedReadHalf, outgoing: mpsc::Sender<Incoming>) {
    use tokio::io::AsyncReadExt;

    let mut buffer = Vec::new();
    let mut chunk = [0u8; 4096];

    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(read) => read,
        };

        buffer.extend_from_slice(&chunk[..read]);

        // Un seul `read` peut apporter plusieurs messages (les lecteurs
        // enchaînent souvent OPTIONS et DESCRIBE sans attendre la réponse) :
        // on vide le tampon de tout ce qu'il contient de complet.
        loop {
            match message::parse(&buffer) {
                ParseOutcome::Message { incoming, consumed } => {
                    buffer.drain(..consumed);

                    if outgoing.send(incoming).await.is_err() {
                        return;
                    }
                }
                ParseOutcome::Incomplete => break,
                ParseOutcome::Invalid(reason) => {
                    debug!("⚠️ [RTSP] Connexion fermée : {reason}");
                    return;
                }
            }
        }
    }
}

/// Attend la prochaine frame encodée, ou pour toujours si la session n'a pas
/// encore commencé sa lecture.
///
/// `std::future::pending()` plutôt qu'une branche désactivée du `select!` :
/// le canal n'existe pas avant le `PLAY`, il n'y a donc rien sur quoi
/// attendre.
async fn next_access_unit(
    frames: &mut Option<broadcast::Receiver<Arc<AccessUnit>>>,
) -> Result<Arc<AccessUnit>, broadcast::error::RecvError> {
    match frames.as_mut() {
        Some(receiver) => receiver.recv().await,
        None => std::future::pending().await,
    }
}

/// Traite une requête et retourne la réponse à écrire, plus l'action
/// attendue de la boucle de connexion.
fn handle_request(
    request: &Request,
    session: &mut Session,
    context: &ServerContext,
) -> (Vec<u8>, Step) {
    let cseq = &request.cseq;

    // `OPTIONS` est servi sans contrôle : c'est la requête par laquelle un
    // lecteur découvre le serveur, et son URI est souvent `*` (donc sans
    // jeton possible). Elle ne révèle rien d'autre que la liste des méthodes.
    if request.method == "OPTIONS" {
        return (
            message::response("200 OK", cseq, &[("Public", SUPPORTED_METHODS)], ""),
            Step::Continue,
        );
    }

    if let Some(expected) = &context.api_token {
        // Temps constant, comme sur le serveur HTTP (voir
        // `foxguard_protocol::auth::secure_eq`).
        if request
            .query_parameter("token")
            .is_some_and(|token| foxguard_protocol::auth::secure_eq(token, expected))
        {
            session.authenticated = true;
        }

        if !session.authenticated {
            warn!(
                "⚠️ [RTSP] {} refusé pour {} (jeton invalide ou absent).",
                request.method, session.peer
            );
            // 401 sans en-tête `WWW-Authenticate` : ce serveur n'implémente
            // pas l'authentification Digest de RFC 2326, le jeton passe par
            // l'URI (voir le README). Annoncer un mécanisme qu'on ne sait pas
            // vérifier ferait boucler les lecteurs sur une demande de mot de
            // passe.
            return (
                message::response("401 Unauthorized", cseq, &[], ""),
                Step::Continue,
            );
        }
    }

    // `GET_PARAMETER` sans corps est le maintien de session des lecteurs.
    if request.method == "GET_PARAMETER" {
        return (message::response("200 OK", cseq, &[], ""), Step::Continue);
    }

    if request.method == "TEARDOWN" {
        return (message::response("200 OK", cseq, &[], ""), Step::Close);
    }

    // Le chemin n'est vérifié qu'ici : `OPTIONS` peut légitimement porter
    // `*`, et `TEARDOWN` doit aboutir même sur une URI approximative — on ne
    // refuse pas à un client de s'en aller.
    if !matches_stream_path(request.path(), &context.path) {
        return (
            message::response("404 Not Found", cseq, &[], ""),
            Step::Continue,
        );
    }

    match request.method.as_str() {
        "DESCRIBE" => {
            let parameters = context.stream.parameters();
            let body = sdp::describe_stream(&context.camera_name, parameters.as_ref());

            (
                message::response(
                    "200 OK",
                    cseq,
                    &[
                        ("Content-Type", "application/sdp"),
                        // Base des URL relatives du SDP (`a=control:`).
                        //
                        // SANS la chaîne de requête : le lecteur y accole le
                        // nom de la piste, et `…/stream?token=x/trackID=0`
                        // n'aurait aucun sens — ni comme chemin, ni comme
                        // jeton (voir `Session::authenticated`).
                        (
                            "Content-Base",
                            &format!("{}/", uri_without_query(&request.uri)),
                        ),
                    ],
                    &body,
                ),
                Step::Continue,
            )
        }

        "SETUP" => handle_setup(request, session, cseq),

        "PLAY" => {
            if session.transport.is_none() {
                // RFC 2326 §11.3.10 : jouer sans avoir négocié de transport
                // n'a pas de sens.
                return (
                    message::response("455 Method Not Valid In This State", cseq, &[], ""),
                    Step::Continue,
                );
            }

            session.playing = true;

            info!(
                "▶️ [RTSP] Lecture démarrée pour {} ({})",
                session.peer,
                match session.transport {
                    Some(Transport::Interleaved { .. }) => "TCP entrelacé",
                    _ => "UDP",
                }
            );

            (
                message::response(
                    "200 OK",
                    cseq,
                    &[
                        ("Session", &session.identifier),
                        // `npt=0.000-` : lecture depuis le début et sans
                        // fin annoncée, ce qu'est un direct.
                        ("Range", "npt=0.000-"),
                        (
                            "RTP-Info",
                            &format!("url={};seq=0;rtptime=0", uri_without_query(&request.uri)),
                        ),
                    ],
                    "",
                ),
                Step::StartStreaming,
            )
        }

        _ => (
            message::response("501 Not Implemented", cseq, &[], ""),
            Step::Continue,
        ),
    }
}

/// Négocie le transport et prépare l'émission.
fn handle_setup(request: &Request, session: &mut Session, cseq: &str) -> (Vec<u8>, Step) {
    let Some(header) = request.header("transport") else {
        return (
            message::response("400 Bad Request", cseq, &[], ""),
            Step::Continue,
        );
    };

    let Some(transport) = Transport::negotiate(header) else {
        // 461 est la réponse prévue pour « aucun des transports proposés ne
        // me convient » (RFC 2326 §11.3.14) : le lecteur en propose alors
        // un autre, typiquement du TCP entrelacé après un refus d'UDP.
        debug!("⚠️ [RTSP] Transport non géré : {header}");
        return (
            message::response("461 Unsupported Transport", cseq, &[], ""),
            Step::Continue,
        );
    };

    let mut server_ports = None;

    if matches!(transport, Transport::Udp { .. }) {
        match bind_udp_pair() {
            Ok((rtp, rtcp, ports)) => {
                session.udp = Some((rtp, rtcp));
                server_ports = Some(ports);
            }
            Err(e) => {
                warn!(
                    "⚠️ [RTSP] Ports UDP indisponibles pour {} : {e}",
                    session.peer
                );
                return (
                    message::response("461 Unsupported Transport", cseq, &[], ""),
                    Step::Continue,
                );
            }
        }
    }

    let transport_header = transport.response_header(session.packetizer.ssrc(), server_ports);
    session.transport = Some(transport);

    (
        message::response(
            "200 OK",
            cseq,
            &[
                ("Transport", &transport_header),
                (
                    "Session",
                    &format!("{};timeout={SESSION_TIMEOUT_SECS}", session.identifier),
                ),
            ],
            "",
        ),
        Step::Continue,
    )
}

/// Ouvre deux chaussettes UDP sur des ports CONSÉCUTIFS (RTP puis RTCP),
/// comme l'attendent les lecteurs.
///
/// La paire consécutive n'est pas garantie par le système : on laisse le
/// noyau choisir un port, on tente son voisin, et on réessaie si celui-ci est
/// déjà pris. Après quelques tentatives infructueuses, on se rabat sur deux
/// ports quelconques — un lecteur qui vérifie strictement la contiguïté est
/// rare, alors qu'un échec de `SETUP` est fatal à la lecture.
fn bind_udp_pair() -> Result<(UdpSocket, UdpSocket, (u16, u16))> {
    const ATTEMPTS: usize = 10;

    let mut fallback = None;

    for _ in 0..ATTEMPTS {
        let rtp = std::net::UdpSocket::bind("0.0.0.0:0").context("ouverture du port RTP")?;
        let rtp_port = rtp.local_addr()?.port();

        if let Some(rtcp_port) = rtp_port.checked_add(1)
            && let Ok(rtcp) = std::net::UdpSocket::bind(("0.0.0.0", rtcp_port))
        {
            rtp.set_nonblocking(true)?;
            rtcp.set_nonblocking(true)?;

            return Ok((
                UdpSocket::from_std(rtp)?,
                UdpSocket::from_std(rtcp)?,
                (rtp_port, rtcp_port),
            ));
        }

        if fallback.is_none() {
            fallback = Some((rtp, rtp_port));
        }
    }

    let (rtp, rtp_port) = fallback.context("aucun port UDP disponible")?;
    let rtcp = std::net::UdpSocket::bind("0.0.0.0:0").context("ouverture du port RTCP")?;
    let rtcp_port = rtcp.local_addr()?.port();

    rtp.set_nonblocking(true)?;
    rtcp.set_nonblocking(true)?;

    Ok((
        UdpSocket::from_std(rtp)?,
        UdpSocket::from_std(rtcp)?,
        (rtp_port, rtcp_port),
    ))
}

/// Empaquette une frame encodée et l'envoie au client.
async fn send_access_unit(
    unit: &AccessUnit,
    session: &mut Session,
    writer: &mut OwnedWriteHalf,
) -> Result<()> {
    if !session.playing {
        return Ok(());
    }

    let packets = session.packetizer.packetize(&unit.nals, unit.rtp_timestamp);

    match session.transport {
        Some(Transport::Interleaved { rtp_channel, .. }) => {
            // Les paquets sont rassemblés en UNE écriture : un `write_all`
            // par paquet RTP, c'est un appel système pour 1,4 Ko, et sur une
            // image clé fragmentée en cinquante paquets ça se sent.
            let mut framed = Vec::new();

            for packet in &packets {
                push_interleaved(&mut framed, rtp_channel, packet);
            }

            writer
                .write_all(&framed)
                .await
                .context("émission RTP entrelacée")?;
        }

        Some(Transport::Udp { rtp_port, .. }) => {
            let Some((socket, _)) = &session.udp else {
                return Ok(());
            };

            let destination = SocketAddr::new(session.peer.ip(), rtp_port);

            for packet in &packets {
                // Une erreur d'envoi UDP (destination injoignable, tampon
                // plein) ne tue pas la session : le client peut encore
                // revenir, et c'est le propre d'un transport non fiable.
                if let Err(e) = socket.send_to(packet, destination).await {
                    debug!("⚠️ [RTSP] Paquet RTP perdu pour {destination} : {e}");
                    break;
                }
            }
        }

        None => {}
    }

    Ok(())
}

/// Envoie un rapport d'émetteur RTCP sur le canal ou le port prévu.
async fn send_sender_report(session: &Session, writer: &mut OwnedWriteHalf) -> Result<()> {
    let report = session.packetizer.sender_report();

    match session.transport {
        Some(Transport::Interleaved { rtcp_channel, .. }) => {
            let mut framed = Vec::new();
            push_interleaved(&mut framed, rtcp_channel, &report);

            writer
                .write_all(&framed)
                .await
                .context("émission RTCP entrelacée")?;
        }

        Some(Transport::Udp { rtcp_port, .. }) => {
            if let Some((_, socket)) = &session.udp {
                let destination = SocketAddr::new(session.peer.ip(), rtcp_port);
                let _ = socket.send_to(&report, destination).await;
            }
        }

        None => {}
    }

    Ok(())
}

/// Préfixe un paquet de son en-tête d'entrelacement (RFC 2326 §10.12) :
/// `$`, le canal, puis la longueur sur 16 bits en ordre réseau.
fn push_interleaved(out: &mut Vec<u8>, channel: u8, packet: &[u8]) {
    out.push(b'$');
    out.push(channel);
    out.extend_from_slice(&(packet.len() as u16).to_be_bytes());
    out.extend_from_slice(packet);
}

/// Identifiant de session, unique pour la durée de vie du processus.
fn new_session_identifier(peer: SocketAddr) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);

    // Le port du client entre dans l'identifiant pour que deux sessions
    // successives n'en partagent pas, même après redémarrage du compteur.
    format!(
        "{:08X}{:04X}",
        NEXT.fetch_add(1, Ordering::Relaxed),
        peer.port()
    )
}

/// Vrai si `path` désigne le flux configuré.
///
/// Trois formes doivent être acceptées, parce que ce sont les trois que les
/// lecteurs envoient réellement au cours d'un même dialogue :
///
/// - **`/stream`** — l'URL que l'utilisateur a saisie, utilisée pour le
///   `DESCRIBE` ;
/// - **`/stream/`** — l'URL de contrôle de la SESSION. Le SDP l'annonce par
///   `a=control:*`, que le lecteur résout en `Content-Base`, lequel porte une
///   barre oblique finale pour servir de préfixe aux URL de pistes. C'est
///   cette forme que GStreamer emploie pour son `PLAY` ;
/// - **`/stream/trackID=0`** — l'URL de contrôle de la PISTE, annoncée par
///   `a=control:trackID=0`. C'est celle du `SETUP`.
///
/// En refuser une seule renvoie un 404 au beau milieu d'un dialogue
/// parfaitement normal, et le lecteur abandonne.
fn matches_stream_path(path: &str, configured: &str) -> bool {
    // Une barre oblique finale ne désigne pas une autre ressource.
    let path = path.trim_end_matches('/');
    let configured = configured.trim_end_matches('/');

    if path == configured {
        return true;
    }

    path.strip_prefix(&format!("{configured}/"))
        // UNE seule composante en plus : le nom d'une piste, pas une
        // arborescence arbitraire.
        .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
}

/// Une URI débarrassée de sa chaîne de requête.
fn uri_without_query(uri: &str) -> &str {
    uri.split('?').next().unwrap_or(uri)
}

/// Normalise le chemin du flux : exactement une barre oblique initiale,
/// aucune finale.
///
/// `stream`, `/stream` et `/stream/` dans la configuration désignent tous le
/// même flux : un lecteur ne doit pas recevoir un 404 pour une barre oblique
/// de trop dans un fichier TOML.
fn normalize_path(path: &str) -> String {
    let trimmed = path.trim().trim_matches('/');

    if trimmed.is_empty() {
        "/".to_string()
    } else {
        format!("/{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_configured_path_is_normalized_to_a_single_leading_slash() {
        assert_eq!(normalize_path("stream"), "/stream");
        assert_eq!(normalize_path("/stream"), "/stream");
        assert_eq!(normalize_path("/stream/"), "/stream");
        assert_eq!(normalize_path("  stream  "), "/stream");
    }

    #[test]
    fn an_empty_path_becomes_the_root() {
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("/"), "/");
    }

    #[test]
    fn the_three_urls_a_player_uses_during_one_dialogue_are_all_accepted() {
        // Chacune provient d'une étape réelle du dialogue : l'URL saisie
        // (DESCRIBE), celle de la session (PLAY) et celle de la piste
        // (SETUP). En refuser une renvoie un 404 en pleine lecture — c'est
        // ce que faisait GStreamer sur les deux dernières.
        assert!(matches_stream_path("/stream", "/stream"), "URL saisie");
        assert!(matches_stream_path("/stream/", "/stream"), "URL de session");
        assert!(
            matches_stream_path("/stream/trackID=0", "/stream"),
            "URL de piste"
        );
    }

    #[test]
    fn the_same_holds_for_a_stream_served_at_the_root() {
        assert!(matches_stream_path("/", "/"));
        assert!(matches_stream_path("/trackID=0", "/"));
    }

    #[test]
    fn an_unrelated_path_is_still_refused() {
        assert!(!matches_stream_path("/autre", "/stream"));
        assert!(!matches_stream_path("/stream2", "/stream"));
        // Pas une arborescence arbitraire : juste le nom d'une piste.
        assert!(!matches_stream_path("/stream/a/b", "/stream"));
    }

    #[test]
    fn a_uri_is_stripped_of_its_query_string() {
        // Le `Content-Base` sert de préfixe au nom de la piste : y laisser
        // le jeton donnerait `…?token=x/trackID=0`.
        assert_eq!(
            uri_without_query("rtsp://cam:8554/stream?token=secret"),
            "rtsp://cam:8554/stream"
        );
        assert_eq!(
            uri_without_query("rtsp://cam:8554/stream"),
            "rtsp://cam:8554/stream"
        );
    }

    #[test]
    fn the_interleaved_frame_carries_the_dollar_channel_and_length() {
        let mut out = Vec::new();
        push_interleaved(&mut out, 3, &[0xAA, 0xBB, 0xCC]);

        assert_eq!(out, vec![b'$', 3, 0x00, 0x03, 0xAA, 0xBB, 0xCC]);
    }

    #[test]
    fn session_identifiers_are_distinct() {
        let peer: SocketAddr = "127.0.0.1:5000".parse().unwrap();
        assert_ne!(new_session_identifier(peer), new_session_identifier(peer));
    }

    // `UdpSocket::from_std` exige un réacteur Tokio : la paire de
    // chaussettes n'est créée que depuis une tâche, jamais à froid.
    #[tokio::test]
    async fn a_bound_udp_pair_is_usable_and_reports_its_ports() {
        let (_rtp, _rtcp, (rtp_port, rtcp_port)) = bind_udp_pair().expect("paire UDP");

        assert_ne!(rtp_port, 0);
        assert_ne!(rtcp_port, 0);
        assert_ne!(rtp_port, rtcp_port);
    }
}
