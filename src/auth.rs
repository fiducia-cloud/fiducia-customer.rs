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

use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::json;

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

fn http() -> &'static reqwest::Client {
    static C: OnceLock<reqwest::Client> = OnceLock::new();
    C.get_or_init(reqwest::Client::new)
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
                let bearer = headers
                    .get(AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .filter(|v| v.starts_with("Bearer "));
                let Some(bearer) = bearer else {
                    return Err(deny(StatusCode::UNAUTHORIZED, "missing_bearer_token"));
                };
                let resp = http()
                    .get(format!("{url}/v1/me"))
                    .header(AUTHORIZATION, bearer)
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r
                            .json()
                            .await
                            .map_err(|_| deny(StatusCode::BAD_GATEWAY, "auth_bad_response"))?;
                        let ctx: CustomerCtx = serde_json::from_value(
                            body.get("user").cloned().unwrap_or(serde_json::Value::Null),
                        )
                        .map_err(|_| deny(StatusCode::BAD_GATEWAY, "auth_bad_response"))?;
                        if ctx.user_id.trim().is_empty() {
                            return Err(deny(StatusCode::BAD_GATEWAY, "auth_bad_response"));
                        }
                        if ctx.orgs.is_empty() {
                            return Err(deny(StatusCode::FORBIDDEN, "no_org_membership"));
                        }
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
