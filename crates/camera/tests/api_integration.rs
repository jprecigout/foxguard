//! Tests d'intégration du serveur HTTP / WebSocket : vérifient le routeur
//! Axum exposé par `foxguard_camera::api::create_router` de bout en bout (routage,
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

use foxguard_camera::api::create_router;
use foxguard_camera::capture::SharedState;
use foxguard_camera::h264::H264Stream;

/// Construit un [`SharedState`] minimal pour les tests, avec le jeton API
/// donné, aucune surveillance/enregistrement actifs, et un dossier
/// d'enregistrements TEMPORAIRE.
///
/// Ce dernier point est essentiel : Cargo exécute les tests d'intégration
/// avec le répertoire courant positionné sur le paquet (`crates/camera/`), et
/// non sur le workspace. Avec un chemin relatif, `create_router` créait donc
/// un dossier `output_record/` en plein milieu des sources — visible dans
/// `git status` après chaque `cargo test`.
///
/// Le [`tempfile::TempDir`] est retourné avec l'état et supprime le dossier à
/// son `Drop` : l'appelant doit le garder vivant tant qu'il utilise le
/// routeur.
fn test_state_in(token: &str, dir: &std::path::Path) -> Arc<SharedState> {
    Arc::new(SharedState {
        detection_enabled: AtomicBool::new(false),
        recording_enabled: AtomicBool::new(false),
        api_token: token.to_string(),
        h264: Arc::new(H264Stream::new()),
        pending_enrollment: Mutex::new(None),
        recordings_dir: dir.to_string_lossy().to_string(),
    })
}

/// Comme [`test_state_in`], avec son propre dossier temporaire.
fn test_state(token: &str) -> (Arc<SharedState>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("dossier temporaire");
    let state = test_state_in(token, dir.path());
    (state, dir)
}

/// Requête GET simple, sans corps.
fn get(uri: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("requête GET valide")
}

/// Requête DELETE simple, sans corps.
fn delete(uri: &str) -> Request<Body> {
    Request::builder()
        .method("DELETE")
        .uri(uri)
        .body(Body::empty())
        .expect("requête DELETE valide")
}

// --- Service des enregistrements ---

#[tokio::test]
async fn an_mp4_recording_is_served_as_a_video() {
    // Le type de contenu décide de tout côté navigateur : avec
    // `application/octet-stream`, un `<video>` refuse de lire le fichier.
    let dir = tempfile::tempdir().expect("dossier temporaire");
    std::fs::write(dir.path().join("rec_20260101_000000000.mp4"), b"faux mp4").expect("écriture");

    let app = create_router(test_state_in("secret", dir.path()));
    let response = app
        .oneshot(get("/recordings/rec_20260101_000000000.mp4?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "video/mp4");
}

#[tokio::test]
async fn a_recording_announces_that_it_accepts_byte_ranges() {
    // Sans cette annonce, un `<video>` lit du début à la fin sans jamais
    // pouvoir se déplacer — inexploitable sur un enregistrement de plusieurs
    // heures.
    let dir = tempfile::tempdir().expect("dossier temporaire");
    std::fs::write(dir.path().join("rec_20260101_000000000.mp4"), b"0123456789").expect("écriture");

    let app = create_router(test_state_in("secret", dir.path()));
    let response = app
        .oneshot(get("/recordings/rec_20260101_000000000.mp4?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.headers()["accept-ranges"], "bytes");
}

#[tokio::test]
async fn a_byte_range_request_returns_only_that_range() {
    let dir = tempfile::tempdir().expect("dossier temporaire");
    std::fs::write(dir.path().join("rec_20260101_000000000.mp4"), b"0123456789").expect("écriture");

    let app = create_router(test_state_in("secret", dir.path()));
    let request = Request::builder()
        .uri("/recordings/rec_20260101_000000000.mp4?token=secret")
        .header("Range", "bytes=2-5")
        .body(Body::empty())
        .expect("requête");

    let response = app.oneshot(request).await.expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);

    let body = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();

    assert_eq!(&body[..], b"2345");
}

#[tokio::test]
async fn a_legacy_recording_is_still_served() {
    // Les enregistrements écrits avant le passage au MP4 restent sur les
    // caméras déployées : ils doivent continuer d'être servis.
    let dir = tempfile::tempdir().expect("dossier temporaire");
    std::fs::write(dir.path().join("rec_20260101_000000000.mjpeg"), b"ancien").expect("écriture");

    let app = create_router(test_state_in("secret", dir.path()));
    let response = app
        .oneshot(get("/recordings/rec_20260101_000000000.mjpeg?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);
}

// --- Le flux vidéo ---

// Il n'y a plus qu'UNE route de flux, et plus de route de capacités : la
// caméra n'a plus qu'un format à proposer, donc l'interface n'a plus rien à
// choisir.

// Les deux tests qui suivent passent par un VRAI serveur TCP : l'upgrade
// WebSocket exige un état bas niveau que `oneshot` ne fournit pas (voir la
// note plus bas).

#[tokio::test]
async fn the_video_socket_is_rejected_without_a_token() {
    let addr = spawn_test_server("secret").await;
    let status = ws_upgrade_status(addr, "/ws").await;

    assert_eq!(status, StatusCode::UNAUTHORIZED.as_u16());
}

#[tokio::test]
async fn the_video_socket_accepts_the_upgrade_with_the_right_token() {
    let addr = spawn_test_server("secret").await;
    let status = ws_upgrade_status(addr, "/ws?token=secret").await;

    assert_eq!(status, StatusCode::SWITCHING_PROTOCOLS.as_u16());
}

#[tokio::test]
async fn the_capabilities_route_is_gone() {
    // Elle existait pour annoncer lequel des deux flux était disponible. Avec
    // un seul format, la laisser répondre entretiendrait l'idée qu'il y a
    // encore un choix à faire.
    let (state, _dir) = test_state("secret");

    let response = create_router(state)
        .oneshot(get("/api/capabilities"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_player_route_serves_a_standalone_page_for_a_clip() {
    // Cette page est le point d'accès de la timeline du manager aux clips
    // (voir `clip_player_handler`) : elle doit être servie par la CAMÉRA,
    // puisque c'est chez elle que vivent les clips.
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/play/evt_20260918_154207123.mp4"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);

    let body = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();
    let html = String::from_utf8(body.to_vec()).expect("HTML en UTF-8");

    assert!(html.contains("<html"), "la page servie doit être du HTML");
    // La page lit elle-même les données par la route des enregistrements.
    assert!(html.contains("/recordings/"), "{html}");
}

#[tokio::test]
async fn the_player_page_is_served_without_a_token() {
    // Comme les autres routes de LECTURE (voir la note du README) : c'est la
    // page, pas les données, et l'interface du manager l'affiche dans un
    // cadre sans avoir le jeton de la caméra.
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/play/evt.mp4"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn index_route_serves_the_control_html_page() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

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
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/api/recordings?token=secret"))
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
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    // ".." dans le nom de fichier : rejeté avant tout accès disque (voir
    // `is_safe_recording_name`).
    let response = app
        .oneshot(get("/recordings/..evil.mp4?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recording_download_rejects_an_extension_that_is_not_a_recording() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/recordings/rapport.pdf?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recording_download_returns_404_for_a_legit_but_missing_file() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/recordings/rec_ne_existe_pas.mp4?token=secret"))
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
    // Dossier volontairement « fuité » (`keep`) : le serveur vit dans une
    // tâche détachée qui survit au test, donc le supprimer ici le lui
    // retirerait sous les pieds.
    let dir = tempfile::tempdir().expect("dossier temporaire").keep();
    let app = create_router(test_state_in(token, &dir));
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
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/cette-route-n-existe-pas"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// --- Suppression d'un enregistrement (DELETE /api/recordings/{filename}) ---
//
// C'est la seule route DESTRUCTIVE du serveur : ces tests vérifient d'abord
// qu'elle refuse tout ce qui n'est pas explicitement autorisé, avant de
// vérifier qu'elle fonctionne.

#[tokio::test]
async fn recording_delete_is_rejected_without_a_token() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(delete("/api/recordings/rec_20260918_120854.mp4"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn recording_delete_is_rejected_with_the_wrong_token() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(delete(
            "/api/recordings/rec_20260918_120854.mp4?token=mauvais-jeton",
        ))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn recording_delete_rejects_path_traversal_even_with_a_valid_token() {
    // Un jeton valide ne doit PAS permettre de sortir du dossier des
    // enregistrements : la validation du nom est une seconde barrière,
    // indépendante de l'authentification.
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(delete("/api/recordings/..evil.mp4?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recording_delete_rejects_non_recording_extensions() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(delete("/api/recordings/config.toml?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn recording_delete_returns_404_for_a_legit_but_missing_file() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(delete("/api/recordings/rec_ne_existe_pas.mp4?token=secret"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn recording_delete_removes_an_existing_file() {
    // Le fichier est écrit dans le dossier TEMPORAIRE de ce test, celui-là
    // même que le routeur utilise : le test est donc isolé des autres
    // exécutés en parallèle et ne laisse rien dans l'arborescence du dépôt.
    let dir = tempfile::tempdir().expect("dossier temporaire");
    let app = create_router(test_state_in("secret", dir.path()));

    let name = "rec_test_suppression_20260101_000000.mp4";
    let path = dir.path().join(name);
    std::fs::write(&path, b"contenu de test").expect("écriture du fichier de test");
    assert!(path.exists());

    let response = app
        .oneshot(delete(&format!("/api/recordings/{name}?token=secret")))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(
        !path.exists(),
        "le fichier doit avoir été supprimé du disque"
    );
}

// --- Pilotage de la surveillance (GET / POST /api/monitoring) --------------
//
// La seconde route d'ÉCRITURE du serveur, et celle qui porte le plus de
// conséquences : couper la surveillance d'une caméra. Comme pour la
// suppression d'un enregistrement, on vérifie d'abord qu'elle refuse ce
// qu'elle doit refuser.

/// Requête POST portant un corps JSON.
fn post_json(uri: &str, body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("requête POST valide")
}

/// Corps de réponse décodé en JSON.
async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();

    serde_json::from_slice(&bytes).expect("corps JSON")
}

#[tokio::test]
async fn the_monitoring_state_is_readable_without_a_token() {
    // Route de LECTURE : comme la liste des enregistrements, elle n'est pas
    // authentifiée (voir la note du README).
    let (state, _dir) = test_state("secret");
    state
        .detection_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let app = create_router(Arc::clone(&state));

    let response = app
        .oneshot(get("/api/monitoring"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["detection"], serde_json::json!(true));
    assert_eq!(body["recording"], serde_json::json!(false));
}

#[tokio::test]
async fn enabling_monitoring_is_rejected_without_a_token() {
    let (state, _dir) = test_state("secret");
    let app = create_router(Arc::clone(&state));

    let response = app
        .oneshot(post_json("/api/monitoring", r#"{"enabled":true}"#))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !state
            .detection_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        "une requête refusée ne doit rien avoir changé"
    );
}

#[tokio::test]
async fn enabling_monitoring_is_rejected_with_the_wrong_token() {
    let (state, _dir) = test_state("secret");
    let app = create_router(Arc::clone(&state));

    let response = app
        .oneshot(post_json(
            "/api/monitoring?token=pasbon",
            r#"{"enabled":true}"#,
        ))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert!(
        !state
            .detection_enabled
            .load(std::sync::atomic::Ordering::Relaxed),
        "une requête refusée ne doit rien avoir changé"
    );
}

#[tokio::test]
async fn enabling_monitoring_switches_detection_and_recording_together() {
    // « Surveillance » veut dire détecter ET enregistrer, exactement comme la
    // commande WebSocket `set_monitoring` de l'interface de la caméra.
    let (state, _dir) = test_state("secret");
    let app = create_router(Arc::clone(&state));

    let response = app
        .oneshot(post_json(
            "/api/monitoring?token=secret",
            r#"{"enabled":true}"#,
        ))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);

    // L'état est renvoyé, pour que l'appelant n'ait pas à le redemander.
    let body = json_body(response).await;
    assert_eq!(body["detection"], serde_json::json!(true));
    assert_eq!(body["recording"], serde_json::json!(true));

    assert!(
        state
            .detection_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(
        state
            .recording_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[tokio::test]
async fn cutting_monitoring_switches_both_off() {
    let (state, _dir) = test_state("secret");
    state
        .detection_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    state
        .recording_enabled
        .store(true, std::sync::atomic::Ordering::Relaxed);

    let app = create_router(Arc::clone(&state));

    let response = app
        .oneshot(post_json(
            "/api/monitoring?token=secret",
            r#"{"enabled":false}"#,
        ))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !state
            .detection_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    assert!(
        !state
            .recording_enabled
            .load(std::sync::atomic::Ordering::Relaxed)
    );
}

#[tokio::test]
async fn the_control_page_is_served_with_the_camera_token_inlined() {
    // Comme `/live` : la page vient de la caméra et porte son jeton, pour que
    // le manager puisse l'afficher dans un cadre sans jamais le connaître.
    let (state, _dir) = test_state("jeton-de-cette-camera");
    let app = create_router(state);

    let response = app.oneshot(get("/control")).await.expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();
    let html = String::from_utf8(bytes.to_vec()).expect("HTML en UTF-8");

    assert!(html.contains("jeton-de-cette-camera"));
    assert!(
        !html.contains("__FOXGUARD_API_TOKEN__"),
        "le marqueur doit avoir été remplacé"
    );
}

#[tokio::test]
async fn the_control_page_does_not_open_the_video_socket() {
    // Garde-fou : s'abonner au WebSocket est ce qui DÉMARRE l'encodage H.264.
    // Une page réduite à un interrupteur ne doit pas faire tourner l'encodeur
    // logiciel pour une image que personne ne regarde.
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app.oneshot(get("/control")).await.expect("réponse HTTP");

    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();
    let html = String::from_utf8(bytes.to_vec()).expect("HTML en UTF-8");

    // Le commentaire d'en-tête de la page EXPLIQUE pourquoi elle n'en ouvre
    // pas : on cherche donc l'ouverture elle-même, pas le mot.
    assert!(
        !html.contains("new WebSocket"),
        "la page de pilotage ne doit pas ouvrir de WebSocket"
    );
    assert!(
        !html.contains("/ws?token="),
        "la page de pilotage ne doit pas s'abonner au flux vidéo"
    );
}

// --- Authentification des ARCHIVES ----------------------------------------
//
// C'était le trou le plus large du serveur : le WebSocket du direct exigeait
// un jeton, mais la liste et le téléchargement des enregistrements n'en
// demandaient aucun. Refuser à un inconnu la scène en direct pour lui offrir
// la même scène enregistrée ne protégeait rien.

#[tokio::test]
async fn listing_the_recordings_is_rejected_without_a_token() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/api/recordings"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn listing_the_recordings_is_rejected_with_the_wrong_token() {
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/api/recordings?token=pasbon"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn downloading_a_recording_is_rejected_without_a_token() {
    // Et surtout : le fichier ne doit pas filtrer dans le corps de la
    // réponse. C'est bien le refus qui est vérifié, pas seulement le code.
    let dir = tempfile::tempdir().expect("dossier temporaire");
    std::fs::write(
        dir.path().join("rec_20260101_000000000.mp4"),
        b"des images privees",
    )
    .expect("écriture");

    let app = create_router(test_state_in("secret", dir.path()));
    let response = app
        .oneshot(get("/recordings/rec_20260101_000000000.mp4"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    let body = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes();

    assert!(
        !body.windows(5).any(|w| w == b"image"),
        "le contenu de l'enregistrement ne doit pas filtrer"
    );
}

#[tokio::test]
async fn downloading_a_recording_is_rejected_with_the_wrong_token() {
    let dir = tempfile::tempdir().expect("dossier temporaire");
    std::fs::write(dir.path().join("rec_20260101_000000000.mp4"), b"faux mp4").expect("écriture");

    let app = create_router(test_state_in("secret", dir.path()));
    let response = app
        .oneshot(get("/recordings/rec_20260101_000000000.mp4?token=pasbon"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_token_is_checked_before_the_file_name_is_even_looked_at() {
    // Un nom invalide répond 400 quand le jeton est bon. Sans jeton, c'est
    // 401 : sinon la route distinguerait, pour un inconnu, un nom qu'elle
    // accepte d'un nom qu'elle rejette.
    let (state, _dir) = test_state("secret");
    let app = create_router(state);

    let response = app
        .oneshot(get("/recordings/rapport.pdf"))
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_pages_that_read_archives_carry_the_token_themselves() {
    // L'interface complète et le lecteur de clip demandent tous deux des
    // octets d'archive : ils doivent donc porter le jeton, comme `/live`.
    // Il était ÉCRIT EN DUR dans `controller.html` — changer `api_token`
    // cassait l'interface en silence.
    let (state, _dir) = test_state("jeton-de-cette-camera");
    let app = create_router(Arc::clone(&state));

    for route in ["/", "/play/evt.mp4"] {
        let response = app.clone().oneshot(get(route)).await.expect("réponse HTTP");

        assert_eq!(response.status(), StatusCode::OK, "route {route}");

        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("corps de réponse")
            .to_bytes();
        let html = String::from_utf8(bytes.to_vec()).expect("HTML en UTF-8");

        assert!(html.contains("jeton-de-cette-camera"), "route {route}");
        assert!(
            !html.contains("__FOXGUARD_API_TOKEN__"),
            "le marqueur doit avoir été remplacé sur {route}"
        );
        assert!(
            !html.contains("secret123"),
            "aucun jeton en dur ne doit subsister dans {route}"
        );
    }
}
