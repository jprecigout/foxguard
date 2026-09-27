//! Tests d'intégration du dépôt PostgreSQL.
//!
//! Ils demandent une VRAIE base : ce qu'on vérifie ici — les migrations, le
//! SQL, la conversion des colonnes temporelles — n'a aucun sens contre une
//! imitation. Pour la même raison, ils ne peuvent pas tourner partout.
//!
//! Chaque test s'ignore de lui-même si `FOXGUARD_TEST_DATABASE_URL` n'est pas
//! renseignée, pour qu'un `cargo test` sur un poste sans PostgreSQL reste
//! vert. Pour les exécuter :
//!
//! ```bash
//! docker run -d --rm --name fg-pg -e POSTGRES_PASSWORD=secret \
//!     -e POSTGRES_USER=foxguard -e POSTGRES_DB=foxguard \
//!     -p 55432:5432 postgres:16-alpine
//!
//! FOXGUARD_TEST_DATABASE_URL=postgres://foxguard:secret@localhost:55432/foxguard \
//!     cargo test -p foxguard-manager --test postgres
//! ```
//!
//! Chaque test travaille dans son PROPRE schéma PostgreSQL, créé puis
//! supprimé : ils peuvent donc tourner en parallèle sans se marcher dessus,
//! ce qu'un simple `TRUNCATE` partagé ne permettrait pas.

use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{Duration, Local};
use foxguard_manager::db::EventRepository;
use foxguard_protocol::{DetectionEvent, PersonStatus};

static SCHEMA_COUNTER: AtomicU32 = AtomicU32::new(0);

/// URL de la base de test, ou `None` si elle n'est pas configurée.
fn database_url() -> Option<String> {
    std::env::var("FOXGUARD_TEST_DATABASE_URL")
        .ok()
        .filter(|url| !url.is_empty())
}

/// Ouvre un dépôt isolé dans son propre schéma. Retourne `None` si aucune
/// base de test n'est configurée, ce qui fait passer le test sans rien
/// vérifier.
async fn repository() -> Option<EventRepository> {
    let url = database_url()?;

    let id = SCHEMA_COUNTER.fetch_add(1, Ordering::Relaxed);
    let schema = format!("test_{}_{}", std::process::id(), id);

    // `search_path` fait porter migrations et requêtes sur ce schéma.
    let separator = if url.contains('?') { '&' } else { '?' };
    let scoped = format!("{url}{separator}options=-c%20search_path%3D{schema}");

    // Le schéma doit exister avant que `search_path` ne puisse s'y poser.
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

fn event(camera: &str, name: Option<&str>, minutes_ago: i64) -> DetectionEvent {
    let status = match name {
        Some(name) => PersonStatus::Known {
            name: name.to_string(),
        },
        None => PersonStatus::Unknown,
    };

    DetectionEvent {
        camera: camera.to_string(),
        timestamp: Local::now() - Duration::minutes(minutes_ago),
        status,
    }
}

/// Évite de répéter le court-circuit « pas de base configurée ».
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

#[tokio::test]
async fn migrations_run_on_a_fresh_database() {
    // `connect` applique les migrations : un dépôt neuf doit être vide et
    // interrogeable, sans aucune étape manuelle.
    let repo = repo_or_skip!();

    assert_eq!(repo.count().await.expect("comptage"), 0);
    assert!(repo.recent(10).await.expect("lecture").is_empty());
    assert!(repo.cameras().await.expect("caméras").is_empty());
}

#[tokio::test]
async fn an_event_survives_a_write_and_a_read() {
    let repo = repo_or_skip!();

    repo.record(&event("salon", Some("jerome"), 0))
        .await
        .expect("écriture");

    let events = repo.recent(10).await.expect("lecture");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].camera, "salon");
    assert_eq!(events[0].status.name(), Some("jerome"));
}

#[tokio::test]
async fn an_unknown_person_round_trips_without_a_name() {
    let repo = repo_or_skip!();

    repo.record(&event("entree", None, 0))
        .await
        .expect("écriture");

    let events = repo.recent(10).await.expect("lecture");

    assert!(events[0].status.is_unknown());
    assert_eq!(events[0].status.name(), None);
}

#[tokio::test]
async fn the_timestamp_survives_the_round_trip() {
    // Le point délicat : `TIMESTAMPTZ` et `DateTime<Local>` doivent désigner
    // le même instant après un aller-retour, quel que soit le fuseau du
    // serveur PostgreSQL.
    let repo = repo_or_skip!();

    let original = event("salon", None, 42);
    repo.record(&original).await.expect("écriture");

    let events = repo.recent(1).await.expect("lecture");
    let ecart = (events[0].timestamp - original.timestamp)
        .num_milliseconds()
        .abs();

    assert!(ecart < 1000, "écart de {ecart} ms sur l'horodatage");
}

#[tokio::test]
async fn events_are_returned_most_recent_first() {
    let repo = repo_or_skip!();

    repo.record(&event("ancienne", None, 30)).await.unwrap();
    repo.record(&event("recente", None, 1)).await.unwrap();
    repo.record(&event("intermediaire", None, 10))
        .await
        .unwrap();

    let events = repo.recent(10).await.expect("lecture");
    let ordre: Vec<&str> = events.iter().map(|e| e.camera.as_str()).collect();

    assert_eq!(ordre, vec!["recente", "intermediaire", "ancienne"]);
}

#[tokio::test]
async fn the_limit_caps_the_number_of_returned_events() {
    let repo = repo_or_skip!();

    for i in 0..5 {
        repo.record(&event("salon", None, i)).await.unwrap();
    }

    assert_eq!(repo.recent(2).await.expect("lecture").len(), 2);
    // `count` décrit tout l'historique, pas seulement la page retournée.
    assert_eq!(repo.count().await.expect("comptage"), 5);
}

#[tokio::test]
async fn cameras_are_listed_once_each_and_sorted() {
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0)).await.unwrap();
    repo.record(&event("entree", Some("lou"), 0)).await.unwrap();
    repo.record(&event("salon", Some("mael"), 0)).await.unwrap();

    assert_eq!(
        repo.cameras().await.expect("caméras"),
        vec!["entree", "salon"]
    );
}

#[tokio::test]
async fn retention_deletes_only_what_is_older_than_the_limit() {
    let repo = repo_or_skip!();

    let jour = 24 * 60;
    repo.record(&event("vieux", None, 40 * jour)).await.unwrap();
    repo.record(&event("recent", None, 5 * jour)).await.unwrap();

    let deleted = repo.delete_older_than_days(30).await.expect("purge");

    assert_eq!(deleted, 1);
    let restants = repo.recent(10).await.expect("lecture");
    assert_eq!(restants.len(), 1);
    assert_eq!(restants[0].camera, "recent");
}

#[tokio::test]
async fn a_retention_of_zero_deletes_nothing() {
    // Garde-fou : une valeur mal saisie ne doit jamais vider la table.
    let repo = repo_or_skip!();

    repo.record(&event("tres_vieux", None, 10_000 * 24 * 60))
        .await
        .unwrap();

    assert_eq!(repo.delete_older_than_days(0).await.expect("purge"), 0);
    assert_eq!(repo.count().await.expect("comptage"), 1);
}

#[tokio::test]
async fn connecting_twice_is_idempotent() {
    // Les migrations sont rejouées à chaque démarrage du manager : elles ne
    // doivent ni échouer ni dupliquer quoi que ce soit.
    let Some(url) = database_url() else {
        eprintln!("FOXGUARD_TEST_DATABASE_URL absente : test ignoré");
        return;
    };

    let first = EventRepository::connect(&url, 2)
        .await
        .expect("1er démarrage");
    let before = first.count().await.expect("comptage");

    let second = EventRepository::connect(&url, 2)
        .await
        .expect("2e démarrage");

    assert_eq!(second.count().await.expect("comptage"), before);
}
