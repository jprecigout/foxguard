//! Briques d'authentification communes à la caméra et au manager.
//!
//! Elles vivent ici, malgré le minimalisme de ce crate, parce que ce sont des
//! RÈGLES DE SÉCURITÉ que les deux composants doivent appliquer à
//! l'identique : une comparaison de secret faite en temps constant d'un côté
//! et pas de l'autre, ou une limite de tentatives plus laxiste sur l'un des
//! deux, serait une faille. Rien ici ne touche au réseau : de la logique pure,
//! sur la bibliothèque standard et `subtle` (déjà tiré par `hmac`).

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use subtle::ConstantTimeEq;

/// Compare deux secrets en TEMPS CONSTANT.
///
/// Une comparaison ordinaire (`==`) s'arrête au premier octet différent : sa
/// durée renseigne sur la longueur du préfixe correct, et un jeton se devine
/// alors caractère par caractère. Seule la LONGUEUR peut fuiter ici, ce qui
/// ne dit rien du contenu.
pub fn secure_eq(given: &str, expected: &str) -> bool {
    given.as_bytes().ct_eq(expected.as_bytes()).into()
}

/// Nombre d'échecs d'authentification admis par adresse et par fenêtre.
pub const MAX_FAILURES: u32 = 10;

/// Durée d'une fenêtre de comptage — et donc du blocage qui la suit.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// Nombre d'adresses suivies au-delà duquel les fenêtres expirées sont
/// purgées : la table ne doit pas pouvoir grossir sans fin sous un balayage
/// d'adresses.
const PRUNE_THRESHOLD: usize = 1024;

/// Limite les tentatives d'authentification par adresse IP.
///
/// Au-delà de [`MAX_FAILURES`] échecs dans une fenêtre de
/// [`FAILURE_WINDOW`], l'adresse est refusée d'office jusqu'à la fin de la
/// fenêtre. Un jeton de 32 caractères ne se devine pas par force brute de
/// toute façon ; ceci empêche surtout qu'on s'y essaie à plein débit, et
/// borne le coût des vérifications coûteuses (le hachage de mot de passe du
/// manager).
///
/// # Derrière un reverse-proxy
///
/// Toutes les requêtes arrivent alors de l'adresse du proxy : un attaquant
/// qui épuise le quota bloque tout le monde pendant une minute. C'est une
/// gêne passagère, pas une ouverture, et c'est le prix de ne pas faire
/// confiance à un `X-Forwarded-For` que n'importe qui peut écrire.
#[derive(Default)]
pub struct FailureThrottle {
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

impl FailureThrottle {
    pub fn new() -> Self {
        Self::default()
    }

    /// Vrai si `ip` a épuisé son quota d'échecs pour la fenêtre en cours.
    pub fn is_blocked(&self, ip: IpAddr, now: Instant) -> bool {
        let Ok(failures) = self.failures.lock() else {
            return false;
        };

        matches!(
            failures.get(&ip),
            Some(&(count, started)) if count >= MAX_FAILURES && now - started < FAILURE_WINDOW
        )
    }

    /// Compte un échec d'authentification de `ip`.
    pub fn record_failure(&self, ip: IpAddr, now: Instant) {
        let Ok(mut failures) = self.failures.lock() else {
            return;
        };

        if failures.len() >= PRUNE_THRESHOLD {
            failures.retain(|_, (_, started)| now - *started < FAILURE_WINDOW);
        }

        let entry = failures.entry(ip).or_insert((0, now));

        if now - entry.1 >= FAILURE_WINDOW {
            *entry = (0, now);
        }

        entry.0 = entry.0.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IP: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 7));
    const OTHER: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(192, 168, 1, 8));

    #[test]
    fn secrets_are_compared_exactly() {
        assert!(secure_eq("jeton", "jeton"));
        assert!(!secure_eq("jeton", "jetoN"));
        assert!(!secure_eq("jet", "jeton"));
        assert!(!secure_eq("", "jeton"));
    }

    #[test]
    fn an_address_is_blocked_after_too_many_failures() {
        let throttle = FailureThrottle::new();
        let now = Instant::now();

        for _ in 0..MAX_FAILURES - 1 {
            throttle.record_failure(IP, now);
        }
        assert!(!throttle.is_blocked(IP, now));

        throttle.record_failure(IP, now);
        assert!(throttle.is_blocked(IP, now));
    }

    #[test]
    fn blocking_one_address_spares_the_others() {
        let throttle = FailureThrottle::new();
        let now = Instant::now();

        for _ in 0..MAX_FAILURES {
            throttle.record_failure(IP, now);
        }

        assert!(!throttle.is_blocked(OTHER, now));
    }

    #[test]
    fn the_block_lifts_when_the_window_ends() {
        let throttle = FailureThrottle::new();
        let now = Instant::now();

        for _ in 0..MAX_FAILURES {
            throttle.record_failure(IP, now);
        }

        assert!(!throttle.is_blocked(IP, now + FAILURE_WINDOW));

        // Et la fenêtre suivante repart de zéro.
        throttle.record_failure(IP, now + FAILURE_WINDOW);
        assert!(!throttle.is_blocked(IP, now + FAILURE_WINDOW));
    }
}
