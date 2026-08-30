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
use std::time::Duration;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::sleep;
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
    #[serde(rename = "set_surveillance")]
    SetSurveillance { enabled: bool },
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

// Handler qui transmet un fichier .mjpeg avec une temporisation à 30 FPS
async fn stream_mjpeg_handler(Path(filename): Path<String>) -> Result<Response, StatusCode> {
    if filename.contains("..") || !filename.ends_with(".mjpeg") {
        return Err(StatusCode::BAD_REQUEST);
    }

    let filepath = format!("output_record/{}", filename);
    let mut file = File::open(&filepath)
        .await
        .map_err(|_| StatusCode::NOT_FOUND)?;

    // Création d'un flux (stream) régulé frame par frame
    let stream = async_stream::stream! {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];

        loop {
            match file.read(&mut chunk).await {
                Ok(0) => break, // Fin du fichier
                Ok(n) => {
                    buffer.extend_from_slice(&chunk[..n]);

                    // Recherche des marqueurs de début (0xFF 0xD8) et de fin (0xFF 0xD9) de JPEG
                    while let Some(start) = find_jpeg_start(&buffer) {
                        if let Some(end) = find_jpeg_end(&buffer[start..]) {
                            let frame_end = start + end + 2;
                            let frame_data = buffer[start..frame_end].to_vec();

                            // Envoi de l'image avec l'entête multipart MJPEG
                            let header = format!(
                                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                                frame_data.len()
                            );

                            yield Ok::<_, std::io::Error>(bytes::Bytes::from(header));
                            yield Ok::<_, std::io::Error>(bytes::Bytes::from(frame_data));
                            yield Ok::<_, std::io::Error>(bytes::Bytes::from("\r\n"));

                            // Réduction du buffer
                            buffer.drain(..frame_end);

                            // ⏱️ Pause de 33ms (~30 FPS) pour cadence réelle
                            sleep(Duration::from_millis(33)).await;
                        } else {
                            break; // Frame incomplète, attendre plus de données
                        }
                    }
                }
                Err(_) => break,
            }
        }
    };

    let body = Body::from_stream(stream);

    let response = Response::builder()
        .header("Content-Type", "multipart/x-mixed-replace; boundary=frame")
        .header("Cache-Control", "no-cache, no-store, must-revalidate")
        .body(body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(response)
}

// Fonctions helpers pour localiser les marqueurs JPEG dans le buffer
fn find_jpeg_start(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == [0xFF, 0xD8])
}

fn find_jpeg_end(buf: &[u8]) -> Option<usize> {
    buf.windows(2).position(|w| w == [0xFF, 0xD9])
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
            if let Message::Text(text) = msg {
                if let Ok(cmd) = serde_json::from_str::<ClientCommand>(&text) {
                    match cmd {
                        ClientCommand::SetSurveillance { enabled } => {
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
