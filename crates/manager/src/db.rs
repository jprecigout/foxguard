//! Persistance PostgreSQL des événements de détection.
//!
//! Remplace le stockage en mémoire de la première version : l'historique
//! survit désormais au redémarrage du manager, et n'est plus borné par la
//! mémoire du serveur.
//!
//! Le schéma vit dans `migrations/`, appliqué au démarrage par
//! [`EventRepository::connect`] (voir `sqlx::migrate!`). Il n'y a donc aucune
//! étape manuelle à la première installation ni après une mise à jour.

use anyhow::{Context, Result};
use foxguard_protocol::{DetectionEvent, PersonStatus};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use tracing::info;

/// Valeur de la colonne `status` pour une personne non identifiée.
const STATUS_UNKNOWN: &str = "unknown";
/// Valeur de la colonne `status` pour une personne identifiée.
const STATUS_KNOWN: &str = "known";

/// Accès à la table des événements de détection.
#[derive(Clone)]
pub struct EventRepository {
    pool: PgPool,
}

impl EventRepository {
    /// Ouvre le pool de connexions et applique les migrations en attente.
    ///
    /// Échoue si la base est injoignable : contrairement au broker MQTT, dont
    /// l'indisponibilité est tolérée (la reconnexion est automatique et
    /// l'historique déjà reçu reste consultable), une base absente rendrait
    /// le manager incapable de faire quoi que ce soit d'utile. Mieux vaut
    /// refuser de démarrer que tourner en perdant silencieusement tout ce qui
    /// arrive.
    pub async fn connect(url: &str, max_connections: u32) -> Result<Self> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections.max(1))
            .connect(url)
            .await
            .context("connexion à PostgreSQL impossible")?;

        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .context("application des migrations impossible")?;

        info!("🗄️ Base de données prête (migrations à jour).");

        Ok(Self { pool })
    }

    /// Enregistre un événement reçu d'une caméra.
    pub async fn record(&self, event: &DetectionEvent) -> Result<()> {
        let (status, person_name) = status_to_columns(&event.status);

        sqlx::query(
            "INSERT INTO detection_events (camera, occurred_at, status, person_name) \
             VALUES ($1, $2, $3, $4)",
        )
        .bind(&event.camera)
        .bind(event.timestamp)
        .bind(status)
        .bind(person_name)
        .execute(&self.pool)
        .await
        .context("insertion de l'événement impossible")?;

        Ok(())
    }

    /// Les `limit` événements les plus RÉCENTS, du plus récent au plus
    /// ancien — l'ordre dans lequel une interface les affiche.
    pub async fn recent(&self, limit: i64) -> Result<Vec<DetectionEvent>> {
        let rows = sqlx::query(
            "SELECT camera, occurred_at, status, person_name FROM detection_events \
             ORDER BY occurred_at DESC, id DESC LIMIT $1",
        )
        .bind(limit.max(1))
        .fetch_all(&self.pool)
        .await
        .context("lecture des événements impossible")?;

        rows.iter().map(row_to_event).collect()
    }

    /// Nombre total d'événements conservés.
    pub async fn count(&self) -> Result<i64> {
        let row = sqlx::query("SELECT count(*) AS n FROM detection_events")
            .fetch_one(&self.pool)
            .await
            .context("comptage des événements impossible")?;

        Ok(row.try_get("n")?)
    }

    /// Noms des caméras ayant déjà émis au moins un événement, triés.
    pub async fn cameras(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT DISTINCT camera FROM detection_events ORDER BY camera")
            .fetch_all(&self.pool)
            .await
            .context("lecture des caméras impossible")?;

        rows.iter()
            .map(|row| row.try_get::<String, _>("camera").map_err(Into::into))
            .collect()
    }

    /// Supprime les événements antérieurs à `days` jours et retourne le
    /// nombre de lignes effacées.
    ///
    /// `days == 0` ne supprime RIEN : c'est le même garde-fou que pour la
    /// purge des enregistrements côté caméra — une valeur mal saisie ne doit
    /// jamais être interprétée comme « tout effacer ».
    pub async fn delete_older_than_days(&self, days: u32) -> Result<u64> {
        if days == 0 {
            return Ok(0);
        }

        let result = sqlx::query(
            "DELETE FROM detection_events \
             WHERE occurred_at < now() - make_interval(days => $1)",
        )
        .bind(days as i32)
        .execute(&self.pool)
        .await
        .context("purge des événements impossible")?;

        Ok(result.rows_affected())
    }
}

/// Traduit un statut de reconnaissance vers les colonnes `status` et
/// `person_name`.
///
/// Extrait pour être testable sans base : c'est ici que vit toute la logique
/// de la correspondance, le reste n'étant que du SQL.
fn status_to_columns(status: &PersonStatus) -> (&'static str, Option<&str>) {
    match status {
        PersonStatus::Unknown => (STATUS_UNKNOWN, None),
        PersonStatus::Known { name } => (STATUS_KNOWN, Some(name.as_str())),
    }
}

/// Reconstruit un statut à partir des colonnes lues.
///
/// Tolérante par principe : un `status` inconnu de cette version, ou un
/// `known` sans nom, est ramené à [`PersonStatus::Unknown`] plutôt que de
/// faire échouer la lecture. Une ligne écrite par un manager plus récent ne
/// doit pas rendre tout l'historique illisible (même règle de compatibilité
/// que le format de fil, voir le crate `foxguard-protocol`).
fn status_from_columns(status: &str, person_name: Option<String>) -> PersonStatus {
    match (status, person_name) {
        (STATUS_KNOWN, Some(name)) if !name.is_empty() => PersonStatus::Known { name },
        _ => PersonStatus::Unknown,
    }
}

/// Convertit une ligne lue en événement du protocole.
fn row_to_event(row: &PgRow) -> Result<DetectionEvent> {
    let status = status_from_columns(row.try_get("status")?, row.try_get("person_name")?);

    Ok(DetectionEvent {
        camera: row.try_get("camera")?,
        timestamp: row.try_get("occurred_at")?,
        status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- status_to_columns ---

    #[test]
    fn an_unknown_person_has_no_name_column() {
        assert_eq!(status_to_columns(&PersonStatus::Unknown), ("unknown", None));
    }

    #[test]
    fn a_known_person_carries_their_name() {
        let status = PersonStatus::Known {
            name: "jerome".to_string(),
        };
        assert_eq!(status_to_columns(&status), ("known", Some("jerome")));
    }

    // --- status_from_columns ---

    #[test]
    fn a_known_row_is_read_back_as_a_known_person() {
        let status = status_from_columns("known", Some("lou".to_string()));
        assert_eq!(status.name(), Some("lou"));
    }

    #[test]
    fn an_unknown_row_is_read_back_as_unknown() {
        assert!(status_from_columns("unknown", None).is_unknown());
    }

    #[test]
    fn both_conversions_round_trip() {
        for original in [
            PersonStatus::Unknown,
            PersonStatus::Known {
                name: "mael".to_string(),
            },
        ] {
            let (status, name) = status_to_columns(&original);
            let restored = status_from_columns(status, name.map(str::to_string));
            assert_eq!(restored, original);
        }
    }

    #[test]
    fn a_status_from_a_newer_version_degrades_to_unknown() {
        // Ligne écrite par un manager plus récent : on préfère la lire comme
        // « inconnu » plutôt que rendre tout l'historique illisible.
        assert!(status_from_columns("visiteur_attendu", None).is_unknown());
    }

    #[test]
    fn a_known_row_without_a_name_degrades_to_unknown() {
        // Incohérence en base : « connu » sans nom n'a pas de sens.
        assert!(status_from_columns("known", None).is_unknown());
        assert!(status_from_columns("known", Some(String::new())).is_unknown());
    }
}
