//! API HTTP de consultation du manager, et service du bundle de l'interface.
//!
//! En LECTURE SEULE : le manager agrège et expose, il ne pilote aucune
//! caméra. Le pilotage reste sur l'interface embarquée de chaque caméra.

use std::sync::Arc;

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use tower_http::services::ServeDir;

use foxguard_protocol::DetectionEvent;

use crate::store::EventStore;

/// Nombre d'événements retournés par défaut par `GET /api/events`.
const DEFAULT_LIMIT: usize = 100;

/// Plafond du paramètre `limit`, pour qu'une requête ne puisse pas demander
/// la sérialisation de tout l'historique d'un coup.
const MAX_LIMIT: usize = 1000;

/// État partagé avec les handlers HTTP.
pub struct AppState {
    pub store: Arc<EventStore>,
}

/// Paramètres de `GET /api/events?limit=...`.
#[derive(Debug, Deserialize)]
pub struct EventsQuery {
    limit: Option<usize>,
}

/// Réponse de `GET /api/events`.
#[derive(Debug, Serialize)]
pub struct EventsResponse {
    /// Nombre d'événements retournés dans `events`.
    count: usize,
    /// Nombre total d'événements actuellement conservés en mémoire.
    total: usize,
    /// Du plus récent au plus ancien.
    events: Vec<DetectionEvent>,
}

/// Événements les plus récents, tous flux confondus.
async fn events_handler(
    State(state): State<Arc<AppState>>,
    Query(query): Query<EventsQuery>,
) -> Json<EventsResponse> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let events = state.store.recent(limit);

    Json(EventsResponse {
        count: events.len(),
        total: state.store.len(),
        events,
    })
}

/// Caméras ayant émis au moins un événement encore présent dans
/// l'historique.
async fn cameras_handler(State(state): State<Arc<AppState>>) -> Json<Vec<String>> {
    Json(state.store.cameras())
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

#[cfg(test)]
mod tests {
    use super::*;
    use foxguard_protocol::PersonStatus;

    fn state_with(count: usize) -> Arc<AppState> {
        // Capacité volontairement bien supérieure au nombre d'événements
        // injectés : ces tests portent sur la PAGINATION de l'API, pas sur
        // l'éviction de l'historique (couverte dans `store`). Sans cette
        // marge, `total` serait plafonné par le store et le test ne
        // vérifierait plus ce qu'il annonce.
        let store = EventStore::new(10_000);

        for i in 0..count {
            store.record(DetectionEvent::now(
                format!("camera-{i}"),
                PersonStatus::Unknown,
            ));
        }

        Arc::new(AppState {
            store: Arc::new(store),
        })
    }

    #[tokio::test]
    async fn events_returns_an_empty_list_when_nothing_was_received() {
        let Json(response) =
            events_handler(State(state_with(0)), Query(EventsQuery { limit: None })).await;

        assert_eq!(response.count, 0);
        assert_eq!(response.total, 0);
        assert!(response.events.is_empty());
    }

    #[tokio::test]
    async fn events_are_capped_by_the_default_limit() {
        let Json(response) =
            events_handler(State(state_with(150)), Query(EventsQuery { limit: None })).await;

        assert_eq!(response.count, DEFAULT_LIMIT);
        // `total` décrit tout l'historique, pas seulement la page retournée.
        assert_eq!(response.total, 150);
    }

    #[tokio::test]
    async fn an_explicit_limit_is_honoured() {
        let Json(response) =
            events_handler(State(state_with(50)), Query(EventsQuery { limit: Some(5) })).await;

        assert_eq!(response.count, 5);
    }

    #[tokio::test]
    async fn an_oversized_limit_is_clamped() {
        // Une requête ne doit pas pouvoir demander la sérialisation de tout
        // l'historique en une fois.
        let Json(response) = events_handler(
            State(state_with(50)),
            Query(EventsQuery {
                limit: Some(usize::MAX),
            }),
        )
        .await;

        assert_eq!(response.count, 50);
    }

    #[tokio::test]
    async fn a_zero_limit_is_clamped_to_one() {
        let Json(response) =
            events_handler(State(state_with(10)), Query(EventsQuery { limit: Some(0) })).await;

        assert_eq!(response.count, 1);
    }

    #[tokio::test]
    async fn cameras_lists_each_emitter_once() {
        let state = state_with(0);
        state
            .store
            .record(DetectionEvent::now("salon", PersonStatus::Unknown));
        state
            .store
            .record(DetectionEvent::now("salon", PersonStatus::Unknown));
        state
            .store
            .record(DetectionEvent::now("entree", PersonStatus::Unknown));

        let Json(cameras) = cameras_handler(State(state)).await;

        assert_eq!(cameras, vec!["entree", "salon"]);
    }
}
