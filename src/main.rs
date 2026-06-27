use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::{routing::get, Json, Router};
use maud::{html, Markup, PreEscaped, DOCTYPE};
use serde_json::json;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
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
        .route("/", get(root))
        .route("/app", get(customer_home))
        .route("/app/", get(customer_home))
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

async fn root(State(config): State<AppConfig>, headers: HeaderMap) -> Response {
    if should_serve_customer_app(&config, &headers) {
        return customer_page(&config, CustomerTab::Overview).into_response();
    }

    match tokio::fs::read_to_string(config.static_dir.join("index.html")).await {
        Ok(body) => Html(body).into_response(),
        Err(_) => (StatusCode::NOT_FOUND, "static index not found").into_response(),
    }
}

async fn customer_home(State(config): State<AppConfig>) -> Markup {
    customer_page(&config, CustomerTab::Overview)
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

async fn customer_ws(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(customer_ws_stream)
}

async fn customer_events() -> impl IntoResponse {
    let stream = async_stream::stream! {
        yield Ok::<Event, Infallible>(stream_event("connected", 0));

        let mut interval = tokio::time::interval(Duration::from_secs(STREAM_HEARTBEAT_SECS));
        let mut sequence = 1_u64;
        loop {
            interval.tick().await;
            yield Ok::<Event, Infallible>(stream_event("refresh", sequence));
            sequence = sequence.saturating_add(1);
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
    Overview,
    Locks,
    Requests,
    Kv,
    Services,
}

impl CustomerTab {
    fn all() -> [CustomerTab; 5] {
        [
            CustomerTab::Overview,
            CustomerTab::Locks,
            CustomerTab::Requests,
            CustomerTab::Kv,
            CustomerTab::Services,
        ]
    }

    fn href(self) -> &'static str {
        match self {
            CustomerTab::Overview => "/app",
            CustomerTab::Locks => "/app/locks",
            CustomerTab::Requests => "/app/requests",
            CustomerTab::Kv => "/app/kv",
            CustomerTab::Services => "/app/services",
        }
    }

    fn label(self) -> &'static str {
        match self {
            CustomerTab::Overview => "Overview",
            CustomerTab::Locks => "Locks",
            CustomerTab::Requests => "Requests",
            CustomerTab::Kv => "Config KV",
            CustomerTab::Services => "Services",
        }
    }

    fn count(self) -> &'static str {
        match self {
            CustomerTab::Overview => "12",
            CustomerTab::Locks => "4",
            CustomerTab::Requests => "6",
            CustomerTab::Kv => "3",
            CustomerTab::Services => "5",
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
                                    p { "Customer operations across locks, requests, KV, schedules, leadership, and live service registrations." }
                                }
                                div class="toolbar" {
                                    button type="button" hx-get="/app/fragments/summary" hx-target="#summary" hx-swap="innerHTML" { "Refresh" }
                                    a href="/api/info" { "API info" }
                                }
                            }
                            section id="summary" hx-get="/app/fragments/summary" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
                                (summary_markup())
                            }
                            div class="panel-grid" {
                                section id="locks-panel" class="panel" hx-get="/app/fragments/locks" hx-trigger="fiducia:refresh from:body" hx-swap="innerHTML" {
                                    (locks_markup())
                                }
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
        "held" | "current" | "healthy" | "committed" | "linearized" => "tag tag--ok",
        "renewing" | "ttl" | "redirected" => "tag tag--warn",
        "degraded" | "rejected" => "tag tag--error",
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
        AppConfig {
            static_dir: temp_static_dir(),
            customer_static_dir: temp_customer_static_dir(),
            customer_app_host: "app.fiducia.cloud".to_string(),
            customer_site_mode: false,
            supabase_url: None,
            supabase_anon_key: None,
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
        let (status, ct, _body) = send("/docs/diagram").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/html"), "ct={ct}");
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
        assert!(body.contains("Customer operations across locks"));
        assert!(body.contains("checkout:tenant-42"));
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
