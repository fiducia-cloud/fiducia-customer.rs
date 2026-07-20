//! Inbound billing webhook ingestion for Stripe and PayPal.
//!
//! The route is unauthenticated by necessity (providers POST to it directly), so
//! the provider SIGNATURE is the only thing that makes a body trustworthy. This
//! module verifies it via `fiducia-payments` before touching any state, then
//! records the event in `billing_webhook_events` with the unique
//! `(provider, provider_event_id)` index acting as the idempotency primitive: a
//! provider's at-least-once redelivery either inserts a fresh row (first sight)
//! or conflicts (already handled), and we ACK 200 either way so the provider
//! stops retrying.
//!
//! Secrets come from the environment (not `AppConfig`, to avoid threading a new
//! field through every construction site): `STRIPE_WEBHOOK_SECRET` and
//! `PAYPAL_WEBHOOK_ID`. A provider whose secret is unset returns 503 rather than
//! silently accepting unverifiable events.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use sea_orm::sea_query::OnConflict;
use sea_orm::{ActiveValue::Set, DbErr, EntityTrait};
use serde_json::json;
use sha2::{Digest, Sha256};

use fiducia_payments::{paypal, stripe, Provider, VerifiedEvent};

use crate::entity::billing_webhook_events as wh;
use crate::AppConfig;

/// Max time to fetch a PayPal verification certificate.
const CERT_FETCH_TIMEOUT_SECS: u64 = 5;

/// `POST /api/customer/billing/webhooks/:provider` — verify and record a
/// provider webhook. Always fail closed: an unknown provider, missing secret,
/// or bad signature never reaches the ledger.
pub async fn webhook(
    State(config): State<AppConfig>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let provider: Provider = match provider.parse() {
        Ok(p) => p,
        Err(_) => return deny(StatusCode::NOT_FOUND, "unknown_provider"),
    };

    let verified = match verify(provider, &headers, &body).await {
        Ok(event) => event,
        Err(reject) => return reject.response(),
    };

    match record(&config, &verified, &body).await {
        Ok(Ingest::Recorded) => {
            (StatusCode::OK, Json(json!({ "ok": true, "deduped": false }))).into_response()
        }
        // Already processed a prior delivery — ACK so the provider stops retrying.
        Ok(Ingest::Deduped) => {
            (StatusCode::OK, Json(json!({ "ok": true, "deduped": true }))).into_response()
        }
        Err(IngestError::Unavailable) => deny(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable"),
        Err(IngestError::Db(error)) => {
            tracing::error!(%error, provider = %provider, "billing webhook persist failed");
            deny(StatusCode::INTERNAL_SERVER_ERROR, "persist_failed")
        }
    }
}

/// Why a webhook was rejected before it could be recorded. Kept small (no
/// `Response` inside) so the hot Result stays cheap; mapped to an opaque HTTP
/// response once, at the handler boundary.
enum Reject {
    /// Provider is known but its secret is unset — fail closed rather than
    /// accept unverifiable events.
    NotConfigured,
    /// A required signature header was absent.
    MissingHeader,
    /// The PayPal verification certificate could not be fetched.
    CertFetchFailed,
    /// The signature did not verify (forged, wrong secret, stale, tampered).
    Signature,
}

impl Reject {
    fn response(&self) -> Response {
        match self {
            // Opaque to the caller: a prober learns only "rejected"; the specific
            // reason is logged where the reject is raised.
            Reject::NotConfigured => deny(StatusCode::SERVICE_UNAVAILABLE, "provider_not_configured"),
            Reject::MissingHeader => deny(StatusCode::BAD_REQUEST, "missing_signature_header"),
            Reject::CertFetchFailed => deny(StatusCode::BAD_GATEWAY, "cert_fetch_failed"),
            Reject::Signature => deny(StatusCode::BAD_REQUEST, "signature_verification_failed"),
        }
    }
}

/// Verify the signature for `provider` and return the authenticated event, or a
/// [`Reject`] the caller maps to an HTTP response.
async fn verify(
    provider: Provider,
    headers: &HeaderMap,
    body: &Bytes,
) -> Result<VerifiedEvent, Reject> {
    match provider {
        Provider::Stripe => {
            let secret = env_secret("STRIPE_WEBHOOK_SECRET")?;
            let sig = header(headers, "stripe-signature")?;
            stripe::verify(body, &sig, &secret, now_unix(), stripe::DEFAULT_TOLERANCE_SECS)
                .map_err(reject)
        }
        Provider::Paypal => {
            let webhook_id = env_secret("PAYPAL_WEBHOOK_ID")?;
            let cert_url = header(headers, "paypal-cert-url")?;
            // Gate the URL BEFORE fetching it: fetching an attacker-controlled
            // cert URL would be an SSRF. `paypal::verify` re-checks as well.
            if !paypal::cert_url_is_paypal(&cert_url) {
                tracing::warn!(%cert_url, "paypal cert url is not a PayPal host");
                return Err(Reject::Signature);
            }
            let cert_pem = fetch_cert(&cert_url).await.map_err(|error| {
                tracing::warn!(%error, "paypal cert fetch failed");
                Reject::CertFetchFailed
            })?;
            let transmission = paypal::Transmission {
                transmission_id: &header(headers, "paypal-transmission-id")?,
                transmission_time: &header(headers, "paypal-transmission-time")?,
                signature_b64: &header(headers, "paypal-transmission-sig")?,
                cert_url: &cert_url,
                auth_algo: &header(headers, "paypal-auth-algo")?,
            };
            paypal::verify(&transmission, &webhook_id, body, &cert_pem).map_err(reject)
        }
    }
}

enum Ingest {
    Recorded,
    Deduped,
}

enum IngestError {
    Unavailable,
    Db(DbErr),
}

/// Idempotently record a verified event. The unique index does the dedup: a
/// conflicting insert surfaces as `RecordNotInserted`, which we treat as "seen".
async fn record(
    config: &AppConfig,
    event: &VerifiedEvent,
    raw_body: &Bytes,
) -> Result<Ingest, IngestError> {
    let db = config.pool.as_ref().ok_or(IngestError::Unavailable)?;
    let payload_sha256 = hex::encode(Sha256::digest(raw_body));

    let model = wh::ActiveModel {
        id: Set(uuid::Uuid::new_v4()),
        provider: Set(event.provider.as_str().to_string()),
        provider_event_id: Set(event.id.clone()),
        event_type: Set(event.event_type.clone()),
        signature_verified: Set(true),
        payload_sha256: Set(payload_sha256),
        ..Default::default()
    };

    let result = wh::Entity::insert(model)
        .on_conflict(
            OnConflict::columns([wh::Column::Provider, wh::Column::ProviderEventId])
                .do_nothing()
                .to_owned(),
        )
        .exec(db)
        .await;

    match result {
        Ok(_) => Ok(Ingest::Recorded),
        // Nothing inserted => the (provider, event_id) row already existed.
        Err(DbErr::RecordNotInserted) => Ok(Ingest::Deduped),
        Err(error) => Err(IngestError::Db(error)),
    }
}

// --- helpers -----------------------------------------------------------------

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Read a required header as an owned string.
fn header(headers: &HeaderMap, name: &str) -> Result<String, Reject> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
        .ok_or(Reject::MissingHeader)
}

/// Read a required provider secret from the environment; absent => fail closed so
/// an unconfigured provider cannot accept unverifiable events.
fn env_secret(name: &str) -> Result<String, Reject> {
    std::env::var(name)
        .ok()
        .filter(|s| !s.is_empty())
        .ok_or(Reject::NotConfigured)
}

/// Map any verification failure to the opaque signature reject — a probing
/// attacker learns only "rejected", while the reason is logged server-side.
fn reject(error: fiducia_payments::VerifyError) -> Reject {
    tracing::warn!(%error, "billing webhook signature rejected");
    Reject::Signature
}

fn deny(status: StatusCode, code: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": code }))).into_response()
}

/// Fetch the PayPal verification certificate. The caller has already gated the
/// host to `*.paypal.com`; this only bounds the request time.
async fn fetch_cert(cert_url: &str) -> Result<String, reqwest::Error> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(CERT_FETCH_TIMEOUT_SECS))
            // Never follow redirects: the host gate validated the *cert_url*, but
            // a 30x from a genuine paypal.com URL to an attacker host would slip
            // past it. Pin the fetch to the URL we approved.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("paypal cert client must build")
    });
    client.get(cert_url).send().await?.error_for_status()?.text().await
}
