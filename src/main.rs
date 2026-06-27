use axum::http::{header, HeaderName, HeaderValue};
use axum::{routing::get, Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    fiducia_telemetry::init(SERVICE);

    // Directory of the built Astro site. Defaults to the bundled `static/`
    // (populated from fiducia-ui.web's `dist/` at build time), but can be
    // pointed straight at the frontend dist via STATIC_DIR for local dev.
    let static_dir: PathBuf = std::env::var("STATIC_DIR")
        .unwrap_or_else(|_| "static".to_string())
        .into();

    let app = build_router(static_dir.clone());

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));

    tracing::info!(
        "{SERVICE} listening on http://{addr} (serving {})",
        static_dir.display()
    );
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Build the application router. Separated from `main` so tests can exercise the
/// routes without binding a socket or initializing telemetry.
fn build_router(static_dir: PathBuf) -> Router {
    // Everything else is served from the static Astro build. Requests for
    // directories resolve to index.html, and unknown paths fall back to the
    // generated 404 page so client routing keeps working.
    let serve_dir = ServeDir::new(&static_dir)
        .append_index_html_on_directories(true)
        .fallback(ServeFile::new(static_dir.join("404.html")));

    // Routes are declared as flat literals (not nested) so the shared API-docs
    // generator (remote/tools/generate-api-docs.mjs, which scans the router's
    // route declarations) records their true paths.
    Router::new()
        // Liveness/readiness probe (matches the sibling canonical.cloud
        // convention); also available as /api/health.
        .route("/healthz", get(health))
        .route("/api/health", get(health))
        .route("/api/info", get(info))
        // Generated API docs (AGENTS.md "API Docs Contract").
        .route("/docs/api", get(api_docs_html))
        .route("/api/docs", get(api_docs_html))
        .route("/api/docs.json", get(api_docs_json))
        // Mermaid architecture diagram (rendered client-side).
        .route("/docs/diagram", get(diagram_html))
        // Everything else: the static Astro site.
        .fallback_service(serve_dir)
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

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok", "service": SERVICE }))
}

async fn info() -> Json<serde_json::Value> {
    Json(json!({
        "service": SERVICE,
        "version": env!("CARGO_PKG_VERSION"),
        "domain": "fiducia.cloud",
        "role": "website",
        // The coordination API is not served here — it lives in the data-plane
        // and control-plane services.
        "components": {
            "data_plane": "fiducia-node",
            "control_plane": "fiducia-brain",
        },
    }))
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
    fn temp_static_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "fiducia-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
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

    /// Send a GET through the router and return (status, content-type, body).
    async fn send(uri: &str) -> (StatusCode, String, String) {
        let app = build_router(temp_static_dir());
        let resp = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
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
    async fn static_asset_served_with_correct_mime() {
        let (status, ct, body) = send("/_astro/app.css").await;
        assert_eq!(status, StatusCode::OK);
        assert!(ct.contains("text/css"), "ct={ct}");
        assert!(body.contains("rebeccapurple"));
    }

    #[tokio::test]
    async fn unknown_path_falls_back_to_the_404_page() {
        // SPA-style fallback: the styled 404 page is served (ServeFile returns 200).
        let (status, _ct, body) = send("/does/not/exist").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("no quorum on this page"));
    }
}
