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
use tokio::fs::File;
use tokio::sync::broadcast::error::RecvError;
use tokio_util::io::ReaderStream;
use tower_http::services::ServeDir;

use crate::camera::SharedState;

// Compteur global pour attribuer un ID unique séquentiel à chaque client
static NEXT_CLIENT_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Serialize)]
pub struct VideoFile {
    pub name: String,
    pub size_mb: f64,
}

// Structure pour extraire le paramètre ?token=
#[derive(Deserialize)]
pub struct AuthQuery {
    pub token: Option<String>,
}

/// Commandes JSON reçues par WebSocket
#[derive(Deserialize)]
#[serde(tag = "command")]
pub enum ClientCommand {
    #[serde(rename = "set_detection")]
    SetDetection { enabled: bool },
    #[serde(rename = "set_recording")]
    SetRecording { enabled: bool },
}

/// Handler pour lister les enregistrements disponibles dans output_record
async fn list_recordings_handler() -> Json<Vec<VideoFile>> {
    let mut files = Vec::new();

    if let Ok(entries) = std::fs::read_dir("output_record") {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                if let (Some(name), Ok(metadata)) = (path.file_name(), path.metadata()) {
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
    }

    // Tri par nom décroissant (du plus récent au plus ancien)
    files.sort_by(|a, b| b.name.cmp(&a.name));
    Json(files)
}

// Handler qui transmet un fichier .mjpeg sous forme de vrai flux vidéo
async fn stream_mjpeg_handler(Path(filename): Path<String>) -> Result<Response, StatusCode> {
    // Sécurité basique sur le nom de fichier
    if filename.contains("..") || !filename.ends_with(".mjpeg") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let filepath = format!("output_record/{}", filename);
    let file = File::open(&filepath)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Convertit le fichier en Stream binaire
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    let response = Response::builder()
        .header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
        .header("Cache-Control", "no-cache, no-store, must-revalidate")
        .body(body)
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

/// Handler de mise à niveau vers WebSocket
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
                    // Le client a pris du retard, on ignore simplement les images sautées et on continue la boucle !
                    // eprintln!(
                    //     "⚠️ [WS Client #{}] Réseau lent, {} images sautées",
                    //     client_id, _skipped
                    // );
                    continue;
                }
                Err(RecvError::Closed) => {
                    // Le canal principal s'est fermé (ex: arrêt de la caméra)
                    break;
                }
            }
        }
    });

    // Tâche 2: Commandes entraintes par texte/JSON
    let state_cmd = Arc::clone(&state);
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            if let Message::Text(text) = msg {
                if let Ok(cmd) = serde_json::from_str::<ClientCommand>(&text) {
                    match cmd {
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
