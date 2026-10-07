//! Serveur HTTP / WebSocket (Axum) : sert l'interface de contrôle web,
//! diffuse le flux vidéo en direct, reçoit les commandes de l'UI (activer
//! la surveillance, capturer une photo de référence, ...) et expose la
//! liste et la relecture des enregistrements.

use axum::{
    Json, Router,
    extract::{
        ConnectInfo, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::{delete, get},
};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::broadcast;
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};
use tracing::{debug, error, info, warn};

use foxguard_protocol::ticket::Scope;

use crate::auth::{self, Access, has_token};
use crate::capture::SharedState;
use crate::h264::{H264Stream, NAL_SPS, nal_type};
use crate::retention::is_recording_file;

/// Marqueur de début d'une NAL au format Annex-B.
///
/// C'est ce que `VideoDecoder` attend quand on le configure SANS
/// `description` : il y lit lui-même les paramètres de séquence, qui sont
/// réémis avec chaque image clé (voir `crate::h264`). L'alternative (format
/// AVCC, longueurs préfixées, paramètres passés à la configuration) obligerait
/// à transporter ces paramètres à part et à les tenir à jour — pour aucun
/// gain ici.
const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];

/// En-tête binaire précédant chaque unité d'accès sur `GET /ws`.
///
/// `[u8 image clé][u64 horodatage en microsecondes]`, en ordre réseau. Le
/// navigateur en a besoin pour construire un `EncodedVideoChunk` : le type
/// (`key` ou `delta`) décide s'il peut commencer à décoder là, et
/// l'horodatage doit croître.
const H264_FRAME_HEADER: usize = 1 + 8;

// Compteur global pour attribuer un ID unique séquentiel à chaque client
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

/// Un enregistrement vidéo disponible dans `output_record/`, tel que
/// renvoyé par `GET /api/recordings`.
#[derive(Serialize)]
pub struct VideoFile {
    pub name: String,
    pub size_mb: f64,
}

pub use crate::auth::AuthQuery;

/// Ce qu'une connexion WebSocket a le droit de faire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WsAccess {
    /// Ouverte avec le jeton d'API : vidéo ET commandes. C'est l'interface
    /// complète de la caméra.
    Full,
    /// Ouverte avec un ticket du manager : vidéo seulement. Les commandes
    /// reçues sont ignorées — le manager est en lecture seule, et un ticket
    /// qui permettrait de couper la surveillance donnerait à l'interface du
    /// manager exactement le pouvoir qu'on refuse de lui confier.
    ViewOnly,
}

/// Commandes JSON reçues par WebSocket
#[derive(Deserialize)]
#[serde(tag = "command")]
pub enum ClientCommand {
    #[serde(rename = "set_monitoring")]
    SetMonitoring { enabled: bool },
    #[serde(rename = "set_detection")]
    SetDetection { enabled: bool },
    #[serde(rename = "set_recording")]
    SetRecording { enabled: bool },
    /// Capture la prochaine frame caméra comme nouveau gabarit de référence
    /// pour la reconnaissance faciale (s'ajoute aux gabarits existants pour
    /// ce nom, voir `capture::start_camera_loop`).
    #[serde(rename = "capture_reference")]
    CaptureReference { name: String },
}

/// État de surveillance de la caméra, tel que renvoyé par
/// `GET /api/monitoring` et modifié par `POST /api/monitoring`.
///
/// Les deux interrupteurs sont renvoyés séparément parce qu'ils le sont
/// réellement (voir [`ClientCommand::SetDetection`] et
/// [`ClientCommand::SetRecording`]), même si la « surveillance » au sens de
/// l'interface les bascule ensemble.
#[derive(Serialize, Deserialize, PartialEq, Eq, Debug)]
pub struct MonitoringState {
    /// Surveillance IA (YOLO/YuNet/ArcFace).
    pub detection: bool,
    /// Enregistrement continu sur disque.
    pub recording: bool,
}

/// Corps de `POST /api/monitoring`.
#[derive(Deserialize)]
pub struct MonitoringRequest {
    pub enabled: bool,
}

/// État de la surveillance (`GET /api/monitoring`).
///
/// # Pourquoi une route HTTP et pas la commande WebSocket existante
///
/// L'interrupteur existe déjà sur le WebSocket (voir
/// [`ClientCommand::SetMonitoring`]), et c'est par là que passe l'interface
/// complète de la caméra — qui a de toute façon le flux vidéo ouvert.
///
/// Mais s'abonner au WebSocket est précisément ce qui DÉMARRE l'encodage
/// H.264 (voir `crate::capture::capture_loop`). Une page qui n'affiche qu'un
/// interrupteur ferait donc tourner l'encodeur logiciel — le poste de dépense
/// le plus lourd du système — pour une image que personne ne regarde. C'est
/// exactement le garde-fou que la caméra s'applique à respecter.
///
/// Deux appels HTTP, eux, ne coûtent rien et ne réveillent personne.
///
/// Elle n'est PAS authentifiée : elle ne révèle qu'un booléen — la caméra
/// veille, ou non — et la page de pilotage doit pouvoir l'afficher avant que
/// l'utilisateur ne touche à quoi que ce soit. L'ÉCRITURE l'est (voir
/// [`set_monitoring_handler`]).
async fn monitoring_handler(State(state): State<Arc<SharedState>>) -> Json<MonitoringState> {
    Json(MonitoringState {
        detection: state.detection_enabled.load(Ordering::Relaxed),
        recording: state.recording_enabled.load(Ordering::Relaxed),
    })
}

/// Active ou coupe la surveillance (`POST /api/monitoring?token=...`).
///
/// AUTHENTIFIÉE par le même jeton que le WebSocket et que la suppression
/// d'enregistrement : couper la surveillance d'une caméra est l'action la plus
/// lourde de conséquences qu'elle expose, et elle ne peut pas rester ouverte à
/// quiconque atteint le port.
///
/// Les deux interrupteurs sont basculés ENSEMBLE, exactement comme
/// [`ClientCommand::SetMonitoring`] : « surveillance » veut dire détecter et
/// enregistrer, et les séparer donnerait à l'interface du manager un réglage
/// que l'interface de la caméra n'a pas.
///
/// Retourne le nouvel état, pour que l'appelant n'ait pas à le redemander.
async fn set_monitoring_handler(
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
    Json(request): Json<MonitoringRequest>,
) -> Response {
    // Le jeton, ou un ticket de portée « surveillance » : c'est ce que reçoit
    // la page `/control` ouverte depuis le manager (voir `crate::auth`).
    if auth::authorize(&auth, &state, Scope::Monitoring).is_none() {
        warn!("⚠️ Tentative de pilotage de la surveillance rejetée (Token invalide).");
        return (StatusCode::UNAUTHORIZED, "Accès refusé").into_response();
    }

    state
        .detection_enabled
        .store(request.enabled, Ordering::Relaxed);
    state
        .recording_enabled
        .store(request.enabled, Ordering::Relaxed);

    info!("🛡️ Surveillance modifiée par HTTP : {}", request.enabled);

    Json(MonitoringState {
        detection: request.enabled,
        recording: request.enabled,
    })
    .into_response()
}

/// Vrai si la requête porte le jeton d'API de cette caméra.
///
/// Partagée par TOUTES les routes réservées au jeton, et c'est le point : une
/// vérification plus laxiste d'un côté que de l'autre est une faille, et c'est
/// exactement ce qui s'était produit — la suppression d'un enregistrement
/// était protégée, son TÉLÉCHARGEMENT ne l'était pas.
fn is_authorized(auth: &AuthQuery, state: &SharedState) -> bool {
    has_token(auth, state)
}

/// Liste des enregistrements disponibles (`GET /api/recordings?token=...`).
///
/// # Pourquoi les archives sont authentifiées
///
/// Elles ne l'étaient pas, et c'était le trou le plus large du système : le
/// WebSocket du direct exigeait un jeton, mais cette route et le
/// téléchargement laissaient quiconque atteignait le port récupérer
/// l'INTÉGRALITÉ des enregistrements — y compris le clip de chaque détection.
/// Refuser à un inconnu de voir la scène en direct pour lui offrir la même
/// scène enregistrée ne protégeait rien.
async fn list_recordings_handler(
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> Response {
    if !is_authorized(&auth, &state) {
        warn!("⚠️ Liste des enregistrements refusée (Token invalide).");
        return (StatusCode::UNAUTHORIZED, "Accès refusé").into_response();
    }

    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir(&state.recordings_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && is_recording_file(&path)
                && let (Some(name), Ok(metadata)) = (path.file_name(), path.metadata())
            {
                let size_mb = (metadata.len() as f64) / (1024.0 * 1024.0);
                files.push(VideoFile {
                    name: name.to_string_lossy().to_string(),
                    size_mb: (size_mb * 100.0).round() / 100.0,
                });
            }
        }
    }

    // Tri par nom décroissant (du plus récent au plus ancien)
    files.sort_by(|a, b| b.name.cmp(&a.name));
    Json(files).into_response()
}

/// Sert un fichier d'enregistrement (`GET /recordings/{filename}`).
///
/// # Pourquoi déléguer à `ServeFile`
///
/// Les enregistrements sont désormais des **MP4** dès que l'encodage H.264
/// est actif (voir `crate::capture::recording`), et un MP4 se lit dans un
/// `<video>`. Or un `<video>` ne se contente pas de télécharger : il demande
/// des PLAGES D'OCTETS, pour ne charger que ce qu'il affiche et pour permettre
/// de se déplacer dans la vidéo. Un serveur qui renvoie toujours le fichier
/// entier le laisse lire du début à la fin, sans jamais pouvoir avancer — ce
/// qui est inexploitable sur un enregistrement de plusieurs heures.
///
/// `ServeFile` apporte tout cela : requêtes par plage, type de contenu déduit
/// de l'extension (`video/mp4`), `Last-Modified` et réponses 304. Le
/// réécrire à la main n'apporterait que des occasions de se tromper.
///
/// Le nom de fichier est validé AVANT, lui (voir [`is_safe_recording_name`]) :
/// `ServeFile` ne reçoit qu'un chemin déjà sûr.
async fn recording_handler(
    Path(filename): Path<String>,
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
    request: axum::extract::Request,
) -> Response {
    // AUTHENTIFIÉ, comme la liste : c'est ici que passent les octets des
    // enregistrements (voir [`list_recordings_handler`] pour pourquoi). Un
    // ticket n'ouvre que le clip pour lequel il a été émis.
    if auth::authorize(&auth, &state, Scope::Clip(&filename)).is_none() {
        warn!("⚠️ Téléchargement d'un enregistrement refusé (Token invalide).");
        return (StatusCode::UNAUTHORIZED, "Accès refusé").into_response();
    }

    if !is_safe_recording_name(&filename) {
        return StatusCode::BAD_REQUEST.into_response();
    }

    let filepath = std::path::Path::new(&state.recordings_dir).join(&filename);

    match ServeFile::new(filepath).oneshot(request).await {
        Ok(response) => response.into_response(),
        Err(e) => {
            error!("❌ Lecture de l'enregistrement {filename} impossible : {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Valide un nom d'enregistrement reçu du client avant tout accès disque.
///
/// N'accepte qu'un nom de fichier SIMPLE portant une extension
/// d'enregistrement : ni chemin, ni remontée de répertoire. Sans cette
/// vérification, `GET /recordings/..%2F..%2Fetc%2Fpasswd` ou un `DELETE` du
/// même acabit sortiraient du dossier des enregistrements.
///
/// Partagée par le téléchargement et la suppression : une vérification plus
/// laxiste d'un côté que de l'autre serait une faille, d'autant que le
/// second est destructif.
fn is_safe_recording_name(filename: &str) -> bool {
    !filename.is_empty()
        && !filename.contains("..")
        && !filename.contains('/')
        && !filename.contains('\\')
        && is_recording_file(std::path::Path::new(filename))
}

/// Handler de suppression d'un enregistrement, déclenché depuis l'interface
/// web (`DELETE /api/recordings/{filename}?token=...`).
///
/// AUTHENTIFIÉ par le même jeton que le WebSocket : c'est la seule route
/// destructive du serveur, elle ne peut pas rester ouverte à quiconque
/// atteint le port. Les archives le sont désormais aussi en LECTURE (voir
/// [`list_recordings_handler`]) ; seules les PAGES restent servies sans
/// jeton, puisque ce sont elles qui le portent.
async fn delete_recording_handler(
    Path(filename): Path<String>,
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> StatusCode {
    if !is_authorized(&auth, &state) {
        warn!("⚠️ Tentative de suppression d'enregistrement rejetée (Token invalide).");
        return StatusCode::UNAUTHORIZED;
    }

    if !is_safe_recording_name(&filename) {
        return StatusCode::BAD_REQUEST;
    }

    let filepath = std::path::Path::new(&state.recordings_dir).join(&filename);

    match std::fs::remove_file(&filepath) {
        Ok(()) => {
            info!("🗑️ Enregistrement supprimé : {}", filepath.display());
            StatusCode::NO_CONTENT
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        Err(e) => {
            error!(
                "❌ Suppression impossible pour {} : {}",
                filepath.display(),
                e
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// Ce que la caméra sait faire, pour que l'interface choisisse son flux
/// Upgrade WebSocket du flux vidéo (`GET /ws?token=...`).
///
/// # Pourquoi un WebSocket et pas le flux RTSP
///
/// **Aucun navigateur n'implémente RTSP.** Pour afficher du H.264 dans une
/// page, il faut le lui apporter par un transport qu'il connaît, et le faire
/// décoder par lui. On réutilise donc le WebSocket — déjà en place, déjà
/// authentifié — et le navigateur décode avec `VideoDecoder` (WebCodecs) vers
/// un `<canvas>`.
///
/// C'est la voie la moins coûteuse des trois possibles : pas de conteneur à
/// écrire (contrairement au MP4 fragmenté qu'exigerait un `<video>`), et pas
/// de pile WebRTC — dont les dépendances cryptographiques sont précisément ce
/// que la compilation croisée ARM64 de ce dépôt s'applique à éviter.
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> Response {
    let access = match auth::authorize(&auth, &state, Scope::Stream) {
        Some(Access::Token) => WsAccess::Full,
        Some(Access::Ticket) => WsAccess::ViewOnly,
        None => {
            warn!("⚠️ Tentative de connexion WebSocket rejetée (Token invalide).");
            return (StatusCode::UNAUTHORIZED, "Accès refusé").into_response();
        }
    };

    // La place est réservée AVANT l'upgrade : deux connexions simultanées ne
    // peuvent pas passer toutes les deux sur la dernière place libre.
    let Some(slot) = StreamSlot::acquire(&state) else {
        warn!(
            "⚠️ [WS] Connexion refusée pour {addr} : {} clients déjà connectés \
             (`[server] max_stream_clients`).",
            state.max_stream_clients
        );
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Trop de clients connectés au flux vidéo",
        )
            .into_response();
    };

    let stream = Arc::clone(&state.h264);

    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    info!(
        "✅ [WS] Nouveau client connecté #{} (IP: {}, accès : {:?})",
        client_id, addr, access
    );

    ws.on_upgrade(move |socket| async move {
        // La place est libérée quand la connexion se termine, quelle qu'en
        // soit la raison.
        let _slot = slot;
        handle_socket(socket, state, stream, client_id, addr, access).await;
    })
    .into_response()
}

/// Une place parmi les `max_stream_clients` du flux vidéo, rendue à sa
/// destruction.
struct StreamSlot(Arc<SharedState>);

impl StreamSlot {
    fn acquire(state: &Arc<SharedState>) -> Option<Self> {
        state
            .stream_clients
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < state.max_stream_clients).then_some(count + 1)
            })
            .ok()
            .map(|_| Self(Arc::clone(state)))
    }
}

impl Drop for StreamSlot {
    fn drop(&mut self) {
        self.0.stream_clients.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Pousse le flux encodé à un navigateur, et traite ses commandes.
///
/// Les commandes comptent : l'interface n'ouvre qu'UNE connexion, qui porte
/// la vidéo dans un sens et le pilotage dans l'autre. Servir la vidéo sans
/// lire le client laisserait les interrupteurs de l'interface sans effet —
/// en silence, puisque rien côté page ne distingue un message ignoré d'un
/// message traité.
async fn handle_socket(
    socket: WebSocket,
    state: Arc<SharedState>,
    stream: Arc<H264Stream>,
    client_id: u64,
    addr: SocketAddr,
    access: WsAccess,
) {
    let (sender, receiver) = socket.split();

    let commands = spawn_command_task(receiver, state, client_id, access);

    tokio::select! {
        _ = commands => {},
        _ = push_h264_frames(sender, stream, addr) => {},
    }

    info!("❌ [WS] Client déconnecté #{} (IP: {})", client_id, addr);
}

/// Boucle d'émission du flux H.264.
async fn push_h264_frames(
    mut sender: SplitSink<WebSocket, Message>,
    stream: Arc<H264Stream>,
    addr: SocketAddr,
) {
    // L'abonnement AVANT la demande d'image clé : s'abonner est ce qui
    // déclenche l'encodage, et une image clé demandée à un encodeur à l'arrêt
    // serait produite puis oubliée.
    let mut frames = stream.subscribe();
    stream.request_keyframe();

    // Horodatage propre à cette connexion, en microsecondes. Il doit croître,
    // sans quoi le décodeur rejette les frames ; sa valeur absolue n'importe
    // pas, puisque la page affiche chaque image dès qu'elle arrive.
    let mut timestamp_us: u64 = 0;
    let mut started = false;

    loop {
        let unit = match frames.recv().await {
            Ok(unit) => unit,

            // Le client n'a pas suivi la cadence. On repart d'une image clé :
            // son décodeur resterait sinon bloqué sur une image de référence
            // qu'il n'a jamais reçue.
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                debug!("⏭️ [WS H.264] {skipped} frame(s) sautée(s) pour {addr}");
                stream.request_keyframe();
                started = false;
                continue;
            }

            Err(broadcast::error::RecvError::Closed) => break,
        };

        // Un décodeur ne peut commencer que sur une image clé : tout ce qui
        // précède la première est inutile, et le lui envoyer ne produirait
        // que des erreurs dans sa console.
        if !started {
            if !unit.keyframe {
                continue;
            }

            // La chaîne de codec se lit dans le SPS de CETTE image clé, qui
            // l'accompagne toujours. Elle précède la première frame : sans
            // elle, le navigateur n'a pas de quoi configurer son décodeur.
            let Some(codec) = codec_string(&unit.nals) else {
                warn!("⚠️ [WS H.264] Image clé sans SPS exploitable, client {addr} non servi.");
                continue;
            };

            let announcement = format!(r#"{{"type":"config","codec":"{codec}"}}"#);

            if sender
                .send(Message::Text(announcement.into()))
                .await
                .is_err()
            {
                break;
            }

            started = true;
        }

        if sender
            .send(Message::Binary(
                encode_h264_frame(&unit, timestamp_us).into(),
            ))
            .await
            .is_err()
        {
            break;
        }

        timestamp_us += 1_000;
    }
}

/// Sérialise une unité d'accès pour le navigateur : l'en-tête documenté par
/// [`H264_FRAME_HEADER`], puis les NAL au format Annex-B.
fn encode_h264_frame(unit: &crate::h264::AccessUnit, timestamp_us: u64) -> Vec<u8> {
    let payload: usize = unit
        .nals
        .iter()
        .map(|nal| ANNEX_B_START_CODE.len() + nal.len())
        .sum();

    let mut message = Vec::with_capacity(H264_FRAME_HEADER + payload);

    message.push(u8::from(unit.keyframe));
    message.extend_from_slice(&timestamp_us.to_be_bytes());

    for nal in &unit.nals {
        // Les NAL arrivent sans préfixe (c'est RTP qui les voulait ainsi, voir
        // `crate::rtsp::rtp`) : on le remet pour le décodeur du navigateur.
        message.extend_from_slice(&ANNEX_B_START_CODE);
        message.extend_from_slice(nal);
    }

    message
}

/// Chaîne de codec attendue par `VideoDecoder.configure`, lue dans le SPS
/// d'une unité d'accès.
///
/// `avc1.PPCCLL` : profil, contraintes et niveau — les trois octets qui
/// suivent l'en-tête de la NAL. Le navigateur s'en sert pour savoir s'il sait
/// décoder AVANT de recevoir la moindre frame, et refuse de se configurer
/// sans.
///
/// Même source que le `profile-level-id` du SDP (voir `crate::rtsp`), lue ici
/// sur la frame elle-même plutôt que sur le jeu de paramètres mémorisé : elle
/// y est forcément, et c'est une occasion de moins de servir au navigateur un
/// codec qui ne décrit pas ce qu'il va recevoir.
fn codec_string(nals: &[Vec<u8>]) -> Option<String> {
    let sps = nals.iter().find(|nal| nal_type(nal) == Some(NAL_SPS))?;
    let bytes = sps.get(1..4)?;

    Some(format!(
        "avc1.{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2]
    ))
}

/// Interface complète de la caméra (`GET /`).
///
/// Elle reçoit le jeton d'API, puisqu'elle pilote TOUT : elle est donc
/// protégée par une authentification HTTP Basic dont le mot de passe est ce
/// même jeton (voir [`auth::has_basic_token`]). Elle était auparavant servie à
/// quiconque atteignait le port — et avec elle, le jeton maître.
///
/// Le jeton y est injecté au moment de servir la page plutôt qu'écrit dans le
/// fichier : il y était auparavant ÉCRIT EN DUR, et changer `[server]
/// api_token` cassait silencieusement l'interface.
async fn index_handler(State(state): State<Arc<SharedState>>, headers: HeaderMap) -> Response {
    if !auth::has_basic_token(&headers, &state) {
        return auth::basic_challenge();
    }

    Html(include_str!("../static/controller.html").replace(API_TOKEN_PLACEHOLDER, &state.api_token))
        .into_response()
}

/// Marqueur remplacé par le jeton d'API dans l'interface complète.
const API_TOKEN_PLACEHOLDER: &str = "__FOXGUARD_API_TOKEN__";

/// Marqueur remplacé, dans les pages à portée limitée, par la chaîne de
/// requête qui les authentifie (`ticket=…`, ou `token=…` si la page a été
/// ouverte avec le jeton). Voir [`auth::page_auth_query`].
const AUTH_QUERY_PLACEHOLDER: &str = "__FOXGUARD_AUTH_QUERY__";

/// Sert une page à portée limitée, ou la refuse.
///
/// # Pourquoi ces pages existent
///
/// L'interface du manager est servie par une AUTRE ORIGINE : elle ne peut lire
/// ni les enregistrements de la caméra, ni piloter sa surveillance — le
/// navigateur le lui interdit, et ouvrir ces routes à toutes les origines
/// (`Access-Control-Allow-Origin: *`) serait une bien mauvaise façon de
/// contourner cette protection. La caméra sert donc elle-même la page capable
/// de le faire, que le manager affiche dans un cadre. Et le format
/// d'enregistrement reste connu du seul composant qui l'écrit.
///
/// # Pourquoi elles exigent un ticket
///
/// Elles étaient servies sans authentification, avec le jeton maître injecté :
/// qui atteignait le port HTTP de la caméra en extrayait un jeton capable de
/// supprimer les archives. Elles exigent désormais un ticket de LEUR portée,
/// que le manager signe au moment du clic, et reçoivent en retour un ticket de
/// session de même portée — jamais le jeton.
fn scoped_page(
    page: &'static str,
    auth: &AuthQuery,
    state: &SharedState,
    scope: Scope<'_>,
) -> Response {
    let Some(access) = auth::authorize(auth, state, scope) else {
        return (
            StatusCode::UNAUTHORIZED,
            Html(
                "<!doctype html><meta charset=\"utf-8\"><title>FoxGuard</title>\
                 <body style=\"font-family:sans-serif;background:#121212;color:#e0e0e0;padding:24px\">\
                 <p>Accès refusé : ouvrez cette page depuis l'interface du manager, \
                 ou avec le jeton de la caméra.</p></body>",
            ),
        )
            .into_response();
    };

    Html(page.replace(
        AUTH_QUERY_PLACEHOLDER,
        &auth::page_auth_query(access, state, scope),
    ))
    .into_response()
}

/// Page de lecture autonome d'un clip (`GET /play/{filename}?ticket=…`).
///
/// Le ticket n'ouvre QUE ce clip : la page reçoit de quoi lire ce fichier,
/// pas la liste des archives ni le droit d'en supprimer.
///
/// Le nom du fichier n'est PAS validé ici : il n'entre que dans la portée du
/// ticket, et `GET /recordings/{filename}`, qui sert les octets, le valide
/// (voir [`is_safe_recording_name`]).
async fn clip_player_handler(
    Path(filename): Path<String>,
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> Response {
    scoped_page(
        include_str!("../static/clip-player.html"),
        &auth,
        &state,
        Scope::Clip(&filename),
    )
}

/// Vue en direct seule (`GET /live?ticket=…`).
///
/// L'interface du manager n'en a plus besoin — elle décode le direct
/// elle-même — mais la page reste utile pour afficher le direct d'une seule
/// caméra, sur un écran dédié par exemple. Volontairement RÉDUITE au direct :
/// pas d'interrupteur, pas de capture, pas de suppression.
async fn live_handler(
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> Response {
    scoped_page(
        include_str!("../static/live.html"),
        &auth,
        &state,
        Scope::Stream,
    )
}

/// Interrupteur de surveillance seul (`GET /control?ticket=…`).
///
/// Le manager n'a toujours AUCUNE route d'écriture : il signe un ticket de
/// portée « surveillance », et c'est cette page, servie par la caméra, qui
/// appelle `POST /api/monitoring` sur elle-même.
///
/// La page est volontairement RÉDUITE à l'interrupteur : pas de vidéo (voir
/// [`monitoring_handler`] pour pourquoi elle n'ouvre surtout pas le
/// WebSocket), pas de capture de référence, pas de suppression.
async fn control_handler(
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> Response {
    scoped_page(
        include_str!("../static/control.html"),
        &auth,
        &state,
        Scope::Monitoring,
    )
}

/// Démarre la tâche qui traite les commandes JSON d'un client.
///
/// Séparée de l'émission vidéo parce que les deux sens de la connexion n'ont
/// rien à voir : la vidéo descend en continu, les commandes montent par
/// à-coups. Les traiter dans la même boucle ferait attendre un interrupteur
/// derrière la frame en cours d'envoi.
fn spawn_command_task(
    mut receiver: SplitStream<WebSocket>,
    state_cmd: Arc<SharedState>,
    client_id: u64,
    access: WsAccess,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // La lecture continue même en lecture seule : c'est elle qui détecte
        // la fermeture de la connexion par le navigateur.
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg
                && let Ok(cmd) = serde_json::from_str::<ClientCommand>(&text)
            {
                if access != WsAccess::Full {
                    warn!(
                        "⚠️ [WS Client #{}] Commande ignorée : connexion ouverte par ticket, \
                         en lecture seule.",
                        client_id
                    );
                    continue;
                }

                match cmd {
                    ClientCommand::SetMonitoring { enabled } => {
                        state_cmd
                            .detection_enabled
                            .store(enabled, Ordering::Relaxed);
                        state_cmd
                            .recording_enabled
                            .store(enabled, Ordering::Relaxed);
                        info!(
                            "🛡️ [WS Client #{}] A modifié la surveillance générale : {}",
                            client_id, enabled
                        );
                    }
                    ClientCommand::SetDetection { enabled } => {
                        state_cmd
                            .detection_enabled
                            .store(enabled, Ordering::Relaxed);
                        info!(
                            "🔍 [WS Client #{}] A modifié la détection : {}",
                            client_id, enabled
                        );
                    }
                    ClientCommand::SetRecording { enabled } => {
                        state_cmd
                            .recording_enabled
                            .store(enabled, Ordering::Relaxed);
                        info!(
                            "💾 [WS Client #{}] A modifié l'enregistrement : {}",
                            client_id, enabled
                        );
                    }
                    ClientCommand::CaptureReference { name } => {
                        if let Ok(mut pending) = state_cmd.pending_enrollment.lock() {
                            *pending = Some(name.clone());
                        }
                        info!(
                            "📸 [WS Client #{}] Capture de photo de référence demandée pour : {}",
                            client_id, name
                        );
                    }
                }
            }
        }
    })
}

/// Helper pour instancier le routeur Axum
pub fn create_router(state: Arc<SharedState>) -> Router {
    // S'assurer que le dossier des enregistrements existe
    let _ = std::fs::create_dir_all(&state.recordings_dir);

    Router::new()
        .route("/", get(index_handler)) // Servir l'interface web sur la racine
        .route("/play/{filename}", get(clip_player_handler))
        .route("/live", get(live_handler))
        .route("/control", get(control_handler))
        .route("/ws", get(ws_handler))
        .route(
            "/api/monitoring",
            get(monitoring_handler).post(set_monitoring_handler),
        )
        .route("/api/recordings", get(list_recordings_handler)) // API Liste des vidéos
        .route("/recordings/{filename}", get(recording_handler))
        .route(
            "/api/recordings/{filename}",
            delete(delete_recording_handler),
        )
        .nest_service("/static", ServeDir::new("static"))
        // Les couches s'appliquent de l'intérieur vers l'extérieur : les
        // en-têtes de sécurité sont posés sur TOUTES les réponses, y compris
        // le 429 de la limitation.
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::throttle_failures,
        ))
        .layer(middleware::from_fn_with_state(
            Arc::clone(&state),
            auth::security_headers,
        ))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::AccessUnit;

    /// SPS plausible : en-tête 0x67, profil 0x42 (Baseline), contraintes
    /// 0xC0, niveau 0x1E (3.0).
    fn keyframe() -> AccessUnit {
        AccessUnit {
            nals: vec![
                vec![0x67, 0x42, 0xC0, 0x1E, 0xAB],
                vec![0x68, 0xCE, 0x3C, 0x80],
                vec![0x65, 1, 2, 3],
            ],
            keyframe: true,
            rtp_timestamp: 0,
        }
    }

    #[test]
    fn the_codec_string_is_read_from_the_sps() {
        assert_eq!(
            codec_string(&keyframe().nals).as_deref(),
            Some("avc1.42c01e")
        );
    }

    #[test]
    fn a_frame_without_an_sps_yields_no_codec_string() {
        // Une frame P n'en porte pas : c'est bien pour ça qu'on attend une
        // image clé avant d'annoncer quoi que ce soit.
        let delta = vec![vec![0x41u8, 1, 2, 3]];
        assert_eq!(codec_string(&delta), None);
    }

    #[test]
    fn a_truncated_sps_yields_no_codec_string_rather_than_a_panic() {
        assert_eq!(codec_string(&[vec![0x67, 0x42]]), None);
    }

    #[test]
    fn a_frame_is_serialized_with_its_header_then_annex_b_nals() {
        let message = encode_h264_frame(&keyframe(), 12_345);

        assert_eq!(message[0], 1, "image clé");
        assert_eq!(
            u64::from_be_bytes(message[1..9].try_into().unwrap()),
            12_345
        );

        // Chaque NAL est précédée de son préfixe de délimitation : c'est ce
        // que `VideoDecoder` attend quand il est configuré sans description.
        let payload = &message[H264_FRAME_HEADER..];
        assert_eq!(&payload[..4], &ANNEX_B_START_CODE);
        assert_eq!(&payload[4..9], &[0x67, 0x42, 0xC0, 0x1E, 0xAB]);
        assert_eq!(&payload[9..13], &ANNEX_B_START_CODE);
    }

    #[test]
    fn a_delta_frame_is_marked_as_such() {
        // Le navigateur en fait un `EncodedVideoChunk` de type `delta`, sur
        // lequel il ne tentera pas de démarrer un décodage.
        let delta = AccessUnit {
            nals: vec![vec![0x41, 9, 9]],
            keyframe: false,
            rtp_timestamp: 3600,
        };

        assert_eq!(encode_h264_frame(&delta, 0)[0], 0);
    }

    #[test]
    fn the_serialized_length_matches_the_documented_layout() {
        let unit = keyframe();
        let expected: usize = H264_FRAME_HEADER
            + unit
                .nals
                .iter()
                .map(|nal| ANNEX_B_START_CODE.len() + nal.len())
                .sum::<usize>();

        assert_eq!(encode_h264_frame(&unit, 0).len(), expected);
    }
}
