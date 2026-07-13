// fiducia-backend entrypoint: the axum app for fiducia.cloud's website tier.
// Serves the static Astro marketing site, the Maud/HTMX customer portal and its
// WS/SSE fragment streams, plus the DB-backed api_keys + @fiducia/sync endpoints.
mod auth;
mod entity;
mod store;

use auth::{bearer_token, Authenticator, CustomerCtx, CUSTOMER_SESSION_COOKIE};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{routing::get, Json, Router};
use fiducia_interfaces_db::customer::ApiKeysRow;
use fiducia_sync_core::WriteAck;
use maud::{html, Markup, DOCTYPE};
use sea_orm::{ConnectOptions, Database, DatabaseConnection, DbErr};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::convert::Infallible;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower_http::catch_panic::CatchPanicLayer;
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
const HTMX_JS: &str = include_str!("../assets/htmx.min.js");
const CUSTOMER_CSS: &str = include_str!("../assets/customer.css");

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

    // Customer state is always durable. A missing/unreachable database is a
    // deployment error, not permission to serve invented customer data.
    let pool = connect_customer_db().await?;

    let config = AppConfig {
        static_dir: static_dir.clone(),
        customer_app_host: std::env::var("CUSTOMER_APP_HOST")
            .unwrap_or_else(|_| "app.fiducia.cloud".to_string()),
        customer_site_mode: std::env::var("FIDUCIA_SITE_MODE")
            .map(|v| v.eq_ignore_ascii_case("customer"))
            .unwrap_or(false),
        supabase_url: Some(required_env("SUPABASE_URL")?),
        supabase_publishable_key: Some(required_env("SUPABASE_PUBLISHABLE_KEY")?),
        auth_url: Some(required_env("FIDUCIA_AUTH_URL")?),
        pool: Some(pool),
        authenticator: Authenticator::from_env(),
    };

    let app = build_router(config);

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(
        "{SERVICE} listening on http://{addr} (marketing={})",
        static_dir.display()
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Connect to the customer Postgres plane. Production startup fails closed when
/// `DATABASE_URL` is absent or unreachable.
async fn connect_customer_db() -> Result<DatabaseConnection, Box<dyn std::error::Error>> {
    let url = required_env("DATABASE_URL")?;
    let mut options = ConnectOptions::new(url);
    options.max_connections(5).sqlx_logging(false);
    let pool = Database::connect(options).await?;
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

/// Build the application router. Separated from `main` so tests can exercise the
/// routes without binding a socket or initializing telemetry.
fn build_router(config: AppConfig) -> Router {
    // Everything else is served from the static Astro build. Requests for
    // directories resolve to index.html, and unknown paths fall back to the
    // generated 404 page so client routing keeps working.
    let serve_dir = ServeDir::new(&config.static_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(config.static_dir.join("404.html")));
    // Routes are declared as flat literals (not nested) so the shared API-docs
    // generator (remote/tools/generate-api-docs.mjs, which scans the router's
    // route declarations) records their true paths.
    Router::new()
        // Liveness/readiness probe (matches the sibling canonical.cloud
        // convention); also available as /api/health.
        .route("/healthz", get(health))
        .route("/api/health", get(health))
        .route("/api/info", get(info))
        .route("/assets/htmx.min.js", get(htmx_js))
        .route("/assets/customer.css", get(customer_css))
        .route("/login", get(customer_login).post(customer_login_submit))
        .route("/logout", axum::routing::post(customer_logout))
        .route(
            "/api/customer/api-keys",
            get(customer_api_keys_json).post(create_customer_api_key),
        )
        .route(
            "/api/customer/api-keys/rotate",
            axum::routing::post(rotate_customer_api_key),
        )
        // Local-first sync write path: the @fiducia/sync client POSTs queued
        // optimistic writes to /api/customer/sync/{table}; we persist via SQLx and
        // return the committed row version so the client can adopt it and clear
        // `dirty`. Generic in the table (only api_keys is DB-wired today).
        .route(
            "/api/customer/sync/:table",
            axum::routing::post(sync_write).get(sync_catchup),
        )
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
        .route(
            "/app/api-keys",
            get(customer_api_keys).post(create_customer_api_key_form),
        )
        .route("/app/security", get(customer_security))
        .route(
            "/app/security/sessions/revoke",
            axum::routing::post(revoke_customer_session_form),
        )
        .route(
            "/app/settings",
            get(customer_settings).post(update_customer_preferences_form),
        )
        .route("/app/preferences", get(customer_settings))
        .route("/app/locks", get(customer_locks))
        .route("/app/requests", get(customer_requests))
        .route("/app/kv", get(customer_kv))
        .route("/app/services", get(customer_services))
        .route(CUSTOMER_WS_PATH, get(customer_ws))
        .route(CUSTOMER_EVENTS_PATH, get(customer_events))
        .route("/app/fragments/summary", get(summary_fragment))
        .route("/app/fragments/api-keys", get(api_keys_fragment))
        .route(
            "/app/fragments/preferences",
            get(customer_preferences_fragment),
        )
        .route(
            "/app/fragments/security-sessions",
            get(customer_sessions_fragment),
        )
        .route("/app/fragments/locks", get(locks_fragment))
        .route("/app/fragments/requests", get(requests_fragment))
        .route("/app/fragments/kv", get(kv_fragment))
        .route("/app/fragments/services", get(services_fragment))
        // Generated API docs (AGENTS.md "API Docs Contract").
        .route("/docs/api", get(api_docs_html))
        .route("/api/docs", get(api_docs_html))
        .route("/api/docs.json", get(api_docs_json))
        // Mermaid architecture diagram (rendered client-side).
        .route("/docs/diagram", get(diagram_html))
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
        .layer(CatchPanicLayer::new())
}

#[derive(Clone)]
struct AppConfig {
    static_dir: PathBuf,
    customer_app_host: String,
    customer_site_mode: bool,
    supabase_url: Option<String>,
    supabase_publishable_key: Option<String>,
    auth_url: Option<String>,
    /// Customer Postgres pool. `None` exists only in isolated route tests and
    /// always produces a service-unavailable response.
    pool: Option<DatabaseConnection>,
    /// Verifies the customer's Supabase session for `/api/customer/*` and scopes
    /// writes to their org. Fail-closed (`Deny`) when no auth backend is set.
    authenticator: Authenticator,
}

#[derive(Debug, Deserialize)]
struct CustomerLoginForm {
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct SupabasePasswordSession {
    access_token: String,
}

#[derive(Debug, Deserialize)]
struct AuthCreatedKeyResponse {
    api_key: String,
    key: AuthCreatedKeyMeta,
}

#[derive(Debug, Deserialize)]
struct AuthCreatedKeyMeta {
    key_id: String,
    org_id: String,
    env: String,
    require_idempotency: bool,
}

async fn htmx_js() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/javascript; charset=utf-8",
        )],
        HTMX_JS,
    )
}

async fn customer_css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        CUSTOMER_CSS,
    )
}

async fn customer_login() -> Markup {
    customer_login_markup(None)
}

async fn customer_login_submit(
    State(config): State<AppConfig>,
    Form(form): Form<CustomerLoginForm>,
) -> Response {
    let email = form.email.trim();
    if email.is_empty() || form.password.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            customer_login_markup(Some("Email and password are required.")),
        )
            .into_response();
    }
    let (Some(supabase_url), Some(publishable_key)) = (
        config.supabase_url.as_deref(),
        config.supabase_publishable_key.as_deref(),
    ) else {
        return dependency_error(
            "supabase",
            "customer_login_not_configured",
            "SUPABASE_URL and SUPABASE_PUBLISHABLE_KEY are required",
        );
    };

    let response = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => {
            client
                .post(format!(
                    "{}/auth/v1/token?grant_type=password",
                    supabase_url.trim_end_matches('/')
                ))
                .header("apikey", publishable_key)
                .json(&json!({ "email": email, "password": form.password }))
                .send()
                .await
        }
        Err(error) => return dependency_error("supabase", "supabase_login_failed", error),
    };
    let response = match response {
        Ok(response) if response.status().is_success() => response,
        Ok(_) => {
            return (
                StatusCode::UNAUTHORIZED,
                customer_login_markup(Some("Supabase rejected those credentials.")),
            )
                .into_response()
        }
        Err(error) => return dependency_error("supabase", "supabase_login_failed", error),
    };
    let session = match response.json::<SupabasePasswordSession>().await {
        Ok(session) => session,
        Err(error) => return dependency_error("supabase", "supabase_login_failed", error),
    };

    let mut headers = HeaderMap::new();
    let bearer = match HeaderValue::from_str(&format!("Bearer {}", session.access_token)) {
        Ok(value) => value,
        Err(error) => return dependency_error("supabase", "supabase_login_failed", error),
    };
    headers.insert(header::AUTHORIZATION, bearer);
    if let Err(response) = config.authenticator.authenticate(&headers).await {
        return response;
    }

    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/app".to_string()),
            (
                header::SET_COOKIE,
                make_customer_session_cookie(&session.access_token),
            ),
        ],
    )
        .into_response()
}

fn make_customer_session_cookie(token: &str) -> String {
    let secure = if std::env::var("FIDUCIA_INSECURE_COOKIES").as_deref() == Ok("1") {
        ""
    } else {
        "; Secure"
    };
    format!(
        "{CUSTOMER_SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=3600{secure}"
    )
}

async fn customer_logout() -> Response {
    let secure = if std::env::var("FIDUCIA_INSECURE_COOKIES").as_deref() == Ok("1") {
        ""
    } else {
        "; Secure"
    };
    (
        StatusCode::SEE_OTHER,
        [
            (header::LOCATION, "/login".to_string()),
            (
                header::SET_COOKIE,
                format!(
                    "{CUSTOMER_SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{secure}"
                ),
            ),
        ],
    )
        .into_response()
}

fn customer_login_markup(message: Option<&str>) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Sign in · Fiducia Customer" }
                link rel="stylesheet" href="/assets/customer.css";
                script src="/assets/htmx.min.js" defer {}
            }
            body {
                main class="auth-shell" {
                    section class="auth-card" {
                        p class="eyebrow" { "Customer application" }
                        h1 { "Sign in to Fiducia" }
                        p class="muted" { "Supabase authenticates your credentials; fiducia-auth verifies the resulting identity and organization membership." }
                        @if let Some(message) = message {
                            p class="auth-message" role="alert" { (message) }
                        }
                        form method="post" action="/login" hx-post="/login" hx-target="body" hx-swap="outerHTML" {
                            label for="email" { "Email" }
                            input id="email" name="email" type="email" autocomplete="email" required;
                            label for="password" { "Password" }
                            input id="password" name="password" type="password" autocomplete="current-password" required;
                            button type="submit" { "Sign in" }
                        }
                        p class="muted" { "Operator accounts use the separate admin application and cookie boundary." }
                    }
                }
            }
        }
    }
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
            "rendering": "maud+htmx",
            "streams": {
                "websocket": CUSTOMER_WS_PATH,
                "sse": CUSTOMER_EVENTS_PATH,
                "heartbeat_secs": STREAM_HEARTBEAT_SECS,
            },
            "regions": CUSTOMER_REGIONS,
            "supabase_login": config.supabase_url.is_some()
                && config.supabase_publishable_key.is_some(),
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
struct CreateCustomerApiKeyForm {
    name: String,
    environment: String,
    scope: String,
}

#[derive(Debug, Deserialize)]
struct RotateCustomerApiKeyRequest {
    prefix: String,
}

/// One queued optimistic write from the @fiducia/sync client. `table` is implied
/// by the route (`api_keys`) but echoed by the client, so we accept it. `payload`
/// is the row the client optimistically stored; `base_version` is the version it
/// was edited on top of (for the ack the client reconciles against).
#[derive(Debug, Deserialize, Serialize)]
struct SyncWriteRequest {
    id: String,
    #[serde(default)]
    #[allow(dead_code)]
    table: Option<String>,
    #[serde(default)]
    op: Option<String>,
    #[serde(default)]
    payload: Option<serde_json::Value>,
    #[serde(default)]
    #[serde(rename = "base_version")]
    _base_version: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RevokeCustomerSecuritySessionRequest {
    device: String,
}

#[derive(Debug, Deserialize)]
struct RevokeCustomerSessionForm {
    device: String,
}

#[derive(Debug, Deserialize)]
struct CustomerPreferencesForm {
    region: String,
    timezone: String,
    density: String,
    notify_lock_contention: Option<String>,
    notify_key_rotation: Option<String>,
    notify_mfa: Option<String>,
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

async fn customer_api_keys_json(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    // Scoped to the caller's org(s) — a key list must never disclose other tenants.
    let keys = match store::list_api_keys(pool, &ctx.org_uuids()).await {
        Ok(rows) => rows.iter().map(api_key_row_to_display).collect::<Vec<_>>(),
        Err(err) => return dependency_error("postgres", "api_keys_list_failed", err),
    };

    Json(json!({
        "api_keys": keys,
        "default_require_idempotency": true,
        "allowed_environments": ["live", "test"],
        "allowed_scopes": allowed_api_key_scopes(),
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
    }
    let token = bearer_token(&headers);
    let (row, secret) =
        match issue_customer_api_key(&config, &ctx, token.as_deref(), &payload).await {
            Ok(issued) => issued,
            Err(response) => return response,
        };
    let mut api_key = api_key_row_to_display(&row);
    api_key["environment"] = json!(payload.environment);
    api_key["require_idempotency"] = json!(payload.require_idempotency.unwrap_or(true));
    (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "api_key": api_key,
            "secret": secret,
            "secret_once": true,
        })),
    )
        .into_response()
}

async fn issue_customer_api_key(
    config: &AppConfig,
    ctx: &CustomerCtx,
    token: Option<&str>,
    payload: &CreateCustomerApiKeyRequest,
) -> Result<(ApiKeysRow, String), Response> {
    if let Some(error) = validate_api_key_request(payload) {
        return Err((StatusCode::BAD_REQUEST, error).into_response());
    }
    let pool = customer_pool(config)?;
    let Some(org_id) = ctx.org_uuids().into_iter().next() else {
        return Err((StatusCode::FORBIDDEN, "no_org_membership").into_response());
    };
    let Some(token) = token else {
        return Err((StatusCode::UNAUTHORIZED, "missing_customer_session").into_response());
    };
    let Some(auth_url) = config.auth_url.as_deref() else {
        return Err(dependency_error(
            "fiducia-auth",
            "key_authority_not_configured",
            "FIDUCIA_AUTH_URL is required",
        ));
    };
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|error| dependency_error("fiducia-auth", "key_create_failed", error))?;
    let response = client
        .post(format!("{}/v1/keys", auth_url.trim_end_matches('/')))
        .bearer_auth(token)
        .json(&json!({
            "name": payload.name.trim(),
            "org_id": org_id,
            "scopes": [payload.scope.as_str()],
            "env": payload.environment,
            "require_idempotency": payload.require_idempotency.unwrap_or(true),
        }))
        .send()
        .await
        .map_err(|error| dependency_error("fiducia-auth", "key_create_failed", error))?;
    if !response.status().is_success() {
        return Err(dependency_error(
            "fiducia-auth",
            "key_create_rejected",
            response.status(),
        ));
    }
    let issued = response
        .json::<AuthCreatedKeyResponse>()
        .await
        .map_err(|error| dependency_error("fiducia-auth", "key_create_bad_response", error))?;
    if issued.key.org_id != org_id.to_string()
        || issued.key.require_idempotency != payload.require_idempotency.unwrap_or(true)
    {
        return Err(dependency_error(
            "fiducia-auth",
            "key_create_contract_mismatch",
            "auth response did not match the requested organization or policy",
        ));
    }
    let verifier_secret = issued
        .api_key
        .split_once('.')
        .map(|(_, secret)| secret)
        .ok_or_else(|| {
            dependency_error(
                "fiducia-auth",
                "key_create_bad_response",
                "raw key did not contain a verifier secret",
            )
        })?;
    let new_key = store::NewApiKey {
        key_id: &issued.key.key_id,
        org_id,
        name: payload.name.trim(),
        secret_hash: hash_secret(verifier_secret),
        scopes: json!([payload.scope]),
        env: &payload.environment,
        require_idempotency: payload.require_idempotency.unwrap_or(true),
    };
    match store::insert_api_key(pool, new_key).await {
        Ok(row) => Ok((row, issued.api_key)),
        Err(error) => {
            let compensation = client
                .delete(format!(
                    "{}/v1/keys/{}",
                    auth_url.trim_end_matches('/'),
                    issued.key.key_id
                ))
                .bearer_auth(token)
                .send()
                .await;
            if compensation
                .as_ref()
                .map(|response| !response.status().is_success())
                .unwrap_or(true)
            {
                tracing::error!(
                    key_id = %issued.key.key_id,
                    "failed to compensate auth key after customer Postgres insert failure"
                );
            }
            Err(dependency_error("postgres", "api_key_insert_failed", error))
        }
    }
}

async fn rotate_customer_api_key(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Json(payload): Json<RotateCustomerApiKeyRequest>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let presented = payload.prefix.trim();
    let left = presented
        .split_once('.')
        .map(|(left, _)| left)
        .unwrap_or(presented);
    let key_id = left.rsplit('_').next().unwrap_or_default();
    if key_id.is_empty()
        || !key_id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_key_prefix", "ok": false })),
        )
            .into_response();
    }
    let Some(token) = bearer_token(&headers) else {
        return (StatusCode::UNAUTHORIZED, "missing_customer_session").into_response();
    };
    let Some(auth_url) = config.auth_url.as_deref() else {
        return dependency_error(
            "fiducia-auth",
            "key_authority_not_configured",
            "FIDUCIA_AUTH_URL is required",
        );
    };
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
    {
        Ok(client) => client,
        Err(error) => return dependency_error("fiducia-auth", "key_rotate_failed", error),
    };
    let response = match client
        .post(format!(
            "{}/v1/keys/{key_id}/rotate",
            auth_url.trim_end_matches('/')
        ))
        .bearer_auth(&token)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) if response.status() == reqwest::StatusCode::NOT_FOUND => {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({ "ok": false, "error": "key_not_found" })),
            )
                .into_response()
        }
        Ok(response) => {
            return dependency_error("fiducia-auth", "key_rotate_rejected", response.status())
        }
        Err(error) => return dependency_error("fiducia-auth", "key_rotate_failed", error),
    };
    let issued = match response.json::<AuthCreatedKeyResponse>().await {
        Ok(issued) => issued,
        Err(error) => return dependency_error("fiducia-auth", "key_rotate_bad_response", error),
    };
    if issued.key.key_id != key_id || !customer.orgs.iter().any(|org| org == &issued.key.org_id) {
        return dependency_error(
            "fiducia-auth",
            "key_rotate_contract_mismatch",
            "auth response did not match the requested key or customer organization",
        );
    }
    let verifier_secret = match issued.api_key.split_once('.') {
        Some((_, secret)) => secret,
        None => {
            return dependency_error(
                "fiducia-auth",
                "key_rotate_bad_response",
                "raw key did not contain a verifier secret",
            )
        }
    };
    let updated = store::rotate_secret(
        pool,
        key_id,
        hash_secret(verifier_secret),
        &customer.org_uuids(),
    )
    .await;
    if !matches!(updated, Ok(Some(_))) {
        let compensation = client
            .delete(format!(
                "{}/v1/keys/{key_id}",
                auth_url.trim_end_matches('/')
            ))
            .bearer_auth(&token)
            .send()
            .await;
        if compensation
            .as_ref()
            .map(|response| !response.status().is_success())
            .unwrap_or(true)
        {
            tracing::error!(
                key_id,
                "failed to revoke authoritative key after relational rotation failure"
            );
        }
        return match updated {
            Ok(None) => (
                StatusCode::NOT_FOUND,
                Json(json!({ "ok": false, "error": "key_not_found" })),
            )
                .into_response(),
            Err(error) => dependency_error("postgres", "api_key_rotate_failed", error),
            Ok(Some(_)) => unreachable!(),
        };
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "prefix": format!("fdc_{}_{}", issued.key.env, key_id),
            "rotated_at_ms": unix_epoch_ms(),
            "replacement_secret": issued.api_key,
            "overlap_seconds": 0,
        })),
    )
        .into_response()
}

/// The @fiducia/sync write path, generic in `{table}` (only `api_keys` is DB-wired
/// today). Persists the queued optimistic write and returns the committed row
/// version (a shared `WriteAck`) so the client adopts it and clears `dirty`.
/// Other clients reconcile through tenant-scoped catch-up or Supabase RLS.
async fn sync_write(
    State(config): State<AppConfig>,
    Path(table): Path<String>,
    headers: HeaderMap,
    Json(req): Json<SyncWriteRequest>,
) -> Response {
    // Authenticate before any state read/write: the sync path is a row-mutating
    // surface and was previously an unauthenticated IDOR into api_keys.
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(c) => c,
        Err(e) => return e,
    };
    if table != "api_keys" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": "unsupported_sync_table", "table": table })),
        )
            .into_response();
    }
    // Idempotency: replay the original ack for a retried key instead of re-running
    // the UPDATE (whose trigger would re-bump `version`). Matches the client's
    // stable `Idempotency-Key: table:id:op:base_version`.
    let idem_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(|key| scoped_idempotency_key(&ctx, &table, key, &req));
    if let Some(key) = &idem_key {
        match idempotency_begin(&config, key).await {
            Ok(Idem::Replay(v)) => return ack(&req.id, v).into_response(),
            Ok(Idem::InFlight) => {
                return (
                    StatusCode::CONFLICT,
                    Json(json!({ "ok": false, "error": "idempotency_in_flight" })),
                )
                    .into_response()
            }
            Ok(Idem::Proceed) => {}
            Err(err) => return dependency_error("postgres", "idempotency_claim_failed", err),
        }
    }

    // Scoped to the caller's org so a client can only mutate its own rows.
    let version = sync_write_api_keys_row(&config, &req, &ctx).await;
    let version = match version {
        Ok(version) => version,
        Err(error) => {
            if let Some(key) = &idem_key {
                if let Err(release_error) = idempotency_release(&config, key).await {
                    tracing::error!(error = %release_error, "failed to release unsuccessful idempotency claim");
                }
            }
            match error {
                SyncMutationError::InvalidId => {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "ok": false, "error": "invalid_row_id" })),
                    )
                        .into_response()
                }
                SyncMutationError::NoOrg => {
                    return (
                        StatusCode::FORBIDDEN,
                        Json(json!({ "ok": false, "error": "no_org_membership" })),
                    )
                        .into_response()
                }
                SyncMutationError::NotFound => {
                    return (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "ok": false, "error": "row_not_found" })),
                    )
                        .into_response()
                }
                SyncMutationError::Database(err) => {
                    return dependency_error("postgres", "sync_write_failed", err)
                }
            }
        }
    };

    if let Some(key) = &idem_key {
        if let Err(err) = idempotency_commit(&config, key, version).await {
            return dependency_error("postgres", "idempotency_commit_failed", err);
        }
    }
    ack(&req.id, version).into_response()
}

/// Namespace client-provided idempotency keys by authenticated identity, org set,
/// endpoint, and request body. The database never stores attacker-controlled key
/// text directly, and one tenant cannot claim another tenant's retry key.
fn scoped_idempotency_key(
    ctx: &CustomerCtx,
    table: &str,
    client_key: &str,
    request: &SyncWriteRequest,
) -> String {
    use sha2::{Digest, Sha256};

    let mut orgs = ctx.orgs.clone();
    orgs.sort();
    orgs.dedup();
    let request_json = serde_json::to_vec(request).unwrap_or_default();
    let mut digest = Sha256::new();
    digest.update(client_key.as_bytes());
    digest.update([0]);
    digest.update(request_json);
    format!(
        "v2:{}:{}:{table}:{:x}",
        ctx.user_id,
        orgs.join(","),
        digest.finalize()
    )
}

/// Idempotency decision for a claimed/seen key.
enum Idem {
    /// A previously-committed write — replay this version.
    Replay(i64),
    /// A concurrent claim is still in-flight — do not re-run.
    InFlight,
    /// We own the durable key — run the mutation.
    Proceed,
}

/// Begin idempotent handling of `key` in the durable ledger.
async fn idempotency_begin(config: &AppConfig, key: &str) -> Result<Idem, DbErr> {
    let pool = config
        .pool
        .as_ref()
        .ok_or_else(|| DbErr::Custom("customer database unavailable".to_string()))?;
    if store::idem_claim(pool, key).await? {
        return Ok(Idem::Proceed);
    }
    Ok(match store::idem_committed(pool, key).await? {
        Some(Some(version)) => Idem::Replay(version),
        Some(None) => Idem::InFlight,
        None => Idem::InFlight,
    })
}

/// Record the committed version for `key` in the durable ledger.
async fn idempotency_commit(config: &AppConfig, key: &str, version: i64) -> Result<(), DbErr> {
    let pool = config
        .pool
        .as_ref()
        .ok_or_else(|| DbErr::Custom("customer database unavailable".to_string()))?;
    store::idem_record(pool, key, version).await
}

/// Release a claim when the protected mutation did not commit. Stale in-flight
/// claims are also recoverable in the store, covering process crashes.
async fn idempotency_release(config: &AppConfig, key: &str) -> Result<(), DbErr> {
    let pool = config
        .pool
        .as_ref()
        .ok_or_else(|| DbErr::Custom("customer database unavailable".to_string()))?;
    store::idem_release(pool, key).await
}

#[derive(Debug, Deserialize)]
struct CatchupParams {
    /// Return rows with `version` strictly greater than this cursor (default 0).
    #[serde(default)]
    since: i64,
}

/// Catch-up hydration: `GET /api/customer/sync/{table}?since=<version>` returns the
/// authoritative rows newer than the client's cursor (org-scoped, ordered by
/// version, index-backed) so anything missed while offline reconciles. Feeds the
/// SDK's `hydrate()`.
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
            let pool = match customer_pool(&config) {
                Ok(pool) => pool,
                Err(response) => return response,
            };
            let orgs = ctx.org_uuids();
            if orgs.is_empty() {
                vec![]
            } else {
                match store::catchup_api_keys(pool, &orgs, params.since, 500).await {
                    Ok(rows) => rows.iter().map(api_key_row_to_display).collect(),
                    Err(err) => return dependency_error("postgres", "sync_catchup_failed", err),
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
    Json(json!({ "table": table, "since": params.since, "rows": rows })).into_response()
}

/// Build the shared write-ack the @fiducia/sync client reconciles against.
fn ack(id: &str, committed_version: i64) -> (StatusCode, Json<WriteAck>) {
    (
        StatusCode::OK,
        Json(WriteAck {
            id: id.to_string(),
            committed_version,
        }),
    )
}

/// Persist one queued optimistic write to `api_keys`. Returns the committed row
/// version or a concrete error.
enum SyncMutationError {
    InvalidId,
    NoOrg,
    NotFound,
    Database(DbErr),
}

async fn sync_write_api_keys_row(
    config: &AppConfig,
    req: &SyncWriteRequest,
    ctx: &CustomerCtx,
) -> Result<i64, SyncMutationError> {
    let pool = config.pool.as_ref().ok_or_else(|| {
        SyncMutationError::Database(DbErr::Custom("customer database unavailable".to_string()))
    })?;
    let id = Uuid::parse_str(&req.id).map_err(|_| SyncMutationError::InvalidId)?;
    let op = req.op.as_deref().unwrap_or("upsert");
    // Every mutation is scoped to the caller's org(s); a row in another tenant's
    // org yields no match (Option::None), so this is not a cross-tenant IDOR.
    let orgs = ctx.org_uuids();
    if orgs.is_empty() {
        return Err(SyncMutationError::NoOrg);
    }

    let committed = if op == "delete" {
        // A delete on a revocable credential is a soft revoke, not a row drop, so
        // audit/history stay intact. Version still bumps.
        store::soft_delete(pool, id, &orgs).await
    } else {
        let payload = req.payload.clone().unwrap_or_else(|| json!({}));
        let revoked = match payload.get("status").and_then(|v| v.as_str()) {
            Some("revoked") => Some(true),
            Some(_) => Some(false),
            None => payload.get("revoked").and_then(|v| v.as_bool()),
        };
        let patch = store::ApiKeyPatch {
            name: payload
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
            scopes: payload_scopes(&payload),
            env: payload
                .get("environment")
                .and_then(|v| v.as_str())
                .or_else(|| payload.get("env").and_then(|v| v.as_str()))
                .map(str::to_owned),
            revoked,
        };
        // COALESCE keeps existing values for any field the client omitted; the
        // trigger bumps version + updated_at on the UPDATE.
        store::upsert_fields(pool, id, &orgs, patch).await
    };

    match committed {
        Ok(Some(row)) => Ok(row.version),
        Ok(None) => Err(SyncMutationError::NotFound),
        Err(err) => Err(SyncMutationError::Database(err)),
    }
}

/// SHA-256 of an API-key secret. Only the hash is ever persisted — the plaintext
/// secret is shown to the caller once and never stored.
fn hash_secret(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    format!("sha256:{digest:x}")
}

/// Normalize a client `scopes` field to a jsonb array, or `None` to leave it
/// untouched. The display row stores scopes as a comma string; the column is an
/// array — accept either shape.
fn payload_scopes(payload: &serde_json::Value) -> Option<serde_json::Value> {
    match payload.get("scopes") {
        Some(serde_json::Value::Array(items)) => Some(serde_json::Value::Array(items.clone())),
        Some(serde_json::Value::String(csv)) => {
            let items: Vec<serde_json::Value> = csv
                .split(',')
                .map(|part| part.trim())
                .filter(|part| !part.is_empty())
                .map(|part| json!(part))
                .collect();
            Some(json!(items))
        }
        _ => None,
    }
}

/// Map a DB row to the display shape the frontend renders (and stores in
/// IndexedDB). `prefix` is the public `key_id`; scopes collapse to a comma string.
fn api_key_row_to_display(row: &ApiKeysRow) -> serde_json::Value {
    let scopes = row
        .scopes
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();

    json!({
        "id": row.id.to_string(),
        "name": row.name,
        "prefix": format!("fdc_{}_{}", row.env, row.key_id),
        "scopes": scopes,
        "last_used": if row.last_used_at.is_some() { "recently" } else { "never" },
        "status": if row.revoked { "revoked" } else { "active" },
        "environment": row.env,
        "require_idempotency": row.require_idempotency,
        "version": row.version,
    })
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

async fn customer_preferences_fragment(
    State(config): State<AppConfig>,
    headers: HeaderMap,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    preferences_fragment_markup(&config, &customer, false).await
}

async fn update_customer_preferences_form(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<CustomerPreferencesForm>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    if !CUSTOMER_REGIONS.contains(&form.region.as_str()) {
        return (StatusCode::BAD_REQUEST, "invalid_region").into_response();
    }
    if !["comfortable", "compact"].contains(&form.density.as_str()) {
        return (StatusCode::BAD_REQUEST, "invalid_density").into_response();
    }
    let user_id = match caller_user_id(&config, &customer).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let row = match store::upsert_preferences(
        pool,
        user_id,
        form.region,
        form.timezone,
        form.density,
        form.notify_key_rotation.is_some(),
        form.notify_lock_contention.is_some(),
        form.notify_mfa.is_some(),
    )
    .await
    {
        Ok(row) => row,
        Err(error) => return dependency_error("postgres", "preferences_write_failed", error),
    };
    preferences_form_markup(&prefs_from_row(&row), true).into_response()
}

async fn preferences_fragment_markup(
    config: &AppConfig,
    customer: &CustomerCtx,
    saved: bool,
) -> Response {
    let user_id = match caller_user_id(config, customer).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let pool = match customer_pool(config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let preferences = match store::get_preferences(pool, user_id).await {
        Ok(Some(row)) => prefs_from_row(&row),
        Ok(None) => default_customer_preferences(),
        Err(error) => return dependency_error("postgres", "preferences_read_failed", error),
    };
    preferences_form_markup(&preferences, saved).into_response()
}

async fn customer_sessions_fragment(
    State(config): State<AppConfig>,
    headers: HeaderMap,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    sessions_fragment_markup(&config, &customer, None).await
}

async fn revoke_customer_session_form(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<RevokeCustomerSessionForm>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    let device = form.device.trim();
    if device.is_empty() {
        return (StatusCode::BAD_REQUEST, "device_required").into_response();
    }
    let user_id = match caller_user_id(&config, &customer).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let message = match store::revoke_session(pool, user_id, device).await {
        Ok(true) => Some("Session revoked."),
        Ok(false) => Some("Session was already revoked or no longer exists."),
        Err(error) => return dependency_error("postgres", "session_revoke_failed", error),
    };
    sessions_fragment_markup(&config, &customer, message).await
}

async fn sessions_fragment_markup(
    config: &AppConfig,
    customer: &CustomerCtx,
    message: Option<&str>,
) -> Response {
    let user_id = match caller_user_id(config, customer).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let pool = match customer_pool(config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let sessions = match store::list_sessions(pool, user_id).await {
        Ok(sessions) => sessions,
        Err(error) => return dependency_error("postgres", "sessions_list_failed", error),
    };
    sessions_table_markup(&sessions, message).into_response()
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
        return customer_page_response(&config, &headers, CustomerTab::Dashboard).await;
    }

    match tokio::fs::read_to_string(config.static_dir.join("index.html")).await {
        Ok(body) => Html(body).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "static index not found").into_response(),
    }
}

async fn customer_home(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Dashboard).await
}

async fn customer_auth(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Auth).await
}

async fn customer_api_keys(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::ApiKeys).await
}

async fn customer_security(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Security).await
}

async fn customer_settings(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Settings).await
}

async fn customer_locks(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Locks).await
}

async fn customer_requests(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Requests).await
}

async fn customer_kv(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Kv).await
}

async fn customer_services(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    customer_page_response(&config, &headers, CustomerTab::Services).await
}

async fn create_customer_api_key_form(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<CreateCustomerApiKeyForm>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    let payload = CreateCustomerApiKeyRequest {
        name: form.name,
        environment: form.environment,
        scope: form.scope,
        require_idempotency: Some(true),
    };
    let token = bearer_token(&headers);
    let (_row, secret) =
        match issue_customer_api_key(&config, &customer, token.as_deref(), &payload).await {
            Ok(issued) => issued,
            Err(response) => return response,
        };
    api_keys_fragment_markup(&config, &customer, Some(&secret)).await
}

async fn api_keys_fragment(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    api_keys_fragment_markup(&config, &customer, None).await
}

async fn api_keys_fragment_markup(
    config: &AppConfig,
    customer: &CustomerCtx,
    secret: Option<&str>,
) -> Response {
    let pool = match customer_pool(config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let keys = match store::list_api_keys(pool, &customer.org_uuids()).await {
        Ok(keys) => keys,
        Err(error) => return dependency_error("postgres", "api_keys_list_failed", error),
    };
    api_keys_table_markup(&keys, secret).into_response()
}

fn api_keys_table_markup(keys: &[ApiKeysRow], secret: Option<&str>) -> Markup {
    html! {
        @if let Some(secret) = secret {
            section class="panel secret-once" role="status" {
                h2 { "Copy this secret now" }
                code { (secret) }
                p class="muted" { "The plaintext is shown once. Only its SHA-256 verifier is stored." }
            }
        }
        section class="panel" aria-labelledby="api-keys-heading" {
            div class="panel__header" {
                h2 id="api-keys-heading" { "Customer API keys" }
                span { (keys.len()) " total" }
            }
            div class="table-wrap" {
                table {
                    thead {
                        tr {
                            th { "Name" }
                            th { "Prefix" }
                            th { "Environment" }
                            th { "Scopes" }
                            th { "State" }
                        }
                    }
                    tbody {
                        @if keys.is_empty() {
                            tr { td colspan="5" class="muted" { "No API keys yet." } }
                        } @else {
                            @for key in keys {
                                tr {
                                    td { (&key.name) }
                                    td { code { (format!("fdc_{}_{}", key.env, key.key_id)) } }
                                    td { (&key.env) }
                                    td { code { (key.scopes.to_string()) } }
                                    td { @if key.revoked { "revoked" } @else { "active" } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

async fn customer_page_response(
    config: &AppConfig,
    headers: &HeaderMap,
    active: CustomerTab,
) -> Response {
    match config.authenticator.authenticate(headers).await {
        Ok(customer) => customer_page(config, &customer, active).into_response(),
        Err(response) if response.status() == StatusCode::UNAUTHORIZED => {
            (StatusCode::SEE_OTHER, [(header::LOCATION, "/login")]).into_response()
        }
        Err(response) => response,
    }
}

async fn summary_fragment(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    protected_fragment(&config, &headers, summary_markup()).await
}

async fn locks_fragment(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    protected_fragment(&config, &headers, locks_markup()).await
}

async fn requests_fragment(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    protected_fragment(&config, &headers, requests_markup()).await
}

async fn kv_fragment(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    protected_fragment(&config, &headers, kv_markup()).await
}

async fn services_fragment(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    protected_fragment(&config, &headers, services_markup()).await
}

async fn protected_fragment(config: &AppConfig, headers: &HeaderMap, fragment: Markup) -> Response {
    match config.authenticator.authenticate(headers).await {
        Ok(_) => fragment.into_response(),
        Err(response) => response,
    }
}

async fn customer_ws(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    if let Err(response) = config.authenticator.authenticate(&headers).await {
        return response;
    }
    ws.on_upgrade(customer_ws_stream)
}

async fn customer_events(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    if let Err(response) = config.authenticator.authenticate(&headers).await {
        return response;
    }
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

    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(STREAM_HEARTBEAT_SECS))
                .text("keepalive"),
        )
        .into_response()
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
        "fragments": {
            "summary": summary_markup().into_string(),
            "locks": locks_markup().into_string(),
            "requests": requests_markup().into_string(),
            "kv": kv_markup().into_string(),
            "services": services_markup().into_string(),
        },
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
    Locks,
    Requests,
    Kv,
    Services,
}

impl CustomerTab {
    fn all() -> [CustomerTab; 9] {
        [
            CustomerTab::Dashboard,
            CustomerTab::Auth,
            CustomerTab::ApiKeys,
            CustomerTab::Security,
            CustomerTab::Settings,
            CustomerTab::Locks,
            CustomerTab::Requests,
            CustomerTab::Kv,
            CustomerTab::Services,
        ]
    }

    fn href(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "/app",
            CustomerTab::Auth => "/app/auth",
            CustomerTab::ApiKeys => "/app/api-keys",
            CustomerTab::Security => "/app/security",
            CustomerTab::Settings => "/app/settings",
            CustomerTab::Locks => "/app/locks",
            CustomerTab::Requests => "/app/requests",
            CustomerTab::Kv => "/app/kv",
            CustomerTab::Services => "/app/services",
        }
    }

    fn label(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "Dashboard",
            CustomerTab::Auth => "Account",
            CustomerTab::ApiKeys => "API Keys",
            CustomerTab::Security => "Security",
            CustomerTab::Settings => "Settings",
            CustomerTab::Locks => "Locks",
            CustomerTab::Requests => "Requests",
            CustomerTab::Kv => "Config KV",
            CustomerTab::Services => "Services",
        }
    }

    fn description(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "Account posture, API access, realtime health, and customer operations in one workspace.",
            CustomerTab::Auth => "Your Supabase identity, verified organization membership, and isolated customer session.",
            CustomerTab::ApiKeys => "Create, rotate, scope, and audit customer API keys for production integrations.",
            CustomerTab::Security => "Two-factor authentication, trusted sessions, recovery, and account protection.",
            CustomerTab::Settings => "Preferences, notifications, default region, and team-level customer settings.",
            CustomerTab::Locks => "Live distributed lock state, fencing tokens, and wait queues.",
            CustomerTab::Requests => "Recent mutating requests, routing outcomes, idempotency posture, and leader decisions.",
            CustomerTab::Kv => "Configuration KV revisions, TTL status, and regional consistency.",
            CustomerTab::Services => "Service discovery registrations, leader health, and instance counts.",
        }
    }
}

fn customer_page(config: &AppConfig, customer: &CustomerCtx, active: CustomerTab) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { "Fiducia Customer Portal" }
                link rel="stylesheet" href="/assets/customer.css";
                script src="/assets/htmx.min.js" defer {}
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
                            span class="status-pill" data-status="online" { "verified" }
                            span class="status-pill" { (customer.email.as_deref().unwrap_or(&customer.user_id)) }
                            form method="post" action="/logout" {
                                button type="submit" { "Sign out" }
                            }
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
                                            }
                                        } @else {
                                            a href=(tab.href()) {
                                                span { (tab.label()) }
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
                            (customer_tab_content(config, customer, active))
                        }
                    }
                }
            }
        }
    }
}

fn customer_tab_content(config: &AppConfig, customer: &CustomerCtx, active: CustomerTab) -> Markup {
    match active {
        CustomerTab::Dashboard => dashboard_markup(config, customer),
        CustomerTab::Auth => auth_markup(customer),
        CustomerTab::ApiKeys => api_keys_markup(),
        CustomerTab::Security => security_markup(),
        CustomerTab::Settings => settings_markup(),
        CustomerTab::Locks => html! {
            div class="panel-grid" {
                section id="locks-panel" class="panel" hx-get="/app/fragments/locks" hx-trigger="load, every 15s" hx-swap="innerHTML" {
                    (locks_markup())
                }
                (realtime_events_markup())
            }
        },
        CustomerTab::Requests => html! {
            section id="requests-panel" class="panel" hx-get="/app/fragments/requests" hx-trigger="load, every 15s" hx-swap="innerHTML" {
                (requests_markup())
            }
        },
        CustomerTab::Kv => html! {
            section id="kv-panel" class="panel" hx-get="/app/fragments/kv" hx-trigger="load, every 15s" hx-swap="innerHTML" {
                (kv_markup())
            }
        },
        CustomerTab::Services => html! {
            section id="services-panel" class="panel" hx-get="/app/fragments/services" hx-trigger="load, every 15s" hx-swap="innerHTML" {
                (services_markup())
            }
        },
    }
}

fn dashboard_markup(config: &AppConfig, customer: &CustomerCtx) -> Markup {
    html! {
        section id="summary" hx-get="/app/fragments/summary" hx-trigger="load, every 15s" hx-swap="innerHTML" {
            (summary_markup())
        }
        div class="panel-grid panel-grid--dashboard" {
            (auth_status_panel(config, customer))
            (api_key_summary_panel())
            (security_summary_panel())
            (preferences_summary_panel())
        }
        div class="panel-grid" {
            section id="locks-panel" class="panel" hx-get="/app/fragments/locks" hx-trigger="load, every 15s" hx-swap="innerHTML" {
                (locks_markup())
            }
            (realtime_events_markup())
        }
        section id="requests-panel" class="panel" hx-get="/app/fragments/requests" hx-trigger="load, every 15s" hx-swap="innerHTML" {
            (requests_markup())
        }
        section id="kv-panel" class="panel" hx-get="/app/fragments/kv" hx-trigger="load, every 15s" hx-swap="innerHTML" {
            (kv_markup())
        }
        section id="services-panel" class="panel" hx-get="/app/fragments/services" hx-trigger="load, every 15s" hx-swap="innerHTML" {
            (services_markup())
        }
    }
}

fn auth_status_panel(config: &AppConfig, customer: &CustomerCtx) -> Markup {
    let supabase_state =
        if config.supabase_url.is_some() && config.supabase_publishable_key.is_some() {
            "configured"
        } else {
            "missing env"
        };
    let project_url = config.supabase_url.as_deref().unwrap_or("not configured");

    html! {
        section class="panel" aria-labelledby="auth-status-heading" {
            div class="panel__header" {
                h2 id="auth-status-heading" { "Supabase Auth" }
                span data-auth-status="" { "verified" }
            }
            div class="panel-body stack" {
                div class="identity-row" {
                    div {
                        p class="eyebrow" { "Customer session" }
                        p class="identity-row__primary" data-auth-email="" { (customer.email.as_deref().unwrap_or(&customer.user_id)) }
                    }
                    (status_tag(supabase_state))
                }
                p class="muted" { "Project: " span class="mono" { (project_url) } }
                div class="action-row" {
                    a class="button-link" href="/app/auth" { "Account" }
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
                span { "Postgres-backed" }
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
                        dd { "rotation invalidates the previous secret immediately" }
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
                span { "Supabase-managed" }
            }
            div class="panel-body stack" {
                p class="muted" { "Supabase owns MFA and passkey enrollment; trusted session records are loaded from the customer database." }
                dl class="detail-list" {
                    div {
                        dt { "Identity" }
                        dd { "verified by fiducia-auth" }
                    }
                    div {
                        dt { "Enrollment state" }
                        dd { "not guessed by this service" }
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
                span { "Postgres-backed" }
            }
            div class="panel-body stack" {
                p class="muted" { "Set default region, alert cadence, timezone, and customer-visible notifications." }
                p { "Values are rendered from the authenticated user's persisted row." }
                a class="button-link" href="/app/settings" { "Open settings" }
            }
        }
    }
}

fn auth_markup(customer: &CustomerCtx) -> Markup {
    html! {
        section class="panel" aria-labelledby="customer-session-heading" {
            div class="panel__header" {
                h2 id="customer-session-heading" { "Customer identity" }
                span class="status-pill" data-status="online" { "verified" }
            }
            div class="split-panel" {
                div class="session-box" {
                    p class="eyebrow" { "Supabase user" }
                    p class="identity-row__primary" { (customer.email.as_deref().unwrap_or(&customer.user_id)) }
                    p class="muted" { "Verified by fiducia-auth on this request." }
                }
                div class="session-box" {
                    p class="eyebrow" { "Organization membership" }
                    @for org in &customer.orgs {
                        code { (org) }
                    }
                }
            }
            form method="post" action="/logout" hx-post="/logout" {
                button type="submit" { "Sign out" }
            }
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
            form class="form-grid form-grid--inline" method="post" action="/app/api-keys"
                hx-post="/app/api-keys" hx-target="#api-key-results" hx-swap="innerHTML" {
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
                button type="submit" { "Create key" }
            }
        }
        div id="api-key-results" hx-get="/app/fragments/api-keys" hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading customer API keys…" }
        }
    }
}

fn security_markup() -> Markup {
    html! {
        section class="panel" aria-labelledby="auth-security-heading" {
            div class="panel__header" {
                h2 id="auth-security-heading" { "Supabase account security" }
                span { "provider managed" }
            }
            p class="muted" {
                "MFA and passkeys are managed by Supabase Auth. This application does not display guessed enrollment state; production-key policy will only claim enforcement after fiducia-auth exposes a verified assurance level."
            }
        }
        div id="security-sessions" hx-get="/app/fragments/security-sessions" hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading trusted sessions…" }
        }
    }
}

fn sessions_table_markup(
    sessions: &[fiducia_interfaces_db::customer::CustomerSessionsRow],
    message: Option<&str>,
) -> Markup {
    html! {
        section class="panel" aria-labelledby="sessions-heading" {
            div class="panel__header" {
                h2 id="sessions-heading" { "Trusted sessions" }
                span { (sessions.len()) " recorded" }
            }
            @if let Some(message) = message {
                p class="inline-message" role="status" { (message) }
            }
            div class="table-wrap" {
                table {
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
                        @if sessions.is_empty() {
                            tr { td colspan="5" class="muted" { "No trusted sessions have been recorded." } }
                        } @else {
                            @for session in sessions {
                                tr {
                                    td { (&session.device) }
                                    td { (session.location.as_deref().unwrap_or("unknown")) }
                                    td { (session.last_seen.to_rfc3339()) }
                                    td { (&session.status) }
                                    td {
                                        @if session.status != "revoked" {
                                            form method="post" action="/app/security/sessions/revoke"
                                                hx-post="/app/security/sessions/revoke"
                                                hx-target="#security-sessions"
                                                hx-swap="innerHTML" {
                                                input type="hidden" name="device" value=(&session.device);
                                                button type="submit" { "Revoke" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn settings_markup() -> Markup {
    html! {
        div id="customer-preferences" hx-get="/app/fragments/preferences" hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading persisted preferences…" }
        }
    }
}

fn preferences_form_markup(preferences: &CustomerPreferences, saved: bool) -> Markup {
    html! {
        section class="panel" aria-labelledby="preferences-heading" {
            div class="panel__header" {
                h2 id="preferences-heading" { "Preferences" }
                span { "Postgres-backed" }
            }
            @if saved {
                p class="inline-message" role="status" { "Preferences saved." }
            }
            form class="settings-grid" method="post" action="/app/settings"
                hx-post="/app/settings" hx-target="#customer-preferences" hx-swap="innerHTML" {
                label class="form-field" {
                    span { "Default region" }
                    select name="region" {
                        @for region in CUSTOMER_REGIONS {
                            option value=(*region) selected[preferences.region == *region] { (*region) }
                        }
                    }
                }
                label class="form-field" {
                    span { "Timezone" }
                    input name="timezone" value=(&preferences.timezone) required;
                }
                label class="form-field" {
                    span { "Dashboard density" }
                    select name="density" {
                        option value="comfortable" selected[preferences.density == "comfortable"] { "Comfortable" }
                        option value="compact" selected[preferences.density == "compact"] { "Compact" }
                    }
                }
                fieldset class="toggle-group" {
                    legend { "Notifications" }
                    label class="checkbox-line" {
                        input type="checkbox" name="notify_lock_contention" value="1"
                            checked[preferences.notify_lock_contention];
                        span { "Lock contention" }
                    }
                    label class="checkbox-line" {
                        input type="checkbox" name="notify_key_rotation" value="1"
                            checked[preferences.notify_key_rotation];
                        span { "API key rotation" }
                    }
                    label class="checkbox-line" {
                        input type="checkbox" name="notify_mfa" value="1"
                            checked[preferences.notify_mfa];
                        span { "MFA changes" }
                    }
                }
                button type="submit" { "Save preferences" }
            }
        }
    }
}

fn realtime_events_markup() -> Markup {
    html! {
        section class="panel" aria-labelledby="events-heading" {
            div class="panel__header" {
                h2 id="events-heading" { "Refresh channel" }
                span { "HTMX" }
            }
            div id="realtime-events" class="event-stream" aria-live="polite" {
                div class="empty-state" { "Authenticated fragments refresh from this Rust server every 15 seconds." }
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
                p class="metric__hint" { "loaded from customer PostgreSQL after sign-in" }
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
                p class="metric__label" { "Coordination telemetry" }
                p class="metric__value" { "scoped" }
                p class="metric__hint" { "hidden until node observability is tenant-aware" }
            }
        }
    }
}

fn locks_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Locks" }
            span { "unavailable" }
        }
        div class="empty-state" {
            "Lock inventory is not shown because the node observability API is cluster-wide, not customer-scoped."
        }
    }
}

fn requests_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Requests" }
            span { "unavailable" }
        }
        div class="empty-state" {
            "Per-request telemetry requires a customer-scoped audit source; invented request samples are never displayed."
        }
    }
}

fn kv_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Config KV" }
            span { "unavailable" }
        }
        div class="empty-state" {
            "KV inventory is hidden until the node can enforce an authenticated customer namespace."
        }
    }
}

fn services_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Service Discovery" }
            span { "unavailable" }
        }
        div class="empty-state" {
            "Service discovery is cluster-wide today and is not exposed as customer-owned data."
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

    fn test_config() -> AppConfig {
        // No pool: authenticated route tests exercise dependency failures without
        // inventing customer data or requiring a live Postgres/node deployment.
        AppConfig {
            static_dir: temp_static_dir(),
            customer_app_host: "app.fiducia.cloud".to_string(),
            customer_site_mode: false,
            supabase_url: None,
            supabase_publishable_key: None,
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
        assert_eq!(
            post_json(
                deny(),
                "/api/customer/sync/api_keys",
                r#"{"id":"x","op":"upsert"}"#
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
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

    async fn spawn_mock(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), task)
    }

    #[tokio::test]
    async fn customer_login_is_server_mediated_and_issues_only_customer_cookie() {
        let supabase = Router::new().route(
            "/auth/v1/token",
            axum::routing::post(|| async { Json(json!({ "access_token": "customer.jwt" })) }),
        );
        let auth = Router::new().route(
            "/v1/me",
            get(|| async {
                Json(json!({
                    "user": {
                        "user_id": "00000000-0000-4000-8000-000000000002",
                        "email": "customer@example.com",
                        "orgs": ["00000000-0000-4000-8000-000000000001"],
                        "roles": []
                    }
                }))
            }),
        );
        let (supabase_url, supabase_task) = spawn_mock(supabase).await;
        let (auth_url, auth_task) = spawn_mock(auth).await;
        let mut config = test_config();
        config.supabase_url = Some(supabase_url);
        config.supabase_publishable_key = Some("public-publishable-key".to_string());
        config.authenticator = Authenticator::AuthService(auth_url);
        let app = Router::new()
            .route("/login", axum::routing::post(customer_login_submit))
            .with_state(config);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from("email=customer%40example.com&password=correct"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get("location").unwrap(), "/app");
        let cookie = response
            .headers()
            .get("set-cookie")
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cookie.starts_with("fiducia_customer_session=customer.jwt"));
        assert!(cookie.contains("HttpOnly"));
        assert!(!cookie.contains("fiducia_admin_session"));
        supabase_task.abort();
        auth_task.abort();
    }

    #[tokio::test]
    async fn customer_pages_redirect_missing_sessions_to_customer_login() {
        let mut config = test_config();
        config.authenticator = Authenticator::AuthService("http://127.0.0.1:1".to_string());
        let response = build_router(config)
            .oneshot(Request::builder().uri("/app").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get("location").unwrap(), "/login");
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
        assert_eq!(v["customer_portal"]["rendering"], "maud+htmx");
        assert_eq!(v["customer_portal"]["streams"]["websocket"], "/app/ws");
        assert_eq!(v["customer_portal"]["streams"]["sse"], "/app/events");
        assert_eq!(v["customer_portal"]["supabase_login"], false);
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
        assert!(body.contains("/assets/htmx.min.js"));
        assert!(!body.contains("/_customer/"));
    }

    #[tokio::test]
    async fn app_route_serves_the_customer_portal() {
        let (status, ct, body) = send("/app").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
        assert!(body.contains("Account posture, API access"));
        assert!(body.contains("Supabase Auth"));
        assert!(body.contains("verified"));
        assert!(body.contains("test@fiducia.cloud"));
        assert!(body.contains("API Keys"));
        assert!(body.contains("Supabase-managed"));
        assert!(body.contains("Preferences"));
        assert!(body.contains("node observability API is cluster-wide"));
        assert!(!body.contains("checkout:tenant-42"));
    }

    #[tokio::test]
    async fn customer_account_routes_render_customer_controls() {
        let cases = [
            ("/app/auth", "Verified by fiducia-auth"),
            ("/app/signup", "Organization membership"),
            ("/app/api-keys", "Create API key"),
            ("/app/security", "provider managed"),
            ("/app/settings", "Loading persisted preferences"),
            ("/app/preferences", "Loading persisted preferences"),
        ];

        for (uri, needle) in cases {
            let (status, ct, body) = send(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert!(ct.contains("text/html"), "{uri} ct={ct}");
            assert!(body.contains(needle), "{uri} missing {needle}");
        }
    }

    #[tokio::test]
    async fn htmx_locks_fragment_is_rendered() {
        let (status, ct, body) = send("/app/fragments/locks").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
        assert!(body.contains("not shown"));
        assert!(!body.contains("checkout:tenant-42"));
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

    async fn post_sync(config: AppConfig, key: Option<&str>, base: i64) -> serde_json::Value {
        let app = build_router(config);
        let mut builder = Request::builder()
            .method("POST")
            .uri("/api/customer/sync/api_keys")
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(k) = key {
            builder = builder.header("idempotency-key", k);
        }
        let body = json!({ "id": "k1", "op": "upsert", "base_version": base }).to_string();
        let resp = app
            .oneshot(builder.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn sync_write_requires_durable_storage_and_rejects_unknown_tables() {
        let acked = post_sync(test_config(), None, 4).await;
        assert_eq!(acked["error"], "sync_write_failed");

        // A table with no implementation is rejected instead of acknowledging a
        // write that was never persisted.
        let app = build_router(test_config());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/customer/sync/customer_preferences")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        json!({ "id": "p1", "base_version": 0 }).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn sync_write_idempotency_requires_durable_ledger() {
        let config = test_config();
        let first = post_sync(config.clone(), Some("api_keys:k1:upsert:7"), 7).await;
        assert_eq!(first["error"], "idempotency_claim_failed");
        let retry = post_sync(config.clone(), Some("api_keys:k1:upsert:7"), 999).await;
        assert_eq!(retry["error"], "idempotency_claim_failed");
        let other = post_sync(config.clone(), Some("api_keys:k2:upsert:2"), 2).await;
        assert_eq!(other["error"], "idempotency_claim_failed");
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
        let (status, ct, body) = send("/assets/htmx.min.js").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            ct.contains("text/javascript") || ct.contains("application/javascript"),
            "ct={ct}"
        );
        assert!(body.contains("htmx"));
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
