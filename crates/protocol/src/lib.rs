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

pub mod auth;
pub mod ticket;

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

// Les définitions TypeScript de ces types sont GÉNÉRÉES à partir d'eux (voir
// `ui/src/generated/`), et non écrites à la main côté interface. Un champ
// renommé ou supprimé ici casse donc la compilation de l'interface, au lieu
// de produire une erreur à l'exécution.
//
// La génération a lieu pendant `cargo test --workspace` : ts-rs installe un
// test par type dérivant `TS`, DANS LE CRATE QUI LE DÉFINIT. Les types d'ici
// ne sont donc pas écrits par un test du manager, et `-p foxguard-manager`
// n'en régénérerait aucun.
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

    /// Vignette JPEG de la détection, encodée en base64 (voir
    /// [`encode_thumbnail`]).
    ///
    /// Transportée DANS l'événement plutôt que référencée par une URL sur la
    /// caméra : la timeline de l'interface doit rester consultable depuis
    /// n'importe où (et des mois plus tard), alors que la caméra n'est
    /// joignable que depuis son réseau local et purge ses fichiers au bout de
    /// quelques jours. Les événements ne sont publiés qu'aux CHANGEMENTS
    /// d'état, pas à chaque frame : le surcoût sur le fil reste de l'ordre de
    /// quelques kilo-octets par détection.
    ///
    /// `None` quand la caméra n'en produit pas (fonctionnalité désactivée, ou
    /// caméra antérieure à son introduction).
    // `#[serde(default)]` : c'est lui qui fait qu'une charge utile écrite par
    // une caméra ANTÉRIEURE à ce champ reste comprise (voir la règle de
    // compatibilité en tête de module).
    //
    // `#[ts(optional)]` : et c'est ce qui le dit aussi à l'interface. Sans
    // lui, ts-rs annoncerait un champ obligatoire côté TypeScript, alors
    // qu'il peut parfaitement être absent.
    //
    // Pas de `skip_serializing_if` ici, malgré la tentation : ts-rs ne sait
    // pas l'analyser et avertit à chaque compilation. Il ne ferait
    // qu'économiser `"thumbnail":null` — une trentaine d'octets sur un
    // message publié à chaque CHANGEMENT d'état, pas à chaque frame. Deux
    // avertissements permanents à chaque build coûtent plus cher que ça.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub thumbnail: Option<String>,

    /// URL de base par laquelle la caméra émettrice est joignable depuis un
    /// navigateur, ex. `http://192.168.1.42:8080`.
    ///
    /// Elle décrit la CAMÉRA, et non l'événement : c'est pourquoi elle vit
    /// ici plutôt que dans la référence du clip, où elle a commencé. Le
    /// manager s'en sert pour deux choses — le clip de cet événement, et le
    /// DIRECT de la caméra — et la seconde ne doit pas dépendre de
    /// l'existence du premier.
    ///
    /// `None` quand la caméra ne la déclare pas (`[server] public_url`) : elle
    /// ne peut pas la deviner, puisqu'elle écoute en général sur `0.0.0.0` et
    /// que son adresse vue du navigateur dépend du réseau et d'un éventuel
    /// proxy. Aucun lien n'est alors proposé, plutôt qu'un lien mort.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub base_url: Option<String>,

    /// Nom du fichier d'enregistrement couvrant la détection, s'il y en a un.
    ///
    /// Le clip lui-même n'est PAS transporté : quelques secondes de vidéo
    /// pèsent des mégaoctets, qui n'auraient aucune raison de traverser le
    /// broker pour un clip que personne n'ouvrira peut-être jamais.
    /// L'événement ne porte que de quoi aller le chercher.
    #[serde(default)]
    #[cfg_attr(feature = "ts", ts(optional))]
    pub clip: Option<String>,
}

impl DetectionEvent {
    /// Construit un événement horodaté à l'instant présent, sans média
    /// associé (voir [`Self::with_thumbnail`] et [`Self::with_clip`]).
    pub fn now(camera: impl Into<String>, status: PersonStatus) -> Self {
        Self {
            camera: camera.into(),
            timestamp: Local::now(),
            status,
            thumbnail: None,
            base_url: None,
            clip: None,
        }
    }

    /// Attache une vignette JPEG (encodée en base64 au passage).
    #[must_use]
    pub fn with_thumbnail(mut self, jpeg: &[u8]) -> Self {
        self.thumbnail = Some(encode_thumbnail(jpeg));
        self
    }

    /// Déclare l'URL de base de la caméra émettrice.
    ///
    /// Une chaîne vide est traitée comme une absence : c'est la valeur par
    /// défaut de `[server] public_url`, et elle ne décrit rien.
    #[must_use]
    pub fn with_base_url(mut self, base_url: &str) -> Self {
        self.base_url = (!base_url.is_empty()).then(|| base_url.to_string());
        self
    }

    /// Attache le nom du fichier de clip couvrant la détection.
    #[must_use]
    pub fn with_clip(mut self, clip: impl Into<String>) -> Self {
        self.clip = Some(clip.into());
        self
    }

    /// Décode la vignette attachée, ou `None` s'il n'y en a pas (ou si elle
    /// est illisible).
    pub fn decoded_thumbnail(&self) -> Option<Vec<u8>> {
        decode_thumbnail(self.thumbnail.as_deref()?)
    }

    /// URL de la page de LECTURE du clip sur la caméra.
    ///
    /// Sans ticket : la caméra exige, pour servir cette page, un ticket de
    /// portée « clip » que le manager ajoute au moment du clic (voir
    /// [`ticket`]).
    ///
    /// Elle pointe vers `/play/<fichier>` et non vers le fichier lui-même :
    /// un enregistrement peut être dans le format maison de FoxGuard, qu'aucun
    /// navigateur ne sait jouer tel quel. C'est la caméra qui sert la page
    /// capable de le lire — elle est le seul composant à connaître ses
    /// formats, et la seule origine autorisée à en lire les fichiers.
    pub fn clip_url(&self) -> Option<String> {
        Some(format!(
            "{}/play/{}",
            self.camera_base()?,
            self.clip.as_ref()?
        ))
    }

    /// URL de base, débarrassée d'une éventuelle barre oblique finale.
    fn camera_base(&self) -> Option<&str> {
        let base_url = self.base_url.as_deref()?.trim_end_matches('/');

        (!base_url.is_empty()).then_some(base_url)
    }
}

/// Encode une vignette JPEG pour le transport dans un événement.
///
/// Base64 et non des octets bruts : la charge utile est du JSON, où un
/// `Vec<u8>` se sérialiserait en tableau de nombres décimaux — environ
/// quatre octets de fil par octet d'image, contre un tiers de surcoût ici.
pub fn encode_thumbnail(jpeg: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(jpeg)
}

/// Décode une vignette reçue, ou `None` si ce n'est pas du base64 valide.
///
/// Tolérante par principe, comme le reste du décodage du protocole : une
/// vignette illisible ne doit pas faire perdre l'événement lui-même (voir
/// la règle de compatibilité en tête de module).
pub fn decode_thumbnail(encoded: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()
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
            thumbnail: None,
            base_url: None,
            clip: None,
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

    // --- Média attaché (vignette, clip) ---

    #[test]
    fn an_event_without_media_carries_no_media_content() {
        // Les champs apparaissent, à `null` (voir la note sur
        // `skip_serializing_if` près de leur déclaration), mais ne
        // transportent rien.
        let json = serde_json::to_string(&event(PersonStatus::Unknown)).expect("sérialisation");

        assert!(json.contains("\"thumbnail\":null"), "{json}");
        assert!(json.contains("\"clip\":null"), "{json}");

        let parsed: DetectionEvent = serde_json::from_str(&json).expect("relecture");
        assert_eq!(parsed.decoded_thumbnail(), None);
        assert_eq!(parsed.clip, None);
    }

    #[test]
    fn an_event_without_media_stays_small_on_the_wire() {
        // Garde-fou de volume : un message sans média doit rester de l'ordre
        // de la centaine d'octets, pas du kilo-octet.
        let json = serde_json::to_string(&event(PersonStatus::Unknown)).expect("sérialisation");

        assert!(json.len() < 160, "{} octets : {json}", json.len());
    }

    #[test]
    fn a_payload_from_an_older_camera_still_parses() {
        // C'EST LE TEST DE COMPATIBILITÉ : le format exact qu'écrivent les
        // caméras déjà déployées, sans aucun des champs de média.
        let raw =
            r#"{"camera":"entree","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown"}"#;

        let parsed: DetectionEvent = serde_json::from_str(raw).expect("format historique accepté");

        assert_eq!(parsed.thumbnail, None);
        assert_eq!(parsed.clip, None);
    }

    #[test]
    fn a_thumbnail_survives_a_round_trip_through_the_wire() {
        // Des octets JPEG plausibles, dont un hors ASCII : c'est précisément
        // ce que du base64 doit savoir faire traverser du JSON.
        let jpeg = [0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0xFF, 0xD9];

        let original = event(PersonStatus::Unknown).with_thumbnail(&jpeg);
        let json = serde_json::to_string(&original).expect("sérialisation");
        let parsed: DetectionEvent = serde_json::from_str(&json).expect("désérialisation");

        assert_eq!(parsed.decoded_thumbnail().as_deref(), Some(&jpeg[..]));
    }

    #[test]
    fn an_unreadable_thumbnail_does_not_cost_the_whole_event() {
        // Tolérance par principe : la vignette est un agrément, l'événement
        // est l'information.
        let raw = r#"{"camera":"entree","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown","thumbnail":"pas du base64 !!"}"#;

        let parsed: DetectionEvent = serde_json::from_str(raw).expect("événement lisible");

        assert_eq!(parsed.camera, "entree");
        assert_eq!(parsed.decoded_thumbnail(), None);
    }

    #[test]
    fn a_clip_reference_survives_a_round_trip() {
        let original = event(PersonStatus::Unknown)
            .with_base_url("http://192.168.1.42:8080")
            .with_clip("evt_20260918_154207123.mp4");

        let json = serde_json::to_string(&original).expect("sérialisation");
        let parsed: DetectionEvent = serde_json::from_str(&json).expect("désérialisation");

        assert_eq!(parsed.clip.as_deref(), Some("evt_20260918_154207123.mp4"));
        assert_eq!(parsed.base_url.as_deref(), Some("http://192.168.1.42:8080"));
    }

    #[test]
    fn a_clip_url_points_at_the_cameras_player_page() {
        let event = event(PersonStatus::Unknown)
            .with_base_url("http://192.168.1.42:8080")
            .with_clip("evt.mp4");

        assert_eq!(
            event.clip_url().as_deref(),
            Some("http://192.168.1.42:8080/play/evt.mp4")
        );
    }

    #[test]
    fn a_trailing_slash_in_the_public_url_does_not_double_up() {
        let event = event(PersonStatus::Unknown)
            .with_base_url("http://cam.local/")
            .with_clip("evt.mp4");

        assert_eq!(
            event.clip_url().as_deref(),
            Some("http://cam.local/play/evt.mp4")
        );
    }

    #[test]
    fn a_camera_without_a_public_url_yields_no_link() {
        // La caméra ne peut pas deviner son URL vue du navigateur : mieux
        // vaut pas de lien qu'un lien mort.
        let event = event(PersonStatus::Unknown).with_clip("evt.mp4");

        assert_eq!(event.clip_url(), None);
    }

    #[test]
    fn an_empty_public_url_is_treated_as_absent() {
        // C'est la valeur par défaut de `[server] public_url` : elle ne
        // décrit rien, et ne doit pas produire une URL commençant par `/`.
        let event = event(PersonStatus::Unknown)
            .with_base_url("")
            .with_clip("evt.mp4");

        assert_eq!(event.base_url, None);
        assert_eq!(event.clip_url(), None);
    }

    #[test]
    fn an_event_from_a_camera_without_media_fields_still_parses() {
        // Compatibilité : les deux champs sont `#[serde(default)]`, une
        // caméra qui ne les renseigne pas reste comprise.
        let raw = r#"{"camera":"e","timestamp":"2026-09-18T15:42:07+02:00","status":"unknown"}"#;

        let parsed: DetectionEvent = serde_json::from_str(raw).expect("format historique");

        assert_eq!(parsed.base_url, None);
        assert_eq!(parsed.clip, None);
        assert_eq!(parsed.clip_url(), None);
    }
}
