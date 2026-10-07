//! Tickets d'accès aux caméras : comment le manager, et les pages qu'une
//! caméra sert, obtiennent un droit PRÉCIS sans jamais porter le jeton d'API.
//!
//! # Le problème
//!
//! Le jeton d'API d'une caméra donne TOUS les droits : couper la
//! surveillance, supprimer des enregistrements, regarder. Il n'a rien à faire
//! dans une page web servie par une autre machine, ni dans la base du
//! manager — ni même dans les pages que la caméra sert à quiconque atteint son
//! port, ce qui était le cas de `/live`, `/control` et `/play`.
//!
//! # La solution
//!
//! Un ticket est un droit signé, et il est :
//!
//! - **limité à une portée** ([`Scope`]) : regarder le direct, piloter la
//!   surveillance, OU lire UN clip. Un ticket de direct ne coupe rien, un
//!   ticket de clip n'ouvre que ce clip ;
//! - **lié à une caméra** : son nom entre dans la signature, un ticket émis
//!   pour le garage n'ouvre pas le jardin ;
//! - **daté** : il expire, et une caméra refuse un ticket qui prétend durer
//!   plus que ce que sa portée admet.
//!
//! Deux clés les signent, pour deux usages :
//!
//! - le SECRET PARTAGÉ entre le manager et les caméras (`[stream]
//!   ticket_secret` / `[server] stream_ticket_secret`) : le manager signe des
//!   tickets COURTS ([`TICKET_TTL_SECS`]), juste au moment où l'interface en a
//!   besoin ;
//! - la CLÉ DE SESSION d'une caméra, tirée au hasard à chaque démarrage et
//!   jamais transmise : quand une caméra sert une page sur présentation d'un
//!   ticket court, elle y injecte un ticket de même portée signé avec cette
//!   clé, valable le temps d'une consultation ([`SESSION_TTL_SECS`]). La page
//!   peut ainsi se reconnecter ou se déplacer dans un clip sans redemander
//!   quoi que ce soit au manager.
//!
//! La caméra vérifie les signatures elle-même : pas d'aller-retour, pas d'état
//! partagé.
//!
//! # Format
//!
//! `<expiration en secondes Unix>.<HMAC-SHA256 en hexadécimal>`, où le HMAC
//! porte sur [`DOMAIN`], le nom de la caméra, la portée et l'expiration.
//! Uniquement des chiffres, un point et de l'hexadécimal : le ticket passe tel
//! quel dans une chaîne de requête, sans encodage.
//!
//! Ni la caméra ni la portée n'y figurent : celui qui vérifie connaît l'une et
//! l'autre (la route appelée DIT la portée attendue), et les en retirer évite
//! d'avoir à les encoder pour l'URL.
//!
//! # Ce que cela suppose
//!
//! Des horloges à peu près à l'heure, des deux côtés. Un écart de plus de
//! [`TICKET_TTL_SECS`] fait refuser les tickets du manager — d'où un message de
//! journal qui le dit explicitement côté caméra, plutôt qu'un 401 muet.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// Séparation de domaine : la signature d'un ticket ne peut être confondue
/// avec aucun autre usage, présent ou futur, de la même clé.
const DOMAIN: &[u8] = b"foxguard-ticket-v1";

/// Durée de validité d'un ticket émis par le manager.
///
/// Deux minutes : le ticket ne sert qu'à OUVRIR une connexion ou une page.
/// L'interface en redemande un à chaque fois.
pub const TICKET_TTL_SECS: i64 = 120;

/// Validité maximale qu'une caméra accepte d'un ticket du manager, quelle que
/// soit l'expiration annoncée.
///
/// Un garde-fou contre un manager mal configuré (ou dont l'horloge avance) :
/// un ticket « valable un an » serait signé correctement, et serait pourtant
/// exactement ce que ce mécanisme cherche à éviter. La marge au-dessus de
/// [`TICKET_TTL_SECS`] absorbe un écart d'horloge raisonnable.
pub const MAX_TICKET_TTL_SECS: i64 = 600;

/// Durée de validité d'un ticket de SESSION, signé par la caméra pour une page
/// qu'elle sert.
///
/// Douze heures : de quoi laisser une mosaïque ou un clip ouverts une journée
/// sans que la page casse en silence. Ce ticket ne sort jamais de la page qui
/// l'a reçu, et sa portée reste celle du ticket qui l'a obtenu.
pub const SESSION_TTL_SECS: i64 = 12 * 3600;

/// Longueur minimale du secret partagé, en octets.
///
/// 32 caractères : ce que produit `openssl rand -hex 16`, et assez pour qu'un
/// secret ne se devine pas. Un secret plus court est refusé au chargement de
/// la configuration plutôt qu'accepté en silence.
pub const MIN_SECRET_LEN: usize = 32;

/// Ce qu'un ticket autorise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// Regarder le direct (`GET /live`, `GET /ws` en lecture seule).
    Stream,
    /// Piloter la surveillance (`GET /control`, `POST /api/monitoring`).
    Monitoring,
    /// Lire UN enregistrement (`GET /play/{fichier}`,
    /// `GET /recordings/{fichier}`).
    Clip(&'a str),
}

impl Scope<'_> {
    /// Représentation signée. Le nom de fichier d'un clip ne contient ni
    /// octet nul ni barre oblique (la caméra le valide avant), il ne peut donc
    /// pas déborder sur le champ suivant.
    fn write_to(&self, mac: &mut HmacSha256) {
        match self {
            Scope::Stream => mac.update(b"stream"),
            Scope::Monitoring => mac.update(b"monitoring"),
            Scope::Clip(file) => {
                mac.update(b"clip:");
                mac.update(file.as_bytes());
            }
        }
    }
}

/// Vérifie un secret lu dans une configuration, avant toute utilisation.
///
/// Vide est accepté : c'est le défaut, et il DÉSACTIVE les tickets du
/// manager — la caméra n'accepte alors que son jeton, et le manager ne propose
/// ni direct, ni clip, ni interrupteur. Trop court ne l'est pas : un secret
/// devinable vaudrait une caméra ouverte, et le dire au démarrage vaut mieux
/// que de le découvrir après.
///
/// Partagée par les deux configurations : un secret accepté d'un côté et
/// refusé de l'autre ferait une panne incompréhensible.
pub fn validate_secret(secret: &str) -> Result<(), String> {
    if !secret.is_empty() && secret.len() < MIN_SECRET_LEN {
        return Err(format!(
            "secret des tickets trop court ({} caractères, {MIN_SECRET_LEN} au \
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
    /// La signature ne correspond pas : autre clé, autre caméra, autre
    /// portée, ou ticket falsifié.
    BadSignature,
    /// L'expiration est passée.
    Expired,
    /// L'expiration est trop lointaine pour cette clé.
    TooLong,
}

/// Signe un ticket de portée `scope` pour `camera`, valable jusqu'à
/// `expires_at` (secondes Unix).
pub fn issue(key: &[u8], camera: &str, scope: Scope<'_>, expires_at: i64) -> String {
    let signature = sign(key, camera, scope, expires_at).finalize().into_bytes();
    format!("{expires_at}.{}", to_hex(&signature))
}

/// Vérifie un ticket présenté à `camera` pour `scope`, à l'instant `now`
/// (secondes Unix), en refusant toute expiration au-delà de `now + max_ttl`.
///
/// `max_ttl` dépend de la clé : [`MAX_TICKET_TTL_SECS`] pour le secret
/// partagé, [`SESSION_TTL_SECS`] pour la clé de session de la caméra.
///
/// La comparaison de signature est à TEMPS CONSTANT (`verify_slice`) : une
/// comparaison ordinaire s'arrête au premier octet différent, et sa durée
/// renseigne sur la longueur du préfixe correct.
pub fn verify(
    key: &[u8],
    camera: &str,
    scope: Scope<'_>,
    ticket: &str,
    now: i64,
    max_ttl: i64,
) -> Result<(), TicketError> {
    let (expires, signature) = ticket.split_once('.').ok_or(TicketError::Malformed)?;
    let expires_at: i64 = expires.parse().map_err(|_| TicketError::Malformed)?;
    let signature = from_hex(signature).ok_or(TicketError::Malformed)?;

    // La signature d'abord : un ticket forgé n'a pas à apprendre si son
    // expiration aurait convenu.
    sign(key, camera, scope, expires_at)
        .verify_slice(&signature)
        .map_err(|_| TicketError::BadSignature)?;

    if expires_at <= now {
        return Err(TicketError::Expired);
    }

    if expires_at - now > max_ttl {
        return Err(TicketError::TooLong);
    }

    Ok(())
}

/// HMAC prêt à finaliser, sur le domaine, la caméra, la portée et
/// l'expiration.
///
/// Les champs sont séparés par un octet nul, qui ne peut apparaître dans
/// aucun d'eux : sans séparateur, la caméra `jardin1` à l'expiration `23` et
/// la caméra `jardin` à l'expiration `123` signeraient la même chaîne.
fn sign(key: &[u8], camera: &str, scope: Scope<'_>, expires_at: i64) -> HmacSha256 {
    // `new_from_slice` n'échoue jamais pour HMAC, qui accepte toute longueur
    // de clé.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepte toute longueur de clé");
    mac.update(DOMAIN);
    mac.update(&[0]);
    mac.update(camera.as_bytes());
    mac.update(&[0]);
    scope.write_to(&mut mac);
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

    fn check(camera: &str, scope: Scope<'_>, ticket: &str) -> Result<(), TicketError> {
        verify(SECRET, camera, scope, ticket, NOW, MAX_TICKET_TTL_SECS)
    }

    #[test]
    fn a_fresh_ticket_is_accepted_for_its_camera_and_scope() {
        let ticket = issue(SECRET, "jardin", Scope::Stream, NOW + TICKET_TTL_SECS);
        assert_eq!(check("jardin", Scope::Stream, &ticket), Ok(()));
    }

    #[test]
    fn a_ticket_does_not_open_another_camera() {
        let ticket = issue(SECRET, "jardin", Scope::Stream, NOW + TICKET_TTL_SECS);
        assert_eq!(
            check("garage", Scope::Stream, &ticket),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn a_stream_ticket_does_not_grant_monitoring() {
        // Le cœur du découpage : regarder ne permet pas de couper.
        let ticket = issue(SECRET, "jardin", Scope::Stream, NOW + 60);
        assert_eq!(
            check("jardin", Scope::Monitoring, &ticket),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn a_clip_ticket_opens_only_its_own_clip() {
        let ticket = issue(SECRET, "jardin", Scope::Clip("rec_1.mp4"), NOW + 60);

        assert_eq!(check("jardin", Scope::Clip("rec_1.mp4"), &ticket), Ok(()));
        assert_eq!(
            check("jardin", Scope::Clip("rec_2.mp4"), &ticket),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn a_ticket_signed_with_another_key_is_rejected() {
        let ticket = issue(
            b"un tout autre secret, assez long",
            "jardin",
            Scope::Stream,
            NOW + 60,
        );
        assert_eq!(
            check("jardin", Scope::Stream, &ticket),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn an_expired_ticket_is_rejected() {
        let ticket = issue(SECRET, "jardin", Scope::Stream, NOW);
        assert_eq!(
            check("jardin", Scope::Stream, &ticket),
            Err(TicketError::Expired)
        );
    }

    #[test]
    fn extending_the_expiry_breaks_the_signature() {
        // Le cas d'attaque évident : recopier un ticket périmé en repoussant
        // son expiration.
        let ticket = issue(SECRET, "jardin", Scope::Stream, NOW - 10);
        let signature = ticket.split_once('.').unwrap().1;
        let forged = format!("{}.{signature}", NOW + 60);

        assert_eq!(
            check("jardin", Scope::Stream, &forged),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn a_ticket_valid_for_too_long_is_rejected_even_if_well_signed() {
        let ticket = issue(
            SECRET,
            "jardin",
            Scope::Stream,
            NOW + MAX_TICKET_TTL_SECS + 1,
        );
        assert_eq!(
            check("jardin", Scope::Stream, &ticket),
            Err(TicketError::TooLong)
        );
    }

    #[test]
    fn a_session_ticket_may_last_longer_with_its_own_limit() {
        let ticket = issue(SECRET, "jardin", Scope::Stream, NOW + SESSION_TTL_SECS);
        assert_eq!(
            verify(
                SECRET,
                "jardin",
                Scope::Stream,
                &ticket,
                NOW,
                SESSION_TTL_SECS
            ),
            Ok(())
        );
    }

    #[test]
    fn the_field_separator_prevents_ambiguous_signatures() {
        // Sans séparateur, « jardin1 » + « 23 » et « jardin » + « 123 »
        // signeraient la même chaîne.
        let ticket = issue(SECRET, "jardin1", Scope::Stream, 23);
        let signature = ticket.split_once('.').unwrap().1;

        assert_eq!(
            verify(
                SECRET,
                "jardin",
                Scope::Stream,
                &format!("123.{signature}"),
                0,
                600
            ),
            Err(TicketError::BadSignature)
        );
    }

    #[test]
    fn garbage_is_reported_as_malformed() {
        for ticket in ["", "abc", "12.zz", "x.00", "12.0"] {
            assert_eq!(
                check("jardin", Scope::Stream, ticket),
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
        let ticket = issue(SECRET, "caméra du jardin", Scope::Clip("é.mp4"), NOW + 60);
        assert!(ticket.chars().all(|c| c.is_ascii_hexdigit() || c == '.'));
    }
}
