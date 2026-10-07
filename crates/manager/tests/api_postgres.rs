//! Tests d'intégration de l'API HTTP du manager.
//!
//! Ils appellent le VRAI routeur Axum, sur une VRAIE base : c'est le seul
//! niveau où l'on vérifie ce que l'interface reçoit effectivement — la forme
//! du JSON, les URL de média, les en-têtes de la vignette. Les tests de `db`
//! garantissent que les données sont bien rangées ; ceux-ci garantissent
//! qu'elles ressortent sous la forme attendue.
//!
//! Comme `tests/postgres.rs`, chaque test s'ignore de lui-même si
//! `FOXGUARD_TEST_DATABASE_URL` n'est pas renseignée (voir l'en-tête de ce
//! fichier-là pour la commande de lancement).

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use chrono::Local;
use foxguard_manager::api::{self, AppState};
use foxguard_manager::db::EventRepository;
use foxguard_protocol::ticket::{self, Scope};
use foxguard_protocol::{DetectionEvent, PersonStatus};
use http_body_util::BodyExt;
use tower::ServiceExt;

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

/// Quelques octets qui ressemblent à du JPEG.
const FAKE_JPEG: [u8; 9] = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0xFF, 0xD9];

fn database_url() -> Option<String> {
    std::env::var("FOXGUARD_TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
}

/// Ouvre un dépôt isolé dans son propre schéma (voir `tests/postgres.rs`).
async fn repository() -> Option<EventRepository> {
    let url = database_url()?;

    let id = SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed);
    let schema = format!("api_{}_{}", std::process::id(), id);

    let separator = if url.contains('?') { '&' } else { '?' };
    let scoped = format!("{url}{separator}options=-c%20search_path%3D{schema}");

    let admin = sqlx::postgres::PgPoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .expect("connexion d'administration");
    sqlx::query(&format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
        .execute(&admin)
        .await
        .expect("création du schéma de test");

    Some(
        EventRepository::connect(&scoped, 2)
            .await
            .expect("dépôt de test"),
    )
}

macro_rules! repo_or_skip {
    () => {
        match repository().await {
            Some(repo) => repo,
            None => {
                eprintln!("FOXGUARD_TEST_DATABASE_URL absente : test ignoré");
                return;
            }
        }
    };
}

/// Construit le routeur sur un dépôt donné.
///
/// `ui_dir` pointe vers un dossier inexistant : les tests ne portent que sur
/// les routes `/api/*`, et un bundle d'interface n'a rien à faire ici.
fn router(repository: EventRepository) -> axum::Router {
    router_with_secret(repository, Some(TEST_TICKET_SECRET))
}

/// Secret des tickets de direct des tests (32 caractères, le minimum admis).
const TEST_TICKET_SECRET: &str = "0123456789abcdef0123456789abcdef";

/// Comme [`router`], avec ou sans secret de tickets.
fn router_with_secret(repository: EventRepository, secret: Option<&str>) -> axum::Router {
    api::create_router(
        Arc::new(AppState {
            repository: Arc::new(repository),
            stream_ticket_secret: secret.map(str::to_string),
            authenticator: None,
        }),
        "dossier-inexistant-pour-les-tests",
    )
}

/// Exécute une requête GET et retourne (statut, en-têtes, corps).
async fn get(
    repository: EventRepository,
    uri: &str,
) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
    let response = router(repository)
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("requête"),
        )
        .await
        .expect("réponse");

    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("corps de réponse")
        .to_bytes()
        .to_vec();

    (status, headers, body)
}

async fn get_json(repository: EventRepository, uri: &str) -> serde_json::Value {
    let (status, _, body) = get(repository, uri).await;
    assert_eq!(status, StatusCode::OK, "{uri}");

    serde_json::from_slice(&body).expect("JSON valide")
}

fn event(camera: &str, name: Option<&str>) -> DetectionEvent {
    let status = match name {
        Some(name) => PersonStatus::Known {
            name: name.to_string(),
        },
        None => PersonStatus::Unknown,
    };

    DetectionEvent {
        camera: camera.to_string(),
        timestamp: Local::now(),
        status,
        thumbnail: None,
        base_url: None,
        clip: None,
    }
}

fn today() -> String {
    Local::now().format("%Y-%m-%d").to_string()
}

// --- Forme des événements ---

#[tokio::test]
async fn an_event_carries_an_identifier_and_its_flattened_status() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", Some("jerome")))
        .await
        .expect("écriture");

    let body = get_json(repo, "/api/events").await;
    let first = &body["events"][0];

    assert_eq!(first["camera"], "salon");
    // Statut aplati, comme sur le fil MQTT : l'interface traite les deux de
    // la même façon.
    assert_eq!(first["status"], "known");
    assert_eq!(first["name"], "jerome");
    assert!(first["id"].is_number(), "{first}");
}

#[tokio::test]
async fn an_identifier_is_a_json_number_and_not_a_string() {
    // ts-rs traduirait un `i64` en `bigint` par prudence ; l'API le déclare
    // en `number`. Si la sérialisation ne suivait pas, l'interface recevrait
    // un type qu'elle n'attend pas.
    let repo = repo_or_skip!();
    repo.record(&event("salon", None)).await.expect("écriture");

    let body = get_json(repo, "/api/events").await;

    assert!(body["events"][0]["id"].is_i64(), "{}", body["events"][0]);
    // Plus de `total` : il coûtait un `count(*)` de toute la table à chaque
    // rafraîchissement, pour un nombre que l'interface n'affichait pas.
    assert!(body.get("total").is_none(), "{body}");
}

#[tokio::test]
async fn an_event_without_media_exposes_no_urls() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None)).await.expect("écriture");

    let body = get_json(repo, "/api/events").await;
    let first = &body["events"][0];

    assert!(first["thumbnail_url"].is_null(), "{first}");
    assert!(first["clip_url"].is_null(), "{first}");
}

#[tokio::test]
async fn an_event_with_a_thumbnail_exposes_its_url() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None).with_thumbnail(&FAKE_JPEG))
        .await
        .expect("écriture");

    let body = get_json(repo, "/api/events").await;
    let first = &body["events"][0];
    let id = first["id"].as_i64().expect("identifiant");

    assert_eq!(
        first["thumbnail_url"].as_str(),
        Some(format!("/api/events/{id}/thumbnail").as_str())
    );
}

#[tokio::test]
async fn the_events_list_never_carries_thumbnail_bytes() {
    // L'économie décisive : une journée chargée ne doit pas sérialiser des
    // mégaoctets d'images base64 pour afficher une liste.
    let repo = repo_or_skip!();

    for _ in 0..5 {
        repo.record(&event("salon", None).with_thumbnail(&FAKE_JPEG))
            .await
            .expect("écriture");
    }

    let (_, _, body) = get(repo, "/api/events").await;
    let text = String::from_utf8(body).expect("UTF-8");

    assert!(
        !text.contains("\"thumbnail\""),
        "la vignette encodée est passée dans la liste : {text}"
    );
    assert!(text.contains("thumbnail_url"), "{text}");
}

#[tokio::test]
async fn a_clip_url_goes_through_the_manager() {
    // Le lien passe par le manager, qui signera le ticket AU MOMENT DU CLIC :
    // un ticket posé dans la liste aurait expiré bien avant.
    let repo = repo_or_skip!();

    repo.record(
        &event("salon", None)
            .with_base_url("http://192.168.1.42:8080")
            .with_clip("evt_20260918_154207123.mp4"),
    )
    .await
    .expect("écriture");

    let body = get_json(repo, "/api/events").await;
    let id = body["events"][0]["id"].as_i64().expect("identifiant");

    assert_eq!(
        body["events"][0]["clip_url"].as_str(),
        Some(format!("/api/events/{id}/clip").as_str())
    );
}

#[tokio::test]
async fn the_clip_route_redirects_to_the_camera_with_a_clip_ticket() {
    let repo = repo_or_skip!();
    let clip = "evt_20260918_154207123.mp4";

    repo.record(
        &event("salon", None)
            .with_base_url("http://192.168.1.42:8080/")
            .with_clip(clip),
    )
    .await
    .expect("écriture");
    let id = repo.recent(1).await.expect("lecture")[0].id;

    let (status, headers, _) = get(repo, &format!("/api/events/{id}/clip")).await;

    assert_eq!(status, StatusCode::SEE_OTHER);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    let location = headers[header::LOCATION].to_str().expect("en-tête lisible");
    let ticket = location
        .strip_prefix(&format!("http://192.168.1.42:8080/play/{clip}?ticket="))
        .expect("page de lecture de la caméra, avec un ticket");

    // Le ticket n'ouvre que CE clip.
    let now = chrono::Utc::now().timestamp();
    let check = |scope| {
        ticket::verify(
            TEST_TICKET_SECRET.as_bytes(),
            "salon",
            scope,
            ticket,
            now,
            ticket::MAX_TICKET_TTL_SECS,
        )
    };
    assert_eq!(check(Scope::Clip(clip)), Ok(()));
    assert!(check(Scope::Clip("autre.mp4")).is_err());
    assert!(check(Scope::Stream).is_err());
}

#[tokio::test]
async fn no_clip_link_is_offered_without_a_ticket_secret() {
    // La caméra refuserait d'ouvrir le clip : le lien serait mort.
    let repo = repo_or_skip!();
    repo.record(
        &event("salon", None)
            .with_base_url("http://192.168.1.42:8080")
            .with_clip("evt.mp4"),
    )
    .await
    .expect("écriture");

    let response = router_with_secret(repo, None)
        .oneshot(
            Request::builder()
                .uri("/api/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("réponse HTTP");
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert!(body["events"][0]["clip_url"].is_null());
}

#[tokio::test]
async fn a_clip_without_a_public_url_exposes_no_link() {
    // Mieux vaut aucun lien qu'un lien mort : la caméra ne peut pas deviner
    // son adresse vue du navigateur.
    let repo = repo_or_skip!();

    repo.record(&event("salon", None).with_clip("evt.mp4"))
        .await
        .expect("écriture");

    let body = get_json(repo, "/api/events").await;

    assert!(body["events"][0]["clip_url"].is_null());
}

// --- Vignette ---

#[tokio::test]
async fn a_thumbnail_is_served_as_a_jpeg_image() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None).with_thumbnail(&FAKE_JPEG))
        .await
        .expect("écriture");

    let id = repo.recent(1).await.expect("lecture")[0].id;

    let (status, headers, body) = get(repo, &format!("/api/events/{id}/thumbnail")).await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers["content-type"], "image/jpeg");
    assert_eq!(body, FAKE_JPEG);
}

#[tokio::test]
async fn a_thumbnail_is_cached_immutably_by_the_browser() {
    // La vignette d'un événement passé ne changera jamais : sans cet
    // en-tête, le navigateur redemanderait les mêmes images à chaque
    // défilement de la timeline.
    let repo = repo_or_skip!();
    repo.record(&event("salon", None).with_thumbnail(&FAKE_JPEG))
        .await
        .expect("écriture");

    let id = repo.recent(1).await.expect("lecture")[0].id;
    let (_, headers, _) = get(repo, &format!("/api/events/{id}/thumbnail")).await;

    let cache_control = headers["cache-control"].to_str().expect("en-tête");
    assert!(cache_control.contains("immutable"), "{cache_control}");
}

#[tokio::test]
async fn a_missing_thumbnail_is_a_404() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None)).await.expect("écriture");

    let id = repo.recent(1).await.expect("lecture")[0].id;
    let (status, _, _) = get(repo, &format!("/api/events/{id}/thumbnail")).await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn an_unknown_event_has_no_thumbnail() {
    let repo = repo_or_skip!();
    let (status, _, _) = get(repo, "/api/events/999999/thumbnail").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_non_numeric_thumbnail_identifier_is_rejected() {
    // Le chemin est typé `i64` : une valeur fantaisiste doit donner une
    // erreur de requête, pas une erreur serveur.
    let repo = repo_or_skip!();
    let (status, _, _) = get(repo, "/api/events/pas-un-nombre/thumbnail").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// --- Journée et caméras ---

#[tokio::test]
async fn a_day_query_returns_the_events_of_that_day() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None)).await.expect("écriture");

    let body = get_json(repo, &format!("/api/events?date={}", today())).await;

    assert_eq!(body["count"], 1);
    assert_eq!(body["truncated"], false);
    assert_eq!(body["events"][0]["camera"], "salon");
}

#[tokio::test]
async fn a_malformed_date_is_rejected() {
    let repo = repo_or_skip!();
    let (status, _, _) = get(repo, "/api/events?date=hier").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_cameras_route_lists_the_emitters() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None)).await.expect("écriture");
    repo.record(&event("entree", Some("lou")))
        .await
        .expect("écriture");

    let body = get_json(repo, "/api/cameras").await;

    assert_eq!(body[0]["name"], "entree");
    assert_eq!(body[1]["name"], "salon");
}

#[tokio::test]
async fn the_control_route_redirects_with_a_monitoring_ticket() {
    // Le manager n'écrit rien : il autorise l'ouverture de l'interrupteur de
    // la caméra, qui bascule elle-même.
    let repo = repo_or_skip!();
    repo.record(&event("salon", None).with_base_url("http://192.168.1.42:8080"))
        .await
        .expect("écriture");

    let (status, headers, _) = get(repo, "/api/cameras/salon/control").await;
    assert_eq!(status, StatusCode::SEE_OTHER);

    let location = headers[header::LOCATION].to_str().expect("en-tête lisible");
    let ticket = location
        .strip_prefix("http://192.168.1.42:8080/control?ticket=")
        .expect("interrupteur de la caméra, avec un ticket");

    assert_eq!(
        ticket::verify(
            TEST_TICKET_SECRET.as_bytes(),
            "salon",
            Scope::Monitoring,
            ticket,
            chrono::Utc::now().timestamp(),
            ticket::MAX_TICKET_TTL_SECS,
        ),
        Ok(())
    );
}

#[tokio::test]
async fn a_camera_exposes_the_route_of_its_monitoring_switch() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None).with_base_url("http://192.168.1.42:8080"))
        .await
        .expect("écriture");

    let body = get_json(repo, "/api/cameras").await;

    assert_eq!(
        body[0]["control_url"].as_str(),
        Some("/api/cameras/salon/control")
    );
    // Plus d'URL de direct « nue » : la caméra refuserait de la servir.
    assert!(body[0].get("live_url").is_none());
}

#[tokio::test]
async fn a_camera_without_a_public_url_exposes_no_live_link() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None)).await.expect("écriture");

    let body = get_json(repo, "/api/cameras").await;

    assert!(body[0]["stream_url"].is_null());
    assert!(body[0]["control_url"].is_null());
}

// --- Tickets de visionnage du direct ---

#[tokio::test]
async fn a_reachable_camera_offers_a_stream_route() {
    let repo = repo_or_skip!();
    repo.record(&event("allée nord", None).with_base_url("http://192.168.1.42:8080"))
        .await
        .expect("écriture");

    let body = get_json(repo, "/api/cameras").await;

    // Le nom est encodé : il est libre, et la route doit rester valide.
    assert_eq!(
        body[0]["stream_url"].as_str(),
        Some("/api/cameras/all%C3%A9e%20nord/stream")
    );
}

#[tokio::test]
async fn no_stream_route_is_offered_without_a_ticket_secret() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None).with_base_url("http://192.168.1.42:8080"))
        .await
        .expect("écriture");

    let response = router_with_secret(repo, None)
        .oneshot(
            Request::builder()
                .uri("/api/cameras")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("réponse HTTP");
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();

    assert!(body[0]["stream_url"].is_null());
}

#[tokio::test]
async fn the_stream_route_hands_out_a_ticket_the_camera_accepts() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None).with_base_url("http://192.168.1.42:8080/"))
        .await
        .expect("écriture");

    let (status, headers, body) = get(repo, "/api/cameras/salon/stream").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");

    let body: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
    let url = body["url"].as_str().expect("url");
    let ticket = url
        .strip_prefix("ws://192.168.1.42:8080/ws?ticket=")
        .expect("WebSocket de la caméra, avec un ticket");

    // La vérification est exactement celle que fait la caméra.
    assert_eq!(
        ticket::verify(
            TEST_TICKET_SECRET.as_bytes(),
            "salon",
            Scope::Stream,
            ticket,
            chrono::Utc::now().timestamp(),
            ticket::MAX_TICKET_TTL_SECS,
        ),
        Ok(())
    );
}

#[tokio::test]
async fn no_ticket_is_signed_for_an_unknown_camera() {
    let repo = repo_or_skip!();
    let (status, _, _) = get(repo, "/api/cameras/inconnue/stream").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn no_ticket_is_signed_for_a_camera_without_a_public_url() {
    let repo = repo_or_skip!();
    repo.record(&event("salon", None)).await.expect("écriture");

    let (status, _, _) = get(repo, "/api/cameras/salon/stream").await;

    assert_eq!(status, StatusCode::NOT_FOUND);
}

// --- Authentification, en-têtes, compression ---

/// Routeur dont l'authentification est ACTIVE (`admin` / `correct horse`).
fn authenticated_router(repository: EventRepository) -> axum::Router {
    let hash = foxguard_manager::auth::hash_password("correct horse").expect("hachage");

    api::create_router(
        Arc::new(AppState {
            repository: Arc::new(repository),
            stream_ticket_secret: Some(TEST_TICKET_SECRET.to_string()),
            authenticator: Some(Arc::new(
                foxguard_manager::auth::Authenticator::new("admin", &hash).expect("auth"),
            )),
        }),
        "dossier-inexistant-pour-les-tests",
    )
}

fn basic(username: &str, password: &str) -> String {
    use base64::Engine;
    let encoded =
        base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
    format!("Basic {encoded}")
}

#[tokio::test]
async fn every_route_requires_credentials_when_auth_is_enabled() {
    let repo = repo_or_skip!();
    let app = authenticated_router(repo);

    // Le bundle de l'interface aussi (`/`), pas seulement l'API.
    for uri in [
        "/api/events",
        "/api/cameras",
        "/api/cameras/salon/stream",
        "/",
    ] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .expect("réponse HTTP");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert!(
            response.headers().contains_key(header::WWW_AUTHENTICATE),
            "{uri}"
        );
    }
}

#[tokio::test]
async fn the_right_credentials_open_the_api_and_wrong_ones_do_not() {
    let repo = repo_or_skip!();
    let app = authenticated_router(repo);

    let with = |credentials: String| {
        Request::builder()
            .uri("/api/cameras")
            .header(header::AUTHORIZATION, credentials)
            .body(Body::empty())
            .unwrap()
    };

    let ok = app
        .clone()
        .oneshot(with(basic("admin", "correct horse")))
        .await
        .unwrap();
    assert_eq!(ok.status(), StatusCode::OK);

    let wrong = app
        .oneshot(with(basic("admin", "battery staple")))
        .await
        .unwrap();
    assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn the_health_probe_stays_public() {
    // Un superviseur l'interroge sans identifiants.
    let repo = repo_or_skip!();
    let response = authenticated_router(repo)
        .oneshot(
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("réponse HTTP");

    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn responses_carry_security_headers() {
    let repo = repo_or_skip!();
    let (_, headers, _) = get(repo, "/api/health").await;

    assert_eq!(headers[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
    assert_eq!(headers[header::REFERRER_POLICY], "no-referrer");
    assert!(
        headers[header::CONTENT_SECURITY_POLICY]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'")
    );
}

#[tokio::test]
async fn a_day_of_events_is_compressed_for_a_client_that_accepts_it() {
    let repo = repo_or_skip!();
    for _ in 0..50 {
        repo.record(&event("salon", None)).await.expect("écriture");
    }

    let response = router(repo)
        .oneshot(
            Request::builder()
                .uri("/api/events?limit=50")
                .header(header::ACCEPT_ENCODING, "gzip")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("réponse HTTP");

    assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
}

#[tokio::test]
async fn the_health_probe_answers() {
    let repo = repo_or_skip!();
    let (status, _, body) = get(repo, "/api/health").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, b"ok");
}
