// fiducia-backend entrypoint: the axum app for fiducia.cloud's website tier.
// Serves the static Astro marketing site, the Maud/HTMX customer portal and its
// WS/SSE fragment streams, plus the DB-backed api_keys + @fiducia/sync endpoints.
mod auth;

use auth::{Authenticator, CustomerCtx};
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{routing::get, Json, Router};
use fiducia_interfaces_db::customer::ApiKeysRow;
use fiducia_sync_core::{ChangeEvent, ChangeOp, WriteAck};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::broadcast;
use uuid::Uuid;
use tower_http::catch_panic::CatchPanicLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

const SERVICE: &str = "fiducia-backend";

/// Bound request handling time. The site is static; nothing legitimately runs long.
const REQUEST_TIMEOUT_SECS: u64 = 30;
/// Cap request bodies — this tier only serves GETs.
const MAX_BODY_BYTES: usize = 64 * 1024;
const STREAM_HEARTBEAT_SECS: u64 = 15;
const CUSTOMER_WS_PATH: &str = "/app/ws";
const CUSTOMER_EVENTS_PATH: &str = "/app/events";

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

    // The api_keys vertical is DB-backed when DATABASE_URL points at the customer
    // Postgres plane. If it is unset (or the DB is unreachable) we fall back to the
    // in-memory mock path so the portal — and the E2E suite — still boot with no DB.
    let pool = connect_customer_db().await;

    // One process-wide broadcast channel fans server-pushed frames out to every
    // connected WS/SSE client: the existing `fiducia:refresh` fragment frames AND
    // the new `fiducia:sync` change frames emitted on api_keys mutations.
    let (stream_tx, _) = broadcast::channel::<String>(256);

    let config = AppConfig {
        static_dir: static_dir.clone(),
        customer_static_dir: customer_static_dir.clone(),
        customer_app_host: std::env::var("CUSTOMER_APP_HOST")
            .unwrap_or_else(|_| "app.fiducia.cloud".to_string()),
        customer_site_mode: std::env::var("FIDUCIA_SITE_MODE")
            .map(|v| v.eq_ignore_ascii_case("customer"))
            .unwrap_or(false),
        supabase_url: std::env::var("SUPABASE_URL").ok().filter(|v| !v.is_empty()),
        supabase_anon_key: std::env::var("SUPABASE_ANON_KEY")
            .ok()
            .filter(|v| !v.is_empty()),
        pool,
        stream_tx,
        idempotency: Arc::new(Mutex::new(HashMap::new())),
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

/// Open a pool to the customer Postgres plane when `DATABASE_URL` is set. Returns
/// `None` (mock path) if the var is unset/empty, or if the connection fails — the
/// portal must boot without a DB, so a bad/missing DB is degraded, never fatal.
async fn connect_customer_db() -> Option<PgPool> {
    let url = std::env::var("DATABASE_URL").ok().filter(|v| !v.is_empty())?;
    match sqlx::postgres::PgPoolOptions::new()
        .max_connections(5)
        .connect(&url)
        .await
    {
        Ok(pool) => {
            tracing::info!("customer DB connected — api_keys served from Postgres");
            Some(pool)
        }
        Err(err) => {
            tracing::error!("customer DB connect failed ({err}); falling back to mock api_keys");
            None
        }
    }
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

    // Routes are declared as flat literals (not nested) so the shared API-docs
    // generator (remote/tools/generate-api-docs.mjs, which scans the router's
    // route declarations) records their true paths.
    Router::new()
        // Liveness/readiness probe (matches the sibling canonical.cloud
        // convention); also available as /api/health.
        .route("/healthz", get(health))
        .route("/api/health", get(health))
        .route("/api/info", get(info))
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
            axum::routing::post(sync_write),
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
        .route("/app/api-keys", get(customer_api_keys))
        .route("/app/security", get(customer_security))
        .route("/app/settings", get(customer_settings))
        .route("/app/preferences", get(customer_settings))
        .route("/app/locks", get(customer_locks))
        .route("/app/requests", get(customer_requests))
        .route("/app/kv", get(customer_kv))
        .route("/app/services", get(customer_services))
        .route(CUSTOMER_WS_PATH, get(customer_ws))
        .route(CUSTOMER_EVENTS_PATH, get(customer_events))
        .route("/app/fragments/summary", get(summary_fragment))
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
        .layer(CatchPanicLayer::new())
}

#[derive(Clone)]
struct AppConfig {
    static_dir: PathBuf,
    customer_static_dir: PathBuf,
    customer_app_host: String,
    customer_site_mode: bool,
    supabase_url: Option<String>,
    supabase_anon_key: Option<String>,
    /// Customer Postgres pool. `None` keeps the in-memory mock api_keys path.
    pool: Option<PgPool>,
    /// Fans `fiducia:refresh` + `fiducia:sync` frames out to WS/SSE subscribers.
    stream_tx: broadcast::Sender<String>,
    /// Idempotency-Key → committed_version, so a retried sync write replays its
    /// original ack instead of re-running the UPDATE (which would re-bump version).
    /// In-process + bounded — retries happen within a connection's lifetime.
    idempotency: Arc<Mutex<HashMap<String, i64>>>,
}

/// Cap on the in-process idempotency cache; cleared wholesale past this (retries
/// are short-lived, so a coarse bound is fine and avoids unbounded growth).
const IDEMPOTENCY_CACHE_CAP: usize = 10_000;

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

/// One queued optimistic write from the @fiducia/sync client. `table` is implied
/// by the route (`api_keys`) but echoed by the client, so we accept it. `payload`
/// is the row the client optimistically stored; `base_version` is the version it
/// was edited on top of (for the ack the client reconciles against).
#[derive(Debug, Deserialize)]
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
    base_version: Option<i64>,
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

async fn customer_api_keys_json(State(config): State<AppConfig>) -> Json<serde_json::Value> {
    // DB-backed when a pool is present; otherwise the in-memory mock keys. A query
    // failure degrades to the mock so a rendered table never disappears on a blip.
    let keys = match &config.pool {
        Some(pool) => match sqlx::query_as::<_, ApiKeysRow>(
            "select * from api_keys order by created_at asc",
        )
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows.iter().map(api_key_row_to_display).collect::<Vec<_>>(),
            Err(err) => {
                tracing::error!("api_keys list query failed: {err}");
                mock_api_keys_display()
            }
        },
        None => mock_api_keys_display(),
    };

    Json(json!({
        "api_keys": keys,
        "default_require_idempotency": true,
        "allowed_environments": ["live", "test"],
        "allowed_scopes": allowed_api_key_scopes(),
    }))
}

async fn create_customer_api_key(
    State(config): State<AppConfig>,
    Json(payload): Json<CreateCustomerApiKeyRequest>,
) -> impl IntoResponse {
    if let Some(error) = validate_api_key_request(&payload) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": error, "ok": false })),
        );
    }

    let environment_prefix = if payload.environment == "live" {
        "fid_live"
    } else {
        "fid_test"
    };
    // The prefix is a public identifier (safe to derive); the secret must not
    // be — it is CSPRNG bytes, unguessable from the creation time.
    let prefix = format!("{environment_prefix}_{}", &random_token_hex(4)[..8]);
    let secret = format!("{prefix}_{}", random_token_hex(24));

    // DB path: persist the key (storing only the secret hash) and broadcast the
    // new row as a fiducia:sync change so any connected client folds it in.
    if let Some(pool) = &config.pool {
        match insert_api_key(pool, &payload, &prefix, &secret).await {
            Ok(row) => {
                broadcast_api_key_change(&config, &row);
                let mut api_key = api_key_row_to_display(&row);
                api_key["environment"] = json!(payload.environment);
                api_key["require_idempotency"] = json!(payload.require_idempotency.unwrap_or(true));
                return (
                    StatusCode::CREATED,
                    Json(json!({
                        "ok": true,
                        "api_key": api_key,
                        "secret": secret,
                        "secret_once": true,
                    })),
                );
            }
            Err(err) => tracing::error!("api_key insert failed: {err}"),
        }
    }

    // Mock path (no DB, or an insert that failed): unchanged legacy response.
    (
        StatusCode::CREATED,
        Json(json!({
            "ok": true,
            "api_key": {
                "name": payload.name.trim(),
                "prefix": prefix,
                "scopes": payload.scope,
                "last_used": "never",
                "status": "active",
                "environment": payload.environment,
                "require_idempotency": payload.require_idempotency.unwrap_or(true),
            },
            "secret": secret,
            "secret_once": true,
        })),
    )
}

async fn rotate_customer_api_key(
    State(config): State<AppConfig>,
    Json(payload): Json<RotateCustomerApiKeyRequest>,
) -> impl IntoResponse {
    let prefix = payload.prefix.trim();
    if prefix.is_empty() || !prefix.starts_with("fid_") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_key_prefix", "ok": false })),
        );
    }

    let issued_at_ms = unix_epoch_ms();
    let replacement_secret = format!("{prefix}_{}", random_token_hex(24));

    // DB path: replace the stored secret hash. The bump_row_version trigger
    // advances `version` + `updated_at`; broadcast the bumped row.
    if let Some(pool) = &config.pool {
        match sqlx::query_as::<_, ApiKeysRow>(
            "update api_keys set secret_hash = $1 where key_id = $2 returning *",
        )
        .bind(hash_secret(&replacement_secret))
        .bind(prefix)
        .fetch_optional(pool)
        .await
        {
            Ok(Some(row)) => broadcast_api_key_change(&config, &row),
            Ok(None) => {}
            Err(err) => tracing::error!("api_key rotate failed: {err}"),
        }
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "prefix": prefix,
            "rotated_at_ms": issued_at_ms,
            "replacement_secret": replacement_secret,
            "overlap_seconds": 900,
        })),
    )
}

/// The @fiducia/sync write path, generic in `{table}` (only `api_keys` is DB-wired
/// today). Persists the queued optimistic write, returns the committed row version
/// (a shared `WriteAck`) so the client adopts it and clears `dirty`, and broadcasts
/// the change so every other client reconciles. Honors the client's Idempotency-Key.
async fn sync_write(
    State(config): State<AppConfig>,
    Path(table): Path<String>,
    headers: HeaderMap,
    Json(req): Json<SyncWriteRequest>,
) -> impl IntoResponse {
    // Idempotency: replay the original ack for a retried key instead of re-running
    // the UPDATE (whose trigger would re-bump `version`). Matches the client's
    // stable `Idempotency-Key: table:id:op:base_version`.
    let idem_key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    if let Some(key) = &idem_key {
        if let Some(v) = config.idempotency.lock().unwrap().get(key).copied() {
            return ack(&req.id, v);
        }
    }

    let committed = match table.as_str() {
        "api_keys" => sync_write_api_keys_row(&config, &req).await,
        // No DB-wired write handler for this table yet — fall through to the
        // monotonic fallback ack so the client's queue still drains.
        _ => None,
    };
    let version = committed.unwrap_or_else(|| req.base_version.unwrap_or(0) + 1);

    if let Some(key) = idem_key {
        let mut cache = config.idempotency.lock().unwrap();
        if cache.len() >= IDEMPOTENCY_CACHE_CAP {
            cache.clear();
        }
        cache.insert(key, version);
    }
    ack(&req.id, version)
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

/// Persist one queued optimistic write to `api_keys`, broadcasting the committed
/// change. Returns the committed row version, or `None` when there was no pool /
/// no matching row / a bad id (caller falls back to a monotonic ack).
async fn sync_write_api_keys_row(config: &AppConfig, req: &SyncWriteRequest) -> Option<i64> {
    let pool = config.pool.as_ref()?;
    let id = Uuid::parse_str(&req.id).ok()?;
    let op = req.op.as_deref().unwrap_or("upsert");

    let committed = if op == "delete" {
        // A delete on a revocable credential is a soft revoke, not a row drop, so
        // audit/history stay intact. Version still bumps.
        sqlx::query_as::<_, ApiKeysRow>(
            "update api_keys set revoked = true where id = $1 returning *",
        )
        .bind(id)
        .fetch_optional(pool)
        .await
    } else {
        let payload = req.payload.clone().unwrap_or_else(|| json!({}));
        let name = payload.get("name").and_then(|v| v.as_str());
        let scopes = payload_scopes(&payload);
        let env = payload
            .get("environment")
            .and_then(|v| v.as_str())
            .or_else(|| payload.get("env").and_then(|v| v.as_str()));
        let revoked = match payload.get("status").and_then(|v| v.as_str()) {
            Some("revoked") => Some(true),
            Some(_) => Some(false),
            None => payload.get("revoked").and_then(|v| v.as_bool()),
        };
        // COALESCE keeps existing values for any field the client omitted; the
        // trigger bumps version + updated_at on the UPDATE.
        sqlx::query_as::<_, ApiKeysRow>(
            "update api_keys set \
                name = coalesce($2, name), \
                scopes = coalesce($3, scopes), \
                env = coalesce($4, env), \
                revoked = coalesce($5, revoked) \
             where id = $1 returning *",
        )
        .bind(id)
        .bind(name)
        .bind(scopes)
        .bind(env)
        .bind(revoked)
        .fetch_optional(pool)
        .await
    };

    match committed {
        Ok(Some(row)) => {
            broadcast_api_key_change(config, &row);
            Some(row.version)
        }
        Ok(None) => None,
        Err(err) => {
            tracing::error!("api_keys sync write failed: {err}");
            None
        }
    }
}

/// SHA-256 of an API-key secret. Only the hash is ever persisted — the plaintext
/// secret is shown to the caller once and never stored.
fn hash_secret(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    format!("sha256:{digest:x}")
}

/// Insert a new api_keys row (org-scoped to the first org) and return it with the
/// server-assigned `id` + `version`.
async fn insert_api_key(
    pool: &PgPool,
    payload: &CreateCustomerApiKeyRequest,
    prefix: &str,
    secret: &str,
) -> Result<ApiKeysRow, sqlx::Error> {
    let org_id: Uuid =
        sqlx::query_scalar("select id from orgs order by created_at asc limit 1")
            .fetch_one(pool)
            .await?;

    sqlx::query_as::<_, ApiKeysRow>(
        "insert into api_keys (key_id, org_id, name, secret_hash, scopes, env) \
         values ($1, $2, $3, $4, $5, $6) returning *",
    )
    .bind(prefix)
    .bind(org_id)
    .bind(payload.name.trim())
    .bind(hash_secret(secret))
    .bind(json!([payload.scope]))
    .bind(&payload.environment)
    .fetch_one(pool)
    .await
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
        "prefix": row.key_id,
        "scopes": scopes,
        "last_used": if row.last_used_at.is_some() { "recently" } else { "never" },
        "status": if row.revoked { "revoked" } else { "active" },
        "environment": row.env,
        "version": row.version,
    })
}

/// Broadcast a single api_keys upsert as a `fiducia:sync` frame over the shared
/// stream. Send errors (no subscribers) are ignored.
fn broadcast_api_key_change(config: &AppConfig, row: &ApiKeysRow) {
    // Built from the shared fiducia-sync-core ChangeEvent so the server frame and
    // the @fiducia/sync client decoder agree on exactly one envelope shape.
    let change = ChangeEvent {
        table: "api_keys".to_string(),
        op: ChangeOp::Upsert,
        id: row.id.to_string(),
        version: row.version,
        row: api_key_row_to_display(row),
        at_ms: unix_epoch_ms() as i64,
    };
    let frame = json!({ "event": "fiducia:sync", "changes": [change] });
    let _ = config.stream_tx.send(frame.to_string());
}

/// The mock api_keys rendered as the same display JSON the DB path emits.
fn mock_api_keys_display() -> Vec<serde_json::Value> {
    api_keys().iter().map(api_key_json).collect()
}

async fn customer_preferences_json() -> Json<CustomerPreferences> {
    Json(default_customer_preferences())
}

async fn update_customer_preferences(
    Json(payload): Json<CustomerPreferences>,
) -> impl IntoResponse {
    if !CUSTOMER_REGIONS.contains(&payload.region.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_region", "ok": false })),
        );
    }
    if !["comfortable", "compact"].contains(&payload.density.as_str()) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_density", "ok": false })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "preferences": payload,
            "saved_at_ms": unix_epoch_ms(),
        })),
    )
}

async fn customer_security_sessions_json() -> Json<serde_json::Value> {
    Json(json!({
        "sessions": sessions().iter().map(session_json).collect::<Vec<_>>(),
        "revoke_supported": true,
    }))
}

async fn revoke_customer_security_session(
    Json(payload): Json<RevokeCustomerSecuritySessionRequest>,
) -> impl IntoResponse {
    let device = payload.device.trim();
    if device.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "device_required", "ok": false })),
        );
    }

    let found = sessions().iter().any(|row| row.device == device);
    if !found {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "session_not_found", "ok": false })),
        );
    }

    (
        StatusCode::OK,
        Json(json!({
            "ok": true,
            "device": device,
            "status": "revoked",
            "revoked_at_ms": unix_epoch_ms(),
        })),
    )
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
        "admin:read",
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

async fn customer_locks(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Locks)
}

async fn customer_requests(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Requests)
}

async fn customer_kv(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Kv)
}

async fn customer_services(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Services)
}

async fn summary_fragment() -> Markup {
    summary_markup()
}

async fn locks_fragment() -> Markup {
    locks_markup()
}

async fn requests_fragment() -> Markup {
    requests_markup()
}

async fn kv_fragment() -> Markup {
    kv_markup()
}

async fn services_fragment() -> Markup {
    services_markup()
}

async fn customer_ws(State(config): State<AppConfig>, ws: WebSocketUpgrade) -> Response {
    let rx = config.stream_tx.subscribe();
    ws.on_upgrade(move |socket| customer_ws_stream(socket, rx))
}

async fn customer_events(State(config): State<AppConfig>) -> impl IntoResponse {
    let mut rx = config.stream_tx.subscribe();
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
                // Server-pushed frames (e.g. fiducia:sync on an api_keys mutation)
                // ride the same SSE stream as a distinct `fiducia-sync` event.
                frame = rx.recv() => {
                    match frame {
                        Ok(payload) => yield Ok::<Event, Infallible>(sync_stream_event(&payload)),
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
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

async fn customer_ws_stream(mut socket: WebSocket, mut rx: broadcast::Receiver<String>) {
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
            // Forward broadcast frames (fiducia:sync change events) verbatim; the
            // client disambiguates by the JSON `event` field.
            frame = rx.recv() => {
                match frame {
                    Ok(payload) => {
                        if socket.send(Message::Text(payload)).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
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

/// Wrap a broadcast payload (a fiducia:sync frame) as an SSE event the sync SDK's
/// EventSource listener (`fiducia-sync`) folds into the local store.
fn sync_stream_event(payload: &str) -> Event {
    Event::default().event("fiducia-sync").data(payload)
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

/// Hex-encoded CSPRNG token for demo secrets. The customer portal is a mock
/// (no key is persisted), but it must still model correct behaviour: a secret
/// is never derived from a timestamp — it comes from the OS CSPRNG, so it can't
/// be guessed from roughly-known creation time. Mirrors fiducia-auth, which is
/// the real issuer.
fn random_token_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    // getrandom only fails if the OS entropy source is unavailable; treat that
    // as fatal for a secret rather than falling back to a weak value.
    getrandom::getrandom(&mut buf).expect("OS CSPRNG unavailable");
    let mut out = String::with_capacity(bytes * 2);
    for b in buf {
        out.push_str(&format!("{b:02x}"));
    }
    out
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
            CustomerTab::Auth => "Login & Signup",
            CustomerTab::ApiKeys => "API Keys",
            CustomerTab::Security => "Security",
            CustomerTab::Settings => "Settings",
            CustomerTab::Locks => "Locks",
            CustomerTab::Requests => "Requests",
            CustomerTab::Kv => "Config KV",
            CustomerTab::Services => "Services",
        }
    }

    fn count(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "12",
            CustomerTab::Auth => "2",
            CustomerTab::ApiKeys => "3",
            CustomerTab::Security => "2FA",
            CustomerTab::Settings => "8",
            CustomerTab::Locks => "4",
            CustomerTab::Requests => "6",
            CustomerTab::Kv => "3",
            CustomerTab::Services => "5",
        }
    }

    fn description(self) -> &'static str {
        match self {
            CustomerTab::Dashboard => "Account posture, API access, realtime health, and customer operations in one workspace.",
            CustomerTab::Auth => "Supabase Auth login, signup, magic link, and session controls for end customers.",
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

struct LockRow {
    key: &'static str,
    tenant: &'static str,
    region: &'static str,
    leader: &'static str,
    holder: &'static str,
    fencing_token: u64,
    lease: &'static str,
    queue: usize,
    status: &'static str,
}

struct RequestRow {
    method: &'static str,
    path: &'static str,
    tenant: &'static str,
    region: &'static str,
    shard: u64,
    leader: &'static str,
    status: &'static str,
    latency: &'static str,
}

struct KvRow {
    key: &'static str,
    revision: u64,
    region: &'static str,
    expires: &'static str,
    status: &'static str,
}

struct ServiceRow {
    service: &'static str,
    instances: usize,
    region: &'static str,
    leader: &'static str,
    status: &'static str,
}

struct ApiKeyRow {
    name: &'static str,
    prefix: &'static str,
    scopes: &'static str,
    last_used: &'static str,
    status: &'static str,
}

struct SessionRow {
    device: &'static str,
    location: &'static str,
    last_seen: &'static str,
    status: &'static str,
}

fn locks() -> [LockRow; 4] {
    [
        LockRow {
            key: "checkout:tenant-42",
            tenant: "tenant-42",
            region: "iad1",
            leader: "node-iad-02",
            holder: "worker-7",
            fencing_token: 18842,
            lease: "11.8s",
            queue: 2,
            status: "held",
        },
        LockRow {
            key: "invoice:tenant-17",
            tenant: "tenant-17",
            region: "sfo1",
            leader: "node-sfo-01",
            holder: "billing-3",
            fencing_token: 9124,
            lease: "7.1s",
            queue: 0,
            status: "held",
        },
        LockRow {
            key: "cron:nightly-rollup",
            tenant: "platform",
            region: "ams1",
            leader: "node-ams-02",
            holder: "scheduler-1",
            fencing_token: 5612,
            lease: "28.4s",
            queue: 1,
            status: "renewing",
        },
        LockRow {
            key: "deploy:tenant-88",
            tenant: "tenant-88",
            region: "fra1",
            leader: "node-fra-01",
            holder: "release-5",
            fencing_token: 2417,
            lease: "free",
            queue: 0,
            status: "available",
        },
    ]
}

fn requests() -> [RequestRow; 6] {
    [
        RequestRow {
            method: "POST",
            path: "/v1/locks/checkout/acquire",
            tenant: "tenant-42",
            region: "iad1",
            shard: 18,
            leader: "node-iad-02",
            status: "committed",
            latency: "0.74 ms",
        },
        RequestRow {
            method: "PUT",
            path: "/v1/kv/features.checkout",
            tenant: "tenant-42",
            region: "iad1",
            shard: 18,
            leader: "node-iad-02",
            status: "committed",
            latency: "0.81 ms",
        },
        RequestRow {
            method: "POST",
            path: "/v1/services/api/heartbeat",
            tenant: "tenant-17",
            region: "sfo1",
            shard: 7,
            leader: "node-sfo-01",
            status: "committed",
            latency: "0.62 ms",
        },
        RequestRow {
            method: "POST",
            path: "/v1/elections/payments/campaign",
            tenant: "tenant-17",
            region: "sfo1",
            shard: 7,
            leader: "node-sfo-01",
            status: "redirected",
            latency: "1.21 ms",
        },
        RequestRow {
            method: "GET",
            path: "/v1/kv/features.search",
            tenant: "tenant-88",
            region: "fra1",
            shard: 29,
            leader: "node-fra-01",
            status: "linearized",
            latency: "0.94 ms",
        },
        RequestRow {
            method: "POST",
            path: "/v1/rate-limit/consume",
            tenant: "tenant-55",
            region: "sin1",
            shard: 33,
            leader: "node-sin-02",
            status: "committed",
            latency: "0.69 ms",
        },
    ]
}

fn kv_entries() -> [KvRow; 3] {
    [
        KvRow {
            key: "features.checkout.new_flow",
            revision: 7734,
            region: "iad1",
            expires: "none",
            status: "current",
        },
        KvRow {
            key: "limits.tenant-42.write_qps",
            revision: 7731,
            region: "iad1",
            expires: "none",
            status: "current",
        },
        KvRow {
            key: "maintenance.banner",
            revision: 7718,
            region: "ams1",
            expires: "42m",
            status: "ttl",
        },
    ]
}

fn services() -> [ServiceRow; 5] {
    [
        ServiceRow {
            service: "checkout-api",
            instances: 8,
            region: "iad1",
            leader: "node-iad-02",
            status: "healthy",
        },
        ServiceRow {
            service: "billing-worker",
            instances: 5,
            region: "sfo1",
            leader: "node-sfo-01",
            status: "healthy",
        },
        ServiceRow {
            service: "scheduler",
            instances: 3,
            region: "ams1",
            leader: "node-ams-02",
            status: "healthy",
        },
        ServiceRow {
            service: "search-indexer",
            instances: 4,
            region: "fra1",
            leader: "node-fra-01",
            status: "healthy",
        },
        ServiceRow {
            service: "edge-ratelimit",
            instances: 12,
            region: "sin1",
            leader: "node-sin-02",
            status: "degraded",
        },
    ]
}

fn api_keys() -> [ApiKeyRow; 3] {
    [
        ApiKeyRow {
            name: "Production checkout",
            prefix: "fid_live_7Qp2",
            scopes: "locks:write, kv:read, requests:write",
            last_used: "38s ago",
            status: "active",
        },
        ApiKeyRow {
            name: "Billing worker",
            prefix: "fid_live_X4a9",
            scopes: "locks:write, services:read",
            last_used: "7m ago",
            status: "rotating",
        },
        ApiKeyRow {
            name: "Staging replay",
            prefix: "fid_test_H2m8",
            scopes: "requests:write, kv:write",
            last_used: "2h ago",
            status: "limited",
        },
    ]
}

fn api_key_json(row: &ApiKeyRow) -> serde_json::Value {
    json!({
        "name": row.name,
        "prefix": row.prefix,
        "scopes": row.scopes,
        "last_used": row.last_used,
        "status": row.status,
    })
}

fn sessions() -> [SessionRow; 3] {
    [
        SessionRow {
            device: "Chrome on macOS",
            location: "Lima, PE",
            last_seen: "current",
            status: "verified",
        },
        SessionRow {
            device: "Safari on iPhone",
            location: "San Francisco, US",
            last_seen: "22m ago",
            status: "active",
        },
        SessionRow {
            device: "CLI token exchange",
            location: "iad1",
            last_seen: "1h ago",
            status: "limited",
        },
    ]
}

fn session_json(row: &SessionRow) -> serde_json::Value {
    json!({
        "device": row.device,
        "location": row.location,
        "last_seen": row.last_seen,
        "status": row.status,
    })
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
                            span class="status-pill" { "linearizable reads" }
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
        CustomerTab::Locks => html! {
            div class="panel-grid" {
                section id="locks-panel" class="panel" hx-get="/app/fragments/locks" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
                    (locks_markup())
                }
                (realtime_events_markup())
            }
        },
        CustomerTab::Requests => html! {
            section id="requests-panel" class="panel" hx-get="/app/fragments/requests" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
                (requests_markup())
            }
        },
        CustomerTab::Kv => html! {
            section id="kv-panel" class="panel" hx-get="/app/fragments/kv" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
                (kv_markup())
            }
        },
        CustomerTab::Services => html! {
            section id="services-panel" class="panel" hx-get="/app/fragments/services" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
                (services_markup())
            }
        },
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
        div class="panel-grid" {
            section id="locks-panel" class="panel" hx-get="/app/fragments/locks" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
                (locks_markup())
            }
            (realtime_events_markup())
        }
        section id="requests-panel" class="panel" hx-get="/app/fragments/requests" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
            (requests_markup())
        }
        section id="kv-panel" class="panel" hx-get="/app/fragments/kv" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
            (kv_markup())
        }
        section id="services-panel" class="panel" hx-get="/app/fragments/services" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
            (services_markup())
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
                        dd { "dual-key overlap enabled" }
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
                span { "2FA ready" }
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
                        option value="admin:read" { "admin:read" }
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
                span { "rotate without downtime" }
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
                        @for row in api_keys() {
                            tr {
                                td { (row.name) }
                                td class="mono" { (row.prefix) }
                                td class="mono" { (row.scopes) }
                                td { (row.last_used) }
                                td {
                                    span class=(status_class(row.status)) data-api-key-status="" { (row.status) }
                                }
                                td {
                                    button class="table-action" type="button" data-api-key-action="rotate" data-key-prefix=(row.prefix) { "Rotate" }
                                }
                            }
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
                        @for row in sessions() {
                            tr {
                                td { (row.device) }
                                td { (row.location) }
                                td { (row.last_seen) }
                                td {
                                    span class=(status_class(row.status)) data-session-status="" { (row.status) }
                                }
                                td {
                                    @if row.status == "verified" {
                                        span class="muted" { "Current" }
                                    } @else {
                                        button class="table-action" type="button" data-session-action="revoke" data-session-device=(row.device) { "Revoke" }
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

fn realtime_events_markup() -> Markup {
    html! {
        section class="panel" aria-labelledby="events-heading" {
            div class="panel__header" {
                h2 id="events-heading" { "Realtime Events" }
                span { "Supabase" }
            }
            div id="realtime-events" class="event-stream" aria-live="polite" {
                div class="empty-state" { "Waiting for realtime changes." }
            }
        }
    }
}

fn summary_markup() -> Markup {
    html! {
        div class="summary-grid" {
            div class="metric" {
                p class="metric__label" { "Active Locks" }
                p class="metric__value" { "4" }
                p class="metric__hint" { "3 held, 1 available" }
            }
            div class="metric" {
                p class="metric__label" { "Requests" }
                p class="metric__value" { "6" }
                p class="metric__hint" { "last minute sample" }
            }
            div class="metric" {
                p class="metric__label" { "KV Revisions" }
                p class="metric__value" { "7,734" }
                p class="metric__hint" { "latest committed revision" }
            }
            div class="metric" {
                p class="metric__label" { "Live Instances" }
                p class="metric__value" { "32" }
                p class="metric__hint" { "5 service groups" }
            }
        }
    }
}

fn locks_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Locks" }
            span data-freshness-clock="" { "fresh" }
        }
        div class="table-wrap" {
            table {
                thead {
                    tr {
                        th { "Key" }
                        th { "Tenant" }
                        th { "Region" }
                        th { "Leader" }
                        th { "Holder" }
                        th { "Fence" }
                        th { "Lease" }
                        th { "Queue" }
                        th { "State" }
                    }
                }
                tbody {
                    @for row in locks() {
                        tr {
                            td class="mono" { (row.key) }
                            td { (row.tenant) }
                            td { (row.region) }
                            td { (row.leader) }
                            td { (row.holder) }
                            td class="mono" { (row.fencing_token) }
                            td { (row.lease) }
                            td { (row.queue) }
                            td { (status_tag(row.status)) }
                        }
                    }
                }
            }
        }
    }
}

fn requests_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Requests" }
            span data-freshness-clock="" { "fresh" }
        }
        div class="table-wrap" {
            table {
                thead {
                    tr {
                        th { "Method" }
                        th { "Path" }
                        th { "Tenant" }
                        th { "Region" }
                        th { "Shard" }
                        th { "Leader" }
                        th { "Status" }
                        th { "Latency" }
                    }
                }
                tbody {
                    @for row in requests() {
                        tr {
                            td { (row.method) }
                            td class="mono" { (row.path) }
                            td { (row.tenant) }
                            td { (row.region) }
                            td class="mono" { (row.shard) }
                            td { (row.leader) }
                            td { (status_tag(row.status)) }
                            td { (row.latency) }
                        }
                    }
                }
            }
        }
    }
}

fn kv_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Config KV" }
            span data-freshness-clock="" { "fresh" }
        }
        div class="table-wrap" {
            table {
                thead {
                    tr {
                        th { "Key" }
                        th { "Revision" }
                        th { "Region" }
                        th { "TTL" }
                        th { "State" }
                    }
                }
                tbody {
                    @for row in kv_entries() {
                        tr {
                            td class="mono" { (row.key) }
                            td class="mono" { (row.revision) }
                            td { (row.region) }
                            td { (row.expires) }
                            td { (status_tag(row.status)) }
                        }
                    }
                }
            }
        }
    }
}

fn services_markup() -> Markup {
    html! {
        div class="panel__header" {
            h2 { "Service Discovery" }
            span data-freshness-clock="" { "fresh" }
        }
        div class="table-wrap" {
            table {
                thead {
                    tr {
                        th { "Service" }
                        th { "Instances" }
                        th { "Region" }
                        th { "Leader" }
                        th { "State" }
                    }
                }
                tbody {
                    @for row in services() {
                        tr {
                            td class="mono" { (row.service) }
                            td { (row.instances) }
                            td { (row.region) }
                            td { (row.leader) }
                            td { (status_tag(row.status)) }
                        }
                    }
                }
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
        // No pool → the mock api_keys path (mirrors the E2E harness, which boots
        // the backend without DATABASE_URL).
        AppConfig {
            static_dir: temp_static_dir(),
            customer_static_dir: temp_customer_static_dir(),
            customer_app_host: "app.fiducia.cloud".to_string(),
            customer_site_mode: false,
            supabase_url: None,
            supabase_anon_key: None,
            pool: None,
            stream_tx: broadcast::channel(16).0,
            idempotency: Arc::new(Mutex::new(HashMap::new())),
        }
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
    async fn customer_api_keys_can_be_listed_created_and_rotated() {
        let (status, ct, body) = send("/api/customer/api-keys").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("application/json"), "ct={ct}");
        let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed["api_keys"].as_array().unwrap().len(), 3);
        assert_eq!(listed["default_require_idempotency"], true);

        let (status, ct, body) = send_json(
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
        assert_eq!(status, StatusCode::CREATED);
        assert!(ct.contains("application/json"), "ct={ct}");
        let created: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(created["ok"], true);
        assert_eq!(created["api_key"]["require_idempotency"], true);
        assert!(created["api_key"]["prefix"]
            .as_str()
            .unwrap()
            .starts_with("fid_live_"));
        assert_eq!(created["secret_once"], true);

        let (status, _ct, body) = send_json(
            "POST",
            "/api/customer/api-keys/rotate",
            json!({ "prefix": created["api_key"]["prefix"] }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let rotated: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(rotated["ok"], true);
        assert_eq!(rotated["overlap_seconds"], 900);
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
    async fn customer_preferences_round_trip_json() {
        let (status, ct, body) = send("/api/customer/preferences").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("application/json"), "ct={ct}");
        let defaults: CustomerPreferences = serde_json::from_str(&body).unwrap();
        assert_eq!(defaults.region, "auto");
        assert!(defaults.notify_key_rotation);

        let (status, _ct, body) = send_json(
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
        assert_eq!(status, StatusCode::OK);
        let saved: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(saved["ok"], true);
        assert_eq!(saved["preferences"]["region"], "iad1");

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
    async fn customer_security_sessions_can_be_listed_and_revoked() {
        let (status, ct, body) = send("/api/customer/security/sessions").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("application/json"), "ct={ct}");
        let listed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(listed["sessions"].as_array().unwrap().len(), 3);
        assert_eq!(listed["revoke_supported"], true);

        let (status, _ct, body) = send_json(
            "POST",
            "/api/customer/security/sessions/revoke",
            json!({ "device": "Safari on iPhone" }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let revoked: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(revoked["ok"], true);
        assert_eq!(revoked["device"], "Safari on iPhone");
        assert_eq!(revoked["status"], "revoked");

        let (status, _ct, body) = send_json(
            "POST",
            "/api/customer/security/sessions/revoke",
            json!({ "device": "Unknown browser" }),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let rejected: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(rejected["error"], "session_not_found");
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
        assert!(body.contains("2FA ready"));
        assert!(body.contains("Preferences"));
        assert!(body.contains("checkout:tenant-42"));
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
    async fn htmx_locks_fragment_is_rendered() {
        let (status, ct, body) = send("/app/fragments/locks").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
        assert!(body.contains("Fence"));
        assert!(body.contains("checkout:tenant-42"));
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
        let resp = app.oneshot(builder.body(Body::from(body)).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn sync_write_is_generic_in_table_and_acks_a_monotonic_version() {
        // No pool in tests -> the fallback ack (base_version + 1) for ANY table, so
        // the client's write-queue always drains.
        let acked = post_sync(test_config(), None, 4).await;
        assert_eq!(acked["id"], "k1");
        assert_eq!(acked["committed_version"], 5);

        // A table with no DB-wired handler still returns a valid ack (generic route).
        let app = build_router(test_config());
        let resp = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/customer/sync/customer_preferences")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(json!({ "id": "p1", "base_version": 0 }).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn sync_write_idempotency_key_replays_the_same_ack() {
        // The Arc<Mutex> idempotency cache is shared across clones of the config.
        let config = test_config();
        let first = post_sync(config.clone(), Some("api_keys:k1:upsert:7"), 7).await;
        assert_eq!(first["committed_version"], 8);

        // Same key, different base -> the ORIGINAL ack is replayed (not 1000), so a
        // retried POST never re-runs the UPDATE / re-bumps version.
        let retry = post_sync(config.clone(), Some("api_keys:k1:upsert:7"), 999).await;
        assert_eq!(retry["committed_version"], 8);

        // A different key is computed fresh.
        let other = post_sync(config.clone(), Some("api_keys:k2:upsert:2"), 2).await;
        assert_eq!(other["committed_version"], 3);
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
