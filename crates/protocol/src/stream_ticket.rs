//! Tickets de visionnage du direct : comment l'interface du manager ouvre le
//! WebSocket d'une caméra SANS en connaître le jeton d'API.
//!
//! # Le problème
//!
//! Le flux `GET /ws` d'une caméra est authentifié par son jeton d'API. Or ce
//! jeton donne TOUS les droits — couper la surveillance, supprimer des
//! enregistrements — et il n'a rien à faire dans une page web servie par une
//! autre machine, ni dans la base du manager.
//!
//! # La solution
//!
//! Le manager et les caméras partagent un SECRET, distinct du jeton d'API et
//! qui ne sert qu'à ça. Avec lui, le manager signe des tickets :
//!
//! - **liés à une caméra** : le nom de la caméra entre dans la signature, un
//!   ticket émis pour le garage n'ouvre pas le jardin ;
//! - **courts** : quelques minutes, le temps d'ouvrir la connexion. Un ticket
//!   recopié depuis la console du navigateur ne vaut plus rien peu après ;
//! - **en lecture seule** : la caméra n'accepte d'une connexion ouverte par
//!   ticket aucune commande (voir `ws_handler` côté caméra).
//!
//! La caméra vérifie la signature elle-même, sans rien demander au manager :
//! pas d'aller-retour, pas d'état partagé, et une caméra continue d'accepter
//! son propre jeton exactement comme avant.
//!
//! # Format
//!
//! `<expiration en secondes Unix>.<HMAC-SHA256 en hexadécimal>`, où le HMAC
//! porte sur [`DOMAIN`], le nom de la caméra et l'expiration. Uniquement des
//! chiffres, un point et de l'hexadécimal : le ticket passe tel quel dans une
//! chaîne de requête, sans encodage.
//!
//! Le nom de la caméra n'y figure PAS : la caméra le connaît, et l'en retirer
//! évite d'avoir à l'encoder pour l'URL.
//!
//! # Ce que cela suppose
//!
//! Des horloges à peu près à l'heure, des deux côtés. Un écart de plus de
//! [`TICKET_TTL_SECS`] fait refuser les tickets — d'où un message de journal
//! qui le dit explicitement côté caméra, plutôt qu'un 401 muet.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Séparation de domaine : la signature d'un ticket ne peut être confondue
/// avec aucun autre usage, présent ou futur, du même secret.
const DOMAIN: &[u8] = b"foxguard-stream-ticket-v1";

/// Durée de validité d'un ticket émis par le manager.
///
/// Deux minutes : le ticket ne sert qu'à OUVRIR la connexion, qui dure
/// ensuite aussi longtemps qu'elle veut. L'interface en redemande un à chaque
/// reconnexion.
pub const TICKET_TTL_SECS: i64 = 120;

/// Validité maximale qu'une caméra accepte, quelle que soit l'expiration
/// annoncée.
///
/// Un garde-fou contre un manager mal configuré (ou dont l'horloge avance) :
/// un ticket « valable un an » serait signé correctement, et serait pourtant
/// exactement ce que ce mécanisme cherche à éviter. La marge au-dessus de
/// [`TICKET_TTL_SECS`] absorbe un écart d'horloge raisonnable.
pub const MAX_TICKET_TTL_SECS: i64 = 600;

/// Longueur minimale du secret partagé, en octets.
///
/// 32 caractères : ce que produit `openssl rand -hex 16`, et assez pour qu'un
/// secret ne se devine pas. Un secret plus court est refusé au chargement de
/// la configuration plutôt qu'accepté en silence.
pub const MIN_SECRET_LEN: usize = 32;

/// Vérifie un secret lu dans une configuration, avant toute utilisation.
///
/// Vide est accepté : c'est le défaut, et il DÉSACTIVE les tickets — la
/// caméra n'accepte alors que son jeton, et le manager ne propose pas de
/// direct. Trop court ne l'est pas : un secret devinable vaudrait un flux
/// ouvert, et le dire au démarrage vaut mieux que de le découvrir après.
///
/// Partagée par les deux configurations : un secret accepté d'un côté et
/// refusé de l'autre ferait une panne incompréhensible.
pub fn validate_secret(secret: &str) -> Result<(), String> {
    if !secret.is_empty() && secret.len() < MIN_SECRET_LEN {
        return Err(format!(
            "secret des tickets de direct trop court ({} caractères, {MIN_SECRET_LEN} au \
             minimum) : générez-en un avec `openssl rand -hex 32`",
            secret.len()
        ));
    }

    Ok(())
}

/// Pourquoi un ticket a été refusé.
///
/// Distingué parce que les causes n'appellent pas la même réaction : une
/// signature fausse est une tentative (ou un secret différent des deux
/// côtés), une expiration est presque toujours une horloge déréglée.
#[derive(Debug, PartialEq, Eq)]
pub enum TicketError {
    /// Le ticket n'a pas la forme `<expiration>.<signature>`.
    Malformed,
    /// La signature ne correspond pas : autre secret, autre caméra, ou
    /// ticket falsifié.
    BadSignature,
    /// L'expiration est passée.
    Expired,
    /// L'expiration est trop lointaine (voir [`MAX_TICKET_TTL_SECS`]).
    TooLong,
}

/// Signe un ticket pour `camera`, valable jusqu'à `expires_at` (secondes
/// Unix).
pub fn issue(secret: &[u8], camera: &str, expires_at: i64) -> String {
    let signature = sign(secret, camera, expires_at).finalize().into_bytes();
    format!("{expires_at}.{}", to_hex(&signature))
}

/// Vérifie un ticket présenté à `camera`, à l'instant `now` (secondes Unix).
///
/// La comparaison de signature est à TEMPS CONSTANT (`verify_slice`) : une
/// comparaison ordinaire s'arrête au premier octet différent, et sa durée
/// renseigne sur la longueur du préfixe correct.
pub fn verify(secret: &[u8], camera: &str, ticket: &str, now: i64) -> Result<(), TicketError> {
    let (expires, signature) = ticket.split_once('.').ok_or(TicketError::Malformed)?;
    let expires_at: i64 = expires.parse().map_err(|_| TicketError::Malformed)?;
    let signature = from_hex(signature).ok_or(TicketError::Malformed)?;

    // La signature d'abord : un ticket forgé n'a pas à apprendre si son
    // expiration aurait convenu.
    sign(secret, camera, expires_at)
        .verify_slice(&signature)
        .map_err(|_| TicketError::BadSignature)?;

    if expires_at <= now {
        return Err(TicketError::Expired);
    }

    if expires_at - now > MAX_TICKET_TTL_SECS {
        return Err(TicketError::TooLong);
    }

    Ok(())
}

/// HMAC prêt à finaliser, sur le domaine, la caméra et l'expiration.
///
/// Les champs sont séparés par un octet nul, qui ne peut apparaître dans
/// aucun des deux : sans séparateur, la caméra `jardin1` à l'expiration `23`
/// et la caméra `jardin` à l'expiration `123` signeraient la même chaîne.
fn sign(secret: &[u8], camera: &str, expires_at: i64) -> HmacSha256 {
    // `new_from_slice` n'échoue jamais pour HMAC, qui accepte toute longueur
    // de clé.
    let mut mac = HmacSha256::new_from_slice(secret).expect("HMAC accepte toute longueur de clé");
    mac.update(DOMAIN);
    mac.update(&[0]);
    mac.update(camera.as_bytes());
    mac.update(&[0]);
    mac.update(expires_at.to_string().as_bytes());
    mac
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) || !text.is_ascii() {
        return None;
    }

    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
    const NOW: i64 = 1_800_000_000;

    #[test]
    fn a_fresh_ticket_is_accepted_by_its_camera() {
        let ticket = issue(SECRET, "jardin", NOW + TICKET_TTL_SECS);
        assert_eq!(verify(SECRET, "jardin", &ticket, NOW), Ok(()));
    }

    #[test]
    fn a_ticket_does_not_open_another_camera() {
        let ticket = issue(SECRET, "jardin", NOW + TICKET_TTL_SECS);
        assert_eq!(
            verify(SECRET, "garage", &ticket, NOW),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn a_ticket_signed_with_another_secret_is_rejected() {
        let ticket = issue(b"un tout autre secret, assez long", "jardin", NOW + 60);
        assert_eq!(
            verify(SECRET, "jardin", &ticket, NOW),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn an_expired_ticket_is_rejected() {
        let ticket = issue(SECRET, "jardin", NOW);
        assert_eq!(verify(SECRET, "jardin", &ticket, NOW), Err(TicketError::Expired));
    }

    #[test]
    fn extending_the_expiry_breaks_the_signature() {
        // Le cas d'attaque évident : recopier un ticket périmé en repoussant
        // son expiration.
        let ticket = issue(SECRET, "jardin", NOW - 10);
        let signature = ticket.split_once('.').unwrap().1;
        let forged = format!("{}.{signature}", NOW + 60);

        assert_eq!(
            verify(SECRET, "jardin", &forged, NOW),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn a_ticket_valid_for_too_long_is_rejected_even_if_well_signed() {
        let ticket = issue(SECRET, "jardin", NOW + MAX_TICKET_TTL_SECS + 1);
        assert_eq!(verify(SECRET, "jardin", &ticket, NOW), Err(TicketError::TooLong));
    }

    #[test]
    fn the_field_separator_prevents_ambiguous_signatures() {
        // Sans séparateur, « jardin1 » + « 23 » et « jardin » + « 123 »
        // signeraient la même chaîne.
        let ticket = issue(SECRET, "jardin1", 23);
        let signature = ticket.split_once('.').unwrap().1;

        assert_eq!(
            verify(SECRET, "jardin", &format!("123.{signature}"), 0),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn garbage_is_reported_as_malformed() {
        for ticket in ["", "abc", "12.zz", "x.00", "12.0"] {
            assert_eq!(
                verify(SECRET, "jardin", ticket, NOW),
                Err(TicketError::Malformed),
                "{ticket:?}"
            );
        }
    }

    #[test]
    fn an_empty_secret_disables_tickets_and_a_short_one_is_refused() {
        assert!(validate_secret("").is_ok());
        assert!(validate_secret("trop-court").is_err());
        assert!(validate_secret(std::str::from_utf8(SECRET).unwrap()).is_ok());
    }

    #[test]
    fn a_ticket_needs_no_url_encoding() {
        let ticket = issue(SECRET, "caméra du jardin", NOW + 60);
        assert!(ticket.chars().all(|c| c.is_ascii_hexdigit() || c == '.'));
    }
}
