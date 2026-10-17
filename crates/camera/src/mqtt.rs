//! Publication d'événements de détection sur un broker MQTT : nom de la
//! caméra, horodatage, et statut de la personne détectée (inconnue, ou
//! connue avec son nom). Fonctionnalité optionnelle, activée via
//! `[mqtt] enabled = true` dans `camera-config.toml` (voir [`crate::config::MqttConfig`]).
//!
//! La détection du CHANGEMENT d'état (ne publier qu'une fois par transition,
//! pas à chaque tentative de reconnaissance) vit dans
//! `crate::capture::tracking`, qui reste volontairement dépourvu d'effets de
//! bord : ce module-ci ne fait que la connexion au broker et la publication
//! proprement dite.
//!
//! Le même client porte le partage des photos de référence entre caméras
//! (voir `foxguard_protocol::faces`) : il s'abonne au topic des photos et
//! transmet ce qu'il reçoit, sans l'interpréter, à la bibliothèque de visages
//! (`crate::capture::face_library`), qui seule sait quoi en faire.

use std::time::Duration;

use foxguard_protocol::faces::{self, FaceMessage};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info};

use crate::config::MqttConfig;

// Le format des messages publiés ne vit plus ici : il est défini par le crate
// `foxguard-protocol`, partagé avec `foxguard-manager` qui les consomme. Une
// modification du format devient ainsi une erreur de COMPILATION des deux
// côtés, au lieu d'une panne silencieuse à l'exécution.
//
// `PersonStatus` est ré-exporté pour que le reste de la caméra (notamment
// `crate::capture::tracking`) continue de l'importer depuis ce module.
pub use foxguard_protocol::{DetectionEvent, PersonStatus};

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
    /// Préfixe des topics des photos de référence (`[mqtt] faces_topic`).
    faces_topic: String,
}

/// Ce que le broker apprend à la bibliothèque de visages.
#[derive(Debug)]
pub enum FaceTopicEvent {
    /// (Re)connexion au broker : les photos retenues vont être (re)livrées.
    Connected,
    /// Message reçu sur le topic d'une photo, encore à vérifier.
    Message { file: String, payload: Vec<u8> },
}

/// Abonnement au topic des photos de référence, demandé par
/// `crate::capture::face_library` quand le partage est actif.
pub struct FaceTopic {
    /// Où transmettre ce qui arrive.
    pub events: UnboundedSender<FaceTopicEvent>,
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
    pub fn connect(config: &MqttConfig, camera_name: &str, faces: Option<FaceTopic>) -> Self {
        let mut mqtt_options = MqttOptions::new(
            client_id(camera_name, std::process::id()),
            config.broker_host.clone(),
            config.broker_port,
        );
        mqtt_options.set_keep_alive(Duration::from_secs(30));

        // Le client limite par défaut les paquets à 10 Kio, entrants comme
        // sortants : assez pour un événement et sa vignette, pas pour une
        // photo de référence. Et un paquet hors limite COUPE la connexion
        // (voir `faces::MAX_MESSAGE_BYTES`).
        mqtt_options.set_max_packet_size(faces::MAX_MESSAGE_BYTES, faces::MAX_MESSAGE_BYTES);

        if !config.username.is_empty() {
            mqtt_options.set_credentials(config.username.clone(), config.password.clone());
        }

        let (client, mut event_loop) = AsyncClient::new(mqtt_options, 10);

        let subscriber = client.clone();
        let faces_topic = config.faces_topic.clone();

        tokio::spawn(async move {
            loop {
                match event_loop.poll().await {
                    // L'abonnement est redemandé à CHAQUE connexion : la
                    // session n'est pas persistante, et le broker a oublié
                    // l'abonnement précédent (même raisonnement que le
                    // manager, voir `foxguard_manager::ingest`).
                    Ok(Event::Incoming(Packet::ConnAck(_))) => {
                        let Some(faces) = &faces else { continue };

                        let pattern = faces::subscription(&faces_topic);
                        match subscriber.subscribe(&pattern, QoS::AtLeastOnce).await {
                            Ok(()) => {
                                info!(
                                    "📡 Abonné aux photos de référence partagées (\"{pattern}\")."
                                );
                                let _ = faces.events.send(FaceTopicEvent::Connected);
                            }
                            Err(e) => error!("❌ Abonnement au topic \"{pattern}\" échoué : {e}"),
                        }
                    }

                    Ok(Event::Incoming(Packet::Publish(publish))) => {
                        if let Some(faces) = &faces
                            && let Some(file) = faces::file_in_topic(&faces_topic, &publish.topic)
                        {
                            let _ = faces.events.send(FaceTopicEvent::Message {
                                file: file.to_string(),
                                payload: publish.payload.to_vec(),
                            });
                        }
                    }

                    Ok(_notification) => {}
                    Err(e) => {
                        error!("❌ Connexion MQTT perdue, nouvelle tentative : {:?}", e);
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
            faces_topic: config.faces_topic.clone(),
        }
    }

    /// Publie une photo de référence (ou sa pierre tombale) en message
    /// RETENU sur son topic : c'est le broker qui la remettra aux caméras
    /// absentes au moment de la publication (voir
    /// `foxguard_protocol::faces`).
    ///
    /// Attend que le message soit confié au client, pas qu'il soit acquitté
    /// par le broker : `rumqttc` le retransmet de lui-même après une coupure.
    pub async fn publish_face(&self, message: &FaceMessage) -> anyhow::Result<()> {
        let payload = serde_json::to_vec(message)?;

        // Contrôlé AVANT d'être confié au client : un message hors limite ne
        // serait pas refusé, il couperait la connexion.
        anyhow::ensure!(
            payload.len() <= faces::MAX_MESSAGE_BYTES,
            "message de {} octets, au-delà de la limite de paquet MQTT",
            payload.len()
        );

        self.client
            .publish(
                faces::topic(&self.faces_topic, message.file()),
                QoS::AtLeastOnce,
                true,
                payload,
            )
            .await?;

        Ok(())
    }

    /// Publie un événement de détection en tâche de fond
    /// (`tokio::spawn`) : ne bloque jamais l'appelant, même si le broker est
    /// temporairement injoignable (même principe que
    /// `crate::mail::Mailer::send_alert` pour les e-mails d'alerte).
    ///
    /// L'événement est CONSTITUÉ par l'appelant (voir
    /// `crate::capture::worker`) et non ici : c'est lui qui a la frame sous
    /// les yeux, donc de quoi en extraire une vignette, et qui sait quel clip
    /// vidéo couvre la détection. Ce module ne fait que la connexion au
    /// broker et la publication.
    pub fn publish_event(&self, event: DetectionEvent) {
        let payload = match serde_json::to_vec(&event) {
            Ok(payload) => payload,
            Err(e) => {
                error!("❌ Impossible de sérialiser l'événement MQTT : {:?}", e);
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
                error!("❌ Échec de publication MQTT : {:?}", e);
            }
        });
    }
}

/// Identifiant de client MQTT de cette caméra.
///
/// Unique par caméra ET par processus : la plupart des brokers déconnectent
/// le client précédent quand un second se présente sous le même identifiant.
/// Le seul numéro de processus n'y suffisait pas — en conteneur, chaque
/// caméra tourne sous le PID 1, et deux caméras s'évinçaient en boucle. Avec
/// l'abonnement aux photos retenues, chaque reconnexion fait relivrer toutes
/// les photos : un tel va-et-vient coûterait des mégaoctets à chaque fois.
///
/// Le nom de la caméra est libre (`[camera] name`) : seuls ses caractères
/// ASCII alphanumériques sont gardés, qu'aucun broker ne refuse.
fn client_id(camera_name: &str, pid: u32) -> String {
    let name: String = camera_name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(32)
        .collect();

    format!("foxguard-{name}-{pid}")
}

// Les tests du FORMAT des messages (sérialisation, aller-retour, format de
// fil) vivent dans le crate `foxguard-protocol`, avec les types eux-mêmes :
// les dupliquer ici reviendrait à tester deux fois la même chose, et
// laisserait les deux copies diverger. La connexion au broker et la
// publication demandent un vrai broker pour être testées.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_cameras_in_containers_get_distinct_client_ids() {
        // En conteneur, les deux tournent sous le PID 1.
        assert_ne!(client_id("entree", 1), client_id("jardin", 1));
    }

    #[test]
    fn the_client_id_keeps_only_safe_characters() {
        assert_eq!(client_id("allée/nord 2", 7), "foxguard-allenord2-7");
    }
}
