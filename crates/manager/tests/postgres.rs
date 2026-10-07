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

use chrono::{Duration, Local, NaiveDate};
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
        thumbnail: None,
        base_url: None,
        clip: None,
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
    assert_eq!(events[0].event.camera, "salon");
    assert_eq!(events[0].event.status.name(), Some("jerome"));
}

#[tokio::test]
async fn an_unknown_person_round_trips_without_a_name() {
    let repo = repo_or_skip!();

    repo.record(&event("entree", None, 0))
        .await
        .expect("écriture");

    let events = repo.recent(10).await.expect("lecture");

    assert!(events[0].event.status.is_unknown());
    assert_eq!(events[0].event.status.name(), None);
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
    let ecart = (events[0].event.timestamp - original.timestamp)
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
    let ordre: Vec<&str> = events.iter().map(|e| e.event.camera.as_str()).collect();

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

    let names: Vec<String> = repo
        .cameras()
        .await
        .expect("caméras")
        .into_iter()
        .map(|camera| camera.name)
        .collect();

    assert_eq!(names, vec!["entree", "salon"]);
}

// --- events_for_day ---

#[tokio::test]
async fn a_day_query_returns_only_that_day() {
    let repo = repo_or_skip!();

    let today = Local::now().date_naive();

    repo.record(&event("aujourdhui", None, 60)).await.unwrap();
    // 25 h en arrière : la veille, quelle que soit l'heure d'exécution du
    // test — un décalage de 24 h exactement serait ambigu à minuit.
    repo.record(&event("hier", None, 25 * 60)).await.unwrap();

    let events = repo.events_for_day(today, 100).await.expect("journée");

    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event.camera, "aujourdhui");
}

#[tokio::test]
async fn a_day_query_returns_events_most_recent_first() {
    let repo = repo_or_skip!();

    let today = Local::now().date_naive();
    repo.record(&event("matin", None, 600)).await.unwrap();
    repo.record(&event("midi", None, 300)).await.unwrap();
    repo.record(&event("apres_midi", None, 60)).await.unwrap();

    let events = repo.events_for_day(today, 100).await.expect("journée");
    let ordre: Vec<&str> = events.iter().map(|e| e.event.camera.as_str()).collect();

    assert_eq!(ordre, vec!["apres_midi", "midi", "matin"]);
}

#[tokio::test]
async fn a_day_without_events_returns_nothing() {
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0)).await.unwrap();

    // Une date arbitrairement lointaine dans le passé.
    let empty_day = NaiveDate::from_ymd_opt(2020, 1, 1).unwrap();

    assert!(
        repo.events_for_day(empty_day, 100)
            .await
            .expect("journée")
            .is_empty()
    );
}

#[tokio::test]
async fn a_day_query_is_capped_by_its_limit() {
    // Le plafond protège du cas dégénéré (caméra bloquée en boucle) : il doit
    // s'appliquer réellement.
    let repo = repo_or_skip!();

    let today = Local::now().date_naive();
    for i in 0..10 {
        repo.record(&event("salon", None, i)).await.unwrap();
    }

    assert_eq!(
        repo.events_for_day(today, 4).await.expect("journée").len(),
        4
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
    assert_eq!(restants[0].event.camera, "recent");
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

// --- Média des événements (vignette et clip) ---

/// Quelques octets qui ressemblent à du JPEG, dont un hors ASCII : c'est ce
/// qu'un `BYTEA` doit rendre à l'identique.
const FAKE_JPEG: [u8; 9] = [0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0xFF, 0xD9];

#[tokio::test]
async fn a_thumbnail_survives_the_round_trip_through_the_database() {
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0).with_thumbnail(&FAKE_JPEG))
        .await
        .expect("écriture");

    let events = repo.recent(1).await.expect("lecture");
    assert!(events[0].has_thumbnail);

    let thumbnail = repo
        .thumbnail(events[0].id)
        .await
        .expect("lecture de la vignette")
        .expect("vignette présente");

    assert_eq!(thumbnail, FAKE_JPEG);
}

#[tokio::test]
async fn listing_events_does_not_carry_the_thumbnails() {
    // L'économie qui justifie tout le dispositif : une journée chargée ne
    // doit pas rapatrier des mégaoctets d'images pour afficher une liste.
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0).with_thumbnail(&FAKE_JPEG))
        .await
        .expect("écriture");

    let events = repo.recent(1).await.expect("lecture");

    assert!(events[0].has_thumbnail, "la présence doit être signalée");
    assert_eq!(
        events[0].event.thumbnail, None,
        "le contenu ne doit PAS être rapatrié"
    );
}

#[tokio::test]
async fn an_event_without_a_thumbnail_says_so() {
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0))
        .await
        .expect("écriture");

    let events = repo.recent(1).await.expect("lecture");

    assert!(!events[0].has_thumbnail);
    assert_eq!(repo.thumbnail(events[0].id).await.expect("lecture"), None);
}

#[tokio::test]
async fn an_unreadable_thumbnail_does_not_cost_the_event() {
    // Tolérance par principe : la vignette est un agrément, l'événement est
    // l'information.
    let repo = repo_or_skip!();

    let mut corrupted = event("salon", None, 0);
    corrupted.thumbnail = Some("ceci n'est pas du base64 !!".to_string());

    repo.record(&corrupted).await.expect("écriture");

    let events = repo.recent(1).await.expect("lecture");
    assert_eq!(events.len(), 1, "l'événement doit être enregistré");
    assert!(!events[0].has_thumbnail);
}

#[tokio::test]
async fn an_unknown_identifier_has_no_thumbnail_rather_than_an_error() {
    let repo = repo_or_skip!();

    assert_eq!(repo.thumbnail(999_999).await.expect("lecture"), None);
}

#[tokio::test]
async fn a_clip_reference_survives_the_round_trip() {
    let repo = repo_or_skip!();

    repo.record(
        &event("salon", None, 0)
            .with_base_url("http://192.168.1.42:8080")
            .with_clip("evt_20260918_154207123.mp4"),
    )
    .await
    .expect("écriture");

    let events = repo.recent(1).await.expect("lecture");

    assert_eq!(
        events[0].event.clip_url().as_deref(),
        Some("http://192.168.1.42:8080/play/evt_20260918_154207123.mp4")
    );
}

#[tokio::test]
async fn a_clip_from_a_camera_without_a_public_url_yields_no_link() {
    // La caméra ne peut pas deviner son adresse vue du navigateur : mieux
    // vaut aucun lien qu'un lien mort.
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0).with_clip("evt.mp4"))
        .await
        .expect("écriture");

    let events = repo.recent(1).await.expect("lecture");

    assert_eq!(events[0].event.clip.as_deref(), Some("evt.mp4"));
    assert_eq!(events[0].event.clip_url(), None);
}

#[tokio::test]
async fn a_camera_reports_its_most_recent_base_url() {
    // Une caméra qui change d'adresse doit pouvoir être rejointe à la
    // nouvelle, pas à celle de son premier événement.
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 60).with_base_url("http://ancienne:8080"))
        .await
        .expect("écriture");
    repo.record(&event("salon", None, 1).with_base_url("http://nouvelle:8080"))
        .await
        .expect("écriture");

    let cameras = repo.cameras().await.expect("caméras");

    assert_eq!(cameras.len(), 1);
    assert_eq!(cameras[0].base_url.as_deref(), Some("http://nouvelle:8080"));
}

#[tokio::test]
async fn a_camera_that_declares_no_url_reports_none() {
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0))
        .await
        .expect("écriture");

    let cameras = repo.cameras().await.expect("caméras");
    assert_eq!(cameras[0].base_url, None);
}

#[tokio::test]
async fn the_base_url_does_not_depend_on_a_clip() {
    // Une caméra dont aucune détection n'a produit de clip se regarde quand
    // même : c'est toute la raison d'avoir sorti l'URL de base du clip.
    let repo = repo_or_skip!();

    repo.record(&event("salon", None, 0).with_base_url("http://192.168.1.42:8080"))
        .await
        .expect("écriture");

    let events = repo.recent(1).await.expect("lecture");

    assert_eq!(events[0].event.clip_url(), None);
    assert_eq!(
        events[0].event.base_url.as_deref(),
        Some("http://192.168.1.42:8080")
    );
}

#[tokio::test]
async fn purging_an_event_takes_its_thumbnail_with_it() {
    // C'est ce qui borne le volume occupé par les vignettes : la rétention
    // des événements suffit, il n'y a pas de second ménage à faire.
    let repo = repo_or_skip!();

    let jour = 24 * 60;
    repo.record(&event("vieux", None, 40 * jour).with_thumbnail(&FAKE_JPEG))
        .await
        .expect("écriture");

    let id = repo.recent(1).await.expect("lecture")[0].id;
    assert!(repo.thumbnail(id).await.expect("lecture").is_some());

    assert_eq!(repo.delete_older_than_days(30).await.expect("purge"), 1);
    assert_eq!(repo.thumbnail(id).await.expect("lecture"), None);
}

#[tokio::test]
async fn identifiers_are_distinct_and_stable() {
    // Ils servent d'URL de vignette et de clé d'affichage : deux événements
    // ne peuvent pas les partager.
    let repo = repo_or_skip!();

    for i in 0..3 {
        repo.record(&event("salon", None, i)).await.unwrap();
    }

    let first_read = repo.recent(10).await.expect("lecture");
    let ids: Vec<i64> = first_read.iter().map(|e| e.id).collect();

    assert_eq!(ids.len(), 3);
    assert_eq!(
        ids.iter().collect::<std::collections::HashSet<_>>().len(),
        3
    );

    // Et ils ne bougent pas d'une lecture à l'autre.
    let second_read = repo.recent(10).await.expect("lecture");
    assert_eq!(ids, second_read.iter().map(|e| e.id).collect::<Vec<i64>>());
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
