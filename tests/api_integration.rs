//! Tests d'intégration du serveur HTTP / WebSocket : vérifient le routeur
//! Axum exposé par `foxguard::api::create_router` de bout en bout (routage,
//! extraction des paramètres, en-têtes, code de statut, corps de réponse).
//! Les routes HTTP classiques sont testées sans ouvrir de socket réseau réel
//! (`tower::ServiceExt::oneshot`) ; l'upgrade WebSocket, qui a besoin d'une
//! vraie connexion TCP, est testée via un serveur `axum::serve` réel sur un
//! port éphémère (voir la note plus bas).

use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use foxguard::api::create_router;
use foxguard::capture::SharedState;

/// Construit un [`SharedState`] minimal pour les tests, avec le jeton API
/// donné et aucune surveillance/enregistrement actifs.
fn test_state(token: &str) -> Arc<SharedState> {
    let (tx, _rx) = tokio::sync::broadcast::channel(16);
    Arc::new(SharedState {
        detection_enabled: AtomicBool::new(false),
        recording_enabled: AtomicBool::new(false),
        api_token: token.to_string(),
        tx,
        pending_enrollment: Mutex::new(None),
    })
}

/// Requête GET simple, sans corps.
fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("requête GET valide")
}

#[tokio::test]
async fn index_route_serves_the_control_html_page() {
    let app = create_router(test_state("secret"));

    let response = app.oneshot(get("/")).await.expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();
    let html = String::from_utf8(body.to_vec()).expect("HTML en UTF-8");
    assert!(html.contains("<html"), "la page servie doit être du HTML");
}

#[tokio::test]
async fn recordings_list_route_returns_a_json_array() {
    let app = create_router(test_state("secret"));

    let response = app
        .oneshot(get("/api/recordings"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);

    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.contains("application/json"));

    let body = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();
    let parsed: serde_json::Value =
        serde_json::from_slice(&body).expect("JSON valide renvoyé par /api/recordings");
    assert!(parsed.is_array());
}

#[tokio::test]
async fn recording_download_rejects_filenames_containing_path_traversal() {
    let app = create_router(test_state("secret"));

    // ".." dans le nom de fichier : rejeté avant tout accès disque (voir
    // `stream_mjpeg_handler`).
    let response = app
        .oneshot(get("/recordings/..evil.mjpeg"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recording_download_rejects_non_mjpeg_extensions() {
    let app = create_router(test_state("secret"));

    let response = app
        .oneshot(get("/recordings/rapport.pdf"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recording_download_returns_404_for_a_legit_but_missing_file() {
    let app = create_router(test_state("secret"));

    let response = app
        .oneshot(get("/recordings/rec_ne_existe_pas.mjpeg"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// Les 3 tests suivants portent sur l'authentification de l'upgrade
// WebSocket (`ws_handler`, voir `crate::api`). `tower::ServiceExt::oneshot`
// ne convient pas ici : l'extracteur `WebSocketUpgrade` d'Axum a besoin de
// l'état d'upgrade bas niveau (`hyper::upgrade::OnUpgrade`) que seule une
// vraie connexion TCP acceptée par `axum::serve` fournit — avec `oneshot`,
// la requête échoue systématiquement à l'extraction (426 Upgrade Required)
// avant même d'atteindre notre vérification de jeton. On démarre donc un
// vrai serveur sur un port éphémère et on lui parle en TCP brut, en ne
// lisant que la ligne de statut HTTP de la réponse (sans dérouler le
// protocole de frames WebSocket, hors sujet ici).

/// Démarre `create_router` sur un vrai `TcpListener` (port éphémère) via
/// `axum::serve`, exactement comme `src/main.rs`, et retourne son adresse.
async fn spawn_test_server(token: &str) -> SocketAddr {
    let app = create_router(test_state(token));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("liaison sur un port éphémère");
    let addr = listener.local_addr().expect("adresse locale");

    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("le serveur de test ne doit pas s'arrêter en erreur");
    });

    addr
}

/// Envoie une requête HTTP/1.1 brute d'upgrade WebSocket vers `addr` et
/// retourne le code de statut de la ligne de réponse.
async fn ws_upgrade_status(addr: SocketAddr, path_and_query: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connexion TCP au serveur de test");

    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: 127.0.0.1\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         \r\n",
        path = path_and_query
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("écriture de la requête brute");

    let mut buf = [0u8; 512];
    let n = stream
        .read(&mut buf)
        .await
        .expect("lecture de la réponse brute");
    let response = String::from_utf8_lossy(&buf[..n]);

    let status_line = response.lines().next().expect("au moins une ligne");
    status_line
        .split_whitespace()
        .nth(1)
        .expect("code de statut présent")
        .parse()
        .expect("code de statut numérique")
}

#[tokio::test]
async fn websocket_upgrade_is_rejected_without_a_token() {
    let addr = spawn_test_server("secret-correct").await;
    let status = ws_upgrade_status(addr, "/ws").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED.as_u16());
}

#[tokio::test]
async fn websocket_upgrade_is_rejected_with_the_wrong_token() {
    let addr = spawn_test_server("secret-correct").await;
    let status = ws_upgrade_status(addr, "/ws?token=mauvais-jeton").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED.as_u16());
}

#[tokio::test]
async fn websocket_upgrade_succeeds_with_the_correct_token() {
    let addr = spawn_test_server("secret-correct").await;
    let status = ws_upgrade_status(addr, "/ws?token=secret-correct").await;
    // 101 Switching Protocols : le jeton est valide, l'upgrade est acceptée.
    assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS.as_u16());
}

#[tokio::test]
async fn unknown_route_returns_404() {
    let app = create_router(test_state("secret"));

    let response = app
        .oneshot(get("/cette-route-n-existe-pas"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}
