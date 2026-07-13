// fiducia-backend entrypoint: the axum app for fiducia.cloud's website tier.
// Serves the static Astro marketing site, the Maud/HTMX customer portal and its
// WS/SSE fragment streams, plus authenticated customer APIs. API-key lifecycle
// is delegated to fiducia-auth so there is exactly one credential authority.
mod auth;
mod entity;
mod store;

use auth::{Authenticator, CustomerCtx};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{routing::get, Json, Router};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use sea_orm::{Database, DatabaseConnection};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use uuid::Uuid;

const SERVICE: &str = "fiducia-backend";

/// Bound request handling time. The site is static; nothing legitimately runs long.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Cap request bodies — this tier only serves GETs.
const MAX_BODY_BYTES: usize = 64 * 1024;
const STREAM_HEARTBEAT_SECS: u64 = 15;
const CUSTOMER_WS_PATH: &str = "/app/ws";
const CUSTOMER_EVENTS_PATH: &str = "/app/events";
const CUSTOMER_ORG_HEADER: &str = "x-fiducia-org-id";
const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const CORS_MAX_AGE_SECS: u64 = 10 * 60;

const CUSTOMER_REGIONS: &[&str] = &["auto", "iad1", "sfo1", "ams1", "fra1", "sin1", "syd1"];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    fiducia_telemetry::init(SERVICE);

    // Directory of the built Astro site. Defaults to the bundled `static/`
    // (populated from fiducia-ui.web's `dist/` at build time), but can be
    // pointed straight at the frontend dist via STATIC_DIR for local dev.
    let static_dir: PathBuf = std::env::var("STATIC_DIR")
        .unwrap_or_else(|_| "static".to_string())
        .into();

    let customer_static_dir: PathBuf = std::env::var("CUSTOMER_STATIC_DIR")
        .unwrap_or_else(|_| "customer-static".to_string())
        .into();
    let customer_app_origin = customer_app_origin_from_env()?;

    // Customer state is always durable. A missing/unreachable database is a
    // deployment error, not permission to serve invented customer data.
    let pool = match connect_customer_db().await {
        Ok(pool) => Some(pool),
        Err(error)
            if cfg!(debug_assertions)
                && std::env::var("FIDUCIA_E2E_ALLOW_NO_DATABASE").as_deref() == Ok("1") =>
        {
            tracing::warn!(error = %error, "E2E-only: starting customer shell without Postgres");
            None
        }
        Err(error) => return Err(error),
    };

    let config = AppConfig {
        static_dir: static_dir.clone(),
        customer_static_dir: customer_static_dir.clone(),
        customer_app_host: std::env::var("CUSTOMER_APP_HOST")
            .unwrap_or_else(|_| "app.fiducia.cloud".to_string()),
        customer_app_origin,
        customer_site_mode: std::env::var("FIDUCIA_SITE_MODE")
            .map(|v| v.eq_ignore_ascii_case("customer"))
            .unwrap_or(false),
        supabase_url: std::env::var("SUPABASE_URL").ok().filter(|v| !v.is_empty()),
        supabase_anon_key: std::env::var("SUPABASE_ANON_KEY")
            .ok()
            .filter(|v| !v.is_empty()),
        auth_url: std::env::var("FIDUCIA_AUTH_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| v.trim_end_matches('/').to_string()),
        pool,
        authenticator: Authenticator::from_env(),
    };

    let app = build_router(config);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(
        "{SERVICE} listening on http://{addr} (site={}, customer={})",
        static_dir.display(),
        customer_static_dir.display()
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Connect to the customer Postgres plane. Production startup fails closed when
/// `DATABASE_URL` is absent or unreachable.
async fn connect_customer_db() -> Result<DatabaseConnection, Box<dyn std::error::Error>> {
    let url = required_env("DATABASE_URL")?;
    let pool = Database::connect(&url).await?;
    pool.ping().await?;
    tracing::info!("customer DB connected — customer state is durable");
    Ok(pool)
}

fn required_env(name: &str) -> Result<String, io::Error> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, format!("{name} must be set")))
}

fn customer_app_origin_from_env() -> Result<Option<HeaderValue>, io::Error> {
    match std::env::var("CUSTOMER_APP_ORIGIN") {
        Ok(value) if !value.trim().is_empty() => parse_customer_app_origin(&value).map(Some),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CUSTOMER_APP_ORIGIN must be valid UTF-8",
        )),
    }
}

fn parse_customer_app_origin(value: &str) -> Result<HeaderValue, io::Error> {
    let value = value.trim();
    let uri = value.parse::<Uri>().map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "CUSTOMER_APP_ORIGIN must be an absolute http(s) origin",
        )
    })?;
    let scheme = uri.scheme_str().filter(|scheme| matches!(*scheme, "http" | "https"));
    let authority = uri.authority().filter(|authority| !authority.as_str().contains('@'));
    let root_only = uri
        .path_and_query()
        .map(|path| path.as_str() == "/")
        .unwrap_or(true);
    let (Some(scheme), Some(authority)) = (scheme, authority) else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CUSTOMER_APP_ORIGIN must contain only an http(s) scheme and host",
        ));
    };
    if !root_only {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CUSTOMER_APP_ORIGIN must not contain a path, query, or fragment",
        ));
    }
    let canonical = format!("{scheme}://{authority}");
    if value != canonical && value != format!("{canonical}/") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "CUSTOMER_APP_ORIGIN must be a single exact origin",
        ));
    }
    HeaderValue::from_str(&canonical).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "CUSTOMER_APP_ORIGIN is not a valid HTTP header origin",
        )
    })
}

fn customer_cors(origin: HeaderValue) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(origin)
        .allow_methods([Method::GET, Method::POST, Method::PUT, Method::OPTIONS])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static(CUSTOMER_ORG_HEADER),
            HeaderName::from_static(IDEMPOTENCY_KEY_HEADER),
        ])
        .max_age(Duration::from_secs(CORS_MAX_AGE_SECS))
}

/// Build the application router. Separated from `main` so tests can exercise the
/// routes without binding a socket or initializing telemetry.
fn build_router(config: AppConfig) -> Router {
    // Everything else is served from the static Astro build. Requests for
    // directories resolve to index.html, and unknown paths fall back to the
    // generated 404 page so client routing keeps working.
    let serve_dir = ServeDir::new(&config.static_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(config.static_dir.join("404.html")));
    let customer_assets =
        ServeDir::new(&config.customer_static_dir).append_index_html_on_directories(false);
    let customer_app_origin = config.customer_app_origin.clone();

    // Routes are declared as flat literals (not nested) so the shared API-docs
    // generator (remote/tools/generate-api-docs.mjs, which scans the router's
    // route declarations) records their true paths.
    let router = Router::new()
        // Liveness/readiness probe (matches the sibling canonical.cloud
        // convention); also available as /api/health.
        .route("/healthz", get(health))
        .route("/api/health", get(health))
        .route("/api/info", get(info))
        .route("/api/customer/context", get(customer_context_json))
        .route(
            "/api/customer/api-keys",
            get(customer_api_keys_json).post(create_customer_api_key),
        )
        .route(
            "/api/customer/api-keys/rotate",
            axum::routing::post(rotate_customer_api_key),
        )
        .route(
            "/api/customer/api-keys/revoke",
            axum::routing::post(revoke_customer_api_key),
        )
        // Read-only authenticated catch-up for local browser hydration. Credential
        // mutations go through the explicit create/rotate endpoints above and are
        // owned by fiducia-auth; this BFF exposes no second write authority.
        .route("/api/customer/sync/:table", get(sync_catchup))
        .route(
            "/api/customer/preferences",
            get(customer_preferences_json).put(update_customer_preferences),
        )
        .route(
            "/api/customer/security/sessions",
            get(customer_security_sessions_json),
        )
        .route(
            "/api/customer/security/sessions/revoke",
            axum::routing::post(revoke_customer_security_session),
        )
        .route("/", get(root))
        .route("/app", get(customer_home))
        .route("/app/", get(customer_home))
        .route("/app/dashboard", get(customer_home))
        .route("/app/auth", get(customer_auth))
        .route("/app/signup", get(customer_auth))
        .route("/app/api-keys", get(customer_api_keys))
        .route("/app/security", get(customer_security))
        .route("/app/settings", get(customer_settings))
        .route("/app/preferences", get(customer_settings))
        .route(CUSTOMER_WS_PATH, get(customer_ws))
        .route(CUSTOMER_EVENTS_PATH, get(customer_events))
        .route("/app/fragments/summary", get(summary_fragment))
        // Generated API docs (AGENTS.md "API Docs Contract").
        .route("/docs/api", get(api_docs_html))
        .route("/api/docs", get(api_docs_html))
        .route("/api/docs.json", get(api_docs_json))
        // Mermaid architecture diagram (rendered client-side).
        .route("/docs/diagram", get(diagram_html))
        .nest_service("/_customer", customer_assets)
        // Everything else: the static Astro site.
        .fallback_service(serve_dir)
        .with_state(config)
        // Security headers for the public site. CSP is intentionally just
        // `upgrade-insecure-requests` so the docs/diagram pages can still load
        // their Mermaid/marked CDN + inline init; tighten once those are vendored.
        .layer(SetResponseHeaderLayer::overriding(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::X_FRAME_OPTIONS,
            HeaderValue::from_static("DENY"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::REFERRER_POLICY,
            HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("geolocation=(), microphone=(), camera=()"),
        ))
        .layer(SetResponseHeaderLayer::overriding(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static("upgrade-insecure-requests"),
        ))
        // Hardening stack (outermost last): catch handler panics, bound request
        // time, and cap body size.
        .layer(TraceLayer::new_for_http())
        .layer(TimeoutLayer::new(Duration::from_secs(REQUEST_TIMEOUT_SECS)))
        .layer(RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        .layer(CatchPanicLayer::new());

    match customer_app_origin {
        Some(origin) => router.layer(customer_cors(origin)),
        None => router,
    }
}

#[derive(Clone)]
struct AppConfig {
    static_dir: PathBuf,
    customer_static_dir: PathBuf,
    customer_app_host: String,
    /// Exact standalone customer origin allowed to call this service from a
    /// browser. `None` keeps the service same-origin-only.
    customer_app_origin: Option<HeaderValue>,
    customer_site_mode: bool,
    supabase_url: Option<String>,
    supabase_anon_key: Option<String>,
    /// Base URL of the sole customer credential authority (`fiducia-auth`).
    auth_url: Option<String>,
    /// Customer Postgres connection. `None` exists only in isolated tests and a
    /// debug-only browser harness; data routes then fail closed.
    pool: Option<DatabaseConnection>,
    /// Verifies the customer's Supabase session for `/api/customer/*` and scopes
    /// writes to their org. Fail-closed (`Deny`) when no auth backend is set.
    authenticator: Authenticator,
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": SERVICE }))
}

async fn info(State(config): State<AppConfig>) -> Json<serde_json::Value> {
    Json(json!({
        "service": SERVICE,
        "version": env!("CARGO_PKG_VERSION"),
        "domain": "fiducia.cloud",
        "role": "website",
        "customer_portal": {
            "host": config.customer_app_host,
            "path": "/app",
            "static_prefix": "/_customer",
            "streams": {
                "websocket": CUSTOMER_WS_PATH,
                "sse": CUSTOMER_EVENTS_PATH,
                "heartbeat_secs": STREAM_HEARTBEAT_SECS,
            },
            "regions": CUSTOMER_REGIONS,
            "supabase_realtime": config.supabase_url.is_some()
                && config.supabase_anon_key.is_some(),
        },
        // The coordination API is not served here — it lives in the data-plane
        // and control-plane services.
        "components": {
            "data_plane": "fiducia-node",
            "control_plane": "fiducia-brain",
        },
    }))
}

#[derive(Debug, Deserialize)]
struct CreateCustomerApiKeyRequest {
    name: String,
    environment: String,
    scope: String,
    require_idempotency: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RotateCustomerApiKeyRequest {
    prefix: String,
}

#[derive(Debug, Deserialize)]
struct RevokeCustomerApiKeyRequest {
    prefix: String,
}

#[derive(Debug, Deserialize)]
struct AuthKeyMeta {
    key_id: String,
    name: String,
    scopes: Vec<String>,
    env: String,
    last_used_ms: Option<u64>,
    revoked: bool,
    version: u64,
    require_idempotency: bool,
}

#[derive(Debug, Deserialize)]
struct AuthKeyListResponse {
    keys: Vec<AuthKeyMeta>,
}

#[derive(Debug, Deserialize)]
struct AuthKeyCreateResponse {
    api_key: String,
    key: AuthKeyMeta,
}

#[derive(Debug, Deserialize)]
struct AuthKeyRotateResponse {
    api_key: String,
    key: AuthKeyMeta,
    overlap_seconds: u64,
}

#[derive(Debug, Deserialize)]
struct AuthKeyRevokeResponse {
    revoked: bool,
}

#[derive(Debug, Deserialize)]
struct RevokeCustomerSecuritySessionRequest {
    device: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct CustomerPreferences {
    region: String,
    timezone: String,
    density: String,
    notify_lock_contention: bool,
    notify_key_rotation: bool,
    notify_mfa: bool,
}

#[allow(clippy::result_large_err)] // Axum handlers return the framework Response directly.
fn customer_pool(config: &AppConfig) -> Result<&DatabaseConnection, Response> {
    config.pool.as_ref().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "ok": false,
                "error": "database_unavailable",
                "dependency": "postgres"
            })),
        )
            .into_response()
    })
}

fn dependency_error(dependency: &str, code: &str, error: impl std::fmt::Display) -> Response {
    tracing::error!(dependency, code, error = %error, "required dependency operation failed");
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "ok": false, "error": code, "dependency": dependency })),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
fn selected_customer_org(ctx: &CustomerCtx, headers: &HeaderMap) -> Result<String, Response> {
    if let Some(requested) = headers.get(CUSTOMER_ORG_HEADER) {
        let requested = requested.to_str().map_err(|_| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": "invalid_org_selection" })),
            )
                .into_response()
        })?;
        let requested = requested.trim();
        if requested.is_empty() || requested.len() > 128 {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "ok": false, "error": "invalid_org_selection" })),
            )
                .into_response());
        }
        if ctx.orgs.iter().any(|org_id| org_id == requested) {
            return Ok(requested.to_string());
        }
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": "forbidden_org" })),
        )
            .into_response());
    }

    match ctx.orgs.as_slice() {
        [] => Err((
            StatusCode::FORBIDDEN,
            Json(json!({ "ok": false, "error": "no_org_membership" })),
        )
            .into_response()),
        [org_id] => Ok(org_id.clone()),
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "org_selection_required" })),
        )
            .into_response()),
    }
}

async fn auth_json(
    config: &AppConfig,
    headers: &HeaderMap,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
) -> Result<(StatusCode, serde_json::Value), Response> {
    let Some(base) = config.auth_url.as_deref() else {
        return Err(dependency_error(
            "fiducia-auth",
            "customer_key_authority_not_configured",
            "FIDUCIA_AUTH_URL is unset",
        ));
    };
    let Some(bearer) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
    else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "ok": false, "error": "missing_bearer_token" })),
        )
            .into_response());
    };
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .header(reqwest::header::AUTHORIZATION, bearer);
    if let Some(idempotency_key) = headers.get("idempotency-key") {
        request = request.header("idempotency-key", idempotency_key);
    }
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request.send().await.map_err(|error| {
        dependency_error("fiducia-auth", "customer_key_authority_unreachable", error)
    })?;
    let status =
        StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let body = response
        .json::<serde_json::Value>()
        .await
        .map_err(|error| {
            dependency_error("fiducia-auth", "customer_key_authority_bad_response", error)
        })?;
    Ok((status, body))
}

fn proxied_auth_error(status: StatusCode, body: serde_json::Value) -> Response {
    let error = body
        .get("error")
        .cloned()
        .unwrap_or_else(|| json!("credential_authority_rejected_request"));
    (status, Json(json!({ "ok": false, "error": error }))).into_response()
}

fn auth_key_to_display(key: &AuthKeyMeta) -> Result<serde_json::Value, &'static str> {
    if key.key_id.is_empty() || !matches!(key.env.as_str(), "live" | "test") || key.version == 0 {
        return Err("invalid key metadata");
    }
    Ok(json!({
        "id": key.key_id,
        "name": key.name,
        "prefix": format!("fdc_{}_{}", key.env, key.key_id),
        "scopes": key.scopes.join(", "),
        "last_used": if key.last_used_ms.is_some() { "recently" } else { "never" },
        "status": if key.revoked { "revoked" } else { "active" },
        "environment": key.env,
        "require_idempotency": key.require_idempotency,
        "version": key.version,
    }))
}

fn auth_key_id_from_prefix(prefix: &str) -> Option<&str> {
    let rest = prefix.strip_prefix("fdc_")?;
    let (environment, key_id) = rest.split_once('_')?;
    if matches!(environment, "live" | "test") && !key_id.is_empty() {
        Some(key_id)
    } else {
        None
    }
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

async fn customer_api_keys_json(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let org_id = match selected_customer_org(&ctx, &headers) {
        Ok(org_id) => org_id,
        Err(response) => return response,
    };
    let path = format!("/v1/keys?org_id={}", encode_query_value(&org_id));
    let (status, body) = match auth_json(&config, &headers, reqwest::Method::GET, &path, None).await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    if !status.is_success() {
        return proxied_auth_error(status, body);
    }
    let response: AuthKeyListResponse = match serde_json::from_value(body) {
        Ok(response) => response,
        Err(error) => return dependency_error("fiducia-auth", "auth_key_list_bad_response", error),
    };
    let keys = match response
        .keys
        .iter()
        .map(auth_key_to_display)
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(keys) => keys,
        Err(error) => return dependency_error("fiducia-auth", "auth_key_list_bad_response", error),
    };

    Json(json!({
        "api_keys": keys,
        "default_require_idempotency": true,
        "allowed_environments": ["live", "test"],
        "allowed_scopes": allowed_api_key_scopes(),
    }))
    .into_response()
}

async fn customer_context_json(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    Json(json!({
        "user": {
            "user_id": ctx.user_id,
            "email": ctx.email,
            "orgs": ctx.orgs,
        }
    }))
    .into_response()
}

async fn create_customer_api_key(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Json(payload): Json<CreateCustomerApiKeyRequest>,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    if let Some(error) = validate_api_key_request(&payload) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error, "ok": false })),
        )
            .into_response();
    };

    let org_id = match selected_customer_org(&ctx, &headers) {
        Ok(org_id) => org_id,
        Err(response) => return response,
    };
    let request = json!({
        "name": payload.name.trim(),
        "org_id": org_id,
        "scopes": [payload.scope],
        "env": payload.environment,
        "require_idempotency": payload.require_idempotency.unwrap_or(true),
    });
    let (status, body) = match auth_json(
        &config,
        &headers,
        reqwest::Method::POST,
        "/v1/keys",
        Some(request),
    )
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    if !status.is_success() {
        return proxied_auth_error(status, body);
    }
    let response: AuthKeyCreateResponse = match serde_json::from_value(body) {
        Ok(response) => response,
        Err(error) => {
            return dependency_error("fiducia-auth", "auth_key_create_bad_response", error)
        }
    };
    let display = match auth_key_to_display(&response.key) {
        Ok(display) => display,
        Err(error) => {
            return dependency_error("fiducia-auth", "auth_key_create_bad_response", error)
        }
    };
    (
        StatusCode::CREATED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ok": true,
            "api_key": display,
            "secret": response.api_key,
            "secret_once": true,
        })),
    )
        .into_response()
}

async fn rotate_customer_api_key(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Json(payload): Json<RotateCustomerApiKeyRequest>,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let prefix = payload.prefix.trim();
    let Some(key_id) = auth_key_id_from_prefix(prefix) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_key_prefix", "ok": false })),
        )
            .into_response();
    };

    let org_id = match selected_customer_org(&ctx, &headers) {
        Ok(org_id) => org_id,
        Err(response) => return response,
    };
    let path = format!(
        "/v1/keys/{}/rotate?org_id={}",
        encode_query_value(key_id),
        encode_query_value(&org_id)
    );
    let (status, body) = match auth_json(
        &config,
        &headers,
        reqwest::Method::POST,
        &path,
        Some(json!({})),
    )
    .await
    {
        Ok(result) => result,
        Err(response) => return response,
    };
    if !status.is_success() {
        return proxied_auth_error(status, body);
    }
    let response: AuthKeyRotateResponse = match serde_json::from_value(body) {
        Ok(response) => response,
        Err(error) => {
            return dependency_error("fiducia-auth", "auth_key_rotate_bad_response", error)
        }
    };
    let display = match auth_key_to_display(&response.key) {
        Ok(display) => display,
        Err(error) => {
            return dependency_error("fiducia-auth", "auth_key_rotate_bad_response", error)
        }
    };

    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({
            "ok": true,
            "prefix": prefix,
            "rotated_at_ms": unix_epoch_ms(),
            "replacement_secret": response.api_key,
            "api_key": display,
            "overlap_seconds": response.overlap_seconds,
        })),
    )
        .into_response()
}

async fn revoke_customer_api_key(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Json(payload): Json<RevokeCustomerApiKeyRequest>,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let prefix = payload.prefix.trim();
    let Some(key_id) = auth_key_id_from_prefix(prefix) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_key_prefix", "ok": false })),
        )
            .into_response();
    };
    let org_id = match selected_customer_org(&ctx, &headers) {
        Ok(org_id) => org_id,
        Err(response) => return response,
    };
    let path = format!(
        "/v1/keys/{}?org_id={}",
        encode_query_value(key_id),
        encode_query_value(&org_id)
    );
    let (status, body) =
        match auth_json(&config, &headers, reqwest::Method::DELETE, &path, None).await {
            Ok(result) => result,
            Err(response) => return response,
        };
    if !status.is_success() {
        return proxied_auth_error(status, body);
    }
    let response: AuthKeyRevokeResponse = match serde_json::from_value(body) {
        Ok(response) => response,
        Err(error) => {
            return dependency_error("fiducia-auth", "auth_key_revoke_bad_response", error)
        }
    };
    if !response.revoked {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": "key_not_found" })),
        )
            .into_response();
    }
    Json(json!({ "ok": true, "prefix": prefix, "status": "revoked" })).into_response()
}

#[derive(Debug, Deserialize)]
struct CatchupParams {
    /// Accepted for client compatibility and observability. API-key hydration is
    /// a full authoritative snapshot because fiducia-auth owns the key store.
    #[serde(default)]
    since: i64,
}

/// Catch-up hydration returns a complete, sanitized, org-scoped API-key snapshot
/// from fiducia-auth. The browser uses `hydrate(..., { prune: true })`, so rows
/// removed or revoked while it was offline reconcile without raw database CDC.
async fn sync_catchup(
    State(config): State<AppConfig>,
    Path(table): Path<String>,
    headers: HeaderMap,
    Query(params): Query<CatchupParams>,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let rows: Vec<serde_json::Value> = match table.as_str() {
        "api_keys" => {
            let org_id = match selected_customer_org(&ctx, &headers) {
                Ok(org_id) => org_id,
                Err(response) => return response,
            };
            let path = format!("/v1/keys?org_id={}", encode_query_value(&org_id));
            let (status, body) =
                match auth_json(&config, &headers, reqwest::Method::GET, &path, None).await {
                    Ok(result) => result,
                    Err(response) => return response,
                };
            if !status.is_success() {
                return proxied_auth_error(status, body);
            }
            let response: AuthKeyListResponse = match serde_json::from_value(body) {
                Ok(response) => response,
                Err(error) => {
                    return dependency_error("fiducia-auth", "auth_key_list_bad_response", error)
                }
            };
            match response
                .keys
                .iter()
                .map(auth_key_to_display)
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(keys) => keys,
                Err(error) => {
                    return dependency_error("fiducia-auth", "auth_key_list_bad_response", error)
                }
            }
        }
        _ => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "ok": false, "error": "unsupported_sync_table", "table": table })),
            )
                .into_response()
        }
    };
    Json(json!({
        "table": table,
        "snapshot": true,
        "requested_since": params.since,
        "rows": rows,
    }))
    .into_response()
}

/// Resolve the caller's local `users.id`, provisioning the row on first access.
/// Identity fields come from the verified Supabase session and are never
/// synthesized when the upstream identity is incomplete.
async fn caller_user_id(config: &AppConfig, ctx: &CustomerCtx) -> Result<Uuid, Response> {
    let pool = customer_pool(config)?;
    let sub = Uuid::parse_str(&ctx.user_id).map_err(|_| {
        (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "ok": false, "error": "invalid_user_subject" })),
        )
            .into_response()
    })?;
    let email = ctx
        .email
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "ok": false, "error": "user_email_required" })),
            )
                .into_response()
        })?;
    match store::ensure_user(pool, sub, email).await {
        Ok(id) => Ok(id),
        Err(err) => Err(dependency_error("postgres", "ensure_user_failed", err)),
    }
}

fn prefs_from_row(
    row: &fiducia_interfaces_db::customer::CustomerPreferencesRow,
) -> CustomerPreferences {
    CustomerPreferences {
        region: row.region.clone(),
        timezone: row.timezone.clone(),
        density: row.density.clone(),
        notify_lock_contention: row.notify_lock_contention,
        notify_key_rotation: row.notify_key_rotation,
        notify_mfa: row.notify_mfa,
    }
}

fn session_model_json(
    row: &fiducia_interfaces_db::customer::CustomerSessionsRow,
) -> serde_json::Value {
    json!({
        "device": row.device,
        "location": row.location,
        "last_seen": row.last_seen.to_rfc3339(),
        "status": row.status,
    })
}

async fn customer_preferences_json(
    State(config): State<AppConfig>,
    headers: HeaderMap,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let uid = match caller_user_id(&config, &ctx).await {
        Ok(uid) => uid,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let prefs = match store::get_preferences(pool, uid).await {
        Ok(Some(row)) => prefs_from_row(&row),
        Ok(None) => default_customer_preferences(),
        Err(err) => return dependency_error("postgres", "preferences_read_failed", err),
    };
    Json(prefs).into_response()
}

async fn update_customer_preferences(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Json(payload): Json<CustomerPreferences>,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    if !CUSTOMER_REGIONS.contains(&payload.region.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_region", "ok": false })),
        )
            .into_response();
    }
    if !["comfortable", "compact"].contains(&payload.density.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_density", "ok": false })),
        )
            .into_response();
    }

    let uid = match caller_user_id(&config, &ctx).await {
        Ok(uid) => uid,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    match store::upsert_preferences(
        pool,
        uid,
        payload.region,
        payload.timezone,
        payload.density,
        payload.notify_key_rotation,
        payload.notify_lock_contention,
        payload.notify_mfa,
    )
    .await
    {
        Ok(row) => (
            StatusCode::OK,
            Json(json!({
                "ok": true,
                "preferences": prefs_from_row(&row),
                "saved_at_ms": unix_epoch_ms(),
            })),
        )
            .into_response(),
        Err(err) => dependency_error("postgres", "preferences_write_failed", err),
    }
}

async fn customer_security_sessions_json(
    State(config): State<AppConfig>,
    headers: HeaderMap,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let uid = match caller_user_id(&config, &ctx).await {
        Ok(uid) => uid,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let sessions_json = match store::list_sessions(pool, uid).await {
        Ok(rows) => rows.iter().map(session_model_json).collect::<Vec<_>>(),
        Err(err) => return dependency_error("postgres", "sessions_list_failed", err),
    };
    Json(json!({ "sessions": sessions_json, "revoke_supported": true })).into_response()
}

async fn revoke_customer_security_session(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Json(payload): Json<RevokeCustomerSecuritySessionRequest>,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let device = payload.device.trim();
    if device.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "device_required", "ok": false })),
        )
            .into_response();
    }

    let uid = match caller_user_id(&config, &ctx).await {
        Ok(uid) => uid,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    match store::revoke_session(pool, uid, device).await {
        Ok(revoked) => {
            Json(json!({ "ok": true, "device": device, "revoked": revoked })).into_response()
        }
        Err(err) => dependency_error("postgres", "session_revoke_failed", err),
    }
}

fn validate_api_key_request(payload: &CreateCustomerApiKeyRequest) -> Option<&'static str> {
    if payload.name.trim().is_empty() {
        return Some("name_required");
    }
    if !["live", "test"].contains(&payload.environment.as_str()) {
        return Some("invalid_environment");
    }
    if !allowed_api_key_scopes().contains(&payload.scope.as_str()) {
        return Some("invalid_scope");
    }

    None
}

fn allowed_api_key_scopes() -> &'static [&'static str] {
    &[
        "requests:read",
        "requests:write",
        "locks:read",
        "locks:write",
        "kv:read",
        "kv:write",
        "services:read",
        "services:write",
        "elections:read",
        "elections:write",
        "cron:read",
        "cron:write",
        "rate-limit:read",
        "rate-limit:write",
    ]
}

fn default_customer_preferences() -> CustomerPreferences {
    CustomerPreferences {
        region: "auto".to_string(),
        timezone: "browser".to_string(),
        density: "comfortable".to_string(),
        notify_lock_contention: true,
        notify_key_rotation: true,
        notify_mfa: true,
    }
}

async fn root(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    if should_serve_customer_app(&config, &headers) {
        return customer_page(&config, CustomerTab::Dashboard).into_response();
    }

    match tokio::fs::read_to_string(config.static_dir.join("index.html")).await {
        Ok(body) => Html(body).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "static index not found").into_response(),
    }
}

async fn customer_home(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Dashboard)
}

async fn customer_auth(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Auth)
}

async fn customer_api_keys(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::ApiKeys)
}

async fn customer_security(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Security)
}

async fn customer_settings(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Settings)
}

async fn summary_fragment() -> Markup {
    summary_markup()
}

async fn customer_ws(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(customer_ws_stream)
}

async fn customer_events() -> impl IntoResponse {
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(stream_event("connected", 0));

        let mut interval = tokio::time::interval(Duration::from_secs(STREAM_HEARTBEAT_SECS));
        let mut sequence = 1_u64;
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    yield Ok::<Event, Infallible>(stream_event("refresh", sequence));
                    sequence = sequence.saturating_add(1);
                }
            }
        }
    };

    Sse::new(stream).keep_alive(
        KeepAlive::new()
            .interval(Duration::from_secs(STREAM_HEARTBEAT_SECS))
            .text("keepalive"),
    )
}

async fn customer_ws_stream(mut socket: WebSocket) {
    let initial = stream_payload("connected", 0, "websocket").to_string();
    if socket.send(Message::Text(initial)).await.is_err() {
        return;
    }

    let mut interval = tokio::time::interval(Duration::from_secs(STREAM_HEARTBEAT_SECS));
    let mut sequence = 1_u64;
    loop {
        tokio::select! {
            _ = interval.tick() => {
                let payload = stream_payload("refresh", sequence, "websocket").to_string();
                sequence = sequence.saturating_add(1);
                if socket.send(Message::Text(payload)).await.is_err() {
                    return;
                }
            }
            msg = socket.recv() => {
                match msg {
                    Some(Ok(Message::Text(text))) if text.eq_ignore_ascii_case("ping") => {
                        if socket.send(Message::Text(stream_payload("pong", sequence, "websocket").to_string())).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Ping(payload))) => {
                        if socket.send(Message::Pong(payload)).await.is_err() {
                            return;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => return,
                }
            }
        }
    }
}

fn stream_event(kind: &str, sequence: u64) -> Event {
    Event::default()
        .event("fiducia-refresh")
        .id(sequence.to_string())
        .data(stream_payload(kind, sequence, "sse").to_string())
}

fn stream_payload(kind: &str, sequence: u64, transport: &str) -> serde_json::Value {
    json!({
        "kind": kind,
        "sequence": sequence,
        "transport": transport,
        "event": "fiducia:refresh",
        "at_ms": unix_epoch_ms(),
        "fragments": { "summary": summary_markup().into_string() },
    })
}

fn unix_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn should_serve_customer_app(config: &AppConfig, headers: &HeaderMap) -> bool {
    if config.customer_site_mode {
        return true;
    }

    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case(&config.customer_app_host)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CustomerTab {
    Dashboard,
    Auth,
    ApiKeys,
    Security,
    Settings,
}

impl CustomerTab {
    fn all() -> [CustomerTab; 5] {
        [
            CustomerTab::Dashboard,
            CustomerTab::Auth,
            CustomerTab::ApiKeys,
            CustomerTab::Security,
            CustomerTab::Settings,
        ]
    }

    fn href(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "/app",
            CustomerTab::Auth => "/app/auth",
            CustomerTab::ApiKeys => "/app/api-keys",
            CustomerTab::Security => "/app/security",
            CustomerTab::Settings => "/app/settings",
        }
    }

    fn label(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "Dashboard",
            CustomerTab::Auth => "Login & Signup",
            CustomerTab::ApiKeys => "API Keys",
            CustomerTab::Security => "Security",
            CustomerTab::Settings => "Settings",
        }
    }

    fn count(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "home",
            CustomerTab::Auth => "identity",
            CustomerTab::ApiKeys => "access",
            CustomerTab::Security => "2FA",
            CustomerTab::Settings => "profile",
        }
    }

    fn description(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => {
                "Account posture, API access, preferences, and customer security in one workspace."
            }
            CustomerTab::Auth => {
                "Supabase Auth login, signup, magic link, and session controls for end customers."
            }
            CustomerTab::ApiKeys => {
                "Create, rotate, scope, and audit customer API keys for production integrations."
            }
            CustomerTab::Security => {
                "Two-factor authentication, trusted sessions, recovery, and account protection."
            }
            CustomerTab::Settings => {
                "Preferences, notifications, default region, and team-level customer settings."
            }
        }
    }
}

fn customer_page(config: &AppConfig, active: CustomerTab) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Fiducia Customer Portal" }
                link rel="stylesheet" href="/_customer/assets/customer.css";
                (customer_config_script(config))
            }
            body {
                div class="app-shell" {
                    header class="topbar" {
                        div class="brand" {
                            div class="brand__mark" { "F" }
                            div class="brand__text" {
                                div class="brand__name" { "Fiducia Customer Portal" }
                                div class="brand__subdomain" { (config.customer_app_host) }
                            }
                        }
                        div class="topbar__status" {
                            span class="status-pill" data-backend-stream-status="" data-status="connecting" { "connecting" }
                            span class="status-pill" data-supabase-status="" data-status="offline" { "offline" }
                            span class="status-pill" { "customer app" }
                        }
                    }
                    main class="workspace" {
                        aside class="sidebar" {
                            section class="sidebar__section" {
                                p class="sidebar__label" { "Workspace" }
                                nav class="nav" aria-label="Customer portal" {
                                    @for tab in CustomerTab::all() {
                                        @if tab == active {
                                            a href=(tab.href()) aria-current="page" {
                                                span { (tab.label()) }
                                                span class="nav__count" { (tab.count()) }
                                            }
                                        } @else {
                                            a href=(tab.href()) {
                                                span { (tab.label()) }
                                                span class="nav__count" { (tab.count()) }
                                            }
                                        }
                                    }
                                }
                            }
                            section class="sidebar__section" {
                                div class="region-select" {
                                    label class="sidebar__label" for="region" { "Region" }
                                    select id="region" name="region" {
                                        @for region in CUSTOMER_REGIONS {
                                            option value=(*region) { (*region) }
                                        }
                                    }
                                }
                            }
                        }
                        section class="workspace__main" aria-labelledby="portal-title" {
                            div class="page-heading" {
                                div {
                                    h1 id="portal-title" { (active.label()) }
                                    p { (active.description()) }
                                }
                                div class="toolbar" {
                                    button type="button" hx-get="/app/fragments/summary" hx-target="#summary" hx-swap="innerHTML" { "Refresh" }
                                    a href="/app/api-keys" { "New API key" }
                                    a href="/api/info" { "API info" }
                                }
                            }
                            (customer_tab_content(config, active))
                        }
                    }
                }
                script type="module" src="/_customer/assets/customer.js" {}
            }
        }
    }
}

fn customer_config_script(config: &AppConfig) -> Markup {
    let payload = json!({
        "apiBase": "",
        "customerHost": config.customer_app_host,
        "backendWsPath": CUSTOMER_WS_PATH,
        "backendEventsPath": CUSTOMER_EVENTS_PATH,
        "regions": CUSTOMER_REGIONS,
        "supabaseUrl": config.supabase_url,
        "supabaseAnonKey": config.supabase_anon_key,
    });
    let script = format!(
        "window.FIDUCIA_CUSTOMER_CONFIG = {};",
        serde_json::to_string(&payload).unwrap()
    );
    html! {
        script { (PreEscaped(script)) }
    }
}

fn customer_tab_content(config: &AppConfig, active: CustomerTab) -> Markup {
    match active {
        CustomerTab::Dashboard => dashboard_markup(config),
        CustomerTab::Auth => auth_markup(config),
        CustomerTab::ApiKeys => api_keys_markup(),
        CustomerTab::Security => security_markup(),
        CustomerTab::Settings => settings_markup(),
    }
}

fn dashboard_markup(config: &AppConfig) -> Markup {
    html! {
        section id="summary" hx-get="/app/fragments/summary" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
            (summary_markup())
        }
        div class="panel-grid panel-grid--dashboard" {
            (auth_status_panel(config))
            (api_key_summary_panel())
            (security_summary_panel())
            (preferences_summary_panel())
        }
    }
}

fn auth_status_panel(config: &AppConfig) -> Markup {
    let supabase_state = if config.supabase_url.is_some() && config.supabase_anon_key.is_some() {
        "configured"
    } else {
        "missing env"
    };
    let project_url = config.supabase_url.as_deref().unwrap_or("not configured");

    html! {
        section class="panel" aria-labelledby="auth-status-heading" {
            div class="panel__header" {
                h2 id="auth-status-heading" { "Supabase Auth" }
                span data-auth-status="" { "signed out" }
            }
            div class="panel-body stack" {
                div class="identity-row" {
                    div {
                        p class="eyebrow" { "Customer session" }
                        p class="identity-row__primary" data-auth-email="" { "No customer signed in" }
                    }
                    (status_tag(supabase_state))
                }
                p class="muted" { "Project: " span class="mono" { (project_url) } }
                div class="action-row" {
                    a class="button-link" href="/app/auth" { "Login" }
                    a class="button-link" href="/app/signup" { "Sign up" }
                }
            }
        }
    }
}

fn api_key_summary_panel() -> Markup {
    html! {
        section class="panel" aria-labelledby="api-key-summary-heading" {
            div class="panel__header" {
                h2 id="api-key-summary-heading" { "API Keys" }
                span { "3 active" }
            }
            div class="panel-body stack" {
                p class="muted" { "Issue scoped keys for customer workloads and rotate live keys without downtime." }
                dl class="detail-list" {
                    div {
                        dt { "Default scope" }
                        dd { "requests:write with idempotency keys" }
                    }
                    div {
                        dt { "Rotation" }
                        dd { "rotation reports the remaining edge/LB cache overlap" }
                    }
                }
                a class="button-link" href="/app/api-keys" { "Manage keys" }
            }
        }
    }
}

fn security_summary_panel() -> Markup {
    html! {
        section class="panel" aria-labelledby="security-summary-heading" {
            div class="panel__header" {
                h2 id="security-summary-heading" { "Security" }
                span { "2FA enrollment available" }
            }
            div class="panel-body stack" {
                p class="muted" { "Require TOTP two-factor authentication for admins before production key issuance." }
                dl class="detail-list" {
                    div {
                        dt { "MFA" }
                        dd data-mfa-state="" { "not enrolled" }
                    }
                    div {
                        dt { "Sessions" }
                        dd { "3 trusted" }
                    }
                }
                a class="button-link" href="/app/security" { "Review security" }
            }
        }
    }
}

fn preferences_summary_panel() -> Markup {
    html! {
        section class="panel" aria-labelledby="preferences-summary-heading" {
            div class="panel__header" {
                h2 id="preferences-summary-heading" { "Preferences" }
                span { "team" }
            }
            div class="panel-body stack" {
                p class="muted" { "Set default region, alert cadence, timezone, and customer-visible notifications." }
                dl class="detail-list" {
                    div {
                        dt { "Region" }
                        dd { "auto" }
                    }
                    div {
                        dt { "Alerts" }
                        dd { "critical + key rotation" }
                    }
                }
                a class="button-link" href="/app/settings" { "Open settings" }
            }
        }
    }
}

fn auth_markup(config: &AppConfig) -> Markup {
    let supabase_state = if config.supabase_url.is_some() && config.supabase_anon_key.is_some() {
        "ready"
    } else {
        "configure SUPABASE_URL and SUPABASE_ANON_KEY"
    };

    html! {
        div class="panel-grid panel-grid--forms" {
            section class="panel" aria-labelledby="signin-heading" {
                div class="panel__header" {
                    h2 id="signin-heading" { "Login" }
                    span { (supabase_state) }
                }
                form class="form-grid" data-auth-form="sign-in" {
                    label {
                        span { "Email" }
                        input id="signin-email" type="email" name="email" autocomplete="email" required data-requires-supabase="";
                    }
                    label {
                        span { "Password" }
                        input id="signin-password" type="password" name="password" autocomplete="current-password" required data-requires-supabase="";
                    }
                    button type="submit" data-requires-supabase="" { "Login" }
                }
            }
            section class="panel" aria-labelledby="signup-heading" {
                div class="panel__header" {
                    h2 id="signup-heading" { "Sign up" }
                    span { "Supabase Auth" }
                }
                form class="form-grid" data-auth-form="sign-up" {
                    label {
                        span { "Work email" }
                        input id="signup-email" type="email" name="email" autocomplete="email" required data-requires-supabase="";
                    }
                    label {
                        span { "Full name" }
                        input id="signup-name" type="text" name="full_name" autocomplete="name" required data-requires-supabase="";
                    }
                    label {
                        span { "Company" }
                        input id="signup-company" type="text" name="company_name" autocomplete="organization" data-requires-supabase="";
                    }
                    label {
                        span { "Password" }
                        input id="signup-password" type="password" name="password" autocomplete="new-password" minlength="8" required data-requires-supabase="";
                    }
                    button type="submit" data-requires-supabase="" { "Create account" }
                }
            }
        }
        section class="panel" aria-labelledby="magic-link-heading" {
            div class="panel__header" {
                h2 id="magic-link-heading" { "Magic link" }
                span data-auth-status="" { "signed out" }
            }
            div class="split-panel" {
                form class="form-grid" data-auth-form="magic-link" {
                    label {
                        span { "Email" }
                        input id="magic-email" type="email" name="email" autocomplete="email" required data-requires-supabase="";
                    }
                    button type="submit" data-requires-supabase="" { "Send link" }
                }
                div class="session-box" {
                    p class="eyebrow" { "Current session" }
                    p class="identity-row__primary" data-auth-email="" { "No customer signed in" }
                    div class="action-row" {
                        button type="button" data-auth-action="sign-out" data-requires-supabase="" { "Log out" }
                    }
                }
                div class="session-box" {
                    p class="eyebrow" { "Passkeys" }
                    p class="muted" { "Use WebAuthn passkeys as a phishing-resistant sign-in option once Supabase passkeys are enabled." }
                    div class="action-row" {
                        button type="button" data-passkey-action="sign-in" data-requires-supabase="" { "Sign in with passkey" }
                    }
                }
            }
            div class="inline-message" data-auth-message="" aria-live="polite" {}
        }
    }
}

fn api_keys_markup() -> Markup {
    html! {
        section class="panel" aria-labelledby="create-api-key-heading" {
            div class="panel__header" {
                h2 id="create-api-key-heading" { "Create API key" }
                span { "customer scoped" }
            }
            form class="form-grid form-grid--inline" data-api-key-form="" {
                label {
                    span { "Name" }
                    input type="text" name="name" placeholder="Production checkout" required;
                }
                label {
                    span { "Environment" }
                    select name="environment" {
                        option value="live" { "Live" }
                        option value="test" { "Test" }
                    }
                }
                label {
                    span { "Scopes" }
                    select name="scope" {
                        option value="requests:read" { "requests:read" }
                        option value="requests:write" { "requests:write" }
                        option value="locks:read" { "locks:read" }
                        option value="locks:write" { "locks:write" }
                        option value="kv:read" { "kv:read" }
                        option value="kv:write" { "kv:write" }
                        option value="services:read" { "services:read" }
                        option value="services:write" { "services:write" }
                        option value="elections:read" { "elections:read" }
                        option value="elections:write" { "elections:write" }
                        option value="cron:read" { "cron:read" }
                        option value="cron:write" { "cron:write" }
                        option value="rate-limit:read" { "rate-limit:read" }
                        option value="rate-limit:write" { "rate-limit:write" }
                    }
                }
                label class="checkbox-line" {
                    input type="checkbox" name="require_idempotency" checked;
                    span { "Require Idempotency-Key on mutating calls" }
                }
                button type="submit" { "Create key" }
            }
            div class="inline-message" data-api-key-message="" aria-live="polite" {}
        }
        section class="panel" aria-labelledby="api-keys-heading" {
            div class="panel__header" {
                h2 id="api-keys-heading" { "Customer API keys" }
                span { "bounded rotation overlap" }
            }
            div class="table-wrap" {
                table data-api-keys-table="" {
                    thead {
                        tr {
                            th { "Name" }
                            th { "Prefix" }
                            th { "Scopes" }
                            th { "Last used" }
                            th { "State" }
                            th { "Action" }
                        }
                    }
                    tbody {
                        tr data-api-keys-loading="" {
                            td colspan="6" class="muted" { "Loading customer API keys…" }
                        }
                    }
                }
            }
        }
    }
}

fn security_markup() -> Markup {
    html! {
        div class="panel-grid panel-grid--forms" {
            section class="panel" aria-labelledby="mfa-heading" {
                div class="panel__header" {
                    h2 id="mfa-heading" { "Two-factor authentication" }
                    span data-mfa-state="" { "not enrolled" }
                }
                div class="panel-body stack" {
                    p class="muted" { "Enroll a TOTP authenticator before issuing or rotating production API keys." }
                    div class="action-row" {
                        button type="button" data-mfa-action="enroll-totp" data-requires-supabase="" { "Enroll TOTP" }
                    }
                    img class="mfa-qr" data-mfa-qr="" alt="TOTP QR code" hidden;
                    p class="mono secret-line" data-mfa-secret="" hidden {}
                    label class="form-field" {
                        span { "Authenticator code" }
                        input type="text" inputmode="numeric" autocomplete="one-time-code" data-mfa-code="" data-requires-supabase="";
                    }
                    button type="button" data-mfa-action="verify-totp" data-requires-supabase="" { "Verify 2FA" }
                    div class="inline-message" data-mfa-message="" aria-live="polite" {}
                }
            }
            section class="panel" aria-labelledby="passkeys-heading" {
                div class="panel__header" {
                    h2 id="passkeys-heading" { "Passkeys" }
                    span { "WebAuthn" }
                }
                div class="panel-body stack" {
                    p class="muted" { "Register a passkey after sign-in so email and password accounts can step up to phishing-resistant authentication." }
                    div class="action-row" {
                        button type="button" data-passkey-action="register" data-requires-supabase="" { "Register passkey" }
                        button type="button" data-passkey-action="sign-in" data-requires-supabase="" { "Test passkey sign-in" }
                    }
                }
            }
            section class="panel" aria-labelledby="recovery-heading" {
                div class="panel__header" {
                    h2 id="recovery-heading" { "Recovery" }
                    span { "admin gated" }
                }
                div class="panel-body stack" {
                    p class="muted" { "Recovery codes, break-glass review, and suspicious-login alerts belong to the same customer security workflow." }
                    dl class="detail-list" {
                        div {
                            dt { "Recovery codes" }
                            dd { "pending generation" }
                        }
                        div {
                            dt { "Break-glass" }
                            dd { "requires owner approval" }
                        }
                    }
                }
            }
        }
        section class="panel" aria-labelledby="sessions-heading" {
            div class="panel__header" {
                h2 id="sessions-heading" { "Trusted sessions" }
                span { "audit trail" }
            }
            div class="table-wrap" {
                table data-security-sessions-table="" {
                    thead {
                        tr {
                            th { "Device" }
                            th { "Location" }
                            th { "Last seen" }
                            th { "State" }
                            th { "Action" }
                        }
                    }
                    tbody {
                        tr data-security-sessions-loading="" {
                            td colspan="5" class="muted" { "Loading trusted sessions…" }
                        }
                    }
                }
            }
        }
    }
}

fn settings_markup() -> Markup {
    html! {
        section class="panel" aria-labelledby="preferences-heading" {
            div class="panel__header" {
                h2 id="preferences-heading" { "Preferences" }
                span { "saved per browser" }
            }
            form class="settings-grid" data-preference-form="" {
                label class="form-field" {
                    span { "Default region" }
                    select name="region" {
                        @for region in CUSTOMER_REGIONS {
                            option value=(*region) { (*region) }
                        }
                    }
                }
                label class="form-field" {
                    span { "Timezone" }
                    select name="timezone" {
                        option value="browser" { "Browser default" }
                        option value="utc" { "UTC" }
                        option value="america-lima" { "America/Lima" }
                    }
                }
                label class="form-field" {
                    span { "Dashboard density" }
                    select name="density" {
                        option value="comfortable" { "Comfortable" }
                        option value="compact" { "Compact" }
                    }
                }
                fieldset class="toggle-group" {
                    legend { "Notifications" }
                    label class="checkbox-line" {
                        input type="checkbox" name="notify_lock_contention" checked;
                        span { "Lock contention" }
                    }
                    label class="checkbox-line" {
                        input type="checkbox" name="notify_key_rotation" checked;
                        span { "API key rotation" }
                    }
                    label class="checkbox-line" {
                        input type="checkbox" name="notify_mfa" checked;
                        span { "2FA changes" }
                    }
                }
                button type="submit" { "Save preferences" }
            }
            div class="inline-message" data-preference-message="" aria-live="polite" {}
        }
        section class="panel" aria-labelledby="organization-heading" {
            div class="panel__header" {
                h2 id="organization-heading" { "Organization settings" }
                span { "customer tenant" }
            }
            div class="panel-body detail-grid" {
                dl class="detail-list" {
                    div {
                        dt { "Tenant slug" }
                        dd class="mono" { "tenant-42" }
                    }
                    div {
                        dt { "Plan" }
                        dd { "Production" }
                    }
                    div {
                        dt { "Support route" }
                        dd { "priority" }
                    }
                }
                dl class="detail-list" {
                    div {
                        dt { "Webhook retries" }
                        dd { "exponential backoff" }
                    }
                    div {
                        dt { "Idempotency retention" }
                        dd { "24 hours" }
                    }
                    div {
                        dt { "Default consistency" }
                        dd { "linearizable" }
                    }
                }
            }
        }
    }
}

fn summary_markup() -> Markup {
    html! {
        div class="summary-grid" {
            div class="metric" {
                p class="metric__label" { "API keys" }
                p class="metric__value" { "live" }
                p class="metric__hint" { "sanitized metadata from fiducia-auth after sign-in" }
            }
            div class="metric" {
                p class="metric__label" { "Preferences" }
                p class="metric__value" { "live" }
                p class="metric__hint" { "persisted per authenticated user" }
            }
            div class="metric" {
                p class="metric__label" { "Sessions" }
                p class="metric__value" { "live" }
                p class="metric__hint" { "trusted sessions from customer PostgreSQL" }
            }
            div class="metric" {
                p class="metric__label" { "Application boundary" }
                p class="metric__value" { "customer" }
                p class="metric__hint" { "operator infrastructure controls live only in the admin app" }
            }
        }
    }
}

fn status_tag(status: &str) -> Markup {
    html! {
        span class=(status_class(status)) { (status) }
    }
}

fn status_class(status: &str) -> &'static str {
    match status {
        "active" | "configured" | "enabled" | "held" | "current" | "healthy" | "committed"
        | "linearized" | "verified" => "tag tag--ok",
        "limited" | "missing env" | "pending" | "renewing" | "rotating" | "ttl" | "redirected" => {
            "tag tag--warn"
        }
        "blocked" | "degraded" | "disabled" | "expired" | "rejected" => "tag tag--error",
        _ => "tag tag--info",
    }
}

// Generated API docs (see AGENTS.md "API Docs Contract"). Artifacts are produced
// by remote/tools/generate-api-docs.mjs from the route declarations above and
// committed under generated/; do not hand-edit them.
async fn api_docs_html() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../generated/api-docs.html"))
}

async fn api_docs_json() -> impl axum::response::IntoResponse {
    (
        [("content-type", "application/json; charset=utf-8")],
        include_str!("../generated/api-docs.json"),
    )
}

// Mermaid architecture diagram page (rendered client-side via the Mermaid CDN).
async fn diagram_html() -> axum::response::Html<&'static str> {
    axum::response::Html(include_str!("../docs/diagram.html"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    /// Create a throwaway `static/` dir with the minimum files the static
    /// handler serves (home page, 404 fallback, a hashed asset).
    fn temp_dir(prefix: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn temp_static_dir() -> PathBuf {
        let dir = temp_dir("fiducia-site-test");
        std::fs::create_dir_all(dir.join("_astro")).unwrap();
        std::fs::write(
            dir.join("index.html"),
            "<!doctype html><title>Fiducia</title><h1>home</h1>",
        )
        .unwrap();
        std::fs::write(
            dir.join("404.html"),
            "<!doctype html><title>Not found</title>no quorum on this page",
        )
        .unwrap();
        std::fs::write(dir.join("_astro/app.css"), "body{color:rebeccapurple}").unwrap();
        dir
    }

    fn temp_customer_static_dir() -> PathBuf {
        let dir = temp_dir("fiducia-customer-test");
        std::fs::create_dir_all(dir.join("assets")).unwrap();
        std::fs::write(dir.join("assets/customer.js"), "window.customerLoaded=true").unwrap();
        std::fs::write(dir.join("assets/customer.css"), "body{color:#18212b}").unwrap();
        dir
    }

    fn test_config() -> AppConfig {
        // No pool: authenticated route tests exercise dependency failures without
        // inventing customer data or requiring a live Postgres/node deployment.
        AppConfig {
            static_dir: temp_static_dir(),
            customer_static_dir: temp_customer_static_dir(),
            customer_app_host: "app.fiducia.cloud".to_string(),
            customer_site_mode: false,
            supabase_url: None,
            supabase_anon_key: None,
            auth_url: None,
            pool: None,
            // Tests exercise the handlers as an authenticated customer with a
            // fixed org; production uses `Authenticator::from_env()` (fail-closed).
            authenticator: Authenticator::Static(std::sync::Arc::new(CustomerCtx {
                user_id: "00000000-0000-4000-8000-000000000002".to_string(),
                email: Some("test@fiducia.cloud".to_string()),
                orgs: vec!["00000000-0000-0000-0000-000000000001".to_string()],
            })),
        }
    }

    /// A no-DB config with a chosen authenticator (for auth-gate tests).
    fn config_with_auth(authenticator: Authenticator) -> AppConfig {
        AppConfig {
            authenticator,
            ..test_config()
        }
    }

    async fn post_json(config: AppConfig, uri: &str, body: &str) -> StatusCode {
        build_router(config)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    const CREATE_KEY_BODY: &str = r#"{"name":"k","environment":"live","scope":"requests:write"}"#;

    #[tokio::test]
    async fn unauthenticated_customer_mutations_fail_closed() {
        // No auth backend configured → every /api/customer mutation is denied (503),
        // closing the pre-fix hole where anyone could mint a live API key.
        let deny = || config_with_auth(Authenticator::Deny);
        assert_eq!(
            post_json(deny(), "/api/customer/api-keys", CREATE_KEY_BODY).await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            post_json(
                deny(),
                "/api/customer/api-keys/rotate",
                r#"{"prefix":"fid_live_x"}"#
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
        // The catch-up endpoint is GET-only; mutation attempts never reach auth.
        assert_eq!(
            post_json(deny(), "/api/customer/sync/api_keys", "{}").await,
            StatusCode::METHOD_NOT_ALLOWED
        );
    }

    #[tokio::test]
    async fn authenticated_customer_without_database_fails_closed() {
        let status = post_json(test_config(), "/api/customer/api-keys", CREATE_KEY_BODY).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Send a GET through the router and return (status, content-type, body).
    async fn send(uri: &str) -> (StatusCode, String, String) {
        send_with_host(uri, None).await
    }

    async fn send_with_host(uri: &str, host: Option<&str>) -> (StatusCode, String, String) {
        let app = build_router(test_config());
        let mut builder = Request::builder().uri(uri);
        if let Some(host) = host {
            builder = builder.header(header::HOST, host);
        }
        let resp = app
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, ct, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn send_json(
        method: &str,
        uri: &str,
        payload: serde_json::Value,
    ) -> (StatusCode, String, String) {
        let app = build_router(test_config());
        let resp = app
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, ct, String::from_utf8_lossy(&bytes).into_owned())
    }

    #[tokio::test]
    async fn healthz_and_api_health_report_ok() {
        for uri in ["/healthz", "/api/health"] {
            let (status, _ct, body) = send(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap();
            assert_eq!(v["status"], "ok", "{uri}");
            assert_eq!(v["service"], "fiducia-backend", "{uri}");
        }
    }

    #[tokio::test]
    async fn api_info_describes_the_website_tier() {
        let (status, ct, body) = send("/api/info").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("application/json"), "ct={ct}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["service"], "fiducia-backend");
        assert_eq!(v["domain"], "fiducia.cloud");
        assert_eq!(v["role"], "website");
        assert_eq!(v["customer_portal"]["host"], "app.fiducia.cloud");
        assert_eq!(v["customer_portal"]["path"], "/app");
        assert_eq!(v["customer_portal"]["static_prefix"], "/_customer");
        assert_eq!(v["customer_portal"]["streams"]["websocket"], "/app/ws");
        assert_eq!(v["customer_portal"]["streams"]["sse"], "/app/events");
        assert_eq!(v["customer_portal"]["supabase_realtime"], false);
        assert_eq!(v["components"]["data_plane"], "fiducia-node");
        assert_eq!(v["components"]["control_plane"], "fiducia-brain");
        assert_eq!(v["version"], env!("CARGO_PKG_VERSION"));
    }

    #[tokio::test]
    async fn customer_api_keys_require_database() {
        let (status, _, _) = send("/api/customer/api-keys").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, _, _) = send_json(
            "POST",
            "/api/customer/api-keys",
            json!({
                "name": "Production webhooks",
                "environment": "live",
                "scope": "requests:write",
                "require_idempotency": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn customer_api_key_creation_validates_input() {
        let (status, _ct, body) = send_json(
            "POST",
            "/api/customer/api-keys",
            json!({
                "name": "",
                "environment": "live",
                "scope": "requests:write",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "name_required");

        let (status, _ct, body) = send_json(
            "POST",
            "/api/customer/api-keys",
            json!({
                "name": "bad scope",
                "environment": "live",
                "scope": "root",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["error"], "invalid_scope");
    }

    #[tokio::test]
    async fn customer_preferences_require_database_and_validate_before_io() {
        let (status, _, _) = send("/api/customer/preferences").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, _, _) = send_json(
            "PUT",
            "/api/customer/preferences",
            json!({
                "region": "iad1",
                "timezone": "utc",
                "density": "compact",
                "notify_lock_contention": false,
                "notify_key_rotation": true,
                "notify_mfa": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, _ct, body) = send_json(
            "PUT",
            "/api/customer/preferences",
            json!({
                "region": "moon",
                "timezone": "utc",
                "density": "compact",
                "notify_lock_contention": false,
                "notify_key_rotation": true,
                "notify_mfa": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let rejected: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(rejected["error"], "invalid_region");
    }

    #[tokio::test]
    async fn customer_security_sessions_require_database() {
        let (status, _, _) = send("/api/customer/security/sessions").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

        let (status, _, _) = send_json(
            "POST",
            "/api/customer/security/sessions/revoke",
            json!({ "device": "Safari on iPhone" }),
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn docs_api_and_alias_serve_html() {
        for uri in ["/docs/api", "/api/docs"] {
            let (status, ct, body) = send(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert!(ct.contains("text/html"), "{uri} ct={ct}");
            assert!(body.contains("fiducia-backend.rs API docs"), "{uri}");
        }
    }

    #[tokio::test]
    async fn api_docs_json_is_machine_readable() {
        let (status, ct, body) = send("/api/docs.json").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("application/json"), "ct={ct}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["service"], "fiducia-backend.rs");
        assert!(v["routeCount"].as_u64().unwrap() >= 6);
        let standard = v["standardDocsRoutes"].as_array().unwrap();
        for r in ["/docs/api", "/api/docs", "/api/docs.json"] {
            assert!(
                standard.iter().any(|x| x == r),
                "missing {r} in standardDocsRoutes"
            );
        }
    }

    #[tokio::test]
    async fn diagram_route_serves_html() {
        let (status, ct, body) = send("/docs/diagram").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
        assert!(body.contains("AI-agent fleets"));
        assert!(body.contains("durable brain-Raft: target HA step"));
    }

    #[tokio::test]
    async fn root_serves_the_static_index() {
        let (status, ct, body) = send("/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
        assert!(body.contains("home"));
    }

    #[tokio::test]
    async fn app_host_root_serves_the_customer_portal() {
        let (status, ct, body) = send_with_host("/", Some("app.fiducia.cloud")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
        assert!(body.contains("Fiducia Customer Portal"));
        assert!(body.contains("/_customer/assets/customer.js"));
        assert!(body.contains("\"backendWsPath\":\"/app/ws\""));
    }

    #[tokio::test]
    async fn app_route_serves_the_customer_portal() {
        let (status, ct, body) = send("/app").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
        assert!(body.contains("Account posture, API access"));
        assert!(body.contains("Supabase Auth"));
        assert!(body.contains("Login"));
        assert!(body.contains("Sign up"));
        assert!(body.contains("API Keys"));
        assert!(body.contains("2FA enrollment available"));
        assert!(body.contains("Preferences"));
        assert!(body.contains("operator infrastructure controls live only in the admin app"));
        assert!(!body.contains("Config KV"));
        assert!(!body.contains("Service Discovery"));
    }

    #[tokio::test]
    async fn customer_account_routes_render_customer_controls() {
        let cases = [
            ("/app/auth", "Magic link"),
            ("/app/auth", "Sign in with passkey"),
            ("/app/signup", "Create account"),
            ("/app/api-keys", "Require Idempotency-Key"),
            ("/app/security", "Register passkey"),
            ("/app/settings", "Organization settings"),
            ("/app/preferences", "Save preferences"),
        ];

        for (uri, needle) in cases {
            let (status, ct, body) = send(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert!(ct.contains("text/html"), "{uri} ct={ct}");
            assert!(body.contains(needle), "{uri} missing {needle}");
        }
    }

    #[tokio::test]
    async fn operator_pages_are_absent_from_the_customer_app() {
        for uri in [
            "/app/locks",
            "/app/requests",
            "/app/kv",
            "/app/services",
            "/app/fragments/locks",
        ] {
            let (status, _, body) = send(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert!(body.contains("no quorum on this page"), "{uri}");
            assert!(!body.contains("operator"), "{uri}");
        }
    }

    #[tokio::test]
    async fn customer_sse_stream_is_available() {
        let app = build_router(test_config());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/app/events")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(ct.contains("text/event-stream"), "ct={ct}");
    }

    #[tokio::test]
    async fn customer_sync_is_read_only() {
        let resp = build_router(test_config())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/customer/sync/api_keys")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn customer_websocket_route_requires_upgrade() {
        let app = build_router(test_config());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/app/ws")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn static_asset_served_with_correct_mime() {
        let (status, ct, body) = send("/_astro/app.css").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/css"), "ct={ct}");
        assert!(body.contains("rebeccapurple"));
    }

    #[tokio::test]
    async fn customer_asset_served_with_correct_mime() {
        let (status, ct, body) = send("/_customer/assets/customer.js").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            ct.contains("text/javascript") || ct.contains("application/javascript"),
            "ct={ct}"
        );
        assert!(body.contains("customerLoaded"));
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_the_404_page() {
        // SPA-style fallback: the styled 404 page is served (ServeFile returns 200).
        let (status, _ct, body) = send("/does/not/exist").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("no quorum on this page"));
    }
}

#[cfg(test)]
mod interface_contract_tests {
    use fiducia_interfaces::{LockAcquireManyRequest, ProposeErrorReason};

    #[test]
    fn generated_interfaces_are_importable() {
        let request = LockAcquireManyRequest {
            keys: vec!["orders/42".to_string(), "inventory/sku-7".to_string()],
            holder: Some("worker-a".to_string()),
            ttl_ms: Some(30_000),
            wait: Some(false),
        };

        assert_eq!(request.keys.len(), 2);
        assert!(matches!(
            ProposeErrorReason::NotLeader,
            ProposeErrorReason::NotLeader
        ));
    }
}
