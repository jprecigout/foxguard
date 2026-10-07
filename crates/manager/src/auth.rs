//! Authentification de l'interface et de l'API du manager.
//!
//! # Pourquoi
//!
//! Le manager n'en avait aucune : quiconque atteignait son port voyait tout
//! l'historique des détections et leurs vignettes. Et depuis qu'il signe des
//! tickets (direct, clips, interrupteur — voir `foxguard_protocol::ticket`),
//! il aurait aussi ouvert à n'importe qui le direct de toutes les caméras et
//! le pilotage de leur surveillance. C'était le trou le plus large du système.
//!
//! # Comment
//!
//! HTTP Basic, vérifié contre un hachage Argon2id (`[auth] password_hash`) :
//!
//! - le navigateur affiche sa propre invite, puis rejoint les identifiants à
//!   TOUTES les requêtes de cette origine — appels d'API, vignettes, cadres —
//!   sans une ligne de code côté interface ;
//! - le mot de passe n'est jamais écrit en clair dans la configuration.
//!
//! HTTP Basic transmet le mot de passe à chaque requête : sans TLS, il circule
//! en clair sur le réseau. Voir le service `caddy` de
//! `deploy/server/compose.yml`.
//!
//! # Le coût d'Argon2
//!
//! Vérifier un hachage Argon2 coûte plusieurs dizaines de millisecondes —
//! c'est voulu, c'est ce qui rend une attaque par dictionnaire coûteuse. Mais
//! une journée de timeline, c'est des centaines de vignettes, chacune avec
//! ses identifiants. Une vérification réussie est donc MÉMORISÉE quelques
//! minutes, sous la forme d'une empreinte SHA-256 de l'en-tête : la mémoire
//! du manager ne contient pas le mot de passe, seulement de quoi reconnaître
//! un en-tête déjà vérifié.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use argon2::password_hash::{PasswordHash, PasswordHashString, PasswordHasher, SaltString};
use argon2::{Argon2, PasswordVerifier};
use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use foxguard_protocol::auth::{FailureThrottle, secure_eq};
use sha2::{Digest, Sha256};

/// Durée pendant laquelle une vérification réussie dispense d'en refaire une.
const VERIFIED_TTL: Duration = Duration::from_secs(10 * 60);

/// Taille au-delà de laquelle les entrées expirées du cache sont purgées.
const CACHE_PRUNE_THRESHOLD: usize = 256;

/// Routes servies sans authentification.
///
/// La seule : la sonde de disponibilité, qu'un superviseur (`docker compose`,
/// un équilibreur) interroge sans identifiants, et qui ne révèle rien.
const PUBLIC_PATHS: &[&str] = &["/api/health"];

/// Vérifie les identifiants HTTP Basic des requêtes.
pub struct Authenticator {
    username: String,
    password_hash: PasswordHashString,
    /// Empreintes d'en-têtes `Authorization` déjà vérifiés, et leur échéance.
    verified: Mutex<HashMap<[u8; 32], Instant>>,
    failures: FailureThrottle,
}

impl Authenticator {
    /// `password_hash` est une chaîne PHC Argon2 (`$argon2id$…`), telle que
    /// la produit `foxguard-manager hash-password`.
    pub fn new(username: &str, password_hash: &str) -> anyhow::Result<Self> {
        let password_hash = PasswordHash::new(password_hash)
            .map_err(|e| anyhow::anyhow!("`[auth] password_hash` illisible : {e}"))?
            .serialize();

        Ok(Self {
            username: username.to_string(),
            password_hash,
            verified: Mutex::new(HashMap::new()),
            failures: FailureThrottle::new(),
        })
    }

    /// Vrai si `authorization` (la valeur de l'en-tête) porte les bons
    /// identifiants.
    async fn check(self: &Arc<Self>, authorization: &str) -> bool {
        let fingerprint: [u8; 32] = Sha256::digest(authorization.as_bytes()).into();
        let now = Instant::now();

        if let Ok(verified) = self.verified.lock()
            && verified
                .get(&fingerprint)
                .is_some_and(|expiry| *expiry > now)
        {
            return true;
        }

        let Some((username, password)) = decode_basic(authorization) else {
            return false;
        };

        // Argon2 est volontairement lent : hors de la boucle asynchrone, pour
        // ne pas bloquer les autres requêtes pendant la vérification.
        let this = Arc::clone(self);
        let valid = tokio::task::spawn_blocking(move || {
            // Les DEUX vérifications, toujours : s'arrêter au nom d'utilisateur
            // dirait, par la durée de la réponse, qu'il est faux.
            let username_ok = secure_eq(&username, &this.username);
            let password_ok = Argon2::default()
                .verify_password(password.as_bytes(), &this.password_hash.password_hash())
                .is_ok();
            username_ok && password_ok
        })
        .await
        .unwrap_or(false);

        if valid && let Ok(mut verified) = self.verified.lock() {
            if verified.len() >= CACHE_PRUNE_THRESHOLD {
                verified.retain(|_, expiry| *expiry > now);
            }
            verified.insert(fingerprint, now + VERIFIED_TTL);
        }

        valid
    }
}

/// Décode `Basic <base64(utilisateur:mot de passe)>`.
fn decode_basic(authorization: &str) -> Option<(String, String)> {
    let encoded = authorization.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .ok()?;
    let credentials = String::from_utf8(decoded).ok()?;
    let (username, password) = credentials.split_once(':')?;
    Some((username.to_string(), password.to_string()))
}

/// Hache un mot de passe pour `[auth] password_hash` (Argon2id, sel
/// aléatoire, paramètres par défaut de la bibliothèque).
pub fn hash_password(password: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut argon2::password_hash::rand_core::OsRng);

    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| anyhow::anyhow!("hachage impossible : {e}"))
}

/// Réponse 401 qui déclenche l'invite de connexion du navigateur.
fn challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Basic realm="FoxGuard", charset="UTF-8""#),
        )],
        "Authentification requise",
    )
        .into_response()
}

/// Exige des identifiants valides sur toutes les routes, sauf
/// [`PUBLIC_PATHS`] — y compris le bundle de l'interface.
///
/// `None` : authentification désactivée (`[auth] disabled = true`).
pub async fn require_auth(
    State(authenticator): State<Option<Arc<Authenticator>>>,
    request: Request,
    next: Next,
) -> Response {
    let Some(authenticator) = authenticator else {
        return next.run(request).await;
    };

    if PUBLIC_PATHS.contains(&request.uri().path()) {
        return next.run(request).await;
    }

    let ip = client_ip(&request);

    if let Some(ip) = ip
        && authenticator.failures.is_blocked(ip, Instant::now())
    {
        tracing::warn!("⛔ {ip} bloquée : trop d'échecs d'authentification.");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Trop de tentatives d'authentification. Réessayez dans une minute.",
        )
            .into_response();
    }

    let authorization = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);

    match authorization {
        Some(authorization) if authenticator.check(&authorization).await => next.run(request).await,
        // Un en-tête ABSENT n'est pas un échec : c'est la toute première
        // requête d'un navigateur, avant qu'il n'affiche son invite. Le
        // compter ferait bloquer quelqu'un qui recharge la page.
        None => challenge(),
        Some(_) => {
            if let Some(ip) = ip {
                authenticator.failures.record_failure(ip, Instant::now());
            }
            tracing::warn!("⚠️ Identifiants refusés ({ip:?}).");
            challenge()
        }
    }
}

/// Adresse du client, si le serveur la fournit (absente des tests qui
/// appellent le routeur sans socket réseau).
fn client_ip(request: &Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
}

/// Politique de sécurité du contenu de l'interface.
///
/// - `connect-src` : l'API (`'self'`) et les WebSocket des caméras, dont
///   l'adresse n'est connue qu'à l'exécution ;
/// - `frame-src` : les pages des caméras (clips, interrupteur), même raison ;
/// - `style-src 'unsafe-inline'` : React pose des attributs `style` (position
///   des marques de la timeline) ;
/// - `frame-ancestors 'none'` : l'interface ne s'affiche dans aucun cadre.
const CONTENT_SECURITY_POLICY: &str = "default-src 'self'; img-src 'self' data:; \
     style-src 'self' 'unsafe-inline'; connect-src 'self' ws: wss:; \
     frame-src http: https:; frame-ancestors 'none'; base-uri 'none'; form-action 'none'";

/// En-têtes de sécurité, posés sur toutes les réponses.
pub async fn security_headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Les redirections vers les caméras portent des tickets : aucune URL ne
    // doit fuiter vers un tiers par l'en-tête `Referer`.
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(CONTENT_SECURITY_POLICY),
    );

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basic(username: &str, password: &str) -> String {
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        format!("Basic {encoded}")
    }

    fn authenticator() -> Arc<Authenticator> {
        let hash = hash_password("correct horse").expect("hachage");
        Arc::new(Authenticator::new("admin", &hash).expect("authentificateur"))
    }

    #[tokio::test]
    async fn the_right_credentials_are_accepted() {
        assert!(
            authenticator()
                .check(&basic("admin", "correct horse"))
                .await
        );
    }

    #[tokio::test]
    async fn a_wrong_password_or_username_is_refused() {
        let auth = authenticator();
        assert!(!auth.check(&basic("admin", "battery staple")).await);
        assert!(!auth.check(&basic("root", "correct horse")).await);
        assert!(!auth.check("Bearer quelque-chose").await);
    }

    #[tokio::test]
    async fn a_verified_header_is_remembered() {
        let auth = authenticator();
        let header = basic("admin", "correct horse");

        assert!(auth.check(&header).await);
        assert_eq!(auth.verified.lock().unwrap().len(), 1);
        assert!(auth.check(&header).await);
    }

    #[test]
    fn an_unreadable_hash_is_reported_at_startup() {
        assert!(Authenticator::new("admin", "pas-un-hachage").is_err());
    }

    #[test]
    fn a_password_containing_a_colon_is_decoded_whole() {
        assert_eq!(
            decode_basic(&basic("admin", "a:b")),
            Some(("admin".to_string(), "a:b".to_string()))
        );
    }
}
