//! Réception des événements de détection publiés par les caméras sur MQTT.
//!
//! Le manager est un simple ABONNÉ : il ne publie rien et ne pilote aucune
//! caméra. Chaque message reçu est décodé avec
//! [`foxguard_protocol::DetectionEvent`] — le même type que celui utilisé par
//! la caméra pour l'écrire — puis rangé dans l'historique.

use std::sync::Arc;
use std::time::Duration;

use foxguard_protocol::DetectionEvent;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tracing::{debug, error, info, warn};

use crate::config::MqttConfig;
use crate::store::EventStore;

/// Démarre la tâche de fond qui écoute le broker et alimente `store`.
///
/// Ne retourne pas de `Result` : la connexion est paresseuse et `rumqttc`
/// reconnecte automatiquement. Un broker momentanément injoignable ne doit
/// pas empêcher le manager de démarrer — son API HTTP reste utile pour
/// consulter l'historique déjà reçu.
pub fn spawn(config: MqttConfig, store: Arc<EventStore>) {
    let client_id = format!("foxguard-manager-{}", std::process::id());
    let mut options = MqttOptions::new(client_id, config.broker_host.clone(), config.broker_port);
    options.set_keep_alive(Duration::from_secs(30));

    if !config.username.is_empty() {
        options.set_credentials(config.username.clone(), config.password.clone());
    }

    info!(
        "📡 Abonnement MQTT : {}:{} (topic \"{}\")",
        config.broker_host, config.broker_port, config.topic
    );

    let (client, mut event_loop) = AsyncClient::new(options, 10);

    tokio::spawn(async move {
        // L'abonnement est (re)demandé à chaque connexion réussie : après une
        // coupure, `rumqttc` rétablit la session TCP mais le broker peut
        // avoir oublié nos abonnements (session non persistante). Sans ce
        // ré-abonnement, le manager resterait connecté et parfaitement muet.
        loop {
            match event_loop.poll().await {
                Ok(Event::Incoming(Packet::ConnAck(_))) => {
                    info!("✅ Connecté au broker MQTT.");

                    if let Err(e) = client.subscribe(&config.topic, QoS::AtLeastOnce).await {
                        error!("❌ Abonnement au topic \"{}\" échoué : {e}", config.topic);
                    }
                }

                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    handle_payload(&publish.payload, &store);
                }

                Ok(_) => {}

                Err(e) => {
                    error!("❌ Connexion MQTT perdue, nouvelle tentative : {e}");
                    // Évite de boucler à vide (et de saturer les journaux)
                    // pendant qu'un broker est injoignable.
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
            }
        }
    });
}

/// Décode une charge utile MQTT et l'enregistre dans l'historique.
///
/// Un message illisible est SIGNALÉ puis ignoré : le broker peut porter
/// d'autres producteurs, et une caméra plus récente peut émettre un format
/// que ce manager ne comprend pas encore. Dans les deux cas, abandonner la
/// boucle de réception serait une réaction disproportionnée.
///
/// Extrait de [`spawn`] pour être testable sans broker (voir les tests en fin
/// de fichier).
fn handle_payload(payload: &[u8], store: &EventStore) {
    match serde_json::from_slice::<DetectionEvent>(payload) {
        Ok(event) => {
            match event.status.name() {
                Some(name) => debug!("👤 {} : {} reconnu(e)", event.camera, name),
                None => debug!("❓ {} : personne inconnue", event.camera),
            }

            store.record(event);
        }
        Err(e) => {
            warn!(
                "⚠️ Message MQTT ignoré (format non reconnu) : {e} — {}",
                String::from_utf8_lossy(payload)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_payload_is_recorded() {
        let store = EventStore::new(10);
        let raw = br#"{"camera":"salon","timestamp":"2026-09-18T15:42:07+02:00","status":"known","name":"jerome"}"#;

        handle_payload(raw, &store);

        let recent = store.recent(10);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].camera, "salon");
        assert_eq!(recent[0].status.name(), Some("jerome"));
    }

    #[test]
    fn an_unknown_person_payload_is_recorded() {
        let store = EventStore::new(10);
        let raw =
            br#"{"camera":"entree","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown"}"#;

        handle_payload(raw, &store);

        assert!(store.recent(1)[0].status.is_unknown());
    }

    #[test]
    fn a_malformed_payload_is_ignored_without_panicking() {
        let store = EventStore::new(10);

        handle_payload(b"ceci n'est pas du JSON", &store);

        assert!(
            store.is_empty(),
            "un message illisible ne doit rien enregistrer"
        );
    }

    #[test]
    fn a_json_payload_with_the_wrong_shape_is_ignored() {
        // JSON valide, mais qui ne décrit pas un événement de détection : un
        // autre producteur publie peut-être sur le même topic.
        let store = EventStore::new(10);

        handle_payload(br#"{"temperature": 21.5}"#, &store);

        assert!(store.is_empty());
    }

    #[test]
    fn ingestion_continues_after_a_malformed_message() {
        // Le point important : un message corrompu ne doit pas empêcher le
        // suivant d'être traité.
        let store = EventStore::new(10);

        handle_payload(b"{{{", &store);
        handle_payload(
            br#"{"camera":"salon","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown"}"#,
            &store,
        );

        assert_eq!(store.len(), 1);
    }
}
