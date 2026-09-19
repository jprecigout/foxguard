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
    routing::get,
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

use crate::capture::SharedState;

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
async fn list_recordings_handler() -> Json<Vec<VideoFile>> {
    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir("output_record") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file()
                && let (Some(name), Ok(metadata)) = (path.file_name(), path.metadata())
            {
                let name_str = name.to_string_lossy().to_string();
                if name_str.ends_with(".mjpeg") || name_str.ends_with(".mp4") {
                    let size_mb = (metadata.len() as f64) / (1024.0 * 1024.0);
                    files.push(VideoFile {
                        name: name_str,
                        size_mb: (size_mb * 100.0).round() / 100.0,
                    });
                }
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
async fn stream_mjpeg_handler(Path(filename): Path<String>) -> Result<Response, StatusCode> {
    if filename.contains("..") || !filename.ends_with(".mjpeg") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let filepath = format!("output_record/{}", filename);
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

/// Handler pour servir le fichier HTML de contrôle
async fn index_handler() -> Html<&'static str> {
    Html(include_str!("../static/controller.html"))
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
        println!("⚠️ Tentative de connexion WebSocket rejetée (Token invalide).");
        return (StatusCode::UNAUTHORIZED, "Accès refusé").into_response();
    }

    // Génération d'un ID unique pour identifier ce client
    let client_id = NEXT_CLIENT_ID.fetch_add(1, Ordering::Relaxed);

    println!(
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
                        println!(
                            "🛡️ [WS Client #{}] A modifié la surveillance générale : {}",
                            client_id, enabled
                        );
                    }
                    ClientCommand::SetDetection { enabled } => {
                        state_cmd
                            .detection_enabled
                            .store(enabled, Ordering::Relaxed);
                        println!(
                            "🔍 [WS Client #{}] A modifié la détection : {}",
                            client_id, enabled
                        );
                    }
                    ClientCommand::SetRecording { enabled } => {
                        state_cmd
                            .recording_enabled
                            .store(enabled, Ordering::Relaxed);
                        println!(
                            "💾 [WS Client #{}] A modifié l'enregistrement : {}",
                            client_id, enabled
                        );
                    }
                    ClientCommand::CaptureReference { name } => {
                        if let Ok(mut pending) = state_cmd.pending_enrollment.lock() {
                            *pending = Some(name.clone());
                        }
                        println!(
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
    println!("❌ [WS] Client déconnecté #{} (IP: {})", client_id, addr);
}

/// Helper pour instancier le routeur Axum
pub fn create_router(state: Arc<SharedState>) -> Router {
    // S'assurer que le dossier des enregistrements existe
    let _ = std::fs::create_dir_all("output_record");

    Router::new()
        .route("/", get(index_handler)) // Servir l'interface web sur la racine
        .route("/ws", get(ws_handler))
        .route("/api/recordings", get(list_recordings_handler)) // API Liste des vidéos
        .route("/recordings/{filename}", get(stream_mjpeg_handler))
        .nest_service("/static", ServeDir::new("static"))
        .with_state(state)
}
