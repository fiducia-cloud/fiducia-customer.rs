//! Customer session authentication for the `/api/customer/*` surface.
//!
//! Before this module these routes were unauthenticated: anyone on the internet
//! could mint a live API key (and receive the plaintext secret). They are now
//! gated on a verified Supabase session and scoped to the caller's org.
//!
//! Verification is delegated to **fiducia-auth** (`GET /v1/me`) — the one place
//! that verifies Supabase JWTs — rather than re-implementing JWKS crypto here.
//! The caller's `Authorization: Bearer <supabase jwt>` is forwarded; a 200
//! yields `{ user: { user_id, email, orgs } }`. Fail closed: no auth service
//! configured → 503; missing/invalid token → 401; auth unreachable → 503.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

/// Ceiling on the `fiducia-auth` round trip. See `http()` for why this cannot
/// be left to the server-wide request timeout.
const AUTH_UPSTREAM_TIMEOUT_SECS: u64 = 10;

const fn customer_session_cookie_name(release_hardened: bool) -> &'static str {
    if release_hardened {
        "__Host-fiducia_customer_session"
    } else {
        "fiducia_customer_session"
    }
}

const fn customer_login_csrf_cookie_name(release_hardened: bool) -> &'static str {
    if release_hardened {
        "__Host-fiducia_customer_login_csrf"
    } else {
        "fiducia_customer_login_csrf"
    }
}

/// Release cookies use the browser-enforced `__Host-` prefix, preventing a
/// sibling subdomain from planting a colliding Domain cookie. Debug builds use
/// unprefixed names so explicitly enabled loopback HTTP remains usable.
pub const CUSTOMER_SESSION_COOKIE: &str = customer_session_cookie_name(!cfg!(debug_assertions));
pub const CUSTOMER_LOGIN_CSRF_COOKIE: &str =
    customer_login_csrf_cookie_name(!cfg!(debug_assertions));

/// A verified customer session — who is calling and which orgs they belong to.
/// `user_id`/`email` are carried for audit/attribution; not every handler reads
/// them yet.
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct CustomerCtx {
    pub user_id: String,
    pub email: Option<String>,
    /// Orgs the user belongs to (admin-controlled claims; see fiducia-auth).
    pub orgs: Vec<String>,
    /// The verified Supabase Authenticator Assurance Level forwarded by
    /// fiducia-auth. Missing values are deliberately single-factor so a legacy
    /// auth response can never accidentally bypass MFA enforcement.
    #[serde(default = "default_assurance_level")]
    pub aal: String,
    /// Opaque request-CSRF HMAC input. Never render or log this value.
    #[serde(skip)]
    pub(crate) credential_binding: String,
    #[serde(skip)]
    pub(crate) cookie_authenticated: bool,
}

impl CustomerCtx {
    pub fn csrf_binding(&self) -> &str {
        &self.credential_binding
    }

    pub fn is_browser_session(&self) -> bool {
        self.cookie_authenticated || self.credential_binding.starts_with("development\0")
    }

    pub fn is_aal2(&self) -> bool {
        self.aal == "aal2"
    }
}

fn default_assurance_level() -> String {
    "aal1".to_string()
}

/// How a request is authenticated. Production verifies via fiducia-auth; tests
/// inject a fixed context; an unconfigured deployment denies (fail closed).
/// `Static` is available only to tests and explicitly opted-in debug E2E runs.
#[derive(Clone)]
#[allow(dead_code)]
pub enum Authenticator {
    /// Verify the Bearer session via fiducia-auth `GET {url}/v1/me`.
    AuthService(String),
    /// No customer-auth backend configured — deny every customer data route.
    Deny,
    /// Test-only fixed context.
    Static(Arc<CustomerCtx>),
}

/// Shared client for the `fiducia-auth` hop.
///
/// The timeout is load-bearing, not hygiene: this call runs inside
/// `customer_mfa_assurance_gate`, which is layered OUTSIDE `TimeoutLayer`, so
/// the server-wide `REQUEST_TIMEOUT_SECS` never applies to it. Without a client
/// timeout a blackholed `fiducia-auth` pins every authenticated request
/// indefinitely and exhausts the connection pool. Matches the 10s used by every
/// other upstream client in this tree (`supabase_auth.rs`, `admin/upstream.rs`).
fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(AUTH_UPSTREAM_TIMEOUT_SECS))
            .build()
            .expect("customer auth HTTP client must build")
    })
}

fn deny(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": code }))).into_response()
}

impl Authenticator {
    /// `FIDUCIA_AUTH_URL` selects the auth service; unset → fail closed (`Deny`).
    pub fn from_env() -> Self {
        // Browser E2E boots the real debug backend without a Supabase stack. Keep
        // that path explicit and impossible in release binaries so production
        // remains fail-closed even if this variable is accidentally present.
        if cfg!(debug_assertions)
            && std::env::var("FIDUCIA_E2E_STATIC_CUSTOMER_AUTH").as_deref() == Ok("1")
        {
            return Authenticator::Static(Arc::new(CustomerCtx {
                user_id: "fiducia-e2e-customer".to_string(),
                email: Some("customer-e2e@fiducia.invalid".to_string()),
                orgs: vec!["00000000-0000-4000-8000-000000000001".to_string()],
                aal: "aal2".to_string(),
                credential_binding: "development\0fiducia-e2e-customer".to_string(),
                cookie_authenticated: false,
            }));
        }
        match std::env::var("FIDUCIA_AUTH_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
        {
            Some(url) => Authenticator::AuthService(url.trim_end_matches('/').to_string()),
            None => Authenticator::Deny,
        }
    }

    /// Verify the request and return the caller's context, or a ready `Response`
    /// to short-circuit the handler with.
    pub async fn authenticate(&self, headers: &HeaderMap) -> Result<CustomerCtx, Response> {
        match self {
            Authenticator::Static(ctx) => Ok((**ctx).clone()),
            Authenticator::Deny => Err(deny(
                StatusCode::SERVICE_UNAVAILABLE,
                "customer_auth_not_configured",
            )),
            Authenticator::AuthService(url) => {
                let credential = match presented_credential(headers) {
                    CredentialSelection::Valid(credential) => credential,
                    CredentialSelection::Missing => {
                        return Err(deny(StatusCode::UNAUTHORIZED, "missing_customer_session"))
                    }
                    CredentialSelection::Invalid => {
                        return Err(deny(StatusCode::UNAUTHORIZED, "invalid_customer_session"))
                    }
                };
                let resp = http()
                    .get(format!("{url}/v1/me"))
                    .bearer_auth(&credential.token)
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r
                            .json()
                            .await
                            .map_err(|_| deny(StatusCode::BAD_GATEWAY, "auth_bad_response"))?;
                        let mut ctx: CustomerCtx = serde_json::from_value(
                            body.get("user").cloned().unwrap_or(serde_json::Value::Null),
                        )
                        .map_err(|_| deny(StatusCode::BAD_GATEWAY, "auth_bad_response"))?;
                        if ctx.user_id.trim().is_empty() {
                            return Err(deny(StatusCode::BAD_GATEWAY, "auth_bad_response"));
                        }
                        if ctx.orgs.is_empty() {
                            return Err(deny(StatusCode::FORBIDDEN, "no_org_membership"));
                        }
                        let credential_kind = if credential.cookie_authenticated {
                            "cookie"
                        } else {
                            "authorization"
                        };
                        ctx.credential_binding = format!("{credential_kind}\0{}", credential.token);
                        ctx.cookie_authenticated = credential.cookie_authenticated;
                        Ok(ctx)
                    }
                    Ok(r)
                        if matches!(
                            r.status(),
                            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
                        ) =>
                    {
                        Err(deny(StatusCode::UNAUTHORIZED, "invalid_or_expired_session"))
                    }
                    Ok(_) => Err(deny(StatusCode::SERVICE_UNAVAILABLE, "auth_unavailable")),
                    Err(_) => Err(deny(StatusCode::SERVICE_UNAVAILABLE, "auth_unreachable")),
                }
            }
        }
    }
}

struct PresentedCredential {
    token: String,
    cookie_authenticated: bool,
}

enum CredentialSelection {
    Missing,
    Invalid,
    Valid(PresentedCredential),
}

/// Explicit Authorization always wins and never downgrades to an ambient
/// cookie. Duplicate/malformed bearer headers and duplicate canonical cookies
/// are invalid credentials, not an invitation to try another source.
fn presented_credential(headers: &HeaderMap) -> CredentialSelection {
    if headers.contains_key(AUTHORIZATION) {
        return match authorization_token(headers) {
            Some(token) => CredentialSelection::Valid(PresentedCredential {
                token,
                cookie_authenticated: false,
            }),
            None => CredentialSelection::Invalid,
        };
    }

    if cookie_name_present(headers, CUSTOMER_SESSION_COOKIE) {
        return match cookie_value(headers, CUSTOMER_SESSION_COOKIE) {
            Some(token) => CredentialSelection::Valid(PresentedCredential {
                token,
                cookie_authenticated: true,
            }),
            None => CredentialSelection::Invalid,
        };
    }

    CredentialSelection::Missing
}

pub fn bearer_token(headers: &HeaderMap) -> Option<String> {
    match presented_credential(headers) {
        CredentialSelection::Valid(credential) => Some(credential.token),
        CredentialSelection::Missing | CredentialSelection::Invalid => None,
    }
}

pub(crate) fn cookie_value(headers: &HeaderMap, expected_name: &str) -> Option<String> {
    let mut found = None;
    for value in headers.get_all("cookie") {
        let Ok(value) = value.to_str() else {
            continue;
        };
        for part in value.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name == expected_name && !value.trim().is_empty() {
                if found.is_some() {
                    return None;
                }
                found = Some(value.trim().to_string());
            }
        }
    }
    found
}

fn cookie_name_present(headers: &HeaderMap, expected_name: &str) -> bool {
    headers.get_all("cookie").iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value.split(';').any(|part| {
                part.trim()
                    .split_once('=')
                    .is_some_and(|(name, _)| name == expected_name)
            })
        })
    })
}

fn authorization_token(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?.to_str().ok()?;
    if values.next().is_some() {
        return None;
    }
    value
        .strip_prefix("Bearer ")
        .filter(|token| !token.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The portal must FAIL CLOSED when its identity dependency is broken:
    /// unconfigured auth denies with 503, and an unreachable auth service
    /// denies with 503 — a presented credential is never trusted, and no
    /// unauthenticated fall-through exists on either path.
    #[tokio::test]
    async fn identity_outage_denies_instead_of_falling_through() {
        // Unconfigured deployment: every customer route is refused.
        let deny_all = Authenticator::Deny;
        let denied = deny_all
            .authenticate(&HeaderMap::new())
            .await
            .expect_err("unconfigured auth must deny");
        assert_eq!(denied.status(), StatusCode::SERVICE_UNAVAILABLE);

        // Configured but DOWN: a well-formed session credential still denies.
        // Bind-then-drop a listener so the port refuses connections.
        let dead = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_url = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        let unreachable = Authenticator::AuthService(dead_url);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer plausible.jwt".parse().unwrap());
        let refused = unreachable
            .authenticate(&headers)
            .await
            .expect_err("an unreachable identity provider must deny, never trust");
        assert_eq!(refused.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[test]
    fn customer_cookie_is_isolated_from_admin_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "cookie",
            format!("fiducia_admin_session=admin.jwt; {CUSTOMER_SESSION_COOKIE}=customer.jwt")
                .parse()
                .unwrap(),
        );
        assert_eq!(bearer_token(&headers).as_deref(), Some("customer.jwt"));

        headers.insert("cookie", "fiducia_admin_session=admin.jwt".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn explicit_bearer_beats_ambient_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer explicit.jwt".parse().unwrap());
        headers.insert(
            "cookie",
            format!("{CUSTOMER_SESSION_COOKIE}=ambient.jwt")
                .parse()
                .unwrap(),
        );
        match presented_credential(&headers) {
            CredentialSelection::Valid(credential) => {
                assert_eq!(credential.token, "explicit.jwt");
                assert!(
                    !credential.cookie_authenticated,
                    "an explicit Authorization credential must not be tagged as cookie-borne"
                );
            }
            _ => panic!("explicit bearer plus ambient cookie must select the bearer"),
        }
        assert_eq!(bearer_token(&headers).as_deref(), Some("explicit.jwt"));
    }

    /// A verified user with no org membership is FORBIDDEN (403), not treated
    /// as unauthenticated and not admitted with an empty scope.
    #[tokio::test]
    async fn empty_org_membership_is_forbidden() {
        use axum::routing::get;
        let app = axum::Router::new().route(
            "/v1/me",
            get(|| async {
                Json(json!({
                    "user": { "user_id": "user-1", "email": "u@example.com", "orgs": [] }
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let auth = Authenticator::AuthService(url);
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Bearer orgless.jwt".parse().unwrap());
        let denied = auth
            .authenticate(&headers)
            .await
            .expect_err("a user with zero org memberships must be denied");
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);
        server.abort();
    }

    #[test]
    fn malformed_authorization_never_downgrades_to_cookie() {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, "Basic not-a-bearer".parse().unwrap());
        headers.insert(
            "cookie",
            format!("{CUSTOMER_SESSION_COOKIE}=ambient.jwt")
                .parse()
                .unwrap(),
        );
        assert_eq!(bearer_token(&headers), None);
        assert!(matches!(
            presented_credential(&headers),
            CredentialSelection::Invalid
        ));
    }

    #[test]
    fn duplicate_authorization_headers_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.append(AUTHORIZATION, "Bearer first.jwt".parse().unwrap());
        headers.append(AUTHORIZATION, "Bearer second.jwt".parse().unwrap());
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn duplicate_customer_cookies_are_rejected() {
        let mut headers = HeaderMap::new();
        headers.append(
            "cookie",
            format!("{CUSTOMER_SESSION_COOKIE}=first.jwt")
                .parse()
                .unwrap(),
        );
        headers.append(
            "cookie",
            format!("{CUSTOMER_SESSION_COOKIE}=second.jwt")
                .parse()
                .unwrap(),
        );
        assert_eq!(bearer_token(&headers), None);
    }

    #[test]
    fn release_cookie_names_are_host_only() {
        assert_eq!(
            customer_session_cookie_name(true),
            "__Host-fiducia_customer_session"
        );
        assert_eq!(
            customer_login_csrf_cookie_name(true),
            "__Host-fiducia_customer_login_csrf"
        );
        assert!(!customer_session_cookie_name(false).starts_with("__Host-"));
    }
}
