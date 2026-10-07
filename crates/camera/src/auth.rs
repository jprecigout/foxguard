//! Authentification du serveur HTTP de la caméra.
//!
//! # Trois façons d'entrer, trois niveaux de droits
//!
//! | Présenté | Droits | Par qui |
//! | --- | --- | --- |
//! | le jeton d'API (`?token=`, ou mot de passe HTTP Basic sur `GET /`) | TOUS | le propriétaire de la caméra |
//! | un ticket du MANAGER (`?ticket=`, secret partagé) | une portée, deux minutes | l'interface du manager |
//! | un ticket de SESSION (`?ticket=`, clé de la caméra) | une portée, quelques heures | une page servie par la caméra |
//!
//! Le jeton n'est plus jamais injecté dans une page servie sans
//! authentification. Il l'était dans `/live`, `/control` et `/play` : qui
//! atteignait le port HTTP en extrayait un jeton maître, capable de supprimer
//! les archives. Ces pages exigent désormais un ticket (ou le jeton), et
//! reçoivent en retour un ticket de session de MÊME portée — jamais plus.
//!
//! Voir `foxguard_protocol::ticket` pour le format et les garanties d'un
//! ticket.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use axum::{
    extract::{ConnectInfo, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use base64::Engine;
use foxguard_protocol::auth::secure_eq;
use foxguard_protocol::ticket::{self, Scope, TicketError};
use serde::Deserialize;
use tracing::warn;

use crate::capture::SharedState;

/// Paramètres d'authentification d'une requête (`?token=...` ou
/// `?ticket=...`).
#[derive(Deserialize, Default)]
pub struct AuthQuery {
    pub token: Option<String>,
    /// Ticket du manager, ou de session (voir le tableau en tête de module).
    pub ticket: Option<String>,
}

/// Comment une requête a été authentifiée.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Par le jeton d'API : tous les droits.
    Token,
    /// Par un ticket : la seule portée demandée.
    Ticket,
}

/// Clé de session d'une caméra : signe les tickets qu'elle injecte dans ses
/// propres pages.
///
/// Tirée au hasard à chaque démarrage et jamais écrite nulle part : un
/// redémarrage invalide toutes les sessions, ce qui est exactement ce qu'on
/// attend d'un redémarrage après une fuite.
pub struct SessionKey(pub [u8; 32]);

impl SessionKey {
    /// Clé aléatoire, lue dans `/dev/urandom` — la caméra ne tourne que sous
    /// Linux (V4L2), et cela évite une dépendance de plus à la compilation
    /// croisée ARM64.
    pub fn random() -> std::io::Result<Self> {
        use std::io::Read;

        let mut key = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut key)?;
        Ok(Self(key))
    }
}

/// Vrai si la requête porte le jeton d'API de cette caméra.
///
/// Comparaison en TEMPS CONSTANT (voir `foxguard_protocol::auth::secure_eq`).
pub fn has_token(auth: &AuthQuery, state: &SharedState) -> bool {
    auth.token
        .as_deref()
        .is_some_and(|token| secure_eq(token, &state.api_token))
}

/// Autorise une requête pour `scope`, ou `None` si elle est refusée.
///
/// Le jeton d'abord, puis un ticket de session, puis un ticket du manager.
/// Chaque refus de ticket est journalisé avec sa CAUSE : un ticket expiré est
/// presque toujours une horloge déréglée sur la caméra ou le manager, et un
/// 401 sans explication laisserait chercher longtemps.
pub fn authorize(auth: &AuthQuery, state: &SharedState, scope: Scope<'_>) -> Option<Access> {
    if has_token(auth, state) {
        return Some(Access::Token);
    }

    let ticket = auth.ticket.as_deref()?;
    let now = chrono::Utc::now().timestamp();
    let camera = &state.camera_name;

    // Ticket de session : signé par cette caméra, pour une de ses pages.
    if ticket::verify(
        &state.session_key.0,
        camera,
        scope,
        ticket,
        now,
        ticket::SESSION_TTL_SECS,
    )
    .is_ok()
    {
        return Some(Access::Ticket);
    }

    let Some(secret) = state.stream_ticket_secret.as_deref() else {
        warn!(
            "⚠️ Ticket refusé ({scope:?}) : `[server] stream_ticket_secret` n'est pas \
             configuré sur cette caméra, seuls ses propres tickets de session sont acceptés."
        );
        return None;
    };

    match ticket::verify(
        secret.as_bytes(),
        camera,
        scope,
        ticket,
        now,
        ticket::MAX_TICKET_TTL_SECS,
    ) {
        Ok(()) => Some(Access::Ticket),
        Err(TicketError::Expired) => {
            warn!(
                "⚠️ Ticket expiré ({scope:?}) : vérifiez que les horloges de la caméra et du \
                 manager sont synchronisées (NTP)."
            );
            None
        }
        Err(e) => {
            warn!("⚠️ Ticket refusé ({scope:?}) : {e:?}.");
            None
        }
    }
}

/// Chaîne de requête d'authentification à injecter dans une page servie à
/// une requête autorisée par `access`.
///
/// Jeton présenté → le jeton (le propriétaire l'a déjà, la page n'apprend
/// rien). Ticket présenté → un ticket de SESSION de même portée : la page
/// peut se reconnecter ou se déplacer dans un clip pendant des heures, mais
/// n'obtient jamais plus que ce qu'on lui a accordé.
pub fn page_auth_query(access: Access, state: &SharedState, scope: Scope<'_>) -> String {
    match access {
        Access::Token => format!("token={}", encode_query_value(&state.api_token)),
        Access::Ticket => {
            let expires_at = chrono::Utc::now().timestamp() + ticket::SESSION_TTL_SECS;
            let session =
                ticket::issue(&state.session_key.0, &state.camera_name, scope, expires_at);
            format!("ticket={session}")
        }
    }
}

/// Encode une valeur pour une chaîne de requête. Le jeton est libre
/// (`[server] api_token`) : un `&` ou un `#` y couperait l'URL en deux.
fn encode_query_value(value: &str) -> String {
    value
        .bytes()
        .map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                (byte as char).to_string()
            }
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

/// Vrai si l'en-tête `Authorization: Basic …` porte le jeton d'API comme mot
/// de passe (le nom d'utilisateur est indifférent).
///
/// C'est ce qui protège l'interface complète (`GET /`) : elle reçoit le jeton
/// pour piloter la caméra, et ne doit donc plus être servie à quiconque
/// atteint le port. Le navigateur affiche sa propre invite de connexion, sans
/// une ligne de code côté page.
pub fn has_basic_token(headers: &HeaderMap, state: &SharedState) -> bool {
    let Some(credentials) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Basic "))
        .and_then(|encoded| {
            base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .ok()
        })
        .and_then(|decoded| String::from_utf8(decoded).ok())
    else {
        return false;
    };

    credentials
        .split_once(':')
        .is_some_and(|(_, password)| secure_eq(password, &state.api_token))
}

/// Réponse 401 qui déclenche l'invite de connexion du navigateur.
pub fn basic_challenge() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static(r#"Basic realm="FoxGuard (camera)", charset="UTF-8""#),
        )],
        "Authentification requise : le mot de passe est le jeton `[server] api_token`.",
    )
        .into_response()
}

/// Adresse du client, si le serveur la fournit.
///
/// Absente des tests qui appellent le routeur sans socket réseau
/// (`oneshot`) : la limitation est alors simplement inactive.
fn client_ip(request: &Request) -> Option<IpAddr> {
    request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())
}

/// Limite les échecs d'authentification par adresse (voir
/// `foxguard_protocol::auth::FailureThrottle`).
///
/// En intergiciel plutôt que dans chaque route : toute réponse 401 compte,
/// d'où qu'elle vienne. Une route ajoutée plus tard est donc couverte sans
/// qu'on ait à y penser.
pub async fn throttle_failures(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    let ip = client_ip(&request);

    if let Some(ip) = ip
        && state.auth_failures.is_blocked(ip, Instant::now())
    {
        warn!("⛔ {ip} bloquée : trop d'échecs d'authentification.");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            "Trop de tentatives d'authentification. Réessayez dans une minute.",
        )
            .into_response();
    }

    let response = next.run(request).await;

    if response.status() == StatusCode::UNAUTHORIZED
        && let Some(ip) = ip
    {
        state.auth_failures.record_failure(ip, Instant::now());
    }

    response
}

/// En-têtes de sécurité, posés sur toutes les réponses.
///
/// - `nosniff` : un fichier servi n'est jamais réinterprété comme un script ;
/// - `no-referrer` : les URL de ces pages portent des tickets, qu'un lien
///   suivi ne doit pas transmettre au site suivant ;
/// - `frame-ancestors` : l'interface complète (`/`), qui porte le jeton, ne
///   s'affiche dans AUCUN cadre — elle serait sinon exposée au détournement
///   de clic. Les pages ouvertes par le manager s'affichent dans un cadre :
///   limité à l'origine du manager si elle est déclarée
///   (`[server] manager_origin`), et protégées dans tous les cas par le
///   ticket qu'elles exigent.
pub async fn security_headers(
    State(state): State<Arc<SharedState>>,
    request: Request,
    next: Next,
) -> Response {
    let is_full_interface = request.uri().path() == "/";
    let mut response = next.run(request).await;
    let headers = response.headers_mut();

    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );

    let frame_ancestors = if is_full_interface {
        Some("frame-ancestors 'none'".to_string())
    } else {
        state
            .manager_origin
            .as_deref()
            .map(|origin| format!("frame-ancestors {origin}"))
    };

    if let Some(value) = frame_ancestors.and_then(|value| HeaderValue::from_str(&value).ok()) {
        headers.insert(header::CONTENT_SECURITY_POLICY, value);
    }

    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::H264Stream;

    const SECRET: &str = "0123456789abcdef0123456789abcdef";

    fn state(stream_ticket_secret: Option<&str>) -> SharedState {
        let mut state = SharedState::new("jeton", "jardin", Arc::new(H264Stream::new()), "");
        state.stream_ticket_secret = stream_ticket_secret.map(str::to_string);
        state
    }

    fn auth(token: Option<&str>, ticket: Option<String>) -> AuthQuery {
        AuthQuery {
            token: token.map(str::to_string),
            ticket,
        }
    }

    fn manager_ticket(scope: Scope<'_>) -> String {
        let expires_at = chrono::Utc::now().timestamp() + ticket::TICKET_TTL_SECS;
        ticket::issue(SECRET.as_bytes(), "jardin", scope, expires_at)
    }

    #[test]
    fn the_api_token_grants_everything() {
        let state = state(Some(SECRET));

        for scope in [Scope::Stream, Scope::Monitoring, Scope::Clip("a.mp4")] {
            assert_eq!(
                authorize(&auth(Some("jeton"), None), &state, scope),
                Some(Access::Token)
            );
        }
    }

    #[test]
    fn a_manager_ticket_grants_its_scope_only() {
        let state = state(Some(SECRET));
        let stream = auth(None, Some(manager_ticket(Scope::Stream)));

        assert_eq!(
            authorize(&stream, &state, Scope::Stream),
            Some(Access::Ticket)
        );
        assert_eq!(authorize(&stream, &state, Scope::Monitoring), None);
    }

    #[test]
    fn manager_tickets_are_refused_without_a_shared_secret() {
        let state = state(None);
        let stream = auth(None, Some(manager_ticket(Scope::Stream)));

        assert_eq!(authorize(&stream, &state, Scope::Stream), None);
    }

    #[test]
    fn a_page_opened_with_a_ticket_receives_a_session_ticket_of_the_same_scope() {
        // Sans secret partagé : le ticket de session ne dépend que de la clé
        // de la caméra.
        let state = state(None);
        let query = page_auth_query(Access::Ticket, &state, Scope::Monitoring);
        let session = query
            .strip_prefix("ticket=")
            .expect("un ticket, pas le jeton");

        let presented = auth(None, Some(session.to_string()));
        assert_eq!(
            authorize(&presented, &state, Scope::Monitoring),
            Some(Access::Ticket)
        );
        assert_eq!(authorize(&presented, &state, Scope::Stream), None);
    }

    #[test]
    fn the_token_is_never_injected_for_a_ticket() {
        let state = state(Some(SECRET));
        let query = page_auth_query(Access::Ticket, &state, Scope::Stream);

        assert!(!query.contains("jeton"));
    }

    #[test]
    fn basic_auth_takes_the_token_as_password() {
        let state = state(None);
        let mut headers = HeaderMap::new();
        let encoded = base64::engine::general_purpose::STANDARD.encode("admin:jeton");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {encoded}")).unwrap(),
        );
        assert!(has_basic_token(&headers, &state));

        let wrong = base64::engine::general_purpose::STANDARD.encode("admin:autre");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_str(&format!("Basic {wrong}")).unwrap(),
        );
        assert!(!has_basic_token(&headers, &state));
    }

    #[test]
    fn a_token_with_special_characters_survives_the_query_string() {
        assert_eq!(encode_query_value("a&b#c d"), "a%26b%23c%20d");
    }
}
