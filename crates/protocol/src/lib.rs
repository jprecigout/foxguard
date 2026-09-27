//! Contrat de messages entre `foxguard-camera` et `foxguard-manager`.
//!
//! Ce crate définit le format des événements publiés par une caméra sur MQTT
//! et consommés par le manager. Il est compilé par les deux composants : un
//! changement incompatible devient donc une erreur de COMPILATION, alors
//! qu'avec deux déclarations séparées il aurait produit une panne silencieuse
//! à l'exécution.
//!
//! # Règle de compatibilité
//!
//! La caméra (sur le Raspberry Pi) et le manager (sur le serveur) sont
//! déployés INDÉPENDAMMENT : il y aura toujours des moments où une caméra en
//! v1.2 parle à un manager en v1.4. Le fait de partager ce crate garantit la
//! cohérence des *sources*, pas celle des *binaires déployés*. Toute
//! évolution de [`DetectionEvent`] doit donc rester rétrocompatible :
//!
//! - un nouveau champ est toujours optionnel, avec `#[serde(default)]` ;
//! - un champ existant n'est JAMAIS renommé ni supprimé — on en ajoute un
//!   nouveau et on laisse l'ancien mourir de sa belle mort.
//!
//! C'est exactement la discipline déjà appliquée au fichier de configuration
//! de la caméra, transposée au fil MQTT.

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

// Les définitions TypeScript de ces types sont GÉNÉRÉES à partir d'eux (voir
// `ui/src/generated/`), et non écrites à la main côté interface. Un champ
// renommé ou supprimé ici casse donc la compilation de l'interface, au lieu
// de produire une erreur à l'exécution.
//
// La génération a lieu pendant `cargo test -p foxguard-manager` : ts-rs
// installe un test par type dérivant `TS`, qui écrit le fichier.
#[cfg(feature = "ts")]
use ts_rs::TS;

/// Statut de reconnaissance d'une personne détectée par une caméra.
///
/// Sérialisé sous la forme `{"status": "unknown"}` ou
/// `{"status": "known", "name": "jerome"}` : le nom n'apparaît que pour une
/// personne identifiée.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/generated/")
)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PersonStatus {
    /// Personne détectée (bounding-box YOLO) mais non identifiée par la
    /// reconnaissance faciale.
    Unknown,
    /// Personne identifiée, avec son nom (voir `known_faces/` côté caméra).
    Known { name: String },
}

impl PersonStatus {
    /// Nom de la personne si elle a été identifiée.
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Unknown => None,
            Self::Known { name } => Some(name),
        }
    }

    /// Vrai si la détection correspond à une personne NON identifiée, donc à
    /// une intrusion potentielle.
    pub fn is_unknown(&self) -> bool {
        matches!(self, Self::Unknown)
    }
}

/// Événement publié par une caméra à chaque CHANGEMENT d'état de
/// reconnaissance (et non à chaque frame).
///
/// Exemple de charge utile JSON sur le topic MQTT :
///
/// ```json
/// {"camera":"salon","timestamp":"2026-09-18T15:42:07+02:00","status":"known","name":"jerome"}
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(
    feature = "ts",
    derive(TS),
    ts(export, export_to = "../../../ui/src/generated/")
)]
pub struct DetectionEvent {
    /// Nom de la caméra émettrice (voir `[camera] name` dans sa
    /// configuration), qui distingue plusieurs installations sur un même
    /// broker.
    pub camera: String,

    /// Horodatage de l'événement, au format RFC 3339.
    #[cfg_attr(feature = "ts", ts(type = "string"))]
    pub timestamp: DateTime<Local>,

    /// Statut de reconnaissance, aplati dans l'objet JSON (`status` et, le
    /// cas échéant, `name` sont des champs de premier niveau).
    #[serde(flatten)]
    pub status: PersonStatus,
}

impl DetectionEvent {
    /// Construit un événement horodaté à l'instant présent.
    pub fn now(camera: impl Into<String>, status: PersonStatus) -> Self {
        Self {
            camera: camera.into(),
            timestamp: Local::now(),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(status: PersonStatus) -> DetectionEvent {
        DetectionEvent {
            camera: "salon".to_string(),
            timestamp: DateTime::parse_from_rfc3339("2026-09-18T15:42:07+02:00")
                .unwrap()
                .into(),
            status,
        }
    }

    #[test]
    fn an_unknown_person_serializes_without_a_name_field() {
        let json = serde_json::to_string(&event(PersonStatus::Unknown)).expect("sérialisation");

        assert!(json.contains("\"camera\":\"salon\""));
        assert!(json.contains("\"status\":\"unknown\""));
        assert!(
            !json.contains("\"name\""),
            "le champ name ne doit pas apparaître pour une personne inconnue : {json}"
        );
    }

    #[test]
    fn a_known_person_serializes_with_their_name() {
        let status = PersonStatus::Known {
            name: "jerome".to_string(),
        };
        let json = serde_json::to_string(&event(status)).expect("sérialisation");

        assert!(json.contains("\"status\":\"known\""));
        assert!(json.contains("\"name\":\"jerome\""));
    }

    // Ces deux tests sont le cœur de l'intérêt du crate partagé : ils
    // vérifient que ce que la CAMÉRA écrit est très exactement ce que le
    // MANAGER relit.

    #[test]
    fn an_unknown_event_survives_a_round_trip() {
        let original = event(PersonStatus::Unknown);
        let json = serde_json::to_string(&original).expect("sérialisation");
        let parsed: DetectionEvent = serde_json::from_str(&json).expect("désérialisation");

        assert_eq!(parsed, original);
    }

    #[test]
    fn a_known_event_survives_a_round_trip() {
        let original = event(PersonStatus::Known {
            name: "jerome".to_string(),
        });
        let json = serde_json::to_string(&original).expect("sérialisation");
        let parsed: DetectionEvent = serde_json::from_str(&json).expect("désérialisation");

        assert_eq!(parsed, original);
        assert_eq!(parsed.status.name(), Some("jerome"));
    }

    #[test]
    fn the_wire_format_published_by_a_camera_is_accepted() {
        // Charge utile écrite à la main, telle qu'elle circule réellement sur
        // le broker : si ce test casse, c'est que le format de fil a changé
        // et que les caméras déjà déployées ne seront plus comprises.
        let raw = r#"{"camera":"entree","timestamp":"2026-09-18T15:42:07+02:00","status":"known","name":"lou"}"#;

        let parsed: DetectionEvent = serde_json::from_str(raw).expect("format de fil accepté");

        assert_eq!(parsed.camera, "entree");
        assert_eq!(parsed.status.name(), Some("lou"));
        assert!(!parsed.status.is_unknown());
    }

    #[test]
    fn an_unknown_wire_payload_is_accepted() {
        let raw =
            r#"{"camera":"entree","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown"}"#;

        let parsed: DetectionEvent = serde_json::from_str(raw).expect("format de fil accepté");

        assert!(parsed.status.is_unknown());
        assert_eq!(parsed.status.name(), None);
    }

    #[test]
    fn unexpected_extra_fields_do_not_break_parsing() {
        // Compatibilité ASCENDANTE : un manager ancien doit pouvoir lire les
        // messages d'une caméra plus récente qui a ajouté un champ.
        let raw = r#"{"camera":"entree","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown","confidence":0.82}"#;

        let parsed: DetectionEvent = serde_json::from_str(raw).expect("champ inconnu ignoré");

        assert_eq!(parsed.camera, "entree");
    }
}
