//! API HTTP de consultation du manager, et service du bundle de l'interface.
//!
//! En LECTURE SEULE : le manager agrège et expose, il ne pilote aucune
//! caméra. Le pilotage reste sur l'interface embarquée de chaque caméra — y
//! compris l'interrupteur de surveillance, dont le manager ne fait que donner
//! l'adresse (voir [`CameraInfo::control_url`]).
//!
//! Il SIGNE en revanche des tickets de visionnage du direct (voir
//! [`StreamTicket`]) : ils ouvrent le flux vidéo d'une caméra, et rien
//! d'autre.

use std::sync::Arc;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::StatusCode,
    http::header,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;
use ts_rs::TS;

use chrono::{DateTime, Local, NaiveDate};
use foxguard_protocol::PersonStatus;
use foxguard_protocol::stream_ticket;

use crate::db::{EventRepository, StoredEvent};

/// Nombre d'événements retournés par défaut par `GET /api/events`.
const DEFAULT_LIMIT: usize = 100;

/// Plafond du paramètre `limit`, pour qu'une requête ne puisse pas demander
/// la sérialisation de tout l'historique d'un coup.
const MAX_LIMIT: usize = 1000;

/// Plafond d'une requête par journée. Plus élevé que [`MAX_LIMIT`] : une
/// journée est une unité voulue par l'utilisateur, la tronquer à 1000
/// événements donnerait une vue fausse. Ce plafond ne protège que du cas
/// dégénéré (caméra bloquée en boucle sur une détection).
const MAX_DAY_EVENTS: usize = 5000;

/// État partagé avec les handlers HTTP.
pub struct AppState {
    pub repository: Arc<EventRepository>,
    /// Secret des tickets de visionnage (`[stream] ticket_secret`), ou
    /// `None` si le direct est désactivé.
    pub stream_ticket_secret: Option<String>,
}

/// Traduit une erreur de base en réponse HTTP.
///
/// Le détail est JOURNALISÉ mais pas renvoyé au client : un message d'erreur
/// PostgreSQL expose volontiers des noms de table, de colonne, voire des
/// fragments de requête.
fn internal_error(context: &str, error: anyhow::Error) -> Response {
    tracing::error!("❌ {context} : {error:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "Erreur interne du serveur",
    )
        .into_response()
}

/// Paramètres de `GET /api/events`.
#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    /// Journée à afficher, au format `AAAA-MM-JJ`, interprétée dans le fuseau
    /// LOCAL du serveur. Quand elle est fournie, `limit` est ignoré : on veut
    /// la journée entière.
    date: Option<String>,
    /// Nombre d'événements les plus récents, tous jours confondus.
    limit: Option<usize>,
}

/// Un événement tel que l'interface le reçoit.
///
/// Distinct de `foxguard_protocol::DetectionEvent`, qui est le format du FIL
/// MQTT : celui-ci est le format de l'API HTTP, et les deux n'ont pas les
/// mêmes besoins. L'interface a besoin d'un identifiant (pour demander la
/// vignette) et d'URL prêtes à l'emploi ; elle n'a aucun usage de la vignette
/// encodée en base64 dans la charge utile, qui alourdirait la réponse d'une
/// journée de plusieurs mégaoctets.
#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../../../ui/src/generated/")]
pub struct EventRecord {
    /// Identifiant en base, stable : il sert de clé d'affichage et d'URL de
    /// vignette.
    #[ts(type = "number")]
    pub id: i64,

    /// Caméra émettrice.
    pub camera: String,

    /// Horodatage de la détection, au format RFC 3339.
    #[ts(type = "string")]
    pub timestamp: DateTime<Local>,

    /// Statut de reconnaissance, aplati comme sur le fil MQTT (`status` et,
    /// le cas échéant, `name`), pour que l'interface le traite de la même
    /// façon dans les deux cas.
    #[serde(flatten)]
    pub status: PersonStatus,

    /// URL de la vignette de la détection, ou `null` s'il n'en existe pas
    /// (caméra qui n'en produit pas, ou événement antérieur à la
    /// fonctionnalité).
    pub thumbnail_url: Option<String>,

    /// URL du clip vidéo sur la caméra, ou `null` si la caméra n'a pas
    /// déclaré son URL publique (voir `[server] public_url` de sa
    /// configuration) ou n'a pas écrit de clip.
    ///
    /// Elle pointe vers la CAMÉRA et non vers le manager : le clip pèse
    /// plusieurs mégaoctets et reste là où il a été écrit. Un lien mort est
    /// donc possible — la caméra peut être hors ligne, ou le clip purgé — ce
    /// que l'interface signale plutôt que de le masquer.
    pub clip_url: Option<String>,
}

/// Une caméra connue, telle que l'interface la reçoit.
#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../../../ui/src/generated/")]
pub struct CameraInfo {
    /// Nom déclaré par la caméra (`[camera] name`).
    pub name: String,

    /// URL de la vue en DIRECT de cette caméra, ou `null` si elle n'a pas
    /// déclaré son URL publique.
    ///
    /// Elle pointe vers la caméra, qui sert elle-même cette page : son flux
    /// est authentifié par un jeton que le manager n'a pas — et qu'il n'a
    /// aucune raison d'avoir (voir `live_handler` côté caméra).
    pub live_url: Option<String>,

    /// URL de la page de PILOTAGE de la surveillance de cette caméra, ou
    /// `null` si elle n'a pas déclaré son URL publique.
    ///
    /// Elle pointe elle aussi vers la caméra, et le manager reste donc SANS
    /// route d'écriture : il indique où se trouve l'interrupteur, il ne le
    /// bascule pas. Confier le pilotage au manager voudrait dire recopier le
    /// jeton d'API de chaque caméra dans cette base de données, ce qui
    /// dégraderait le modèle de sécurité de tout le système pour un bouton
    /// (voir `control_handler` côté caméra).
    pub control_url: Option<String>,

    /// Route du manager qui délivre un ticket de visionnage pour cette
    /// caméra (voir [`StreamTicket`]), ou `null` si le direct n'est pas
    /// disponible : caméra sans URL publique, ou tickets non configurés
    /// (`[stream] ticket_secret`).
    pub stream_url: Option<String>,
}

/// Réponse de `GET /api/cameras/{name}/stream` : de quoi ouvrir le flux
/// vidéo d'une caméra, maintenant.
///
/// L'URL pointe vers le WebSocket de la CAMÉRA, que le navigateur ouvre
/// directement : la vidéo ne transite pas par le manager. Elle porte un
/// ticket signé, valable quelques minutes et pour cette seule caméra, qui
/// n'ouvre le flux qu'en lecture seule (voir
/// `foxguard_protocol::stream_ticket`).
///
/// Elle est donc À USAGE IMMÉDIAT : l'interface en redemande une à chaque
/// (re)connexion, plutôt que de la garder.
#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../../../ui/src/generated/")]
pub struct StreamTicket {
    /// `ws://` ou `wss://`, selon que la caméra est publiée en `http://` ou
    /// en `https://`.
    pub url: String,
}

impl EventRecord {
    /// Construit la vue d'API d'un événement conservé.
    fn from_stored(stored: StoredEvent) -> Self {
        Self {
            thumbnail_url: stored
                .has_thumbnail
                .then(|| format!("/api/events/{}/thumbnail", stored.id)),
            clip_url: stored.event.clip_url(),
            id: stored.id,
            camera: stored.event.camera,
            timestamp: stored.event.timestamp,
            status: stored.event.status,
        }
    }
}

/// Réponse de `GET /api/events`.
#[derive(Debug, Serialize, TS)]
#[ts(export, export_to = "../../../ui/src/generated/")]
pub struct EventsResponse {
    /// Nombre d'événements retournés dans `events`.
    pub count: usize,
    /// Nombre total d'événements conservés en base.
    ///
    /// `ts(type = "number")` corrige la correspondance par défaut de ts-rs,
    /// qui traduit `i64` en `bigint` par prudence sur la précision. Or
    /// `serde_json` sérialise ce champ en nombre JSON ordinaire, et
    /// `JSON.parse` en produit donc un `number` : annoncer `bigint` côté
    /// TypeScript décrirait une valeur qui n'arrive jamais.
    #[ts(type = "number")]
    pub total: i64,
    /// Du plus récent au plus ancien.
    pub events: Vec<EventRecord>,
    /// Vrai si le plafond a été atteint et que la journée comporte donc
    /// d'autres événements non renvoyés. L'interface peut ainsi le signaler
    /// plutôt que d'afficher une vue tronquée en silence.
    pub truncated: bool,
}

/// Événements les plus récents, tous flux confondus.
async fn events_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
) -> Response {
    // Deux modes : une JOURNÉE précise (ce qu'affiche l'interface), ou les N
    // plus récents tous jours confondus (pratique en ligne de commande).
    let (events, cap) = match query.date.as_deref() {
        Some(date) => {
            let Ok(day) = NaiveDate::parse_from_str(date, "%Y-%m-%d") else {
                return (
                    StatusCode::BAD_REQUEST,
                    "Paramètre `date` invalide : format attendu AAAA-MM-JJ",
                )
                    .into_response();
            };

            match state
                .repository
                .events_for_day(day, MAX_DAY_EVENTS as i64)
                .await
            {
                Ok(events) => (events, MAX_DAY_EVENTS),
                Err(e) => return internal_error("Lecture des événements du jour", e),
            }
        }
        None => {
            let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);

            match state.repository.recent(limit as i64).await {
                Ok(events) => (events, limit),
                Err(e) => return internal_error("Lecture des événements", e),
            }
        }
    };

    let total = match state.repository.count().await {
        Ok(total) => total,
        Err(e) => return internal_error("Comptage des événements", e),
    };

    Json(EventsResponse {
        count: events.len(),
        total,
        truncated: events.len() >= cap,
        events: events.into_iter().map(EventRecord::from_stored).collect(),
    })
    .into_response()
}

/// Vignette d'un événement (`GET /api/events/{id}/thumbnail`).
///
/// Servie par le manager et non par la caméra : la vignette voyage dans
/// l'événement MQTT et vit en base (voir
/// `foxguard_protocol::DetectionEvent::thumbnail`). La timeline reste donc
/// lisible des mois plus tard, et depuis un réseau qui n'atteint pas les
/// caméras.
async fn thumbnail_handler(State(state): State<Arc<AppState>>, Path(id): Path<i64>) -> Response {
    let thumbnail = match state.repository.thumbnail(id).await {
        Ok(Some(thumbnail)) => thumbnail,
        Ok(None) => return (StatusCode::NOT_FOUND, "Vignette introuvable").into_response(),
        Err(e) => return internal_error("Lecture de la vignette", e),
    };

    (
        [
            (header::CONTENT_TYPE, "image/jpeg"),
            // `immutable` : la vignette d'un événement passé ne changera
            // jamais. Sans cela, le navigateur redemanderait les mêmes
            // images à chaque défilement de la timeline.
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable"),
        ],
        Body::from(thumbnail),
    )
        .into_response()
}

/// Caméras ayant émis au moins un événement encore présent dans
/// l'historique.
async fn cameras_handler(State(state): State<Arc<AppState>>) -> Response {
    match state.repository.cameras().await {
        Ok(cameras) => Json(
            cameras
                .into_iter()
                .map(|camera| {
                    let base = camera
                        .base_url
                        .as_deref()
                        .map(|base| base.trim_end_matches('/').to_string());

                    let stream_url = (base.is_some() && state.stream_ticket_secret.is_some())
                        .then(|| format!("/api/cameras/{}/stream", encode_path_segment(&camera.name)));

                    CameraInfo {
                        live_url: base.as_deref().map(|base| format!("{base}/live")),
                        control_url: base.as_deref().map(|base| format!("{base}/control")),
                        stream_url,
                        name: camera.name,
                    }
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal_error("Lecture des caméras", e),
    }
}

/// Ticket de visionnage du direct d'une caméra
/// (`GET /api/cameras/{name}/stream`).
///
/// La seule route du manager qui signe quelque chose, et ce qu'elle signe ne
/// permet que de REGARDER : le manager reste sans pouvoir d'écriture sur les
/// caméras.
async fn stream_ticket_handler(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
) -> Response {
    let Some(secret) = state.stream_ticket_secret.as_deref() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Direct désactivé : `[stream] ticket_secret` n'est pas configuré",
        )
            .into_response();
    };

    let cameras = match state.repository.cameras().await {
        Ok(cameras) => cameras,
        Err(e) => return internal_error("Lecture des caméras", e),
    };

    // Seule une caméra CONNUE obtient un ticket, à l'adresse qu'elle a
    // elle-même déclarée : le manager ne signe pas pour un nom quelconque, et
    // n'envoie pas le navigateur vers une adresse choisie par l'appelant.
    let Some(ws_base) = cameras
        .into_iter()
        .find(|camera| camera.name == name)
        .and_then(|camera| camera.base_url)
        .and_then(|base| websocket_base(&base))
    else {
        return (StatusCode::NOT_FOUND, "Caméra inconnue ou sans URL publique").into_response();
    };

    let expires_at = chrono::Utc::now().timestamp() + stream_ticket::TICKET_TTL_SECS;
    let ticket = stream_ticket::issue(secret.as_bytes(), &name, expires_at);

    (
        // Un ticket ne se met pas en cache : un navigateur qui resservirait
        // une réponse vieille de cinq minutes présenterait un ticket expiré.
        [(header::CACHE_CONTROL, "no-store")],
        Json(StreamTicket {
            url: format!("{ws_base}/ws?ticket={ticket}"),
        }),
    )
        .into_response()
}

/// URL de base WebSocket d'une caméra, déduite de son URL publique HTTP.
///
/// `None` pour un schéma qui n'est ni `http` ni `https` : mieux vaut ne pas
/// proposer de direct que d'en proposer un qui ne peut pas marcher.
fn websocket_base(base_url: &str) -> Option<String> {
    let base = base_url.trim_end_matches('/');

    if let Some(rest) = base.strip_prefix("https://") {
        Some(format!("wss://{rest}"))
    } else {
        base.strip_prefix("http://").map(|rest| format!("ws://{rest}"))
    }
}

/// Encode un nom de caméra pour un segment de chemin d'URL.
///
/// Un nom est libre (`[camera] name`) : il peut contenir des espaces, des
/// accents ou une barre oblique, qui casseraient la route s'ils y étaient
/// recopiés tels quels.
fn encode_path_segment(segment: &str) -> String {
    segment
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Sonde de disponibilité, pour `docker compose` ou un superviseur.
async fn health_handler() -> &'static str {
    "ok"
}

/// Construit le routeur.
///
/// `ui_dir` est servi à la racine : c'est là qu'atterrira le bundle produit
/// par `ui/` (voir le README). Tant que l'interface React n'existe pas, ce
/// dossier est simplement absent et `/` retourne 404 — le reste de l'API
/// fonctionne normalement.
pub fn create_router(state: Arc<AppState>, ui_dir: &str) -> Router {
    Router::new()
        .route("/api/health", get(health_handler))
        .route("/api/events", get(events_handler))
        .route("/api/events/{id}/thumbnail", get(thumbnail_handler))
        .route("/api/cameras", get(cameras_handler))
        .route("/api/cameras/{name}/stream", get(stream_ticket_handler))
        .with_state(state)
        .fallback_service(ServeDir::new(ui_dir))
}

// Les tests de ces handlers demandent désormais une vraie base PostgreSQL :
// ils vivent dans `tests/postgres.rs`, ignorés automatiquement quand aucune
// base de test n'est configurée. Ce qui reste testable sans base — la
// correspondance entre statut et colonnes — est couvert dans `db`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_websocket_scheme_follows_the_http_one() {
        assert_eq!(websocket_base("http://cam.local:8080/").as_deref(), Some("ws://cam.local:8080"));
        assert_eq!(websocket_base("https://cam.example").as_deref(), Some("wss://cam.example"));
        assert_eq!(websocket_base("ftp://cam"), None);
    }

    #[test]
    fn a_camera_name_is_safe_in_a_path() {
        assert_eq!(encode_path_segment("jardin"), "jardin");
        assert_eq!(encode_path_segment("allée/nord 2"), "all%C3%A9e%2Fnord%202");
    }
}
