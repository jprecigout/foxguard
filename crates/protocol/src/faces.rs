//! Photos de référence de la reconnaissance faciale, partagées entre les
//! caméras par le broker MQTT.
//!
//! # Le problème
//!
//! Chaque caméra reconnaît les visages à partir des photos de SON dossier
//! `known_faces/`. Une personne enregistrée devant la caméra de l'entrée
//! restait donc une inconnue — et une alerte — pour celle du jardin, tant
//! qu'on n'avait pas recopié ses photos à la main sur chaque Raspberry Pi.
//!
//! # La solution
//!
//! Chaque photo est publiée en message RETENU sur `<topic>/<fichier>`. Le
//! broker garde le dernier message de chaque topic et le remet à tout nouvel
//! abonné : une caméra qui démarre, ou qui rejoint l'installation, reçoit
//! toutes les photos d'un coup, sans que personne n'ait à les lui renvoyer.
//! Le manager, abonné au même motif, en tient la liste pour son interface.
//!
//! Le fichier est le seul identifiant : le nom de la personne s'en déduit
//! (voir [`person_name_from_stem`]), exactement comme une caméra le déduit en
//! chargeant son dossier. Un champ « nom » à côté aurait pu le contredire.
//!
//! # Les suppressions
//!
//! Une suppression est publiée sur le même topic sous la forme d'une PIERRE
//! TOMBALE retenue ([`FaceMessage::Removed`]), et non d'un message vide. Un
//! message vide efface le message retenu : une caméra éteinte au moment de la
//! suppression ne l'apprendrait jamais, garderait la photo, et la
//! republierait à son retour (voir la réconciliation côté caméra).
//!
//! # Pourquoi les messages sont signés
//!
//! Une personne reconnue COUPE L'ALERTE. Accepter n'importe quelle photo
//! publiée sur le broker reviendrait à laisser quiconque l'atteint ajouter
//! son visage aux personnes autorisées — sur toutes les caméras d'un coup, et
//! pour toujours puisque le message est retenu. Chaque message est donc signé
//! avec le secret partagé des tickets (voir [`crate::ticket`]), sous un
//! domaine de signature distinct : une signature de photo ne peut pas servir
//! de ticket, ni l'inverse.

use base64::Engine as _;
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::ticket::{from_hex, to_hex};

type HmacSha256 = Hmac<Sha256>;

/// Séparation de domaine, distincte de celle des tickets.
const DOMAIN: &[u8] = b"foxguard-face-v1";

/// Préfixe des topics des photos par défaut (`[mqtt] faces_topic`, côté
/// caméra comme côté manager).
pub const DEFAULT_TOPIC: &str = "foxguard/faces";

/// Taille maximale d'une photo partagée, en octets.
///
/// Une capture de caméra pèse quelques centaines de kilo-octets au plus. La
/// marge sert les photos déposées à la main dans `known_faces/` (un portrait
/// de téléphone), qui sont partagées elles aussi. Au-delà, la photo reste
/// locale : mieux vaut une caméra qui la garde pour elle qu'un broker qui
/// distribue des dizaines de mégaoctets à chaque reconnexion.
pub const MAX_PHOTO_BYTES: usize = 4 * 1024 * 1024;

/// Taille maximale d'un message de photo sur le fil, à configurer comme
/// limite de paquet MQTT des deux côtés.
///
/// Le client MQTT limite par défaut les paquets à 10 Kio, et un paquet trop
/// gros ne fait pas qu'échouer : il COUPE la connexion. Une photo retenue
/// trop grosse ferait donc boucler les reconnexions de toutes les caméras.
/// D'où une limite large au-dessus de ce qu'un message conforme peut peser
/// (une photo de [`MAX_PHOTO_BYTES`] en base64, plus quelques champs).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;

/// Longueur maximale d'un nom de fichier partagé.
const MAX_FILE_NAME_LEN: usize = 128;

/// Topic d'une photo : `<préfixe>/<fichier>`.
pub fn topic(prefix: &str, file: &str) -> String {
    format!("{}/{file}", prefix.trim_end_matches('/'))
}

/// Motif d'abonnement couvrant toutes les photos : `<préfixe>/+`.
pub fn subscription(prefix: &str) -> String {
    topic(prefix, "+")
}

/// Fichier désigné par `topic`, s'il relève bien du préfixe des photos.
///
/// `None` pour tout autre topic — y compris un sous-niveau
/// (`<préfixe>/a/b`) : un nom de fichier ne contient pas de barre oblique.
pub fn file_in_topic<'a>(prefix: &str, topic: &'a str) -> Option<&'a str> {
    let file = topic
        .strip_prefix(prefix.trim_end_matches('/'))?
        .strip_prefix('/')?;

    (!file.is_empty() && !file.contains('/')).then_some(file)
}

/// Vrai si `file` peut être partagé : un nom SIMPLE, en ASCII
/// (`[A-Za-z0-9_-]`), terminé par une extension d'image reconnue.
///
/// Le nom de fichier arrive du réseau et finit dans un chemin disque sur
/// chaque caméra : ni barre oblique, ni `..`, ni caractère qu'un topic MQTT
/// interpréterait (`+`, `#`). C'est aussi ce que produit la capture depuis
/// l'interface (voir `known_faces` côté caméra).
pub fn is_valid_file_name(file: &str) -> bool {
    let Some((stem, extension)) = file.rsplit_once('.') else {
        return false;
    };

    file.len() <= MAX_FILE_NAME_LEN
        && !stem.is_empty()
        && stem
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && matches!(
            extension.to_ascii_lowercase().as_str(),
            "jpg" | "jpeg" | "png"
        )
}

/// Déduit le nom d'une personne du "stem" (nom de fichier sans extension) de
/// sa photo de référence, en ignorant un éventuel suffixe numérique qui
/// distingue plusieurs captures d'une même personne (angles / poses
/// différents) :
///
/// `"jerome"`               → `"jerome"`
/// `"jerome_20260918093604"` → `"jerome"`
///
/// Ici, et non dans la caméra seule : le manager regroupe les photos par
/// personne avec la même règle. Deux règles qui divergeraient montreraient
/// dans l'interface des personnes que les caméras ne reconnaissent pas sous
/// ce nom.
pub fn person_name_from_stem(stem: &str) -> String {
    match stem.rsplit_once('_') {
        Some((base, suffix))
            if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) =>
        {
            base.to_string()
        }
        _ => stem.to_string(),
    }
}

/// Nom de la personne d'un fichier de photo (voir [`person_name_from_stem`]).
pub fn person_name(file: &str) -> String {
    let stem = file.rsplit_once('.').map_or(file, |(stem, _)| stem);
    person_name_from_stem(stem)
}

/// Message publié (retenu) sur le topic d'une photo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FaceMessage {
    /// Une photo de référence, à ajouter au dossier de chaque caméra.
    Photo(FacePhoto),
    /// Pierre tombale : la photo a été supprimée, et doit l'être partout
    /// (voir l'en-tête du module pour pourquoi ce n'est pas un message vide).
    Removed(FaceRemoval),
}

/// Une photo de référence partagée.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FacePhoto {
    /// Nom du fichier dans `known_faces/`, identique sur toutes les caméras.
    pub file: String,
    /// Caméra qui a pris (ou partagé) la photo.
    pub camera: String,
    /// Date de la capture.
    pub captured_at: DateTime<Utc>,
    /// Octets du fichier (JPEG ou PNG, selon l'extension), en base64.
    pub image: String,
    /// HMAC-SHA256 en hexadécimal (voir [`FaceMessage::verify`]).
    pub signature: String,
}

/// Une photo supprimée.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaceRemoval {
    pub file: String,
    /// Caméra par laquelle la suppression a été demandée.
    pub camera: String,
    pub removed_at: DateTime<Utc>,
    pub signature: String,
}

/// Pourquoi un message de photo a été refusé.
#[derive(Debug, PartialEq, Eq)]
pub enum FaceMessageError {
    /// Pas un message de photo lisible (JSON, base64).
    Malformed,
    /// Nom de fichier dangereux ou non partageable (voir
    /// [`is_valid_file_name`]).
    InvalidFileName,
    /// Le fichier annoncé n'est pas celui du topic : une photo signée pour un
    /// fichier ne doit pas pouvoir en écraser un autre.
    TopicMismatch,
    /// Photo au-delà de [`MAX_PHOTO_BYTES`].
    TooLarge,
    /// Signature fausse : autre secret, ou message falsifié.
    BadSignature,
}

impl FaceMessage {
    /// Photo signée, prête à publier.
    pub fn photo(
        secret: &[u8],
        file: &str,
        camera: &str,
        captured_at: DateTime<Utc>,
        image: &[u8],
    ) -> Self {
        let image = base64::engine::general_purpose::STANDARD.encode(image);
        let mut mac = signing_key(secret, b"photo", file, camera, captured_at);
        mac.update(image.as_bytes());

        Self::Photo(FacePhoto {
            file: file.to_string(),
            camera: camera.to_string(),
            captured_at,
            image,
            signature: to_hex(&mac.finalize().into_bytes()),
        })
    }

    /// Pierre tombale signée, prête à publier.
    pub fn removal(secret: &[u8], file: &str, camera: &str, removed_at: DateTime<Utc>) -> Self {
        let mac = signing_key(secret, b"removed", file, camera, removed_at);

        Self::Removed(FaceRemoval {
            file: file.to_string(),
            camera: camera.to_string(),
            removed_at,
            signature: to_hex(&mac.finalize().into_bytes()),
        })
    }

    /// Fichier concerné.
    pub fn file(&self) -> &str {
        match self {
            Self::Photo(photo) => &photo.file,
            Self::Removed(removal) => &removal.file,
        }
    }

    /// Caméra émettrice.
    pub fn camera(&self) -> &str {
        match self {
            Self::Photo(photo) => &photo.camera,
            Self::Removed(removal) => &removal.camera,
        }
    }

    /// Vérifie la signature, en temps constant.
    pub fn verify(&self, secret: &[u8]) -> Result<(), FaceMessageError> {
        let (mac, signature) = match self {
            Self::Photo(photo) => {
                let mut mac = signing_key(
                    secret,
                    b"photo",
                    &photo.file,
                    &photo.camera,
                    photo.captured_at,
                );
                mac.update(photo.image.as_bytes());
                (mac, &photo.signature)
            }
            Self::Removed(removal) => (
                signing_key(
                    secret,
                    b"removed",
                    &removal.file,
                    &removal.camera,
                    removal.removed_at,
                ),
                &removal.signature,
            ),
        };

        let signature = from_hex(signature).ok_or(FaceMessageError::Malformed)?;
        mac.verify_slice(&signature)
            .map_err(|_| FaceMessageError::BadSignature)
    }

    /// Décode et VALIDE un message reçu sur le topic de `topic_file` : nom de
    /// fichier sûr et identique à celui du topic, taille, puis signature.
    ///
    /// C'est le seul point d'entrée que les deux composants utilisent pour
    /// lire un message : une vérification oubliée d'un côté serait une
    /// faille de l'autre.
    pub fn parse(
        topic_file: &str,
        payload: &[u8],
        secret: &[u8],
    ) -> Result<Self, FaceMessageError> {
        if payload.len() > MAX_MESSAGE_BYTES {
            return Err(FaceMessageError::TooLarge);
        }

        let message: Self =
            serde_json::from_slice(payload).map_err(|_| FaceMessageError::Malformed)?;

        if !is_valid_file_name(message.file()) {
            return Err(FaceMessageError::InvalidFileName);
        }

        if message.file() != topic_file {
            return Err(FaceMessageError::TopicMismatch);
        }

        message.verify(secret)?;

        if let Self::Photo(photo) = &message {
            // Décodée ici pour que l'appelant n'ait jamais à traiter un
            // base64 corrompu ou une image trop lourde.
            match photo.decoded_image() {
                Some(image) if image.len() <= MAX_PHOTO_BYTES => {}
                Some(_) => return Err(FaceMessageError::TooLarge),
                None => return Err(FaceMessageError::Malformed),
            }
        }

        Ok(message)
    }
}

impl FacePhoto {
    /// Octets du fichier, ou `None` si le base64 est illisible.
    pub fn decoded_image(&self) -> Option<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.image)
            .ok()
    }
}

/// HMAC prêt à recevoir la suite du message, sur le domaine, la nature du
/// message, le fichier, la caméra et la date.
///
/// Mêmes séparateurs nuls que les tickets (voir `crate::ticket::sign`), et
/// pour la même raison : sans eux, deux messages différents pourraient
/// signer la même chaîne. La date entre en millisecondes, ce qu'un aller-
/// retour RFC 3339 conserve à l'identique.
fn signing_key(
    secret: &[u8],
    kind: &[u8],
    file: &str,
    camera: &str,
    at: DateTime<Utc>,
) -> HmacSha256 {
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepte toute longueur de clé");
    mac.update(DOMAIN);
    mac.update(&[0]);
    mac.update(kind);
    mac.update(&[0]);
    mac.update(file.as_bytes());
    mac.update(&[0]);
    mac.update(camera.as_bytes());
    mac.update(&[0]);
    mac.update(at.timestamp_millis().to_string().as_bytes());
    mac.update(&[0]);
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
    const FILE: &str = "jerome_20261017093604123.jpg";
    const JPEG: &[u8] = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0xFF, 0xD9];

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-10-17T09:36:04.123Z")
            .unwrap()
            .into()
    }

    fn wire(message: &FaceMessage) -> Vec<u8> {
        serde_json::to_vec(message).expect("sérialisation")
    }

    // --- Noms de fichiers et topics ---

    #[test]
    fn a_capture_file_name_is_shareable() {
        assert!(is_valid_file_name(FILE));
        assert!(is_valid_file_name("jean-paul_1.JPEG"));
        assert!(is_valid_file_name("lou.png"));
    }

    #[test]
    fn dangerous_or_foreign_file_names_are_refused() {
        for file in [
            "",
            ".jpg",
            "jerome",
            "../jerome.jpg",
            "a/b.jpg",
            "a\\b.jpg",
            "jérôme.jpg",
            "jean paul.jpg",
            "a+b.jpg",
            "a#b.jpg",
            "a.gif",
            "a.jpg.exe",
        ] {
            assert!(!is_valid_file_name(file), "{file:?}");
        }

        assert!(!is_valid_file_name(&format!("{}.jpg", "a".repeat(200))));
    }

    #[test]
    fn the_topic_carries_the_file_and_reads_back() {
        let topic = topic("foxguard/faces/", FILE);

        assert_eq!(topic, format!("foxguard/faces/{FILE}"));
        assert_eq!(file_in_topic("foxguard/faces", &topic), Some(FILE));
        assert_eq!(subscription("foxguard/faces"), "foxguard/faces/+");
    }

    #[test]
    fn foreign_topics_carry_no_file() {
        assert_eq!(file_in_topic("foxguard/faces", "foxguard/detections"), None);
        assert_eq!(file_in_topic("foxguard/faces", "foxguard/faces"), None);
        assert_eq!(file_in_topic("foxguard/faces", "foxguard/faces/"), None);
        assert_eq!(
            file_in_topic("foxguard/faces", "foxguard/faces/a/b.jpg"),
            None
        );
        assert_eq!(
            file_in_topic("foxguard/faces", "foxguard/facesx/a.jpg"),
            None
        );
    }

    // --- Nom de la personne ---

    #[test]
    fn name_without_timestamp_suffix_is_kept_as_is() {
        assert_eq!(person_name_from_stem("jerome"), "jerome");
    }

    #[test]
    fn numeric_suffix_after_underscore_is_stripped() {
        assert_eq!(person_name_from_stem("jerome_20260918093604"), "jerome");
    }

    #[test]
    fn only_the_last_underscore_group_is_treated_as_a_suffix() {
        // Le nom peut lui-même contenir un underscore ; seul le dernier
        // groupe (purement numérique) est retiré.
        assert_eq!(
            person_name_from_stem("jean_paul_20260918093604"),
            "jean_paul"
        );
    }

    #[test]
    fn non_numeric_suffix_is_not_stripped() {
        assert_eq!(person_name_from_stem("photo_finale"), "photo_finale");
    }

    #[test]
    fn trailing_underscore_with_empty_suffix_is_kept_as_is() {
        assert_eq!(person_name_from_stem("jerome_"), "jerome_");
    }

    #[test]
    fn name_without_any_underscore_is_kept_as_is() {
        assert_eq!(person_name_from_stem("photo123"), "photo123");
    }

    #[test]
    fn the_person_name_of_a_file_ignores_its_extension() {
        assert_eq!(person_name(FILE), "jerome");
        assert_eq!(person_name("lou.png"), "lou");
    }

    // --- Messages ---

    #[test]
    fn a_photo_survives_the_wire_and_its_signature_holds() {
        let message = FaceMessage::photo(SECRET, FILE, "entree", now(), JPEG);

        let parsed = FaceMessage::parse(FILE, &wire(&message), SECRET).expect("photo acceptée");

        assert_eq!(parsed, message);
        let FaceMessage::Photo(photo) = parsed else {
            panic!("une photo");
        };
        assert_eq!(photo.decoded_image().as_deref(), Some(JPEG));
        assert_eq!(photo.camera, "entree");
    }

    #[test]
    fn a_removal_survives_the_wire_and_its_signature_holds() {
        let message = FaceMessage::removal(SECRET, FILE, "jardin", now());

        let parsed =
            FaceMessage::parse(FILE, &wire(&message), SECRET).expect("suppression acceptée");

        assert_eq!(parsed, message);
        assert_eq!(parsed.file(), FILE);
    }

    #[test]
    fn the_wire_format_is_tagged_by_type() {
        let photo =
            String::from_utf8(wire(&FaceMessage::photo(SECRET, FILE, "e", now(), JPEG))).unwrap();
        let removal =
            String::from_utf8(wire(&FaceMessage::removal(SECRET, FILE, "e", now()))).unwrap();

        assert!(photo.contains(r#""type":"photo""#), "{photo}");
        assert!(removal.contains(r#""type":"removed""#), "{removal}");
    }

    #[test]
    fn a_message_signed_with_another_secret_is_refused() {
        let message =
            FaceMessage::photo(b"un tout autre secret, assez long", FILE, "e", now(), JPEG);

        assert_eq!(
            FaceMessage::parse(FILE, &wire(&message), SECRET),
            Err(FaceMessageError::BadSignature)
        );
    }

    #[test]
    fn swapping_the_image_breaks_the_signature() {
        // Le cas d'attaque évident : garder une signature valide et
        // remplacer la photo par la sienne.
        let FaceMessage::Photo(mut photo) = FaceMessage::photo(SECRET, FILE, "e", now(), JPEG)
        else {
            unreachable!()
        };
        photo.image = base64::engine::general_purpose::STANDARD.encode(b"un autre visage");

        assert_eq!(
            FaceMessage::parse(FILE, &wire(&FaceMessage::Photo(photo)), SECRET),
            Err(FaceMessageError::BadSignature)
        );
    }

    #[test]
    fn a_photo_signature_does_not_validate_a_removal() {
        // Même fichier, même caméra, même date : seule la nature du message
        // diffère, et elle est signée.
        let FaceMessage::Photo(photo) = FaceMessage::photo(SECRET, FILE, "e", now(), b"") else {
            unreachable!()
        };
        let forged = FaceMessage::Removed(FaceRemoval {
            file: photo.file,
            camera: photo.camera,
            removed_at: photo.captured_at,
            signature: photo.signature,
        });

        assert_eq!(forged.verify(SECRET), Err(FaceMessageError::BadSignature));
    }

    #[test]
    fn a_message_published_on_another_files_topic_is_refused() {
        let message = FaceMessage::removal(SECRET, FILE, "e", now());

        assert_eq!(
            FaceMessage::parse("lou_1.jpg", &wire(&message), SECRET),
            Err(FaceMessageError::TopicMismatch)
        );
    }

    #[test]
    fn a_dangerous_file_name_is_refused_even_if_signed() {
        let message = FaceMessage::removal(SECRET, "../../etc/passwd.jpg", "e", now());

        assert_eq!(
            FaceMessage::parse("../../etc/passwd.jpg", &wire(&message), SECRET),
            Err(FaceMessageError::InvalidFileName)
        );
    }

    #[test]
    fn garbage_is_reported_as_malformed() {
        for payload in [&b""[..], b"{}", b"pas du JSON", br#"{"type":"photo"}"#] {
            assert_eq!(
                FaceMessage::parse(FILE, payload, SECRET),
                Err(FaceMessageError::Malformed),
                "{}",
                String::from_utf8_lossy(payload)
            );
        }
    }

    #[test]
    fn an_oversized_photo_is_refused() {
        let big = vec![0u8; MAX_PHOTO_BYTES + 1];
        let message = FaceMessage::photo(SECRET, FILE, "e", now(), &big);

        assert_eq!(
            FaceMessage::parse(FILE, &wire(&message), SECRET),
            Err(FaceMessageError::TooLarge)
        );
    }

    #[test]
    fn a_conforming_message_fits_the_packet_limit() {
        // Garde-fou du dimensionnement de `MAX_MESSAGE_BYTES` : un message
        // plus gros que la limite de paquet couperait la connexion MQTT.
        let largest = vec![0xFFu8; MAX_PHOTO_BYTES];
        let message = FaceMessage::photo(
            SECRET,
            FILE,
            "une caméra au nom assez long",
            now(),
            &largest,
        );

        assert!(wire(&message).len() < MAX_MESSAGE_BYTES);
    }
}
