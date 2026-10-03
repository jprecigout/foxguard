//! Serveur HTTP / WebSocket (Axum) : sert l'interface de contrôle web,
//! diffuse le flux vidéo en direct, reçoit les commandes de l'UI (activer
//! la surveillance, capturer une photo de référence, ...) et expose la
//! liste et la relecture des enregistrements.

use axum::{
    Json, Router,
    body::Body,
    extract::{
        ConnectInfo, Path, Query, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::{delete, get},
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::broadcast::error::RecvError;
use tower_http::services::ServeDir;
use tracing::{error, info, warn};

use crate::capture::SharedState;
use crate::retention::is_recording_file;

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

/// Handler qui sert le fichier d'enregistrement `.mjpeg` tel quel (format
/// "framed" horodaté par frame, voir `crate::capture::recording`), pour
/// téléchargement complet puis lecture côté client (voir `playVideo` dans
/// `static/controller.html`), qui recadence la relecture d'après les
/// horodatages réels embarqués dans le fichier plutôt qu'un débit fixe.
///
/// IMPORTANT : cet handler servait auparavant le fichier via un flux
/// `multipart/x-mixed-replace` retemporisé artificiellement à 33ms/frame
/// côté serveur — pensé pour un `<img>` affiché en direct pendant le
/// téléchargement. Or le client télécharge le fichier en entier
/// (`await response.arrayBuffer()`) avant d'en faire quoi que ce soit : ce
/// retemporisage ne faisait donc que ralentir inutilement le téléchargement
/// (jusqu'à plusieurs secondes pour un enregistrement de quelques centaines
/// de frames) sans aucun bénéfice. On sert maintenant le fichier tel quel,
/// aussi vite que le réseau le permet.
async fn stream_mjpeg_handler(
    Path(filename): Path<String>,
    State(state): State<Arc<SharedState>>,
) -> Result<Response, StatusCode> {
    if !is_safe_recording_name(&filename) {
        return Err(StatusCode::BAD_REQUEST);
    }

    let filepath = std::path::Path::new(&state.recordings_dir).join(&filename);
    let bytes = tokio::fs::read(&filepath)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    let response = Response::builder()
        .header("Content-Type", "application/octet-stream")
        .header("Cache-Control", "no-cache, no-store, must-revalidate")
        .body(Body::from(bytes))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(response)
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

/// Handler de mise à niveau vers WebSocket avec authentification par token
pub async fn ws_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Query(auth): Query<AuthQuery>,
    State(state): State<Arc<SharedState>>,
) -> impl IntoResponse {
    let is_authorized = match auth.token {
        Some(token) => token == state.api_token,
        None => false,
    };

    if !is_authorized {
        warn!("⚠️ Tentative de connexion WebSocket rejetée (Token invalide).");
        return (StatusCode::UNAUTHORIZED, "Accès refusé").into_response();
    }

    // Génération d'un ID unique pour identifier ce client
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);

    info!(
        "✅ [WS] Nouveau client connecté #{} (IP: {})",
        client_id, addr
    );
    ws.on_upgrade(move |socket| handle_socket(socket, state, client_id, addr))
        .into_response()
}

/// Gère une connexion WebSocket déjà établie : diffuse le flux vidéo au
/// client et traite les commandes JSON qu'il envoie (voir [`ClientCommand`]).
pub async fn handle_socket(
    socket: WebSocket,
    state: Arc<SharedState>,
    client_id: u64,
    addr: SocketAddr,
) {
    let (mut sender, mut receiver) = socket.split();
    let mut rx = state.tx.subscribe();

    // Tâche 1: Stream Vidéo (Envoi de données binaires JPEG)
    let send_task = tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    // Si l'envoi vers le navigateur échoue (client déconnecté), on stoppe
                    if sender.send(Message::Binary(frame.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(_skipped)) => {
                    continue;
                }
                Err(RecvError::Closed) => {
                    break;
                }
            }
        }
    });

    // Tâche 2: Commandes entraintes par texte/JSON
    let state_cmd = Arc::clone(&state);
    let recv_task = tokio::spawn(async move {
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
    });

    // Terminer si l'une des tâches s'arrête
    tokio::select! {
        _ = send_task => {},
        _ = recv_task => {},
    }

    // 🔴 Lors de la déconnexion
    info!("❌ [WS] Client déconnecté #{} (IP: {})", client_id, addr);
}

/// Helper pour instancier le routeur Axum
pub fn create_router(state: Arc<SharedState>) -> Router {
    // S'assurer que le dossier des enregistrements existe
    let _ = std::fs::create_dir_all(&state.recordings_dir);

    Router::new()
        .route("/", get(index_handler)) // Servir l'interface web sur la racine
        .route("/play/{filename}", get(clip_player_handler))
        .route("/ws", get(ws_handler))
        .route("/api/recordings", get(list_recordings_handler)) // API Liste des vidéos
        .route("/recordings/{filename}", get(stream_mjpeg_handler))
        .route(
            "/api/recordings/{filename}",
            delete(delete_recording_handler),
        )
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state)
}
