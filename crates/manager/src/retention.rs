//! Purge périodique des événements trop anciens.
//!
//! Pendant du nettoyage des enregistrements côté caméra, appliqué ici à la
//! table `detection_events` : sans purge, une installation qui tourne des
//! années finit par ralentir ses propres requêtes d'affichage.

use std::sync::Arc;
use std::time::Duration;

use tracing::{error, info};

use crate::db::EventRepository;

/// Intervalle entre deux passages. La rétention se comptant en jours, un
/// passage quotidien suffit largement.
const CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Démarre la tâche de fond de purge.
///
/// Un premier passage a lieu au démarrage, pour traiter ce qui a expiré
/// pendant un arrêt prolongé. `retention_days == 0` ne démarre aucune tâche.
pub fn spawn_cleanup_task(repository: Arc<EventRepository>, retention_days: u32) {
    if retention_days == 0 {
        info!("🗂️ Purge automatique des événements désactivée (retention_days = 0).");
        return;
    }

    info!("🗂️ Purge automatique des événements : conservation {retention_days} jour(s).");

    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(CLEANUP_INTERVAL);

        loop {
            // Le premier `tick()` est immédiat : le passage a bien lieu au
            // démarrage.
            ticker.tick().await;

            match repository.delete_older_than_days(retention_days).await {
                Ok(0) => {}
                Ok(deleted) => info!("🗑️ Purge : {deleted} événement(s) supprimé(s)."),
                Err(e) => error!("❌ Purge des événements impossible : {e:#}"),
            }
        }
    });
}
