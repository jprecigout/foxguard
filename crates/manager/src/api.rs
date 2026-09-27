//! API HTTP de consultation du manager, et service du bundle de l'interface.
//!
//! En LECTURE SEULE : le manager agrège et expose, il ne pilote aucune
//! caméra. Le pilotage reste sur l'interface embarquée de chaque caméra.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;

use chrono::NaiveDate;
use foxguard_protocol::DetectionEvent;

use crate::db::EventRepository;

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

/// Réponse de `GET /api/events`.
#[derive(Debug, Serialize)]
pub struct EventsResponse {
    /// Nombre d'événements retournés dans `events`.
    count: usize,
    /// Nombre total d'événements conservés en base.
    total: i64,
    /// Du plus récent au plus ancien.
    events: Vec<DetectionEvent>,
    /// Vrai si le plafond a été atteint et que la journée comporte donc
    /// d'autres événements non renvoyés. L'interface peut ainsi le signaler
    /// plutôt que d'afficher une vue tronquée en silence.
    truncated: bool,
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
        events,
    })
    .into_response()
}

/// Caméras ayant émis au moins un événement encore présent dans
/// l'historique.
async fn cameras_handler(State(state): State<Arc<AppState>>) -> Response {
    match state.repository.cameras().await {
        Ok(cameras) => Json(cameras).into_response(),
        Err(e) => internal_error("Lecture des caméras", e),
    }
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
        .route("/api/cameras", get(cameras_handler))
        .with_state(state)
        .fallback_service(ServeDir::new(ui_dir))
}

// Les tests de ces handlers demandent désormais une vraie base PostgreSQL :
// ils vivent dans `tests/postgres.rs`, ignorés automatiquement quand aucune
// base de test n'est configurée. Ce qui reste testable sans base — la
// correspondance entre statut et colonnes — est couvert dans `db`.
