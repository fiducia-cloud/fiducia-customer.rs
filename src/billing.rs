//! Inbound billing webhook ingestion for Stripe and PayPal.
//!
//! The route is unauthenticated by necessity (providers POST to it directly), so
//! the provider SIGNATURE is the only thing that makes a body trustworthy. This
//! module verifies it via `fiducia-payments` before touching any state, then
//! records the event in `billing_webhook_events`.
//!
//! The unique `(provider, provider_event_id)` index is only the first half of the
//! idempotency contract. An exact authenticated redelivery must also match the
//! stored event type and SHA-256 of the exact verified bytes. Reuse of the same
//! provider identity with different authenticated content is an identity
//! conflict, not a successful dedupe, and fails closed.
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
use sea_orm::{ActiveValue::Set, ColumnTrait, DbErr, EntityTrait, QueryFilter};
use serde_json::json;
use sha2::{Digest, Sha256};

use fiducia_payments::{paypal, stripe, Provider, VerifiedEvent};

use crate::entity::billing_webhook_events as wh;
use crate::AppConfig;

/// Max time to fetch a PayPal verification certificate.
const CERT_FETCH_TIMEOUT_SECS: u64 = 5;

/// `POST /api/customer/billing/webhooks/:provider` — verify and record a
/// provider webhook. Always fail closed: an unknown provider, missing secret,
/// bad signature, or conflicting reuse of a provider event identity never
/// reaches downstream billing effects.
pub async fn webhook(
    State(config): State<AppConfig>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let provider: Provider = match provider.parse() {
        Ok(provider) => provider,
        Err(_) => return deny(StatusCode::NOT_FOUND, "unknown_provider"),
    };

    let verified = match verify(provider, &headers, &body).await {
        Ok(event) => event,
        Err(reject) => return reject.response(),
    };

    match record(&config, &verified).await {
        Ok(Ingest::Recorded) => (
            StatusCode::OK,
            Json(json!({ "ok": true, "deduped": false })),
        )
            .into_response(),
        // The prior row matched the exact verified event identity, type, and
        // payload digest, so ACKing is a safe no-op.
        Ok(Ingest::Deduped) => {
            (StatusCode::OK, Json(json!({ "ok": true, "deduped": true }))).into_response()
        }
        Err(IngestError::IdentityConflict) => {
            tracing::error!(
                provider = %provider,
                "authenticated billing event identity reused with different content"
            );
            deny(StatusCode::CONFLICT, "event_identity_conflict")
        }
        Err(IngestError::Unavailable) => {
            deny(StatusCode::SERVICE_UNAVAILABLE, "storage_unavailable")
        }
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
            Reject::NotConfigured => {
                deny(StatusCode::SERVICE_UNAVAILABLE, "provider_not_configured")
            }
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
            let signature = header(headers, "stripe-signature")?;
            stripe::verify(
                body,
                &signature,
                &secret,
                now_unix(),
                stripe::DEFAULT_TOLERANCE_SECS,
            )
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
            verify_paypal_transmission(&transmission, &webhook_id, body, &cert_pem, now_unix())
                .map_err(reject)
        }
    }
}

/// Bind PayPal's signed transmission timestamp to the same five-minute replay
/// budget as Stripe before permitting any verified event into the billing DB.
/// The clock is injectable to prove inclusive boundaries without wall-clock races.
fn verify_paypal_transmission(
    transmission: &paypal::Transmission<'_>,
    webhook_id: &str,
    raw_body: &[u8],
    cert_pem: &str,
    received_at_unix: i64,
) -> Result<VerifiedEvent, fiducia_payments::VerifyError> {
    paypal::verify_at(
        transmission,
        webhook_id,
        raw_body,
        cert_pem,
        received_at_unix,
        paypal::DEFAULT_TOLERANCE_SECS,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ingest {
    Recorded,
    Deduped,
}

#[derive(Debug)]
enum IngestError {
    Unavailable,
    IdentityConflict,
    Db(DbErr),
}

#[derive(Clone, Copy)]
struct StoredEventIdentity<'a> {
    provider: &'a str,
    provider_event_id: &'a str,
    event_type: &'a str,
    signature_verified: bool,
    payload_sha256: &'a str,
}

/// Idempotently record a verified event.
///
/// A unique-index conflict is not automatically a benign retry. The existing
/// row must match the provider, event id, event type, verified flag, and digest
/// of the exact bytes held by [`VerifiedEvent`]. Any disagreement fails closed
/// as an identity conflict.
async fn record(config: &AppConfig, event: &VerifiedEvent) -> Result<Ingest, IngestError> {
    let db = config.pool.as_ref().ok_or(IngestError::Unavailable)?;
    let payload_sha256 = verified_payload_sha256(event);

    let model = wh::ActiveModel {
        id: Set(uuid::Uuid::new_v4()),
        provider: Set(event.provider.as_str().to_string()),
        provider_event_id: Set(event.id.clone()),
        event_type: Set(event.event_type.clone()),
        signature_verified: Set(true),
        payload_sha256: Set(payload_sha256.clone()),
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
        Err(DbErr::RecordNotInserted) => {
            let existing = wh::Entity::find()
                .filter(wh::Column::Provider.eq(event.provider.as_str()))
                .filter(wh::Column::ProviderEventId.eq(event.id.as_str()))
                .one(db)
                .await
                .map_err(IngestError::Db)?;

            let Some(existing) = existing else {
                // A disappeared conflict row is not evidence of an exact retry.
                return Err(IngestError::IdentityConflict);
            };
            let stored = StoredEventIdentity {
                provider: &existing.provider,
                provider_event_id: &existing.provider_event_id,
                event_type: &existing.event_type,
                signature_verified: existing.signature_verified,
                payload_sha256: &existing.payload_sha256,
            };
            if is_exact_redelivery(stored, event, &payload_sha256) {
                Ok(Ingest::Deduped)
            } else {
                Err(IngestError::IdentityConflict)
            }
        }
        Err(error) => Err(IngestError::Db(error)),
    }
}

fn verified_payload_sha256(event: &VerifiedEvent) -> String {
    hex::encode(Sha256::digest(&event.payload))
}

fn is_exact_redelivery(
    stored: StoredEventIdentity<'_>,
    incoming: &VerifiedEvent,
    incoming_payload_sha256: &str,
) -> bool {
    stored.signature_verified
        && stored.provider == incoming.provider.as_str()
        && stored.provider_event_id == incoming.id.as_str()
        && stored.event_type == incoming.event_type.as_str()
        && stored.payload_sha256 == incoming_payload_sha256
}

// --- helpers -----------------------------------------------------------------

fn now_unix() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

/// Read a required header as an owned string.
fn header(headers: &HeaderMap, name: &str) -> Result<String, Reject> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or(Reject::MissingHeader)
}

/// Read a required provider secret from the environment; absent => fail closed so
/// an unconfigured provider cannot accept unverifiable events.
fn env_secret(name: &str) -> Result<String, Reject> {
    std::env::var(name)
        .ok()
        .filter(|secret| !secret.is_empty())
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
    client
        .get(cert_url)
        .send()
        .await?
        .error_for_status()?
        .text()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::Engine;
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::signature::{SignatureEncoding, Signer};
    use rsa::RsaPrivateKey;

    // Test-only RSA key + matching X.509 cert belong to the same pinned sibling
    // crate as the production verifier; they are not operational credentials.
    const PAYPAL_TEST_CERT: &str =
        include_str!("../../fiducia-payments.rs/tests/fixtures/paypal_test_cert.pem");
    const PAYPAL_TEST_KEY: &str =
        include_str!("../../fiducia-payments.rs/tests/fixtures/paypal_test_key.pem");
    const PAYPAL_WEBHOOK: &str = "WH-CUSTOMER-FRESHNESS";
    const PAYPAL_BODY: &[u8] =
        br#"{"id":"WH-customer-1","event_type":"PAYMENT.CAPTURE.COMPLETED"}"#;
    const PAYPAL_CERT_URL: &str = "https://api.paypal.com/v1/notifications/certs/customer-test.pem";

    fn sign_paypal(body: &[u8], time: &str, webhook_id: &str) -> String {
        let private_key =
            RsaPrivateKey::from_pkcs8_pem(PAYPAL_TEST_KEY).expect("non-secret RSA test fixture");
        let signer = SigningKey::<Sha256>::new(private_key);
        let signed_message =
            paypal::signed_message("transmission-customer", time, webhook_id, body);
        base64::engine::general_purpose::STANDARD
            .encode(signer.sign(signed_message.as_bytes()).to_bytes())
    }

    fn paypal_headers<'a>(
        transmission_time: &'a str,
        signature_b64: &'a str,
        cert_url: &'a str,
    ) -> paypal::Transmission<'a> {
        paypal::Transmission {
            transmission_id: "transmission-customer",
            transmission_time,
            signature_b64,
            cert_url,
            auth_algo: "SHA256withRSA",
        }
    }

    #[test]
    fn customer_paypal_boundary_accepts_signed_fresh_event_at_inclusive_window_edges() {
        let time = "1970-01-01T00:00:00Z";
        let signature = sign_paypal(PAYPAL_BODY, time, PAYPAL_WEBHOOK);
        let transmission = paypal_headers(time, &signature, PAYPAL_CERT_URL);
        for now in [0, 300, -300] {
            let event = verify_paypal_transmission(
                &transmission,
                PAYPAL_WEBHOOK,
                PAYPAL_BODY,
                PAYPAL_TEST_CERT,
                now,
            )
            .expect("authentic customer event within replay window");
            assert_eq!(event.provider, Provider::Paypal);
            assert_eq!(event.id, "WH-customer-1");
            assert_eq!(event.payload, PAYPAL_BODY);
        }
    }

    #[test]
    fn customer_paypal_boundary_blocks_signed_replays_after_window_both_directions() {
        let time = "1970-01-01T00:00:00Z";
        let signature = sign_paypal(PAYPAL_BODY, time, PAYPAL_WEBHOOK);
        let transmission = paypal_headers(time, &signature, PAYPAL_CERT_URL);
        for now in [301, -301, i64::MAX, i64::MIN] {
            assert!(matches!(
                verify_paypal_transmission(
                    &transmission,
                    PAYPAL_WEBHOOK,
                    PAYPAL_BODY,
                    PAYPAL_TEST_CERT,
                    now,
                ),
                Err(fiducia_payments::VerifyError::TimestampOutOfTolerance { .. })
            ));
        }
    }

    #[test]
    fn customer_paypal_boundary_rejects_signed_time_tampering_and_body_replay_conflicts() {
        let time = "1970-01-01T00:00:00Z";
        let signature = sign_paypal(PAYPAL_BODY, time, PAYPAL_WEBHOOK);
        let transmission = paypal_headers(time, &signature, PAYPAL_CERT_URL);
        let tampered_body = br#"{"id":"WH-customer-1","event_type":"PAYMENT.CAPTURE.DENIED"}"#;
        assert!(matches!(
            verify_paypal_transmission(
                &transmission,
                PAYPAL_WEBHOOK,
                tampered_body,
                PAYPAL_TEST_CERT,
                0,
            ),
            Err(fiducia_payments::VerifyError::SignatureMismatch)
        ));
        let moved_time = paypal_headers("1970-01-01T00:00:01Z", &signature, PAYPAL_CERT_URL);
        assert!(matches!(
            verify_paypal_transmission(
                &moved_time,
                PAYPAL_WEBHOOK,
                PAYPAL_BODY,
                PAYPAL_TEST_CERT,
                1,
            ),
            Err(fiducia_payments::VerifyError::SignatureMismatch)
        ));
        let bad_calendar = "2026-02-29T00:00:00Z";
        let invalid_time_signature = sign_paypal(PAYPAL_BODY, bad_calendar, PAYPAL_WEBHOOK);
        assert!(matches!(
            verify_paypal_transmission(
                &paypal_headers(bad_calendar, &invalid_time_signature, PAYPAL_CERT_URL),
                PAYPAL_WEBHOOK,
                PAYPAL_BODY,
                PAYPAL_TEST_CERT,
                0,
            ),
            Err(fiducia_payments::VerifyError::InvalidInput(_))
        ));
        assert!(matches!(
            verify_paypal_transmission(
                &paypal_headers(time, &signature, "https://paypal.com.evil.test/cert.pem"),
                PAYPAL_WEBHOOK,
                PAYPAL_BODY,
                PAYPAL_TEST_CERT,
                0,
            ),
            Err(fiducia_payments::VerifyError::InvalidInput(_))
        ));
    }

    fn event(provider: Provider, id: &str, event_type: &str, payload: &[u8]) -> VerifiedEvent {
        VerifiedEvent {
            provider,
            id: id.to_owned(),
            event_type: event_type.to_owned(),
            payload: payload.to_vec(),
        }
    }

    fn stored<'a>(
        provider: &'a str,
        id: &'a str,
        event_type: &'a str,
        signature_verified: bool,
        digest: &'a str,
    ) -> StoredEventIdentity<'a> {
        StoredEventIdentity {
            provider,
            provider_event_id: id,
            event_type,
            signature_verified,
            payload_sha256: digest,
        }
    }

    #[test]
    fn exact_authenticated_redelivery_is_a_safe_no_op() {
        let incoming = event(
            Provider::Stripe,
            "evt_1",
            "invoice.paid",
            br#"{"id":"evt_1","type":"invoice.paid"}"#,
        );
        let digest = verified_payload_sha256(&incoming);
        assert!(is_exact_redelivery(
            stored("stripe", "evt_1", "invoice.paid", true, &digest),
            &incoming,
            &digest,
        ));
    }

    #[test]
    fn reused_identity_with_different_verified_payload_fails_closed() {
        let first = event(
            Provider::Stripe,
            "evt_1",
            "invoice.paid",
            br#"{"id":"evt_1","type":"invoice.paid","amount":100}"#,
        );
        let second = event(
            Provider::Stripe,
            "evt_1",
            "invoice.paid",
            br#"{"id":"evt_1","type":"invoice.paid","amount":101}"#,
        );
        let first_digest = verified_payload_sha256(&first);
        let second_digest = verified_payload_sha256(&second);
        assert_ne!(first_digest, second_digest);
        assert!(!is_exact_redelivery(
            stored("stripe", "evt_1", "invoice.paid", true, &first_digest),
            &second,
            &second_digest,
        ));
    }

    #[test]
    fn reused_identity_with_different_event_type_fails_closed() {
        let incoming = event(
            Provider::Paypal,
            "WH-1",
            "PAYMENT.CAPTURE.DENIED",
            br#"{"id":"WH-1","event_type":"PAYMENT.CAPTURE.DENIED"}"#,
        );
        let digest = verified_payload_sha256(&incoming);
        assert!(!is_exact_redelivery(
            stored("paypal", "WH-1", "PAYMENT.CAPTURE.COMPLETED", true, &digest,),
            &incoming,
            &digest,
        ));
    }

    #[test]
    fn unverified_or_different_identity_never_counts_as_deduped() {
        let incoming = event(
            Provider::Stripe,
            "evt_1",
            "invoice.paid",
            br#"{"id":"evt_1","type":"invoice.paid"}"#,
        );
        let digest = verified_payload_sha256(&incoming);

        for candidate in [
            stored("stripe", "evt_1", "invoice.paid", false, &digest),
            stored("paypal", "evt_1", "invoice.paid", true, &digest),
            stored("stripe", "evt_2", "invoice.paid", true, &digest),
        ] {
            assert!(!is_exact_redelivery(candidate, &incoming, &digest));
        }
    }

    #[test]
    fn digest_is_bound_to_the_payload_retained_by_verified_event() {
        let event = event(
            Provider::Stripe,
            "evt_1",
            "invoice.paid",
            b"verified-provider-bytes",
        );
        assert_eq!(
            verified_payload_sha256(&event),
            hex::encode(Sha256::digest(b"verified-provider-bytes"))
        );
    }
}
