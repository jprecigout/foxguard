//! Publication d'événements de détection sur un broker MQTT : nom de la
//! caméra, horodatage, et statut de la personne détectée (inconnue, ou
//! connue avec son nom). Fonctionnalité optionnelle, activée via
//! `[mqtt] enabled = true` dans `config.toml` (voir [`crate::config::MqttConfig`]).
//!
//! La détection du CHANGEMENT d'état (ne publier qu'une fois par transition,
//! pas à chaque tentative de reconnaissance) vit dans
//! `crate::capture::tracking`, qui reste volontairement dépourvu d'effets de
//! bord : ce module-ci ne fait que la connexion au broker et la publication
//! proprement dite.

use std::time::Duration;

use rumqttc::{AsyncClient, MqttOptions, QoS};
use serde::Serialize;

use crate::config::MqttConfig;

/// Statut de reconnaissance d'une personne suivie, tel que publié sur MQTT.
/// Calculé et comparé au statut précédent par `crate::capture::tracking`
/// (voir `PersonTrack::last_reported_status`), pour ne déclencher une
/// publication qu'aux changements d'état.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersonStatus {
    /// Personne détectée (bounding-box YOLO) mais non identifiée par la
    /// reconnaissance faciale.
    Unknown,
    /// Personne identifiée, avec son nom (voir `known_faces/`).
    Known(String),
}

/// Message JSON publié sur le topic MQTT configuré à chaque changement
/// d'état : `{"camera": "...", "timestamp": "...", "status": "unknown"}` ou
/// `{"camera": "...", "timestamp": "...", "status": "known", "name": "..."}`.
#[derive(Debug, Serialize)]
struct DetectionEvent<'a> {
    camera: &'a str,
    // Horodatage RFC 3339 (ex : "2026-09-18T15:42:07+02:00"), lisible par
    // n'importe quel abonné sans configuration de fuseau supplémentaire.
    timestamp: String,
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<&'a str>,
}

impl<'a> DetectionEvent<'a> {
    fn from_status(camera: &'a str, timestamp: String, status: &'a PersonStatus) -> Self {
        let (status, name) = match status {
            PersonStatus::Unknown => ("unknown", None),
            PersonStatus::Known(name) => ("known", Some(name.as_str())),
        };

        Self {
            camera,
            timestamp,
            status,
            name,
        }
    }
}

/// Client MQTT connecté en tâche de fond, utilisé pour publier les
/// événements de détection. Construit une seule fois au démarrage par
/// [`crate::capture::models::Models::load`] si `[mqtt] enabled = true`.
///
/// `AsyncClient` est bon marché à cloner (canal interne partagé) : chaque
/// publication clone le client plutôt que de synchroniser l'accès.
#[derive(Clone)]
pub struct MqttPublisher {
    client: AsyncClient,
    topic: String,
}

impl MqttPublisher {
    /// Se connecte au broker configuré et démarre la tâche de fond qui
    /// maintient la connexion (poll de l'event loop, requis par `rumqttc`
    /// pour que la connexion et les publications avancent réellement).
    ///
    /// Ne retourne pas de `Result` : l'établissement de la connexion TCP est
    /// paresseux (il n'a lieu que lors du premier `poll()`), donc les
    /// erreurs réseau ne peuvent se manifester qu'une fois la tâche de fond
    /// démarrée. `rumqttc` reconnecte automatiquement après une erreur ; la
    /// tâche de fond se contente de journaliser et de continuer plutôt que
    /// de faire échouer le démarrage de FoxGuard pour un broker
    /// momentanément injoignable.
    pub fn connect(config: &MqttConfig) -> Self {
        // Identifiant de client unique par processus, pour permettre de
        // lancer plusieurs instances FoxGuard (caméras différentes) sur le
        // même broker sans qu'elles ne s'évincent mutuellement (la plupart
        // des brokers MQTT déconnectent le client précédent en cas de
        // doublon d'identifiant).
        let client_id = format!("foxguard-{}", std::process::id());
        let mut mqtt_options =
            MqttOptions::new(client_id, config.broker_host.clone(), config.broker_port);
        mqtt_options.set_keep_alive(Duration::from_secs(30));

        if !config.username.is_empty() {
            mqtt_options.set_credentials(config.username.clone(), config.password.clone());
        }

        let (client, mut event_loop) = AsyncClient::new(mqtt_options, 10);

        tokio::spawn(async move {
            loop {
                match event_loop.poll().await {
                    Ok(_notification) => {}
                    Err(e) => {
                        eprintln!("❌ Connexion MQTT perdue, nouvelle tentative : {:?}", e);
                        // Évite de boucler à vide (et de spammer les logs)
                        // pendant qu'un broker est injoignable ; `rumqttc`
                        // retentera la connexion au prochain `poll()`.
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        });

        Self {
            client,
            topic: config.topic.clone(),
        }
    }

    /// Publie un événement de détection pour la caméra `camera_name`, en
    /// tâche de fond (`tokio::spawn`) : ne bloque jamais l'appelant, même si
    /// le broker est temporairement injoignable (même principe que
    /// `crate::mail::Mailer::send_alert` pour les e-mails d'alerte).
    pub fn publish_status(&self, camera_name: &str, status: &PersonStatus) {
        let timestamp = chrono::Local::now().to_rfc3339();
        let event = DetectionEvent::from_status(camera_name, timestamp, status);

        let payload = match serde_json::to_vec(&event) {
            Ok(payload) => payload,
            Err(e) => {
                eprintln!("❌ Impossible de sérialiser l'événement MQTT : {:?}", e);
                return;
            }
        };

        let client = self.client.clone();
        let topic = self.topic.clone();

        tokio::spawn(async move {
            if let Err(e) = client
                .publish(topic, QoS::AtLeastOnce, false, payload)
                .await
            {
                eprintln!("❌ Échec de publication MQTT : {:?}", e);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_status_serializes_without_a_name_field() {
        let event = DetectionEvent::from_status(
            "salon",
            "2026-09-18T15:42:07+02:00".to_string(),
            &PersonStatus::Unknown,
        );

        let json = serde_json::to_string(&event).expect("sérialisation JSON");

        assert!(json.contains("\"camera\":\"salon\""));
        assert!(json.contains("\"status\":\"unknown\""));
        assert!(
            !json.contains("\"name\""),
            "le champ name ne doit pas apparaître pour une personne inconnue : {json}"
        );
    }

    #[test]
    fn known_status_serializes_with_the_person_name() {
        let status = PersonStatus::Known("jerome".to_string());
        let event =
            DetectionEvent::from_status("salon", "2026-09-18T15:42:07+02:00".to_string(), &status);

        let json = serde_json::to_string(&event).expect("sérialisation JSON");

        assert!(json.contains("\"status\":\"known\""));
        assert!(json.contains("\"name\":\"jerome\""));
    }

    #[test]
    fn event_payload_is_valid_json_with_the_expected_shape() {
        let status = PersonStatus::Known("alice".to_string());
        let event =
            DetectionEvent::from_status("entree", "2026-09-18T15:42:07+02:00".to_string(), &status);

        let value: serde_json::Value =
            serde_json::from_slice(&serde_json::to_vec(&event).unwrap()).unwrap();

        assert_eq!(value["camera"], "entree");
        assert_eq!(value["status"], "known");
        assert_eq!(value["name"], "alice");
        assert_eq!(value["timestamp"], "2026-09-18T15:42:07+02:00");
    }
}
