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
    http::StatusCode,
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

/// Paramètres de requête pour l'upgrade WebSocket (`?token=...`).
#[derive(Deserialize)]
pub struct AuthQuery {
    pub token: Option<String>,
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
/// Comme les autres routes de LECTURE, elle n'est pas authentifiée. L'ÉCRITURE
/// l'est (voir [`set_monitoring_handler`]).
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
    if auth.token.as_deref() != Some(state.api_token.as_str()) {
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

/// Handler pour lister les enregistrements disponibles dans `output_record/`
async fn list_recordings_handler(State(state): State<Arc<SharedState>>) -> Json<Vec<VideoFile>> {
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
    Json(files)
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
    State(state): State<Arc<SharedState>>,
    request: axum::extract::Request,
) -> Response {
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
/// atteint le port. (Les routes de LECTURE, elles, restent non
/// authentifiées, comme avant — voir la note dans le README.)
async fn delete_recording_handler(
    Path(filename): Path<String>,
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> StatusCode {
    let is_authorized = auth.token.as_deref() == Some(state.api_token.as_str());

    if !is_authorized {
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
    if auth.token.as_deref() != Some(state.api_token.as_str()) {
        warn!("⚠️ Tentative de connexion WebSocket rejetée (Token invalide).");
        return (StatusCode::UNAUTHORIZED, "Accès refusé").into_response();
    }

    let stream = Arc::clone(&state.h264);

    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);
    info!(
        "✅ [WS] Nouveau client connecté #{} (IP: {})",
        client_id, addr
    );

    ws.on_upgrade(move |socket| handle_socket(socket, state, stream, client_id, addr))
        .into_response()
}

/// Pousse le flux encodé à un navigateur, et traite ses commandes.
///
/// Les commandes comptent : l'interface n'ouvre qu'UNE connexion, qui porte
/// la vidéo dans un sens et le pilotage dans l'autre. Servir la vidéo sans
/// lire le client laisserait les interrupteurs de l'interface sans effet —
/// en silence, puisque rien côté page ne distingue un message ignoré d'un
/// message traité.
pub async fn handle_socket(
    socket: WebSocket,
    state: Arc<SharedState>,
    stream: Arc<H264Stream>,
    client_id: u64,
    addr: SocketAddr,
) {
    let (sender, receiver) = socket.split();

    let commands = spawn_command_task(receiver, state, client_id);

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

/// Handler pour servir le fichier HTML de contrôle
async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../static/controller.html"))
}

/// Page de lecture autonome d'un clip (`GET /play/{filename}`).
///
/// # Pourquoi la caméra sert une page de lecture
///
/// La timeline de l'interface du manager donne un accès direct au clip de
/// chaque détection. Or les clips restent SUR LA CAMÉRA : ils pèsent
/// plusieurs mégaoctets et n'ont aucune raison de traverser le réseau pour
/// finir dans une base de données (seule la vignette, elle, voyage dans
/// l'événement — voir `foxguard_protocol::DetectionEvent`).
///
/// L'interface du manager, servie par une autre origine, ne peut donc pas
/// lire ces fichiers elle-même : le navigateur le lui interdit, et ouvrir les
/// enregistrements à toutes les origines (`Access-Control-Allow-Origin: *`)
/// serait une bien mauvaise façon de contourner cette protection — ces routes
/// ne sont déjà pas authentifiées. Elle affiche donc cette page, servie par
/// la caméra, dans un cadre : la politique de même origine est respectée sans
/// rien assouplir.
///
/// Et le format d'enregistrement reste connu du seul composant qui l'écrit.
///
/// Le nom du fichier n'est PAS vérifié ici : la page est statique, elle lit
/// elle-même son nom dans l'URL et le redemande à
/// `GET /recordings/{filename}`, qui valide (voir [`is_safe_recording_name`]).
/// Servir la page pour un nom invalide ne donne donc accès à rien — la
/// requête de données qui suivra sera, elle, rejetée.
async fn clip_player_handler(Path(_filename): Path<String>) -> Html<&'static str> {
    Html(include_str!("../static/clip-player.html"))
}

/// Marqueur remplacé par le jeton d'API au moment de servir la page de
/// direct.
const API_TOKEN_PLACEHOLDER: &str = "__FOXGUARD_API_TOKEN__";

/// Vue en direct seule (`GET /live`).
///
/// # Pourquoi la caméra sert sa propre vue en direct
///
/// L'interface du manager donne accès au direct de chaque caméra, mais elle
/// est servie par une AUTRE ORIGINE : elle ne peut pas ouvrir elle-même le
/// WebSocket d'une caméra. Et quand bien même — il faudrait lui confier le
/// jeton d'API de chaque caméra, c'est-à-dire le recopier dans une base de
/// données puis dans une page web. Ce serait une dégradation nette du modèle
/// de sécurité pour afficher une image.
///
/// La caméra sert donc sa vue en direct, que le manager affiche dans un cadre
/// — exactement comme pour les clips. **Le jeton ne quitte jamais la caméra.**
///
/// La page est volontairement RÉDUITE au direct : pas d'interrupteur, pas de
/// capture, pas de suppression. Le manager est en lecture seule, et le
/// pilotage d'une caméra reste sur son interface complète.
///
/// Comme les autres routes de lecture, elle n'est pas authentifiée : c'est la
/// page, pas le flux. Le WebSocket qu'elle ouvre l'est, lui.
async fn live_handler(State(state): State<Arc<SharedState>>) -> Html<String> {
    // Le jeton est injecté ici plutôt qu'écrit dans le fichier : la page est
    // embarquée dans le binaire, et une valeur en dur y obligerait à
    // recompiler pour changer de jeton.
    Html(include_str!("../static/live.html").replace(API_TOKEN_PLACEHOLDER, &state.api_token))
}

/// Interrupteur de surveillance seul (`GET /control`).
///
/// # Pourquoi la caméra sert son propre interrupteur
///
/// Même raisonnement que pour [`live_handler`], et il vaut pour la même
/// raison : l'interface du manager est servie par une AUTRE ORIGINE et n'a pas
/// le jeton d'API de la caméra. Elle ne peut donc pas piloter la caméra
/// elle-même, et le lui permettre voudrait dire recopier le jeton de chaque
/// caméra dans une base de données puis dans une page web.
///
/// La caméra sert donc cette page, que le manager affiche dans un cadre —
/// comme le direct et comme les clips. **Le jeton ne quitte jamais la
/// caméra**, et le manager reste sans la moindre route d'écriture.
///
/// La page est volontairement RÉDUITE à l'interrupteur : pas de vidéo (voir
/// [`monitoring_handler`] pour pourquoi elle n'ouvre surtout pas le
/// WebSocket), pas de capture de référence, pas de suppression.
///
/// # Ce que cela suppose du réseau
///
/// Comme `/live`, cette page n'est pas authentifiée et porte le jeton en
/// clair : qui peut atteindre le port HTTP de la caméra peut la charger, donc
/// couper sa surveillance. C'est le modèle de sécurité qui était DÉJÀ celui de
/// `/live` — le port d'une caméra n'est pas destiné à être exposé tel quel sur
/// un réseau hostile — mais la conséquence est plus lourde ici, puisqu'il
/// s'agit d'une écriture et non d'une lecture.
async fn control_handler(State(state): State<Arc<SharedState>>) -> Html<String> {
    Html(include_str!("../static/control.html").replace(API_TOKEN_PLACEHOLDER, &state.api_token))
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
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg
                && let Ok(cmd) = serde_json::from_str::<ClientCommand>(&text)
            {
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
