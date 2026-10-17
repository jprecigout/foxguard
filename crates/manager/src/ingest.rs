//! Réception des événements de détection publiés par les caméras sur MQTT.
//!
//! Le manager est un simple ABONNÉ : il ne publie rien et ne pilote aucune
//! caméra. Chaque message reçu est décodé avec
//! [`foxguard_protocol::DetectionEvent`] — le même type que celui utilisé par
//! la caméra pour l'écrire — puis rangé dans l'historique.
//!
//! Il écoute aussi les photos de référence que les caméras se partagent (voir
//! `foxguard_protocol::faces`), pour en tenir la liste à l'interface. Il ne
//! les accepte que SIGNÉES, exactement comme une caméra : montrer comme
//! enregistrée une photo que toutes les caméras refusent serait mentir.

use std::sync::Arc;
use std::time::Duration;

use foxguard_protocol::DetectionEvent;
use foxguard_protocol::faces::{self, FaceMessage};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tracing::{debug, error, info, warn};

use crate::config::MqttConfig;
use crate::db::{EventRepository, NewFace};

/// Démarre la tâche de fond qui écoute le broker et alimente `store`.
///
/// `faces_secret` : secret qui signe les photos de référence (`[stream]
/// ticket_secret`), ou `None` pour ne pas les écouter — sans lui, rien ne
/// permet de distinguer une photo d'une caméra d'une photo forgée.
///
/// Ne retourne pas de `Result` : la connexion est paresseuse et `rumqttc`
/// reconnecte automatiquement. Un broker momentanément injoignable ne doit
/// pas empêcher le manager de démarrer — son API HTTP reste utile pour
/// consulter l'historique déjà reçu.
pub fn spawn(config: MqttConfig, repository: Arc<EventRepository>, faces_secret: Option<String>) {
    let client_id = format!("foxguard-manager-{}", std::process::id());
    let mut options = MqttOptions::new(client_id, config.broker_host.clone(), config.broker_port);
    options.set_keep_alive(Duration::from_secs(30));

    // Le client limite par défaut les paquets entrants à 10 Kio : une photo
    // de référence n'y tient pas, et un paquet hors limite COUPE la
    // connexion — que le broker relivrerait aussitôt (message retenu), en
    // boucle (voir `faces::MAX_MESSAGE_BYTES`).
    options.set_max_packet_size(faces::MAX_MESSAGE_BYTES, faces::MAX_MESSAGE_BYTES);

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

                    if faces_secret.is_some() {
                        let pattern = faces::subscription(&config.faces_topic);
                        if let Err(e) = client.subscribe(&pattern, QoS::AtLeastOnce).await {
                            error!("❌ Abonnement au topic \"{pattern}\" échoué : {e}");
                        }
                    }
                }

                Ok(Event::Incoming(Packet::Publish(publish))) => {
                    // Aiguillage par TOPIC, photos d'abord : un motif
                    // d'événements large (`foxguard/#`) couvre aussi les
                    // photos, qui ne sont pas des événements.
                    match faces::file_in_topic(&config.faces_topic, &publish.topic) {
                        Some(file) => {
                            if let Some(secret) = &faces_secret {
                                handle_face(file, &publish.payload, secret.as_bytes(), &repository)
                                    .await;
                            }
                        }
                        None => handle_payload(&publish.payload, &repository).await,
                    }
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
async fn handle_payload(payload: &[u8], repository: &EventRepository) {
    let Some(event) = parse_event(payload) else {
        return;
    };

    // Un échec d'écriture PERD l'événement : `rumqttc` a déjà acquitté le
    // message au broker, qui ne le renverra pas. On le journalise donc en
    // `error!` — c'est le seul signal qu'il en reste. Une file de reprise sur
    // disque serait la parade, si le besoin se confirme.
    if let Err(e) = repository.record(&event).await {
        error!("❌ Événement perdu ({}) : {e:#}", event.camera);
    }
}

/// Vérifie un message de photo de référence et l'applique à la liste.
async fn handle_face(file: &str, payload: &[u8], secret: &[u8], repository: &EventRepository) {
    let Some(message) = parse_face(file, payload, secret) else {
        return;
    };

    let result = match &message {
        FaceMessage::Photo(photo) => {
            // Décodable : `FaceMessage::parse` l'a vérifié.
            let image = photo.decoded_image().unwrap_or_default();
            let person_name = faces::person_name(&photo.file);

            repository
                .upsert_face(&NewFace {
                    file: &photo.file,
                    person_name: &person_name,
                    camera: &photo.camera,
                    captured_at: photo.captured_at,
                    photo: &image,
                })
                .await
        }
        FaceMessage::Removed(removal) => repository.delete_face(&removal.file).await.map(|_| ()),
    };

    if let Err(e) = result {
        error!("❌ Photo de référence {file} non prise en compte : {e:#}");
    }
}

/// Décode et VÉRIFIE un message de photo de référence, ou `None` s'il doit
/// être ignoré.
///
/// Extrait de [`handle_face`] pour être testable sans base de données.
fn parse_face(file: &str, payload: &[u8], secret: &[u8]) -> Option<FaceMessage> {
    // Message retenu effacé à la main : le broker ne sait plus rien de ce
    // fichier, mais ce n'est pas une suppression (elle passe par une pierre
    // tombale signée, voir `foxguard_protocol::faces`).
    if payload.is_empty() {
        return None;
    }

    match FaceMessage::parse(file, payload, secret) {
        Ok(message) => {
            debug!(
                "🖼️ Photo de référence {} ({})",
                message.file(),
                message.camera()
            );
            Some(message)
        }
        Err(e) => {
            warn!("⚠️ Photo de référence ignorée ({file}) : {e:?}");
            None
        }
    }
}

/// Décode une charge utile MQTT, ou `None` si elle n'est pas un événement de
/// détection reconnaissable.
///
/// Extrait de [`handle_payload`] pour être testable sans base de données :
/// c'est ici que vit la tolérance aux messages inattendus, le reste n'étant
/// qu'une écriture.
fn parse_event(payload: &[u8]) -> Option<DetectionEvent> {
    match serde_json::from_slice::<DetectionEvent>(payload) {
        Ok(event) => {
            match event.status.name() {
                Some(name) => debug!("👤 {} : {} reconnu(e)", event.camera, name),
                None => debug!("❓ {} : personne inconnue", event.camera),
            }

            Some(event)
        }
        Err(e) => {
            // Le broker peut porter d'autres producteurs, et une caméra plus
            // récente peut émettre un format que ce manager ne comprend pas
            // encore : on signale et on continue.
            warn!(
                "⚠️ Message MQTT ignoré (format non reconnu) : {e} — {}",
                String::from_utf8_lossy(payload)
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_valid_payload_is_decoded() {
        let raw = br#"{"camera":"salon","timestamp":"2026-09-18T15:42:07+02:00","status":"known","name":"jerome"}"#;

        let event = parse_event(raw).expect("charge utile valide");

        assert_eq!(event.camera, "salon");
        assert_eq!(event.status.name(), Some("jerome"));
    }

    #[test]
    fn an_unknown_person_payload_is_decoded() {
        let raw =
            br#"{"camera":"entree","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown"}"#;

        let event = parse_event(raw).expect("charge utile valide");

        assert!(event.status.is_unknown());
    }

    #[test]
    fn a_malformed_payload_is_ignored_without_panicking() {
        assert!(parse_event(b"ceci n'est pas du JSON").is_none());
    }

    #[test]
    fn a_json_payload_with_the_wrong_shape_is_ignored() {
        // JSON valide, mais qui ne décrit pas un événement de détection : un
        // autre producteur publie peut-être sur le même topic.
        assert!(parse_event(br#"{"temperature": 21.5}"#).is_none());
    }

    // --- Photos de référence ---

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

    #[test]
    fn a_signed_photo_is_accepted() {
        let message =
            FaceMessage::photo(SECRET, "lou_1.jpg", "entree", chrono::Utc::now(), b"jpeg");
        let payload = serde_json::to_vec(&message).unwrap();

        assert_eq!(parse_face("lou_1.jpg", &payload, SECRET), Some(message));
    }

    #[test]
    fn a_photo_signed_with_another_secret_is_ignored() {
        let message = FaceMessage::photo(
            b"un tout autre secret, assez long",
            "lou_1.jpg",
            "entree",
            chrono::Utc::now(),
            b"jpeg",
        );
        let payload = serde_json::to_vec(&message).unwrap();

        assert_eq!(parse_face("lou_1.jpg", &payload, SECRET), None);
    }

    #[test]
    fn a_cleared_retained_message_is_not_a_removal() {
        assert_eq!(parse_face("lou_1.jpg", b"", SECRET), None);
    }

    #[test]
    fn decoding_is_independent_between_messages() {
        // Le point important : un message corrompu ne doit pas empêcher le
        // suivant d'être traité.
        assert!(parse_event(b"{{{").is_none());
        assert!(
            parse_event(
                br#"{"camera":"salon","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown"}"#
            )
            .is_some()
        );
    }
}
