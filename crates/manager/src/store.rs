//! Stockage en mémoire des événements reçus des caméras.
//!
//! VOLATILE et BORNÉ : les événements sont perdus au redémarrage, et seuls
//! les `capacity` plus récents sont conservés. C'est délibéré pour cette
//! première version — un manager qui accumulerait sans limite finirait par
//! épuiser la mémoire du serveur, et la persistance mérite d'être conçue
//! quand les besoins de l'interface seront précisés (filtres, agrégats,
//! durée d'historique voulue).

use std::collections::VecDeque;
use std::sync::Mutex;

use foxguard_protocol::DetectionEvent;

/// Historique borné des événements reçus, du plus ancien au plus récent.
pub struct EventStore {
    events: Mutex<VecDeque<DetectionEvent>>,
    capacity: usize,
}

impl EventStore {
    /// Crée un historique conservant au plus `capacity` événements.
    /// Une capacité nulle est ramenée à 1 : un historique qui oublie tout
    /// immédiatement ne rendrait aucun service et masquerait une erreur de
    /// configuration.
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);

        Self {
            events: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    /// Enregistre un événement, en oubliant le plus ancien si l'historique
    /// est plein.
    pub fn record(&self, event: DetectionEvent) {
        let mut events = self.lock();

        if events.len() == self.capacity {
            events.pop_front();
        }

        events.push_back(event);
    }

    /// Les `limit` événements les plus RÉCENTS, du plus récent au plus
    /// ancien — l'ordre dans lequel une interface les affiche.
    pub fn recent(&self, limit: usize) -> Vec<DetectionEvent> {
        self.lock().iter().rev().take(limit).cloned().collect()
    }

    /// Noms des caméras ayant déjà émis au moins un événement, triés.
    pub fn cameras(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .lock()
            .iter()
            .map(|event| event.camera.clone())
            .collect();

        names.sort();
        names.dedup();
        names
    }

    /// Nombre d'événements actuellement conservés.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Verrouille l'historique en récupérant les données même si le mutex a
    /// été empoisonné par la panique d'un autre thread : perdre tout
    /// l'historique parce qu'une requête HTTP a paniqué serait une réaction
    /// disproportionnée (même raisonnement que `MutexExt` côté caméra).
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<DetectionEvent>> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use foxguard_protocol::PersonStatus;

    fn event(camera: &str, name: Option<&str>) -> DetectionEvent {
        let status = match name {
            Some(name) => PersonStatus::Known {
                name: name.to_string(),
            },
            None => PersonStatus::Unknown,
        };

        DetectionEvent::now(camera, status)
    }

    #[test]
    fn a_new_store_is_empty() {
        let store = EventStore::new(10);
        assert!(store.is_empty());
        assert!(store.recent(10).is_empty());
        assert!(store.cameras().is_empty());
    }

    #[test]
    fn recorded_events_are_returned_most_recent_first() {
        let store = EventStore::new(10);
        store.record(event("salon", None));
        store.record(event("entree", Some("jerome")));

        let recent = store.recent(10);

        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].camera, "entree");
        assert_eq!(recent[1].camera, "salon");
    }

    #[test]
    fn the_limit_caps_the_number_of_returned_events() {
        let store = EventStore::new(10);
        for _ in 0..5 {
            store.record(event("salon", None));
        }

        assert_eq!(store.recent(2).len(), 2);
    }

    #[test]
    fn the_oldest_events_are_forgotten_once_capacity_is_reached() {
        let store = EventStore::new(3);
        store.record(event("un", None));
        store.record(event("deux", None));
        store.record(event("trois", None));
        store.record(event("quatre", None));

        let recent = store.recent(10);

        assert_eq!(recent.len(), 3, "la capacité doit être respectée");
        let cameras: Vec<&str> = recent.iter().map(|e| e.camera.as_str()).collect();
        assert_eq!(cameras, vec!["quatre", "trois", "deux"]);
        assert!(
            !cameras.contains(&"un"),
            "le plus ancien doit avoir été oublié"
        );
    }

    #[test]
    fn a_zero_capacity_is_clamped_to_one() {
        // Une capacité nulle viendrait forcément d'une erreur de
        // configuration : on garde au moins un événement plutôt que de tout
        // jeter en silence.
        let store = EventStore::new(0);
        store.record(event("salon", None));

        assert_eq!(store.len(), 1);
    }

    #[test]
    fn cameras_are_listed_once_each_and_sorted() {
        let store = EventStore::new(10);
        store.record(event("salon", None));
        store.record(event("entree", Some("lou")));
        store.record(event("salon", Some("mael")));

        assert_eq!(store.cameras(), vec!["entree", "salon"]);
    }

    #[test]
    fn cameras_only_reflect_events_still_in_the_history() {
        // Une caméra dont tous les événements ont été oubliés ne doit plus
        // apparaître : la liste décrit l'historique, pas un inventaire.
        let store = EventStore::new(2);
        store.record(event("ancienne", None));
        store.record(event("salon", None));
        store.record(event("entree", None));

        assert_eq!(store.cameras(), vec!["entree", "salon"]);
    }
}
