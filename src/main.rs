// fiducia-backend entrypoint: the axum app for fiducia.cloud's website tier.
// Serves the static Astro marketing site, the Maud/HTMX customer portal and its
// WS/SSE fragment streams, plus authenticated customer APIs. API-key lifecycle
// is delegated to fiducia-auth so there is exactly one credential authority.
mod auth;
mod entity;
mod request_security;
mod store;
mod supabase_auth;

use auth::{
    bearer_token, cookie_value, Authenticator, CustomerCtx, CUSTOMER_LOGIN_CSRF_COOKIE,
    CUSTOMER_SESSION_COOKIE,
};
use supabase_auth::{required_totp_factor, OtpChannel, SupabaseAuth, SupabaseAuthError};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::Request;
use axum::extract::{Form, Path, Query, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{routing::get, Json, Router};
use maud::{html, Markup, DOCTYPE};
use request_security::{RequestSecurity, RequestSecurityError};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};
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
const HTMX_JS: &str = include_str!("../assets/htmx.min.js");
const CUSTOMER_CSS: &str = include_str!("../assets/customer.css");
/// Carries the primary-factor (aal1) Supabase token between `/login/verify` and
/// `/login/mfa` while the user completes TOTP step-up. Short-lived and cleared
/// the instant the aal2 app-session cookie is issued. Distinct from the app
/// session cookie so a verified-TOTP user is never admitted on aal1 alone.
const CUSTOMER_MFA_PENDING_COOKIE: &str = if cfg!(debug_assertions) {
    "fiducia_customer_mfa_pending"
} else {
    "__Host-fiducia_customer_mfa_pending"
};
/// Step-up must complete promptly; the pending token self-expires.
const MFA_PENDING_MAX_AGE_SECS: u64 = 300;
const CUSTOMER_ORG_HEADER: &str = "x-fiducia-org-id";
const IDEMPOTENCY_KEY_HEADER: &str = "idempotency-key";
const CUSTOMER_CSRF_HEADER: &str = "x-fiducia-csrf";
const CORS_MAX_AGE_SECS: u64 = 10 * 60;
const MAX_API_KEY_NAME_CHARS: usize = 100;
const MAX_TIMEZONE_CHARS: usize = 64;
const MAX_SESSION_DEVICE_CHARS: usize = 200;
const DEFAULT_ACTIVITY_LIMIT: u64 = 50;
const MAX_ACTIVITY_LIMIT: u64 = 100;

const CUSTOMER_REGIONS: &[&str] = &["auto", "iad1", "sfo1", "ams1", "fra1", "sin1", "syd1"];

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    fiducia_telemetry::init(SERVICE);

    // Directory of the built Astro site. Defaults to the bundled `static/`
    // (populated from fiducia-marketing.web's `dist/` at build time), but can be
    // pointed straight at the frontend dist via STATIC_DIR for local dev.
    let static_dir: PathBuf = std::env::var("STATIC_DIR")
        .unwrap_or_else(|_| "static".to_string())
        .into();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let customer_app_origin = customer_app_origin_from_env()?;
    let request_security = RequestSecurity::from_env(port)?;
    // Customer state is always durable. A missing/unreachable database is a
    // deployment error, not permission to serve invented customer data.
    let pool = connect_customer_db().await?;

    let config = AppConfig {
        static_dir: static_dir.clone(),
        customer_app_host: std::env::var("CUSTOMER_APP_HOST")
            .unwrap_or_else(|_| "app.fiducia.cloud".to_string()),
        customer_app_origin,
        customer_site_mode: std::env::var("FIDUCIA_SITE_MODE")
            .map(|v| v.eq_ignore_ascii_case("customer"))
            .unwrap_or(false),
        supabase_url: Some(required_env("SUPABASE_URL")?),
        supabase_publishable_key: Some(required_env("SUPABASE_PUBLISHABLE_KEY")?),
        auth_url: Some(required_env("FIDUCIA_AUTH_URL")?),
        pool: Some(pool),
        authenticator: Authenticator::from_env(),
        request_security,
    };

    let app = build_router(config);

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
    let scheme = uri
        .scheme_str()
        .filter(|scheme| matches!(*scheme, "http" | "https"));
    let authority = uri
        .authority()
        .filter(|authority| !authority.as_str().contains('@'));
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
            HeaderName::from_static(CUSTOMER_CSRF_HEADER),
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
    let customer_app_origin = config.customer_app_origin.clone();
    let request_security = config.request_security.clone();
    let sensitive_header_context = SensitiveHeaderContext {
        customer_app_host: config.customer_app_host.clone(),
        customer_site_mode: config.customer_site_mode,
    };
    // Routes are declared as flat literals (not nested) so the shared API-docs
    // generator (remote/tools/generate-api-docs.mjs, which scans the router's
    // route declarations) records their true paths.
    let router = Router::new()
        // Liveness/readiness probe (matches the sibling canonical.cloud
        // convention); also available as /api/health.
        .route("/healthz", get(health))
        .route("/api/health", get(health))
        .route("/api/info", get(info))
        .route("/assets/htmx.min.js", get(htmx_js))
        .route("/assets/customer.css", get(customer_css))
        .route("/login", get(customer_login).post(customer_login_submit))
        .route("/logout", axum::routing::post(customer_logout))
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
        .route("/api/customer/activity", get(customer_activity_json))
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
        .route(
            "/app/api-keys/rotate",
            axum::routing::post(rotate_customer_api_key_form),
        )
        .route(
            "/app/api-keys/revoke",
            axum::routing::post(revoke_customer_api_key_form),
        )
        .route("/app/security", get(customer_security))
        .route("/app/activity", get(customer_activity))
        .route("/app/notifications", get(customer_notifications))
        .route(
            "/app/notifications/read",
            axum::routing::post(read_customer_notification_form),
        )
        .route(
            "/app/security/sessions/revoke",
            axum::routing::post(revoke_customer_session_form),
        )
        .route(
            "/app/settings",
            get(customer_settings).post(update_customer_preferences_form),
        )
        .route("/app/preferences", get(customer_settings))
        // Keep these route paths literal so the shared API-doc generator can
        // derive the complete surface; the constants remain the security and
        // response-metadata source of truth elsewhere in this module.
        .route("/app/ws", get(customer_ws))
        .route("/app/events", get(customer_events))
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
        .route("/app/fragments/activity", get(customer_activity_fragment))
        .route(
            "/app/fragments/notifications",
            get(customer_notifications_fragment),
        )
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
        .layer(middleware::from_fn_with_state(
            request_security,
            request_security_gate,
        ))
        .layer(middleware::from_fn_with_state(
            sensitive_header_context,
            security_headers,
        ));

    match customer_app_origin {
        Some(origin) => router.layer(customer_cors(origin)),
        None => router,
    }
}

#[derive(Clone)]
struct AppConfig {
    static_dir: PathBuf,
    customer_app_host: String,
    /// Exact standalone customer origin allowed to call this service from a
    /// browser. `None` keeps the service same-origin-only.
    customer_app_origin: Option<HeaderValue>,
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
    request_security: RequestSecurity,
}

impl AppConfig {
    /// A Supabase Auth client, when both the project URL and publishable key are
    /// configured. `None` means passwordless/MFA flows are unavailable and their
    /// handlers must fail closed with `customer_login_not_configured`.
    fn supabase_auth(&self) -> Option<SupabaseAuth> {
        match (
            self.supabase_url.as_deref(),
            self.supabase_publishable_key.as_deref(),
        ) {
            (Some(url), Some(key)) => Some(SupabaseAuth::new(url, key)),
            _ => None,
        }
    }
}

/// Map a Supabase auth failure onto a rendered login response. `Rejected` is the
/// user's fault (bad/expired code) and re-renders the given page with the message
/// at 401; transport/parse failures surface as a 503 dependency error.
fn supabase_auth_error_response(
    error: SupabaseAuthError,
    retry_page: Response,
    mut retry_status: StatusCode,
) -> Response {
    match error {
        SupabaseAuthError::Invalid(reason) => {
            tracing::debug!(reason, "rejected malformed passwordless input");
            let mut page = retry_page;
            *page.status_mut() = StatusCode::BAD_REQUEST;
            page
        }
        SupabaseAuthError::Rejected(detail) => {
            tracing::info!(detail, "supabase rejected passwordless/mfa request");
            let mut page = retry_page;
            if retry_status == StatusCode::OK {
                retry_status = StatusCode::UNAUTHORIZED;
            }
            *page.status_mut() = retry_status;
            page
        }
        SupabaseAuthError::Unavailable(detail) => {
            dependency_error("supabase", "supabase_auth_unavailable", detail)
        }
    }
}

fn request_security_error(error: RequestSecurityError) -> Response {
    tracing::warn!(reason = error.code(), "rejected untrusted customer request");
    (
        StatusCode::FORBIDDEN,
        Json(json!({
            "ok": false,
            "error": "customer_request_rejected",
            "reason": error.code()
        })),
    )
        .into_response()
}

fn customer_csrf_token(config: &AppConfig, customer: &CustomerCtx) -> String {
    config.request_security.csrf_token(customer.csrf_binding())
}

fn require_form_security(
    headers: &HeaderMap,
    config: &AppConfig,
    customer: &CustomerCtx,
    provided_csrf: &str,
) -> Result<(), RequestSecurityError> {
    config.request_security.require_same_origin(headers)?;
    config
        .request_security
        .verify_csrf_token(customer.csrf_binding(), provided_csrf)
}

fn require_api_write_security(
    headers: &HeaderMap,
    config: &AppConfig,
    customer: &CustomerCtx,
) -> Result<(), RequestSecurityError> {
    if customer.is_browser_session() {
        config.request_security.require_same_origin(headers)?;
        let provided = headers
            .get(CUSTOMER_CSRF_HEADER)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        config
            .request_security
            .verify_csrf_token(customer.csrf_binding(), provided)
    } else {
        config.request_security.require_api_host(headers)
    }
}

async fn request_security_gate(
    State(security): State<RequestSecurity>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    let method = request.method();
    let browser_surface = path == "/login" || path == "/logout" || path.starts_with("/app");
    let customer_api = path.starts_with("/api/customer");
    let exact_origin_required = path == CUSTOMER_WS_PATH
        || (browser_surface && !matches!(*method, Method::GET | Method::HEAD));
    let result = if exact_origin_required {
        security.require_same_origin(request.headers())
    } else if browser_surface || customer_api {
        security.require_api_host(request.headers())
    } else {
        Ok(())
    };
    if let Err(error) = result {
        return request_security_error(error);
    }
    next.run(request).await
}

/// Harden a customer-sensitive response: never cache it (it carries the user's
/// email, org ids, and CSRF token) and pin the strict portal CSP. Applied both
/// by the path-based middleware and directly by the portal renderer, because the
/// authenticated dashboard is reachable at `/` (app host) as well as `/app*`.
fn apply_sensitive_response_headers(headers: &mut HeaderMap) {
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'; connect-src 'self'; img-src 'self' data:; style-src 'self'",
        ),
    );
    // `same-origin`, NOT `no-referrer`: under `no-referrer` a browser
    // serializes the Origin of any non-GET request (form POST, SPA fetch) as
    // `null`, so `require_same_origin` / `require_api_host` would reject every
    // real-browser mutation while hand-crafted clients that set Origin
    // themselves pass — the inversion of the intent. `same-origin` still never
    // leaks the referrer cross-origin and keeps Origin intact for the gate.
    // Proven by the real-Chromium journeys in fiducia-e2e (npm run test:browser).
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
}

/// Host/mode inputs the outermost header middleware needs to recognize when `/`
/// is serving the authenticated portal (rather than the public marketing index).
#[derive(Clone)]
struct SensitiveHeaderContext {
    customer_app_host: String,
    customer_site_mode: bool,
}

async fn security_headers(
    State(ctx): State<SensitiveHeaderContext>,
    request: Request,
    next: Next,
) -> Response {
    let path = request.uri().path();
    // `/` serves the customer dashboard on the app host (or in customer-site mode),
    // carrying the same email/org/CSRF material as `/app`, but the path prefixes
    // below don't catch it — classify it as sensitive so it is never cacheable.
    let root_is_portal = path == "/"
        && host_serves_customer_app(
            request.headers(),
            &ctx.customer_app_host,
            ctx.customer_site_mode,
        );
    let sensitive = root_is_portal
        || path == "/login"
        || path == "/logout"
        || path.starts_with("/app")
        || path.starts_with("/api/customer");
    let mut response = next.run(request).await;
    if sensitive {
        apply_sensitive_response_headers(response.headers_mut());
    }
    response
}

#[derive(Debug, Deserialize)]
struct CustomerLoginForm {
    csrf_token: String,
    email: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct SupabasePasswordSession {
    access_token: String,
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

async fn customer_login(State(config): State<AppConfig>) -> Response {
    customer_login_page(&config, None)
}

async fn customer_login_submit(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<CustomerLoginForm>,
) -> Response {
    if let Err(error) = require_login_security(&headers, &config, &form.csrf_token) {
        return request_security_error(error);
    }
    let email = form.email.trim();
    if email.is_empty() || form.password.is_empty() {
        let mut response = customer_login_page(&config, Some("Email and password are required."));
        *response.status_mut() = StatusCode::BAD_REQUEST;
        return response;
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
            let mut response =
                customer_login_page(&config, Some("Supabase rejected those credentials."));
            *response.status_mut() = StatusCode::UNAUTHORIZED;
            return response;
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

    let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, "/app")]).into_response();
    append_set_cookie(
        &mut response,
        &make_customer_session_cookie(&session.access_token),
    );
    append_set_cookie(&mut response, &clear_customer_login_csrf_cookie());
    response
}

fn customer_login_page(config: &AppConfig, message: Option<&str>) -> Response {
    let nonce = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let token = config
        .request_security
        .csrf_token(&format!("login\0{nonce}"));
    let mut response = customer_login_markup(message, &token).into_response();
    append_set_cookie(&mut response, &make_customer_login_csrf_cookie(&nonce));
    response
}

fn require_login_security(
    headers: &HeaderMap,
    config: &AppConfig,
    provided_csrf: &str,
) -> Result<(), RequestSecurityError> {
    config.request_security.require_same_origin(headers)?;
    let nonce = cookie_value(headers, CUSTOMER_LOGIN_CSRF_COOKIE)
        .ok_or(RequestSecurityError::InvalidCsrfToken)?;
    config
        .request_security
        .verify_csrf_token(&format!("login\0{nonce}"), provided_csrf)
}

fn append_set_cookie(response: &mut Response, cookie: &str) {
    response.headers_mut().append(
        header::SET_COOKIE,
        HeaderValue::from_str(cookie).expect("server-generated cookie is a valid header value"),
    );
}

fn explicitly_enabled(value: Option<&str>) -> bool {
    value.is_some_and(|value| matches!(value.trim().to_ascii_lowercase().as_str(), "1" | "true"))
}

const fn cookie_secure_suffix_for(
    release_hardened: bool,
    insecure_http_explicitly_enabled: bool,
) -> &'static str {
    if release_hardened || !insecure_http_explicitly_enabled {
        "; Secure"
    } else {
        ""
    }
}

#[cfg(debug_assertions)]
fn cookie_secure_suffix() -> &'static str {
    cookie_secure_suffix_for(
        false,
        explicitly_enabled(std::env::var("FIDUCIA_INSECURE_COOKIES").ok().as_deref()),
    )
}

#[cfg(not(debug_assertions))]
fn cookie_secure_suffix() -> &'static str {
    let insecure_requested =
        explicitly_enabled(std::env::var("FIDUCIA_INSECURE_COOKIES").ok().as_deref());
    if insecure_requested {
        tracing::error!(
            "FIDUCIA_INSECURE_COOKIES is set but IGNORED: release builds always emit Secure cookies"
        );
    }
    cookie_secure_suffix_for(true, insecure_requested)
}

fn make_customer_login_csrf_cookie(nonce: &str) -> String {
    format!(
        "{CUSTOMER_LOGIN_CSRF_COOKIE}={nonce}; Path=/; HttpOnly; SameSite=Strict; Max-Age=600{}",
        cookie_secure_suffix()
    )
}

fn clear_customer_login_csrf_cookie() -> String {
    format!(
        "{CUSTOMER_LOGIN_CSRF_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        cookie_secure_suffix()
    )
}

fn make_customer_session_cookie(token: &str) -> String {
    format!(
        "{CUSTOMER_SESSION_COOKIE}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=3600{}",
        cookie_secure_suffix()
    )
}

fn clear_customer_session_cookie() -> String {
    format!(
        "{CUSTOMER_SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0{}",
        cookie_secure_suffix()
    )
}

#[derive(Debug, Deserialize)]
struct CustomerLogoutForm {
    csrf_token: String,
}

async fn customer_logout(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<CustomerLogoutForm>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    if let Err(error) = require_form_security(&headers, &config, &customer, &form.csrf_token) {
        return request_security_error(error);
    }
    let mut response = (StatusCode::SEE_OTHER, [(header::LOCATION, "/login")]).into_response();
    append_set_cookie(&mut response, &clear_customer_session_cookie());
    response
}

/// Shared chrome for every unauthenticated auth page (login, OTP entry, MFA
/// step-up). Keeps one `head`/shell so the flows are visually one surface.
fn auth_page_shell(title: &str, inner: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) }
                link rel="stylesheet" href="/assets/customer.css";
                script src="/assets/htmx.min.js" defer {}
            }
            body {
                main class="auth-shell" {
                    section class="auth-card" {
                        (inner)
                    }
                }
            }
        }
    }
}

fn customer_login_markup(message: Option<&str>, csrf_token: &str) -> Markup {
    auth_page_shell(
        "Sign in · Fiducia Customer",
        html! {
            p class="eyebrow" { "Customer application" }
            h1 { "Sign in to Fiducia" }
            p class="muted" { "Supabase authenticates you; fiducia-auth verifies the resulting identity and organization membership." }
            @if let Some(message) = message {
                p class="auth-message" role="alert" { (message) }
            }

            // Password grant (unchanged surface).
            form method="post" action="/login" hx-post="/login" hx-target="body" hx-swap="outerHTML" {
                h2 { "Email & password" }
                input type="hidden" name="csrf_token" value=(csrf_token);
                label for="email" { "Email" }
                input id="email" name="email" type="email" autocomplete="email" required;
                label for="password" { "Password" }
                input id="password" name="password" type="password" autocomplete="current-password" required;
                button type="submit" { "Sign in" }
            }

            // Passwordless email — magic link + 6-digit code (also self-signup).
            form method="post" action="/login/otp" hx-post="/login/otp" hx-target="body" hx-swap="outerHTML" {
                h2 { "Email magic link" }
                p class="muted" { "We email a one-tap link and a 6-digit code. New here? This also creates your account." }
                input type="hidden" name="csrf_token" value=(csrf_token);
                input type="hidden" name="method" value="email";
                label for="magic-email" { "Email" }
                input id="magic-email" name="identifier" type="email" autocomplete="email" required;
                button type="submit" { "Email me a link" }
            }

            // Passwordless phone — SMS one-time passcode.
            form method="post" action="/login/otp" hx-post="/login/otp" hx-target="body" hx-swap="outerHTML" {
                h2 { "Phone code" }
                p class="muted" { "We text a 6-digit code to your phone. Use international format, e.g. +14155550123." }
                input type="hidden" name="csrf_token" value=(csrf_token);
                input type="hidden" name="method" value="phone";
                label for="otp-phone" { "Phone" }
                input id="otp-phone" name="identifier" type="tel" autocomplete="tel" inputmode="tel"
                    placeholder="+14155550123" required;
                button type="submit" { "Text me a code" }
            }

            p class="muted" { "Accounts with an authenticator app will be asked for a 6-digit code after this step." }
            p class="muted" { "Operator accounts use the separate admin application and cookie boundary." }
        },
    )
}

/// OTP-entry page shown after a code is dispatched. Carries the channel +
/// identifier forward so `/login/verify` knows how to redeem the code.
fn otp_verify_markup(
    channel: OtpChannel,
    identifier: &str,
    csrf_token: &str,
    message: Option<&str>,
) -> Markup {
    let heading = match channel {
        OtpChannel::Email => "Check your email",
        OtpChannel::Phone => "Check your phone",
    };
    let blurb = match channel {
        OtpChannel::Email => "We emailed a magic link and a 6-digit code. Enter the code, or just tap the link.",
        OtpChannel::Phone => "We texted a 6-digit code to your phone. Enter it below.",
    };
    auth_page_shell(
        "Enter your code · Fiducia Customer",
        html! {
            p class="eyebrow" { "Customer application" }
            h1 { (heading) }
            p class="muted" { (blurb) }
            p class="muted" { "Sending to " strong { (identifier) } "." }
            @if let Some(message) = message {
                p class="auth-message" role="alert" { (message) }
            }
            form method="post" action="/login/verify" hx-post="/login/verify" hx-target="body" hx-swap="outerHTML" {
                input type="hidden" name="csrf_token" value=(csrf_token);
                input type="hidden" name="method" value=(channel.field());
                input type="hidden" name="identifier" value=(identifier);
                label for="otp-code" { "6-digit code" }
                input id="otp-code" name="token" type="text" inputmode="numeric" autocomplete="one-time-code"
                    pattern="[0-9]*" minlength="6" maxlength="8" required;
                button type="submit" { "Verify & continue" }
            }
            form method="post" action="/login/otp" hx-post="/login/otp" hx-target="body" hx-swap="outerHTML" {
                input type="hidden" name="csrf_token" value=(csrf_token);
                input type="hidden" name="method" value=(channel.field());
                input type="hidden" name="identifier" value=(identifier);
                button type="submit" class="link-button" { "Resend code" }
            }
            a href="/login" { "Start over" }
        },
    )
}

/// TOTP step-up page. The primary factor already succeeded; the account has a
/// verified authenticator, so we require its current 6-digit code before issuing
/// the app session cookie. `factor_id`/`challenge_id` ride hidden fields; the
/// aal1 token rides the short-lived pending cookie.
fn mfa_challenge_markup(
    factor_id: &str,
    challenge_id: &str,
    csrf_token: &str,
    message: Option<&str>,
) -> Markup {
    auth_page_shell(
        "Two-factor verification · Fiducia Customer",
        html! {
            p class="eyebrow" { "Two-factor authentication" }
            h1 { "Enter your authenticator code" }
            p class="muted" { "Open your authenticator app (Authy, Google Authenticator, 1Password…) and enter the current 6-digit code for Fiducia." }
            @if let Some(message) = message {
                p class="auth-message" role="alert" { (message) }
            }
            form method="post" action="/login/mfa" hx-post="/login/mfa" hx-target="body" hx-swap="outerHTML" {
                input type="hidden" name="csrf_token" value=(csrf_token);
                input type="hidden" name="factor_id" value=(factor_id);
                input type="hidden" name="challenge_id" value=(challenge_id);
                label for="mfa-code" { "Authenticator code" }
                input id="mfa-code" name="code" type="text" inputmode="numeric" autocomplete="one-time-code"
                    pattern="[0-9]*" minlength="6" maxlength="8" required;
                button type="submit" { "Verify" }
            }
            a href="/login" { "Cancel and sign in again" }
        },
    )
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
    csrf_token: String,
    org_id: String,
    idempotency_key: String,
    name: String,
    environment: String,
    scope: String,
}

#[derive(Debug, Deserialize)]
struct RotateCustomerApiKeyForm {
    csrf_token: String,
    org_id: String,
    idempotency_key: String,
    prefix: String,
}

#[derive(Debug, Deserialize)]
struct RevokeCustomerApiKeyForm {
    csrf_token: String,
    org_id: String,
    idempotency_key: String,
    prefix: String,
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
    org_id: String,
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

#[derive(Debug, Deserialize)]
struct RevokeCustomerSessionForm {
    csrf_token: String,
    device: String,
}

#[derive(Debug, Deserialize)]
struct CustomerPreferencesForm {
    csrf_token: String,
    region: String,
    timezone: String,
    density: String,
    notify_lock_contention: Option<String>,
    notify_key_rotation: Option<String>,
    notify_mfa: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct CustomerOrgSelection {
    org_id: Option<String>,
}

/// Optional page size for the customer activity API. It is bounded before the
/// SeaORM query so a browser-controlled query string cannot create an unbounded
/// audit-log read.
#[derive(Debug, Default, Deserialize)]
struct CustomerActivityQuery {
    limit: Option<u16>,
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

/// Deliberately small customer-facing view of an audit record. In particular,
/// diagnostic metadata, source addresses, and user agents remain server-only.
#[derive(Clone, Debug, Serialize)]
struct CustomerAuditEvent {
    id: Uuid,
    actor: Option<String>,
    action: String,
    target: Option<String>,
    request_id: Option<String>,
    created_at: String,
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

fn no_store_json(status: StatusCode, body: serde_json::Value) -> Response {
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(body),
    )
        .into_response()
}

fn valid_org_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && !value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
}

#[allow(clippy::result_large_err)]
fn selected_customer_org_from(
    ctx: &CustomerCtx,
    headers: &HeaderMap,
    explicit: Option<&str>,
) -> Result<String, Response> {
    let requested = match explicit {
        Some(requested) => Some(requested),
        None => match headers.get(CUSTOMER_ORG_HEADER) {
            Some(requested) => Some(requested.to_str().map_err(|_| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({ "ok": false, "error": "invalid_org_selection" })),
                )
                    .into_response()
            })?),
            None => None,
        },
    };
    if let Some(requested) = requested {
        if !valid_org_id(requested) {
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

#[allow(clippy::result_large_err)]
fn selected_customer_org(ctx: &CustomerCtx, headers: &HeaderMap) -> Result<String, Response> {
    selected_customer_org_from(ctx, headers, None)
}

#[allow(clippy::result_large_err)]
fn customer_page_org(
    ctx: &CustomerCtx,
    headers: &HeaderMap,
    explicit: Option<&str>,
) -> Result<String, Response> {
    if explicit.is_some() || headers.contains_key(CUSTOMER_ORG_HEADER) || ctx.orgs.len() <= 1 {
        selected_customer_org_from(ctx, headers, explicit)
    } else {
        ctx.orgs.first().cloned().ok_or_else(|| {
            (
                StatusCode::FORBIDDEN,
                Json(json!({ "ok": false, "error": "no_org_membership" })),
            )
                .into_response()
        })
    }
}

#[allow(clippy::result_large_err)]
fn require_idempotency_key(headers: &HeaderMap) -> Result<&HeaderValue, Response> {
    let value = headers.get(IDEMPOTENCY_KEY_HEADER).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "idempotency_key_required" })),
        )
            .into_response()
    })?;
    let valid = value.to_str().is_ok_and(|value| {
        !value.is_empty()
            && value.len() <= 200
            && value.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
    });
    if !valid {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid_idempotency_key" })),
        )
            .into_response());
    }
    Ok(value)
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
    let Some(token) = bearer_token(headers) else {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "ok": false, "error": "missing_customer_session" })),
        )
            .into_response());
    };
    let mut request = reqwest::Client::new()
        .request(method, format!("{base}{path}"))
        .bearer_auth(token);
    if let Some(idempotency_key) = headers.get(IDEMPOTENCY_KEY_HEADER) {
        request = request.header(IDEMPOTENCY_KEY_HEADER, idempotency_key);
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

fn auth_key_to_display(
    key: &AuthKeyMeta,
    expected_org_id: &str,
) -> Result<serde_json::Value, &'static str> {
    if key.org_id != expected_org_id
        || !valid_key_id(&key.key_id)
        || !matches!(key.env.as_str(), "live" | "test")
        || key.version == 0
        || key
            .scopes
            .iter()
            .any(|scope| !allowed_api_key_scopes().contains(&scope.as_str()))
    {
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
    if matches!(environment, "live" | "test") && valid_key_id(key_id) {
        Some(key_id)
    } else {
        None
    }
}

fn valid_key_id(key_id: &str) -> bool {
    key_id.len() == 16
        && key_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

fn raw_api_key_matches(raw: &str, key: &AuthKeyMeta) -> bool {
    let expected_prefix = format!("fdc_{}_{}.", key.env, key.key_id);
    raw.strip_prefix(&expected_prefix).is_some_and(|secret| {
        secret.len() == 64
            && secret
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    })
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
        .map(|key| auth_key_to_display(key, &org_id))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(keys) => keys,
        Err(error) => return dependency_error("fiducia-auth", "auth_key_list_bad_response", error),
    };

    no_store_json(
        StatusCode::OK,
        json!({
            "api_keys": keys,
            "default_require_idempotency": true,
            "allowed_environments": ["live", "test"],
            "allowed_scopes": allowed_api_key_scopes(),
        }),
    )
}

async fn customer_context_json(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(ctx) => ctx,
        Err(response) => return response,
    };
    let csrf_token = ctx
        .is_browser_session()
        .then(|| customer_csrf_token(&config, &ctx));
    no_store_json(
        StatusCode::OK,
        json!({
            "csrf_token": csrf_token,
            "user": {
                "user_id": ctx.user_id,
                "email": ctx.email,
                "orgs": ctx.orgs,
            }
        }),
    )
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
    if let Err(error) = require_api_write_security(&headers, &config, &ctx) {
        return request_security_error(error);
    }
    let (display, secret) = match issue_customer_api_key(&config, &headers, &ctx, &payload).await {
        Ok(issued) => issued,
        Err(response) => return response,
    };
    no_store_json(
        StatusCode::CREATED,
        json!({
            "ok": true,
            "api_key": display,
            "secret": secret,
            "secret_once": true,
        }),
    )
}

async fn issue_customer_api_key(
    config: &AppConfig,
    headers: &HeaderMap,
    ctx: &CustomerCtx,
    payload: &CreateCustomerApiKeyRequest,
) -> Result<(serde_json::Value, String), Response> {
    require_idempotency_key(headers)?;
    if let Some(error) = validate_api_key_request(payload) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error, "ok": false })),
        )
            .into_response());
    }
    let org_id = selected_customer_org(ctx, headers)?;
    let request = json!({
        "name": payload.name.trim(),
        "org_id": &org_id,
        "scopes": [&payload.scope],
        "env": &payload.environment,
        "require_idempotency": payload.require_idempotency.unwrap_or(true),
    });
    let (status, body) = auth_json(
        config,
        headers,
        reqwest::Method::POST,
        "/v1/keys",
        Some(request),
    )
    .await?;
    if !status.is_success() {
        return Err(proxied_auth_error(status, body));
    }
    let response: AuthKeyCreateResponse = serde_json::from_value(body)
        .map_err(|error| dependency_error("fiducia-auth", "auth_key_create_bad_response", error))?;
    if !raw_api_key_matches(&response.api_key, &response.key) {
        return Err(dependency_error(
            "fiducia-auth",
            "auth_key_create_bad_response",
            "raw key does not match metadata",
        ));
    }
    let display = auth_key_to_display(&response.key, &org_id)
        .map_err(|error| dependency_error("fiducia-auth", "auth_key_create_bad_response", error))?;
    Ok((display, response.api_key))
}

async fn rotate_customer_api_key(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Json(payload): Json<RotateCustomerApiKeyRequest>,
) -> Response {
    let ctx = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    if let Err(error) = require_api_write_security(&headers, &config, &ctx) {
        return request_security_error(error);
    }
    let prefix = payload.prefix.trim();
    let (display, replacement_secret, overlap_seconds) =
        match rotate_customer_api_key_authority(&config, &headers, &ctx, prefix).await {
            Ok(rotated) => rotated,
            Err(response) => return response,
        };

    no_store_json(
        StatusCode::OK,
        json!({
            "ok": true,
            "prefix": prefix,
            "rotated_at_ms": unix_epoch_ms(),
            "replacement_secret": replacement_secret,
            "api_key": display,
            "overlap_seconds": overlap_seconds,
        }),
    )
}

async fn rotate_customer_api_key_authority(
    config: &AppConfig,
    headers: &HeaderMap,
    ctx: &CustomerCtx,
    prefix: &str,
) -> Result<(serde_json::Value, String, u64), Response> {
    require_idempotency_key(headers)?;
    let Some(key_id) = auth_key_id_from_prefix(prefix) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_key_prefix", "ok": false })),
        )
            .into_response());
    };

    let org_id = match selected_customer_org(ctx, headers) {
        Ok(org_id) => org_id,
        Err(response) => return Err(response),
    };
    let path = format!(
        "/v1/keys/{}/rotate?org_id={}",
        encode_query_value(key_id),
        encode_query_value(&org_id)
    );
    let (status, body) = match auth_json(
        config,
        headers,
        reqwest::Method::POST,
        &path,
        Some(json!({})),
    )
    .await
    {
        Ok(result) => result,
        Err(response) => return Err(response),
    };
    if !status.is_success() {
        return Err(proxied_auth_error(status, body));
    }
    let response: AuthKeyRotateResponse = match serde_json::from_value(body) {
        Ok(response) => response,
        Err(error) => {
            return Err(dependency_error(
                "fiducia-auth",
                "auth_key_rotate_bad_response",
                error,
            ))
        }
    };
    if !raw_api_key_matches(&response.api_key, &response.key) {
        return Err(dependency_error(
            "fiducia-auth",
            "auth_key_rotate_bad_response",
            "raw key does not match metadata",
        ));
    }
    let display = match auth_key_to_display(&response.key, &org_id) {
        Ok(display) => display,
        Err(error) => {
            return Err(dependency_error(
                "fiducia-auth",
                "auth_key_rotate_bad_response",
                error,
            ))
        }
    };
    Ok((display, response.api_key, response.overlap_seconds))
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
    if let Err(error) = require_api_write_security(&headers, &config, &ctx) {
        return request_security_error(error);
    }
    let prefix = payload.prefix.trim();
    if let Err(response) = revoke_customer_api_key_authority(&config, &headers, &ctx, prefix).await
    {
        return response;
    }
    no_store_json(
        StatusCode::OK,
        json!({ "ok": true, "prefix": prefix, "status": "revoked" }),
    )
}

async fn revoke_customer_api_key_authority(
    config: &AppConfig,
    headers: &HeaderMap,
    ctx: &CustomerCtx,
    prefix: &str,
) -> Result<(), Response> {
    require_idempotency_key(headers)?;
    let Some(key_id) = auth_key_id_from_prefix(prefix) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_key_prefix", "ok": false })),
        )
            .into_response());
    };
    let org_id = match selected_customer_org(ctx, headers) {
        Ok(org_id) => org_id,
        Err(response) => return Err(response),
    };
    let path = format!(
        "/v1/keys/{}?org_id={}",
        encode_query_value(key_id),
        encode_query_value(&org_id)
    );
    let (status, body) =
        match auth_json(config, headers, reqwest::Method::DELETE, &path, None).await {
            Ok(result) => result,
            Err(response) => return Err(response),
        };
    if !status.is_success() {
        return Err(proxied_auth_error(status, body));
    }
    let response: AuthKeyRevokeResponse = match serde_json::from_value(body) {
        Ok(response) => response,
        Err(error) => {
            return Err(dependency_error(
                "fiducia-auth",
                "auth_key_revoke_bad_response",
                error,
            ))
        }
    };
    if !response.revoked {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({ "ok": false, "error": "key_not_found" })),
        )
            .into_response());
    }
    Ok(())
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
                .map(|key| auth_key_to_display(key, &org_id))
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
    no_store_json(
        StatusCode::OK,
        json!({
            "table": table,
            "snapshot": true,
            "requested_since": params.since,
            "rows": rows,
        }),
    )
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
    if let Err(error) = require_api_write_security(&headers, &config, &ctx) {
        return request_security_error(error);
    }
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
    let timezone = payload.timezone.trim();
    if timezone.is_empty() || timezone.chars().count() > MAX_TIMEZONE_CHARS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_timezone", "ok": false })),
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
        timezone.to_string(),
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

fn customer_activity_limit(requested: Option<u16>) -> u64 {
    requested
        .map(u64::from)
        .unwrap_or(DEFAULT_ACTIVITY_LIMIT)
        .clamp(1, MAX_ACTIVITY_LIMIT)
}

fn customer_audit_event(row: crate::entity::audit_log::Model) -> CustomerAuditEvent {
    CustomerAuditEvent {
        id: row.id,
        actor: row.actor,
        action: row.action,
        target: row.target,
        request_id: row.request_id,
        created_at: row.created_at.to_rfc3339(),
    }
}

/// Load activity only after the authenticated Supabase identity has selected an
/// organization it is actually a member of. The canonical schema's indexed
/// `org_id` predicate is the second tenant boundary below the auth claim.
async fn customer_activity_events(
    config: &AppConfig,
    headers: &HeaderMap,
    customer: &CustomerCtx,
    explicit_org: Option<&str>,
    limit: u64,
) -> Result<Vec<CustomerAuditEvent>, Response> {
    let org_id = selected_customer_org_from(customer, headers, explicit_org)?;
    let org_id = Uuid::parse_str(&org_id).map_err(|_| {
        dependency_error(
            "fiducia-auth",
            "invalid_verified_org_id",
            "verified organization membership was not a UUID",
        )
    })?;
    let pool = customer_pool(config)?;
    let rows = store::list_audit_events(pool, org_id, limit)
        .await
        .map_err(|error| dependency_error("postgres", "activity_list_failed", error))?;
    Ok(rows.into_iter().map(customer_audit_event).collect())
}

async fn customer_activity_json(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(query): Query<CustomerActivityQuery>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    match customer_activity_events(
        &config,
        &headers,
        &customer,
        None,
        customer_activity_limit(query.limit),
    )
    .await
    {
        Ok(events) => no_store_json(StatusCode::OK, json!({ "events": events })),
        Err(response) => response,
    }
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
    if let Err(error) = require_api_write_security(&headers, &config, &ctx) {
        return request_security_error(error);
    }
    let device = payload.device.trim();
    if device.is_empty() || device.chars().count() > MAX_SESSION_DEVICE_CHARS {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_device", "ok": false })),
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
    if let Err(error) = require_form_security(&headers, &config, &customer, &form.csrf_token) {
        return request_security_error(error);
    }
    if !CUSTOMER_REGIONS.contains(&form.region.as_str()) {
        return (StatusCode::BAD_REQUEST, "invalid_region").into_response();
    }
    if !["comfortable", "compact"].contains(&form.density.as_str()) {
        return (StatusCode::BAD_REQUEST, "invalid_density").into_response();
    }
    let timezone = form.timezone.trim();
    if timezone.is_empty() || timezone.chars().count() > MAX_TIMEZONE_CHARS {
        return (StatusCode::BAD_REQUEST, "invalid_timezone").into_response();
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
        timezone.to_string(),
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
    preferences_form_markup(
        &prefs_from_row(&row),
        true,
        &customer_csrf_token(&config, &customer),
    )
    .into_response()
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
    preferences_form_markup(&preferences, saved, &customer_csrf_token(config, customer))
        .into_response()
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

async fn customer_activity_fragment(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    match customer_activity_events(
        &config,
        &headers,
        &customer,
        selection.org_id.as_deref(),
        DEFAULT_ACTIVITY_LIMIT,
    )
    .await
    {
        Ok(events) => customer_activity_table_markup(&events).into_response(),
        Err(response) => response,
    }
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
    if let Err(error) = require_form_security(&headers, &config, &customer, &form.csrf_token) {
        return request_security_error(error);
    }
    let device = form.device.trim();
    if device.is_empty() || device.chars().count() > MAX_SESSION_DEVICE_CHARS {
        return (StatusCode::BAD_REQUEST, "invalid_device").into_response();
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
    sessions_table_markup(&sessions, message, &customer_csrf_token(config, customer))
        .into_response()
}

fn validate_api_key_request(payload: &CreateCustomerApiKeyRequest) -> Option<&'static str> {
    if payload.name.trim().is_empty() {
        return Some("name_required");
    }
    if payload.name.trim().chars().count() > MAX_API_KEY_NAME_CHARS {
        return Some("name_too_long");
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

async fn root(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    if should_serve_customer_app(&config, &headers) {
        return customer_page_response(
            &config,
            &headers,
            CustomerTab::Dashboard,
            selection.org_id.as_deref(),
        )
        .await;
    }

    match tokio::fs::read_to_string(config.static_dir.join("index.html")).await {
        Ok(body) => Html(body).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "static index not found").into_response(),
    }
}

async fn customer_home(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    customer_page_response(
        &config,
        &headers,
        CustomerTab::Dashboard,
        selection.org_id.as_deref(),
    )
    .await
}

async fn customer_auth(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    customer_page_response(
        &config,
        &headers,
        CustomerTab::Auth,
        selection.org_id.as_deref(),
    )
    .await
}

async fn customer_api_keys(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    customer_page_response(
        &config,
        &headers,
        CustomerTab::ApiKeys,
        selection.org_id.as_deref(),
    )
    .await
}

async fn customer_security(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    customer_page_response(
        &config,
        &headers,
        CustomerTab::Security,
        selection.org_id.as_deref(),
    )
    .await
}

async fn customer_activity(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    customer_page_response(
        &config,
        &headers,
        CustomerTab::Activity,
        selection.org_id.as_deref(),
    )
    .await
}

async fn customer_notifications(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    customer_page_response(
        &config,
        &headers,
        CustomerTab::Notifications,
        selection.org_id.as_deref(),
    )
    .await
}

async fn customer_settings(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    customer_page_response(
        &config,
        &headers,
        CustomerTab::Settings,
        selection.org_id.as_deref(),
    )
    .await
}

/// Fragment: the signed-in user's notification feed. Reads are scoped to the
/// verified caller's `user_id` at the database, so a forged `org_id` can never
/// surface another user's notifications.
async fn customer_notifications_fragment(
    State(config): State<AppConfig>,
    headers: HeaderMap,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    notifications_fragment_markup(&config, &customer, None).await
}

#[derive(Debug, Deserialize)]
struct ReadNotificationForm {
    csrf_token: String,
    id: String,
}

/// Mark one notification read. CSRF-protected like every other browser
/// mutation, and scoped to the caller's `user_id` in the store, so a user can
/// only ever clear their own notifications. Returns the refreshed fragment.
async fn read_customer_notification_form(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<ReadNotificationForm>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    if let Err(error) = require_form_security(&headers, &config, &customer, &form.csrf_token) {
        return request_security_error(error);
    }
    let Ok(id) = Uuid::parse_str(form.id.trim()) else {
        return (StatusCode::BAD_REQUEST, "invalid_notification_id").into_response();
    };
    let user_id = match caller_user_id(&config, &customer).await {
        Ok(user_id) => user_id,
        Err(response) => return response,
    };
    let pool = match customer_pool(&config) {
        Ok(pool) => pool,
        Err(response) => return response,
    };
    let message = match store::mark_notification_read(pool, user_id, id).await {
        Ok(true) => Some("Notification marked read."),
        Ok(false) => Some("Notification was already read or no longer exists."),
        Err(error) => return dependency_error("postgres", "notification_read_failed", error),
    };
    notifications_fragment_markup(&config, &customer, message).await
}

async fn notifications_fragment_markup(
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
    let notifications = match store::list_notifications(pool, user_id, DEFAULT_ACTIVITY_LIMIT).await
    {
        Ok(rows) => rows,
        Err(error) => return dependency_error("postgres", "notifications_list_failed", error),
    };
    // True unread total (not just within the shown page) for an accurate badge.
    let unread = match store::unread_notification_count(pool, user_id).await {
        Ok(count) => count,
        Err(error) => return dependency_error("postgres", "notifications_count_failed", error),
    };
    notifications_table_markup(
        &notifications,
        unread,
        message,
        &customer_csrf_token(config, customer),
    )
    .into_response()
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
    if let Err(error) = require_form_security(&headers, &config, &customer, &form.csrf_token) {
        return request_security_error(error);
    }
    let headers =
        match form_mutation_headers(headers, &customer, &form.org_id, &form.idempotency_key) {
            Ok(headers) => headers,
            Err(response) => return response,
        };
    let payload = CreateCustomerApiKeyRequest {
        name: form.name,
        environment: form.environment,
        scope: form.scope,
        require_idempotency: Some(true),
    };
    let (_display, secret) =
        match issue_customer_api_key(&config, &headers, &customer, &payload).await {
            Ok(issued) => issued,
            Err(response) => return response,
        };
    api_keys_fragment_markup(&config, &headers, &customer, Some(&secret)).await
}

#[allow(clippy::result_large_err)]
fn form_mutation_headers(
    mut headers: HeaderMap,
    customer: &CustomerCtx,
    explicit_org: &str,
    idempotency_key: &str,
) -> Result<HeaderMap, Response> {
    let org_id = selected_customer_org_from(customer, &headers, Some(explicit_org))?;
    let org_header = HeaderValue::from_str(&org_id).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid_org_selection" })),
        )
            .into_response()
    })?;
    let idempotency_header = HeaderValue::from_str(idempotency_key).map_err(|_| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "ok": false, "error": "invalid_idempotency_key" })),
        )
            .into_response()
    })?;
    headers.insert(HeaderName::from_static(CUSTOMER_ORG_HEADER), org_header);
    headers.insert(
        HeaderName::from_static(IDEMPOTENCY_KEY_HEADER),
        idempotency_header,
    );
    require_idempotency_key(&headers)?;
    Ok(headers)
}

async fn rotate_customer_api_key_form(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<RotateCustomerApiKeyForm>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    if let Err(error) = require_form_security(&headers, &config, &customer, &form.csrf_token) {
        return request_security_error(error);
    }
    let headers =
        match form_mutation_headers(headers, &customer, &form.org_id, &form.idempotency_key) {
            Ok(headers) => headers,
            Err(response) => return response,
        };
    let (_, replacement_secret, _) =
        match rotate_customer_api_key_authority(&config, &headers, &customer, form.prefix.trim())
            .await
        {
            Ok(rotated) => rotated,
            Err(response) => return response,
        };
    api_keys_fragment_markup(&config, &headers, &customer, Some(&replacement_secret)).await
}

async fn revoke_customer_api_key_form(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Form(form): Form<RevokeCustomerApiKeyForm>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    if let Err(error) = require_form_security(&headers, &config, &customer, &form.csrf_token) {
        return request_security_error(error);
    }
    let headers =
        match form_mutation_headers(headers, &customer, &form.org_id, &form.idempotency_key) {
            Ok(headers) => headers,
            Err(response) => return response,
        };
    if let Err(response) =
        revoke_customer_api_key_authority(&config, &headers, &customer, form.prefix.trim()).await
    {
        return response;
    }
    api_keys_fragment_markup(&config, &headers, &customer, None).await
}

async fn api_keys_fragment(
    State(config): State<AppConfig>,
    headers: HeaderMap,
    Query(selection): Query<CustomerOrgSelection>,
) -> Response {
    let customer = match config.authenticator.authenticate(&headers).await {
        Ok(customer) => customer,
        Err(response) => return response,
    };
    api_keys_fragment_markup_for_org(
        &config,
        &headers,
        &customer,
        None,
        selection.org_id.as_deref(),
    )
    .await
}

async fn api_keys_fragment_markup(
    config: &AppConfig,
    headers: &HeaderMap,
    customer: &CustomerCtx,
    secret: Option<&str>,
) -> Response {
    api_keys_fragment_markup_for_org(config, headers, customer, secret, None).await
}

async fn api_keys_fragment_markup_for_org(
    config: &AppConfig,
    headers: &HeaderMap,
    customer: &CustomerCtx,
    secret: Option<&str>,
    explicit_org: Option<&str>,
) -> Response {
    let org_id = match selected_customer_org_from(customer, headers, explicit_org) {
        Ok(org_id) => org_id,
        Err(response) => return response,
    };
    let path = format!("/v1/keys?org_id={}", encode_query_value(&org_id));
    let (status, body) = match auth_json(config, headers, reqwest::Method::GET, &path, None).await {
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
        .map(|key| auth_key_to_display(key, &org_id))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(keys) => keys,
        Err(error) => return dependency_error("fiducia-auth", "auth_key_list_bad_response", error),
    };
    api_keys_table_markup(
        &keys,
        secret,
        &customer_csrf_token(config, customer),
        &org_id,
    )
    .into_response()
}

fn api_keys_table_markup(
    keys: &[serde_json::Value],
    secret: Option<&str>,
    csrf_token: &str,
    org_id: &str,
) -> Markup {
    html! {
        @if let Some(secret) = secret {
            section class="panel secret-once" role="status" {
                h2 { "Copy this secret now" }
                code { (secret) }
                p class="muted" { "The plaintext is returned only by the authoritative auth service for this replay-safe request." }
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
                            th { "Actions" }
                        }
                    }
                    tbody {
                        @if keys.is_empty() {
                            tr { td colspan="6" class="muted" { "No API keys yet." } }
                        } @else {
                            @for key in keys {
                                @let prefix = key.get("prefix").and_then(|value| value.as_str()).unwrap_or("");
                                @let active = key.get("status").and_then(|value| value.as_str()) == Some("active");
                                tr {
                                    td { (key.get("name").and_then(|value| value.as_str()).unwrap_or("")) }
                                    td { code { (key.get("prefix").and_then(|value| value.as_str()).unwrap_or("")) } }
                                    td { (key.get("environment").and_then(|value| value.as_str()).unwrap_or("")) }
                                    td { code { (key.get("scopes").and_then(|value| value.as_str()).unwrap_or("")) } }
                                    td { (key.get("status").and_then(|value| value.as_str()).unwrap_or("")) }
                                    td {
                                        @if active {
                                            form method="post" action="/app/api-keys/rotate"
                                                hx-post="/app/api-keys/rotate" hx-target="#api-key-results" hx-swap="innerHTML" {
                                                input type="hidden" name="csrf_token" value=(csrf_token);
                                                input type="hidden" name="org_id" value=(org_id);
                                                input type="hidden" name="idempotency_key" value=(Uuid::new_v4().to_string());
                                                input type="hidden" name="prefix" value=(prefix);
                                                button type="submit" { "Rotate" }
                                            }
                                            form method="post" action="/app/api-keys/revoke"
                                                hx-post="/app/api-keys/revoke" hx-target="#api-key-results" hx-swap="innerHTML" {
                                                input type="hidden" name="csrf_token" value=(csrf_token);
                                                input type="hidden" name="org_id" value=(org_id);
                                                input type="hidden" name="idempotency_key" value=(Uuid::new_v4().to_string());
                                                input type="hidden" name="prefix" value=(prefix);
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

async fn customer_page_response(
    config: &AppConfig,
    headers: &HeaderMap,
    active: CustomerTab,
    explicit_org: Option<&str>,
) -> Response {
    match config.authenticator.authenticate(headers).await {
        Ok(customer) => {
            if let Err(error) = config.request_security.require_api_host(headers) {
                return request_security_error(error);
            }
            let org_id = match customer_page_org(&customer, headers, explicit_org) {
                Ok(org_id) => org_id,
                Err(response) => return response,
            };
            customer_page(
                config,
                &customer,
                active,
                &org_id,
                &customer_csrf_token(config, &customer),
            )
            .into_response()
        }
        Err(response) if response.status() == StatusCode::UNAUTHORIZED => {
            (StatusCode::SEE_OTHER, [(header::LOCATION, "/login")]).into_response()
        }
        Err(response) => response,
    }
}

async fn summary_fragment(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    protected_fragment(&config, &headers, summary_markup()).await
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
        "fragments": { "summary": summary_markup().into_string() },
    })
}

fn unix_epoch_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

fn host_serves_customer_app(
    headers: &HeaderMap,
    customer_app_host: &str,
    customer_site_mode: bool,
) -> bool {
    if customer_site_mode {
        return true;
    }

    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case(customer_app_host)
}

fn should_serve_customer_app(config: &AppConfig, headers: &HeaderMap) -> bool {
    host_serves_customer_app(
        headers,
        &config.customer_app_host,
        config.customer_site_mode,
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CustomerTab {
    Dashboard,
    Auth,
    ApiKeys,
    Security,
    Activity,
    Notifications,
    Settings,
}

impl CustomerTab {
    fn all() -> [CustomerTab; 7] {
        [
            CustomerTab::Dashboard,
            CustomerTab::Auth,
            CustomerTab::ApiKeys,
            CustomerTab::Security,
            CustomerTab::Activity,
            CustomerTab::Notifications,
            CustomerTab::Settings,
        ]
    }

    fn href(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "/app",
            CustomerTab::Auth => "/app/auth",
            CustomerTab::ApiKeys => "/app/api-keys",
            CustomerTab::Security => "/app/security",
            CustomerTab::Activity => "/app/activity",
            CustomerTab::Notifications => "/app/notifications",
            CustomerTab::Settings => "/app/settings",
        }
    }

    fn label(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "Dashboard",
            CustomerTab::Auth => "Account",
            CustomerTab::ApiKeys => "API Keys",
            CustomerTab::Security => "Security",
            CustomerTab::Activity => "Activity",
            CustomerTab::Notifications => "Notifications",
            CustomerTab::Settings => "Settings",
        }
    }

    fn description(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "Account posture, API access, preferences, and customer security in one workspace.",
            CustomerTab::Auth => "Your Supabase identity, verified organization membership, and isolated customer session.",
            CustomerTab::ApiKeys => "Create, rotate, scope, and audit customer API keys for production integrations.",
            CustomerTab::Security => "Two-factor authentication, trusted sessions, recovery, and account protection.",
            CustomerTab::Activity => "Organization-scoped account and API activity from the durable customer audit log.",
            CustomerTab::Notifications => "Key-rotation reminders, lock-contention alerts, and account notices delivered to you.",
            CustomerTab::Settings => "Preferences, notifications, default region, and team-level customer settings.",
        }
    }
}

fn customer_tab_href(tab: CustomerTab, org_id: &str) -> String {
    format!("{}?org_id={}", tab.href(), encode_query_value(org_id))
}

fn customer_page(
    config: &AppConfig,
    customer: &CustomerCtx,
    active: CustomerTab,
    org_id: &str,
    csrf_token: &str,
) -> Markup {
    let summary_href = format!(
        "/app/fragments/summary?org_id={}",
        encode_query_value(org_id)
    );
    let api_keys_href = customer_tab_href(CustomerTab::ApiKeys, org_id);
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                meta name="fiducia-customer-csrf" content=(csrf_token);
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
                                input type="hidden" name="csrf_token" value=(csrf_token);
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
                                        @let href = customer_tab_href(tab, org_id);
                                        @if tab == active {
                                            a href=(href) aria-current="page" {
                                                span { (tab.label()) }
                                            }
                                        } @else {
                                            a href=(href) {
                                                span { (tab.label()) }
                                            }
                                        }
                                    }
                                }
                            }
                            section class="sidebar__section" {
                                form class="region-select" method="get" action=(active.href()) {
                                    label class="sidebar__label" for="customer-org" { "Organization" }
                                    select id="customer-org" name="org_id" {
                                        @for available_org in &customer.orgs {
                                            option value=(available_org) selected[available_org == org_id] { (available_org) }
                                        }
                                    }
                                    button type="submit" { "Switch" }
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
                                    button type="button" hx-get=(summary_href) hx-target="#summary" hx-swap="innerHTML" { "Refresh" }
                                    a href=(api_keys_href) { "New API key" }
                                    a href="/api/info" { "API info" }
                                }
                            }
                            (customer_tab_content(config, customer, active, org_id, csrf_token))
                        }
                    }
                }
            }
        }
    }
}

fn customer_tab_content(
    config: &AppConfig,
    customer: &CustomerCtx,
    active: CustomerTab,
    org_id: &str,
    csrf_token: &str,
) -> Markup {
    match active {
        CustomerTab::Dashboard => dashboard_markup(config, customer, org_id),
        CustomerTab::Auth => auth_markup(customer, csrf_token),
        CustomerTab::ApiKeys => api_keys_markup(org_id, csrf_token),
        CustomerTab::Security => security_markup(org_id),
        CustomerTab::Activity => activity_markup(org_id),
        CustomerTab::Notifications => notifications_markup(org_id),
        CustomerTab::Settings => settings_markup(org_id),
    }
}

fn dashboard_markup(config: &AppConfig, customer: &CustomerCtx, org_id: &str) -> Markup {
    let summary_href = format!(
        "/app/fragments/summary?org_id={}",
        encode_query_value(org_id)
    );
    html! {
        section id="summary" hx-get=(summary_href) hx-trigger="load, every 15s" hx-swap="innerHTML" {
            (summary_markup())
        }
        div class="panel-grid panel-grid--dashboard" {
            (auth_status_panel(config, customer, org_id))
            (api_key_summary_panel(org_id))
            (security_summary_panel(org_id))
            (preferences_summary_panel(org_id))
        }
    }
}

fn auth_status_panel(config: &AppConfig, customer: &CustomerCtx, org_id: &str) -> Markup {
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
                    a class="button-link" href=(customer_tab_href(CustomerTab::Auth, org_id)) { "Account" }
                }
            }
        }
    }
}

fn api_key_summary_panel(org_id: &str) -> Markup {
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
                        dd { "rotation reports the remaining edge/LB cache overlap" }
                    }
                }
                a class="button-link" href=(customer_tab_href(CustomerTab::ApiKeys, org_id)) { "Manage keys" }
            }
        }
    }
}

fn security_summary_panel(org_id: &str) -> Markup {
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
                a class="button-link" href=(customer_tab_href(CustomerTab::Security, org_id)) { "Review security" }
            }
        }
    }
}

fn preferences_summary_panel(org_id: &str) -> Markup {
    html! {
        section class="panel" aria-labelledby="preferences-summary-heading" {
            div class="panel__header" {
                h2 id="preferences-summary-heading" { "Preferences" }
                span { "Postgres-backed" }
            }
            div class="panel-body stack" {
                p class="muted" { "Set default region, alert cadence, timezone, and customer-visible notifications." }
                p { "Values are rendered from the authenticated user's persisted row." }
                a class="button-link" href=(customer_tab_href(CustomerTab::Settings, org_id)) { "Open settings" }
            }
        }
    }
}

fn auth_markup(customer: &CustomerCtx, csrf_token: &str) -> Markup {
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
                input type="hidden" name="csrf_token" value=(csrf_token);
                button type="submit" { "Sign out" }
            }
        }
    }
}

fn api_keys_markup(org_id: &str, csrf_token: &str) -> Markup {
    let fragment_href = format!(
        "/app/fragments/api-keys?org_id={}",
        encode_query_value(org_id)
    );
    html! {
        section class="panel" aria-labelledby="create-api-key-heading" {
            div class="panel__header" {
                h2 id="create-api-key-heading" { "Create API key" }
                span { "customer scoped" }
            }
            form class="form-grid form-grid--inline" method="post" action="/app/api-keys"
                hx-post="/app/api-keys" hx-target="#api-key-results" hx-swap="innerHTML" {
                input type="hidden" name="csrf_token" value=(csrf_token);
                input type="hidden" name="org_id" value=(org_id);
                input type="hidden" name="idempotency_key" value=(Uuid::new_v4().to_string());
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
        div id="api-key-results" hx-get=(fragment_href) hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading customer API keys…" }
        }
    }
}

fn security_markup(org_id: &str) -> Markup {
    let fragment_href = format!(
        "/app/fragments/security-sessions?org_id={}",
        encode_query_value(org_id)
    );
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
        div id="security-sessions" hx-get=(fragment_href) hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading trusted sessions…" }
        }
    }
}

fn activity_markup(org_id: &str) -> Markup {
    let fragment_href = format!(
        "/app/fragments/activity?org_id={}",
        encode_query_value(org_id)
    );
    html! {
        section class="panel" aria-labelledby="activity-heading" {
            div class="panel__header" {
                h2 id="activity-heading" { "Organization activity" }
                span { "audit log" }
            }
            p class="muted" {
                "Only records for the organization selected from your verified Supabase membership are shown. "
                "Network addresses, user agents, and internal audit metadata are never exposed here."
            }
        }
        div id="customer-activity" hx-get=(fragment_href) hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading organization activity…" }
        }
    }
}

fn notifications_markup(org_id: &str) -> Markup {
    let fragment_href = format!(
        "/app/fragments/notifications?org_id={}",
        encode_query_value(org_id)
    );
    html! {
        section class="panel" aria-labelledby="notifications-heading" {
            div class="panel__header" {
                h2 id="notifications-heading" { "Your notifications" }
                span { "account feed" }
            }
            p class="muted" {
                "Key-rotation reminders, lock-contention alerts, MFA nudges, and operator notices "
                "delivered to your account. Delivery preferences live under Settings."
            }
        }
        div id="customer-notifications" hx-get=(fragment_href) hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading notifications…" }
        }
    }
}

fn notifications_table_markup(
    notifications: &[entity::customer_notifications::Model],
    unread: u64,
    message: Option<&str>,
    csrf_token: &str,
) -> Markup {
    html! {
        section class="panel" aria-labelledby="notifications-table-heading" {
            div class="panel__header" {
                h2 id="notifications-table-heading" { "Recent notifications" }
                span { (unread) " unread / " (notifications.len()) " shown" }
            }
            @if let Some(message) = message {
                p class="inline-message" role="status" { (message) }
            }
            div class="table-wrap" {
                table {
                    thead {
                        tr {
                            th { "When" }
                            th { "Severity" }
                            th { "Notification" }
                            th { "State" }
                            th { "Action" }
                        }
                    }
                    tbody {
                        @if notifications.is_empty() {
                            tr { td colspan="5" class="muted" { "You have no notifications." } }
                        } @else {
                            @for note in notifications {
                                tr {
                                    td { (note.created_at.to_rfc3339()) }
                                    td { span class="status-pill" data-severity=(&note.severity) { (&note.severity) } }
                                    td {
                                        strong { (&note.title) }
                                        @if !note.body.is_empty() {
                                            div class="muted" { (&note.body) }
                                        }
                                        @if let Some(link) = &note.link {
                                            div { a href=(link) { "View" } }
                                        }
                                    }
                                    td { @if note.read_at.is_some() { "read" } @else { "unread" } }
                                    td {
                                        @if note.read_at.is_none() {
                                            form method="post" action="/app/notifications/read"
                                                hx-post="/app/notifications/read"
                                                hx-target="#customer-notifications"
                                                hx-swap="innerHTML" {
                                                input type="hidden" name="csrf_token" value=(csrf_token);
                                                input type="hidden" name="id" value=(note.id.to_string());
                                                button type="submit" { "Mark read" }
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

fn customer_activity_table_markup(events: &[CustomerAuditEvent]) -> Markup {
    html! {
        section class="panel" aria-labelledby="activity-table-heading" {
            div class="panel__header" {
                h2 id="activity-table-heading" { "Recent activity" }
                span { (events.len()) " shown" }
            }
            div class="table-wrap" {
                table {
                    thead {
                        tr {
                            th { "When" }
                            th { "Actor" }
                            th { "Action" }
                            th { "Target" }
                            th { "Request" }
                        }
                    }
                    tbody {
                        @if events.is_empty() {
                            tr { td colspan="5" class="muted" { "No customer-visible activity is recorded for this organization yet." } }
                        } @else {
                            @for event in events {
                                tr {
                                    td { (event.created_at) }
                                    td { (event.actor.as_deref().unwrap_or("system")) }
                                    td { code { (&event.action) } }
                                    td { (event.target.as_deref().unwrap_or("—")) }
                                    td { code { (event.request_id.as_deref().unwrap_or("—")) } }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

fn sessions_table_markup(
    sessions: &[fiducia_interfaces_db::customer::CustomerSessionsRow],
    message: Option<&str>,
    csrf_token: &str,
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
                                                input type="hidden" name="csrf_token" value=(csrf_token);
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

fn settings_markup(org_id: &str) -> Markup {
    let fragment_href = format!(
        "/app/fragments/preferences?org_id={}",
        encode_query_value(org_id)
    );
    html! {
        div id="customer-preferences" hx-get=(fragment_href) hx-trigger="load" hx-swap="innerHTML" {
            p class="muted" { "Loading persisted preferences…" }
        }
    }
}

fn preferences_form_markup(
    preferences: &CustomerPreferences,
    saved: bool,
    csrf_token: &str,
) -> Markup {
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
                input type="hidden" name="csrf_token" value=(csrf_token);
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
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt; // for `oneshot`

    const ORG_A: &str = "00000000-0000-4000-8000-000000000001";
    const ORG_B: &str = "00000000-0000-4000-8000-000000000002";
    const KEY_ID: &str = "0123456789abcdef";

    #[derive(Clone, Debug)]
    struct CapturedAuthRequest {
        method: Method,
        path_and_query: String,
        authorization: Option<String>,
        idempotency_key: Option<String>,
        body: serde_json::Value,
    }

    #[derive(Clone, Default)]
    struct MockAuthState {
        requests: Arc<Mutex<Vec<CapturedAuthRequest>>>,
    }

    fn mock_key_meta() -> serde_json::Value {
        json!({
            "key_id": KEY_ID,
            "org_id": ORG_B,
            "name": "Production webhooks",
            "scopes": ["requests:write"],
            "env": "live",
            "last_used_ms": null,
            "revoked": false,
            "version": 1,
            "require_idempotency": true,
            // The upstream contract must never include this, but even a drifted
            // response cannot pass it through the BFF's typed sanitizer.
            "secret_hash": "sha256:must-not-leak"
        })
    }

    async fn mock_auth_request(
        State(state): State<MockAuthState>,
        request: axum::extract::Request,
    ) -> Response {
        let method = request.method().clone();
        let path_and_query = request
            .uri()
            .path_and_query()
            .map(|value| value.as_str())
            .unwrap_or("/")
            .to_string();
        let authorization = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let idempotency_key = request
            .headers()
            .get(IDEMPOTENCY_KEY_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let (parts, body) = request.into_parts();
        let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
            .await
            .expect("read mock auth request body");
        let body = if bytes.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::from_slice(&bytes).expect("mock auth request JSON")
        };
        state.requests.lock().unwrap().push(CapturedAuthRequest {
            method: method.clone(),
            path_and_query: path_and_query.clone(),
            authorization,
            idempotency_key,
            body,
        });

        let path = parts.uri.path();
        let mut rotated_meta = mock_key_meta();
        rotated_meta["version"] = json!(2);
        let response = match (method, path) {
            (Method::GET, "/v1/keys") => json!({ "keys": [mock_key_meta()] }),
            (Method::POST, "/v1/keys") => json!({
                "api_key": format!("fdc_live_{KEY_ID}.{}", "a".repeat(64)),
                "key": mock_key_meta()
            }),
            (Method::POST, "/v1/keys/0123456789abcdef/rotate") => json!({
                "ok": true,
                "api_key": format!("fdc_live_{KEY_ID}.{}", "b".repeat(64)),
                "key": rotated_meta,
                "secret_once": true,
                "overlap_seconds": 60
            }),
            (Method::DELETE, "/v1/keys/0123456789abcdef") => {
                json!({ "revoked": true })
            }
            _ => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": "unexpected_mock_auth_route" })),
                )
                    .into_response()
            }
        };
        Json(response).into_response()
    }

    async fn spawn_mock_auth() -> (String, MockAuthState, tokio::task::JoinHandle<()>) {
        let state = MockAuthState::default();
        let app = Router::new()
            .fallback(mock_auth_request)
            .with_state(state.clone());
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{address}"), state, server)
    }

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
            customer_app_origin: None,
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
                credential_binding: "authorization\0verified-supabase-session".to_string(),
                cookie_authenticated: false,
            })),
            request_security: RequestSecurity::new(
                "https://app.fiducia.cloud",
                b"0123456789abcdef0123456789abcdef".to_vec(),
            )
            .unwrap(),
        }
    }

    /// A no-DB config with a chosen authenticator (for auth-gate tests).
    fn config_with_auth(authenticator: Authenticator) -> AppConfig {
        AppConfig {
            authenticator,
            ..test_config()
        }
    }

    fn multi_org_config() -> AppConfig {
        config_with_auth(Authenticator::Static(Arc::new(CustomerCtx {
            user_id: "00000000-0000-4000-8000-000000000099".to_string(),
            email: Some("multi-org@fiducia.cloud".to_string()),
            orgs: vec![ORG_A.to_string(), ORG_B.to_string()],
            credential_binding: "authorization\0verified-supabase-session".to_string(),
            cookie_authenticated: false,
        })))
    }

    async fn bff_request(
        config: AppConfig,
        method: Method,
        uri: &str,
        body: Option<serde_json::Value>,
        org_id: Option<&str>,
        idempotency_key: Option<&str>,
    ) -> Response {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::HOST, "app.fiducia.cloud")
            .header(header::AUTHORIZATION, "Bearer verified-supabase-session");
        if let Some(org_id) = org_id {
            builder = builder.header(CUSTOMER_ORG_HEADER, org_id);
        }
        if let Some(idempotency_key) = idempotency_key {
            builder = builder.header(IDEMPOTENCY_KEY_HEADER, idempotency_key);
        }
        let body = match body {
            Some(body) => {
                builder = builder.header(header::CONTENT_TYPE, "application/json");
                Body::from(body.to_string())
            }
            None => Body::empty(),
        };
        build_router(config)
            .oneshot(builder.body(body).unwrap())
            .await
            .unwrap()
    }

    async fn response_json(response: Response) -> (StatusCode, HeaderMap, serde_json::Value) {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body = serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            panic!(
                "response was not JSON ({error}): {}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, headers, body)
    }

    async fn post_json(config: AppConfig, uri: &str, body: &str) -> StatusCode {
        build_router(config)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::HOST, "app.fiducia.cloud")
                    .header("content-type", "application/json")
                    .header(IDEMPOTENCY_KEY_HEADER, "test-request-1")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
            .status()
    }

    const CREATE_KEY_BODY: &str = r#"{"name":"k","environment":"live","scope":"requests:write"}"#;

    #[test]
    fn customer_origin_validation_accepts_one_exact_http_origin() {
        assert_eq!(
            parse_customer_app_origin("https://app.fiducia.cloud")
                .unwrap()
                .to_str()
                .unwrap(),
            "https://app.fiducia.cloud"
        );
        assert_eq!(
            parse_customer_app_origin("http://127.0.0.1:4173/")
                .unwrap()
                .to_str()
                .unwrap(),
            "http://127.0.0.1:4173"
        );
        for invalid in [
            "*",
            "app.fiducia.cloud",
            "https://app.fiducia.cloud/path",
            "https://app.fiducia.cloud?query=1",
            "https://user@app.fiducia.cloud",
            "https://app.fiducia.cloud,https://evil.example",
        ] {
            assert!(
                parse_customer_app_origin(invalid).is_err(),
                "accepted invalid origin {invalid}"
            );
        }
    }

    #[test]
    fn org_selection_is_explicit_and_membership_checked() {
        let config = multi_org_config();
        let Authenticator::Static(ctx) = &config.authenticator else {
            panic!("multi-org test config must use static auth");
        };

        assert_eq!(
            selected_customer_org(ctx, &HeaderMap::new())
                .unwrap_err()
                .status(),
            StatusCode::BAD_REQUEST
        );

        let mut selected = HeaderMap::new();
        selected.insert(CUSTOMER_ORG_HEADER, HeaderValue::from_static(ORG_B));
        assert_eq!(selected_customer_org(ctx, &selected).unwrap(), ORG_B);

        selected.insert(
            CUSTOMER_ORG_HEADER,
            HeaderValue::from_static("00000000-0000-4000-8000-000000000003"),
        );
        assert_eq!(
            selected_customer_org(ctx, &selected).unwrap_err().status(),
            StatusCode::FORBIDDEN
        );

        selected.insert(
            CUSTOMER_ORG_HEADER,
            HeaderValue::from_static("org with whitespace"),
        );
        assert_eq!(
            selected_customer_org(ctx, &selected).unwrap_err().status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[test]
    fn auth_key_sanitizer_checks_org_wire_shape_and_scopes() {
        let key: AuthKeyMeta = serde_json::from_value(mock_key_meta()).unwrap();
        let display = auth_key_to_display(&key, ORG_B).unwrap();
        assert_eq!(display["prefix"], format!("fdc_live_{KEY_ID}"));
        assert!(display.get("secret_hash").is_none());
        assert!(auth_key_to_display(&key, ORG_A).is_err());
        assert!(raw_api_key_matches(
            &format!("fdc_live_{KEY_ID}.{}", "a".repeat(64)),
            &key
        ));
        assert!(!raw_api_key_matches(
            &format!("fdc_live_{KEY_ID}.short"),
            &key
        ));
    }

    #[tokio::test]
    async fn cors_allows_only_the_configured_customer_origin_and_headers() {
        let mut config = test_config();
        config.customer_app_origin =
            Some(parse_customer_app_origin("https://app.fiducia.cloud").unwrap());
        let preflight = |origin: &'static str| {
            Request::builder()
                .method(Method::OPTIONS)
                .uri("/api/customer/api-keys")
                .header(header::HOST, "app.fiducia.cloud")
                .header(header::ORIGIN, origin)
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "POST")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization,content-type,idempotency-key,x-fiducia-csrf,x-fiducia-org-id",
                )
                .body(Body::empty())
                .unwrap()
        };

        let allowed = build_router(config.clone())
            .oneshot(preflight("https://app.fiducia.cloud"))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);
        assert_eq!(
            allowed
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://app.fiducia.cloud"
        );
        let allowed_headers = allowed
            .headers()
            .get(header::ACCESS_CONTROL_ALLOW_HEADERS)
            .unwrap()
            .to_str()
            .unwrap();
        for required in [
            "authorization",
            "content-type",
            IDEMPOTENCY_KEY_HEADER,
            CUSTOMER_CSRF_HEADER,
            CUSTOMER_ORG_HEADER,
        ] {
            assert!(allowed_headers.contains(required), "missing {required}");
        }

        let denied = build_router(config)
            .oneshot(preflight("https://evil.example"))
            .await
            .unwrap();
        assert_eq!(
            denied
                .headers()
                .get(header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .unwrap(),
            "https://app.fiducia.cloud",
            "the fixed allow-origin must never reflect a foreign Origin"
        );
    }

    #[tokio::test]
    async fn customer_key_bff_is_org_scoped_sanitized_and_forwards_idempotency() {
        let (auth_url, state, server) = spawn_mock_auth().await;
        let mut config = multi_org_config();
        config.auth_url = Some(auth_url);

        let context = bff_request(
            config.clone(),
            Method::GET,
            "/api/customer/context",
            None,
            None,
            None,
        )
        .await;
        let (status, headers, body) = response_json(context).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(body["user"]["orgs"].as_array().unwrap().len(), 2);

        let missing_org = bff_request(
            config.clone(),
            Method::GET,
            "/api/customer/api-keys",
            None,
            None,
            None,
        )
        .await;
        assert_eq!(missing_org.status(), StatusCode::BAD_REQUEST);

        let listed = bff_request(
            config.clone(),
            Method::GET,
            "/api/customer/api-keys",
            None,
            Some(ORG_B),
            None,
        )
        .await;
        let (status, headers, body) = response_json(listed).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(body["api_keys"][0]["prefix"], format!("fdc_live_{KEY_ID}"));
        assert!(body.to_string().find("secret_hash").is_none());

        let create_body = json!({
            "name": "Production webhooks",
            "environment": "live",
            "scope": "requests:write",
            "require_idempotency": true
        });
        let missing_idempotency = bff_request(
            config.clone(),
            Method::POST,
            "/api/customer/api-keys",
            Some(create_body.clone()),
            Some(ORG_B),
            None,
        )
        .await;
        let (status, _, body) = response_json(missing_idempotency).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "idempotency_key_required");

        let created = bff_request(
            config.clone(),
            Method::POST,
            "/api/customer/api-keys",
            Some(create_body),
            Some(ORG_B),
            Some("customer-create-key-1"),
        )
        .await;
        let (status, headers, body) = response_json(created).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(headers.get(header::PRAGMA).unwrap(), "no-cache");
        assert!(body["secret"].as_str().unwrap().starts_with("fdc_live_"));
        assert!(body.to_string().find("secret_hash").is_none());

        let rotated = bff_request(
            config.clone(),
            Method::POST,
            "/api/customer/api-keys/rotate",
            Some(json!({ "prefix": format!("fdc_live_{KEY_ID}") })),
            Some(ORG_B),
            Some("customer-rotate-key-1"),
        )
        .await;
        let (status, headers, body) = response_json(rotated).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(body["api_key"]["version"], 2);
        assert_eq!(body["overlap_seconds"], 60);

        let revoked = bff_request(
            config.clone(),
            Method::POST,
            "/api/customer/api-keys/revoke",
            Some(json!({ "prefix": format!("fdc_live_{KEY_ID}") })),
            Some(ORG_B),
            Some("customer-revoke-key-1"),
        )
        .await;
        let (status, headers, body) = response_json(revoked).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(body["status"], "revoked");

        let catchup = bff_request(
            config,
            Method::GET,
            "/api/customer/sync/api_keys?since=99",
            None,
            Some(ORG_B),
            None,
        )
        .await;
        let (status, headers, body) = response_json(catchup).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
        assert_eq!(body["snapshot"], true);
        assert_eq!(body["requested_since"], 99);
        assert!(body.to_string().find("secret_hash").is_none());

        let requests = state.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 5);
        assert!(requests
            .iter()
            .all(|request| request.authorization.as_deref()
                == Some("Bearer verified-supabase-session")));
        let create = requests
            .iter()
            .find(|request| request.method == Method::POST && request.path_and_query == "/v1/keys")
            .unwrap();
        assert_eq!(
            create.idempotency_key.as_deref(),
            Some("customer-create-key-1")
        );
        assert_eq!(create.body["org_id"], ORG_B);
        let rotate = requests
            .iter()
            .find(|request| request.path_and_query.contains("/rotate"))
            .unwrap();
        assert_eq!(
            rotate.idempotency_key.as_deref(),
            Some("customer-rotate-key-1")
        );
        assert!(rotate.path_and_query.ends_with(&format!("org_id={ORG_B}")));
        let revoke = requests
            .iter()
            .find(|request| request.method == Method::DELETE)
            .unwrap();
        assert_eq!(
            revoke.idempotency_key.as_deref(),
            Some("customer-revoke-key-1")
        );
        assert!(revoke.path_and_query.ends_with(&format!("org_id={ORG_B}")));
        assert!(requests
            .iter()
            .filter(|request| request.method == Method::GET)
            .all(|request| request.path_and_query.ends_with(&format!("org_id={ORG_B}"))));
        server.abort();
    }

    #[tokio::test]
    async fn server_managed_customer_cookie_is_forwarded_to_the_key_authority() {
        let (auth_url, state, server) = spawn_mock_auth().await;
        let mut config = multi_org_config();
        config.auth_url = Some(auth_url);
        let response = build_router(config)
            .oneshot(
                Request::builder()
                    .uri("/api/customer/api-keys")
                    .header(header::HOST, "app.fiducia.cloud")
                    .header(
                        header::COOKIE,
                        format!("{CUSTOMER_SESSION_COOKIE}=cookie-supabase-session"),
                    )
                    .header(CUSTOMER_ORG_HEADER, ORG_B)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let requests = state.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].authorization.as_deref(),
            Some("Bearer cookie-supabase-session")
        );
        server.abort();
    }

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
        let default_host = if uri.starts_with("/app")
            || uri.starts_with("/login")
            || uri.starts_with("/api/customer")
        {
            "app.fiducia.cloud"
        } else {
            "www.fiducia.cloud"
        };
        let builder = Request::builder()
            .uri(uri)
            .header(header::HOST, host.unwrap_or(default_host));
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
        const MOCK_SUPABASE_TOKEN_PATH: &str = "/auth/v1/token";
        const MOCK_AUTH_ME_PATH: &str = "/v1/me";
        let supabase = Router::new().route(
            MOCK_SUPABASE_TOKEN_PATH,
            axum::routing::post(|| async { Json(json!({ "access_token": "customer.jwt" })) }),
        );
        let auth = Router::new().route(
            MOCK_AUTH_ME_PATH,
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
        let app = build_router(config);
        let login_page = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/login")
                    .header(header::HOST, "app.fiducia.cloud")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(login_page.status(), StatusCode::OK);
        let login_cookie = login_page
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .find_map(|value| {
                let value = value.to_str().ok()?;
                value
                    .starts_with(&format!("{CUSTOMER_LOGIN_CSRF_COOKIE}="))
                    .then(|| value.split(';').next().unwrap().to_string())
            })
            .unwrap();
        let login_html = String::from_utf8(
            axum::body::to_bytes(login_page.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        let csrf_field = &login_html[login_html.find("name=\"csrf_token\"").unwrap()..];
        let csrf_value = &csrf_field[csrf_field.find("value=\"").unwrap() + 7..];
        let csrf_value = &csrf_value[..csrf_value.find('"').unwrap()];

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/login")
                    .header(header::HOST, "app.fiducia.cloud")
                    .header(header::ORIGIN, "https://app.fiducia.cloud")
                    .header("sec-fetch-site", "same-origin")
                    .header(header::COOKIE, login_cookie)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!(
                        "csrf_token={csrf_value}&email=customer%40example.com&password=correct"
                    )))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(response.headers().get("location").unwrap(), "/app");
        let cookies = response
            .headers()
            .get_all(header::SET_COOKIE)
            .iter()
            .map(|value| value.to_str().unwrap())
            .collect::<Vec<_>>();
        assert!(cookies
            .iter()
            .any(|cookie| cookie.starts_with(&format!("{CUSTOMER_SESSION_COOKIE}=customer.jwt"))));
        assert!(cookies.iter().all(|cookie| cookie.contains("HttpOnly")));
        assert!(cookies
            .iter()
            .any(|cookie| cookie.starts_with(&format!("{CUSTOMER_LOGIN_CSRF_COOKIE}=;"))));
        assert!(cookies
            .iter()
            .all(|cookie| !cookie.contains("fiducia_admin_session")));
        supabase_task.abort();
        auth_task.abort();
    }

    #[tokio::test]
    async fn customer_pages_redirect_missing_sessions_to_customer_login() {
        let mut config = test_config();
        config.authenticator = Authenticator::AuthService("http://127.0.0.1:1".to_string());
        let response = build_router(config)
            .oneshot(
                Request::builder()
                    .uri("/app")
                    .header(header::HOST, "app.fiducia.cloud")
                    .body(Body::empty())
                    .unwrap(),
            )
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
                    .header(header::HOST, "app.fiducia.cloud")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(IDEMPOTENCY_KEY_HEADER, "test-request-1")
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
    async fn customer_activity_requires_database() {
        // Auth is static in this unit test, but the activity route must still
        // fail closed rather than inventing an empty tenant audit feed.
        let (status, _, _) = send("/api/customer/activity?limit=0").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn docs_api_and_alias_serve_html() {
        for uri in ["/docs/api", "/api/docs"] {
            let (status, ct, body) = send(uri).await;
            assert_eq!(status, StatusCode::OK, "{uri}");
            assert!(ct.contains("text/html"), "{uri} ct={ct}");
            assert!(body.contains("fiducia-customer.rs API docs"), "{uri}");
        }
    }

    #[tokio::test]
    async fn api_docs_json_is_machine_readable() {
        let (status, ct, body) = send("/api/docs.json").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("application/json"), "ct={ct}");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["service"], "fiducia-customer.rs");
        let routes = v["routes"].as_array().unwrap();
        assert_eq!(v["routeCount"].as_u64().unwrap() as usize, routes.len());
        assert!(
            routes.len() >= 30,
            "generated inventory is unexpectedly incomplete"
        );
        for path in [
            "/api/customer/api-keys",
            "/api/customer/api-keys/rotate",
            "/api/customer/api-keys/revoke",
            "/api/customer/preferences",
            "/api/customer/security/sessions",
            "/api/customer/sync/:table",
            "/app/ws",
            "/app/events",
        ] {
            assert!(
                routes.iter().any(|route| route["path"] == path),
                "generated API inventory is missing {path}"
            );
        }
        for removed in ["/app/kv", "/app/locks", "/app/requests", "/app/services"] {
            assert!(
                routes.iter().all(|route| route["path"] != removed),
                "generated API inventory retained removed route {removed}"
            );
        }
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
        assert!(body.contains("operator infrastructure controls live only in the admin app"));
        assert!(!body.contains("Config KV"));
        assert!(!body.contains("Service Discovery"));
    }

    #[tokio::test]
    async fn customer_account_routes_render_customer_controls() {
        let cases = [
            ("/app/auth", "Verified by fiducia-auth"),
            ("/app/signup", "Organization membership"),
            ("/app/api-keys", "Create API key"),
            ("/app/security", "provider managed"),
            ("/app/activity", "Organization activity"),
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
                    .header(header::HOST, "app.fiducia.cloud")
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
                    .header(header::HOST, "app.fiducia.cloud")
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
                    .header(header::HOST, "app.fiducia.cloud")
                    .header(header::ORIGIN, "https://app.fiducia.cloud")
                    .header("sec-fetch-site", "same-origin")
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

    #[test]
    fn release_cookie_policy_ignores_the_insecure_escape_hatch() {
        assert_eq!(cookie_secure_suffix_for(true, true), "; Secure");
        assert_eq!(cookie_secure_suffix_for(true, false), "; Secure");
        assert_eq!(cookie_secure_suffix_for(false, false), "; Secure");
        assert_eq!(cookie_secure_suffix_for(false, true), "");
    }

    #[tokio::test]
    async fn cookie_mutations_require_exact_origin_and_bound_csrf() {
        let customer = CustomerCtx {
            user_id: "00000000-0000-4000-8000-000000000002".to_string(),
            email: Some("cookie@fiducia.cloud".to_string()),
            orgs: vec![ORG_A.to_string()],
            credential_binding: "cookie\0customer.jwt".to_string(),
            cookie_authenticated: true,
        };
        let mut config = test_config();
        let csrf = customer_csrf_token(&config, &customer);
        config.authenticator = Authenticator::Static(Arc::new(customer));
        let app = build_router(config);
        let request = |origin: &'static str, csrf: Option<&str>| {
            let mut builder = Request::builder()
                .method(Method::POST)
                .uri("/api/customer/api-keys")
                .header(header::HOST, "app.fiducia.cloud")
                .header(header::ORIGIN, origin)
                .header("sec-fetch-site", "same-origin")
                .header(
                    header::COOKIE,
                    format!("{CUSTOMER_SESSION_COOKIE}=customer.jwt"),
                )
                .header(header::CONTENT_TYPE, "application/json")
                .header(IDEMPOTENCY_KEY_HEADER, "cookie-create-1");
            if let Some(csrf) = csrf {
                builder = builder.header(CUSTOMER_CSRF_HEADER, csrf);
            }
            builder
                .body(Body::from(
                    json!({ "name": "", "environment": "live", "scope": "requests:write" })
                        .to_string(),
                ))
                .unwrap()
        };

        let missing = app
            .clone()
            .oneshot(request("https://app.fiducia.cloud", None))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::FORBIDDEN);

        let sibling = app
            .clone()
            .oneshot(request("https://admin.fiducia.cloud", Some(&csrf)))
            .await
            .unwrap();
        assert_eq!(sibling.status(), StatusCode::FORBIDDEN);

        let accepted = app
            .oneshot(request("https://app.fiducia.cloud", Some(&csrf)))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn portal_served_at_root_on_app_host_is_hardened_like_app() {
        // The authenticated dashboard is reachable at both `/app` and `/` (on the
        // customer app host). It carries the user's email, org ids, and CSRF token,
        // so the root path must be just as no-store / strict-CSP as `/app`.
        let response = build_router(test_config())
            .oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::HOST, "app.fiducia.cloud")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body_marker = response
            .headers()
            .get(header::CONTENT_SECURITY_POLICY)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        assert_eq!(
            response.headers().get(header::CACHE_CONTROL).unwrap(),
            "no-store",
            "root-served portal must not be cacheable"
        );
        assert_eq!(response.headers().get(header::PRAGMA).unwrap(), "no-cache");
        assert!(
            body_marker.contains("form-action 'self'"),
            "root-served portal must carry the strict portal CSP, got: {body_marker}"
        );
        // `same-origin`, not `no-referrer`: no-referrer nulls the Origin header
        // browsers attach to mutations, which would break the same-origin gate
        // for every real browser (see the security_headers comment).
        assert_eq!(
            response.headers().get(header::REFERRER_POLICY).unwrap(),
            "same-origin"
        );
    }

    #[tokio::test]
    async fn customer_dynamic_responses_are_never_cacheable() {
        for uri in ["/login", "/app"] {
            let response = build_router(test_config())
                .oneshot(
                    Request::builder()
                        .uri(uri)
                        .header(header::HOST, "app.fiducia.cloud")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.headers().get(header::CACHE_CONTROL).unwrap(),
                "no-store"
            );
            assert_eq!(response.headers().get(header::PRAGMA).unwrap(), "no-cache");
            assert!(response
                .headers()
                .get(header::CONTENT_SECURITY_POLICY)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("form-action 'self'"));
        }
    }

    #[tokio::test]
    async fn multi_org_portal_propagates_only_validated_selection() {
        let response = build_router(multi_org_config())
            .oneshot(
                Request::builder()
                    .uri(format!("/app/api-keys?org_id={ORG_B}"))
                    .header(header::HOST, "app.fiducia.cloud")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = String::from_utf8(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap()
                .to_vec(),
        )
        .unwrap();
        assert!(body.contains(&format!("?org_id={ORG_B}")));
        assert!(body.contains(&format!("name=\"org_id\" value=\"{ORG_B}\"")));
        assert!(body.contains("name=\"csrf_token\""));
        assert!(body.contains("name=\"idempotency_key\""));

        let foreign = build_router(multi_org_config())
            .oneshot(
                Request::builder()
                    .uri("/app/api-keys?org_id=00000000-0000-4000-8000-000000000003")
                    .header(header::HOST, "app.fiducia.cloud")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(foreign.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn api_key_table_exposes_csrf_protected_rotation_and_revocation() {
        let key: AuthKeyMeta = serde_json::from_value(mock_key_meta()).unwrap();
        let display = auth_key_to_display(&key, ORG_B).unwrap();
        let body = api_keys_table_markup(&[display], None, "csrf-test", ORG_B).into_string();
        assert!(body.contains("/app/api-keys/rotate"));
        assert!(body.contains("/app/api-keys/revoke"));
        assert!(body.contains("name=\"csrf_token\" value=\"csrf-test\""));
        assert!(body.contains("name=\"idempotency_key\""));
        assert!(body.contains(&format!("name=\"org_id\" value=\"{ORG_B}\"")));
    }

    #[tokio::test]
    async fn user_controlled_customer_fields_are_bounded_before_io() {
        let (status, _, body) = send_json(
            "POST",
            "/api/customer/api-keys",
            json!({
                "name": "x".repeat(MAX_API_KEY_NAME_CHARS + 1),
                "environment": "live",
                "scope": "requests:write",
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
            "name_too_long"
        );

        let (status, _, body) = send_json(
            "PUT",
            "/api/customer/preferences",
            json!({
                "region": "iad1",
                "timezone": "x".repeat(MAX_TIMEZONE_CHARS + 1),
                "density": "compact",
                "notify_lock_contention": false,
                "notify_key_rotation": true,
                "notify_mfa": true,
            }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
            "invalid_timezone"
        );

        let (status, _, body) = send_json(
            "POST",
            "/api/customer/security/sessions/revoke",
            json!({ "device": "x".repeat(MAX_SESSION_DEVICE_CHARS + 1) }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["error"],
            "invalid_device"
        );
    }

    #[tokio::test]
    async fn websocket_rejects_same_site_sibling_origin() {
        let response = build_router(test_config())
            .oneshot(
                Request::builder()
                    .uri("/app/ws")
                    .header(header::HOST, "app.fiducia.cloud")
                    .header(header::ORIGIN, "https://admin.fiducia.cloud")
                    .header("sec-fetch-site", "same-site")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
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
