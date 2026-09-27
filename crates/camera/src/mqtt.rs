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
        let event = DetectionEvent::now(camera_name, status.clone());

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

// Les tests du FORMAT des messages (sérialisation, aller-retour, format de
// fil) vivent maintenant dans le crate `foxguard-protocol`, avec le type
// lui-même : les dupliquer ici reviendrait à tester deux fois la même chose,
// et laisserait les deux copies diverger. Ce module ne conserve que la
// connexion au broker et la publication, qui demandent un vrai broker pour
// être testées.
