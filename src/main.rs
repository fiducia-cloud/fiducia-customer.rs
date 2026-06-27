use axum::{routing::get, Json, Router};
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::services::{ServeDir, ServeFile};
use tower_http::trace::TraceLayer;

const SERVICE: &str = "fiducia-backend";

#[tokio::main]
async fn main() {
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
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
