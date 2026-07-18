//! Passwordless + multi-factor Supabase Auth flows for the customer portal.
//!
//! The existing `/login` password grant lives in `main.rs`; this module adds the
//! three flows the product requires without re-implementing any crypto:
//!
//!   * **Magic link / email OTP** — `POST /auth/v1/otp {email}` mails a link and a
//!     6-digit code; the code is redeemed at `POST /auth/v1/verify {type:"email"}`.
//!   * **Phone OTP** — `POST /auth/v1/otp {phone}` texts a code redeemed at
//!     `POST /auth/v1/verify {type:"sms"}`.
//!   * **TOTP authenticator (Authy / Google Authenticator / 1Password …)** — the
//!     Supabase MFA API: enroll (`POST /auth/v1/factors`) returns an `otpauth://`
//!     URI + QR any RFC-6238 app scans; challenge/verify step the session up to
//!     assurance level `aal2`. Supabase owns the shared secret and the TOTP math;
//!     this service only relays the user-entered 6-digit code.
//!
//! Design: every network method is a thin shell around a *pure* request/response
//! shaper (URL, JSON body, parse, decision logic) so the branching that actually
//! matters is unit-tested without a live Supabase. Network calls fail closed —
//! any transport or non-2xx response becomes a typed [`SupabaseAuthError`], never
//! a silent success.

use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

/// Which passwordless channel an OTP travels over. Maps to Supabase's request
/// field (`email`/`phone`) and its verify `type` discriminator (`email`/`sms`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OtpChannel {
    Email,
    Phone,
}

impl OtpChannel {
    /// Supabase request-body key that carries the identifier.
    pub const fn field(self) -> &'static str {
        match self {
            OtpChannel::Email => "email",
            OtpChannel::Phone => "phone",
        }
    }

    /// Supabase `POST /auth/v1/verify` `type` discriminator. Email OTP verifies as
    /// `email`; phone OTP verifies as `sms`.
    pub const fn verify_type(self) -> &'static str {
        match self {
            OtpChannel::Email => "email",
            OtpChannel::Phone => "sms",
        }
    }
}

/// A Supabase-issued session. `access_token` is the bearer this service forwards
/// to fiducia-auth and stores in the `__Host-fiducia_customer_session` cookie.
/// The remaining fields mirror the GoTrue response verbatim; they are parsed for
/// contract fidelity (and future silent-refresh support) even though only the
/// access token is consumed today.
#[derive(Clone, Debug, Deserialize)]
#[allow(dead_code)]
pub struct SupabaseSession {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub token_type: Option<String>,
    #[serde(default)]
    pub expires_in: Option<i64>,
}

/// A registered authenticator factor as reported by `GET /auth/v1/user`.
#[derive(Clone, Debug, Deserialize)]
pub struct Factor {
    pub id: String,
    #[serde(default)]
    pub factor_type: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub friendly_name: Option<String>,
}

impl Factor {
    /// A TOTP factor the user has finished enrolling (Supabase status `verified`).
    /// Only verified TOTP factors gate login; a half-finished `unverified` enroll
    /// must never lock a user out.
    pub fn is_verified_totp(&self) -> bool {
        self.factor_type.as_deref() == Some("totp") && self.status.as_deref() == Some("verified")
    }
}

/// The enrollment material returned by `POST /auth/v1/factors`. `qr_code` is an
/// SVG (data-URI or raw markup) Supabase renders for us; `secret` and `uri` are
/// the manual-entry fallbacks for the same shared secret.
#[derive(Clone, Debug)]
pub struct TotpEnrollment {
    pub factor_id: String,
    pub qr_code: String,
    pub secret: String,
    pub uri: String,
}

/// A pending TOTP challenge (`POST /auth/v1/factors/{id}/challenge`).
#[derive(Clone, Debug)]
pub struct TotpChallenge {
    pub challenge_id: String,
    pub factor_id: String,
}

#[derive(Debug)]
pub enum SupabaseAuthError {
    /// Caller-supplied identifier/code failed local validation before any network
    /// round-trip (empty email, non-E.164 phone, non-numeric code).
    Invalid(&'static str),
    /// Supabase answered but rejected the request (bad code, expired OTP, unknown
    /// user). Carries Supabase's own `error`/`msg` when present.
    Rejected(String),
    /// Transport failure or a non-JSON / unparseable success body — treat as a
    /// dependency outage, never as an authenticated result.
    Unavailable(String),
}

impl std::fmt::Display for SupabaseAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SupabaseAuthError::Invalid(reason) => write!(f, "invalid input: {reason}"),
            SupabaseAuthError::Rejected(detail) => write!(f, "supabase rejected request: {detail}"),
            SupabaseAuthError::Unavailable(detail) => write!(f, "supabase unavailable: {detail}"),
        }
    }
}

impl std::error::Error for SupabaseAuthError {}

/// Typed client for the Supabase Auth (GoTrue) REST surface.
#[derive(Clone)]
pub struct SupabaseAuth {
    base_url: String,
    publishable_key: String,
    http: reqwest::Client,
}

impl SupabaseAuth {
    /// Build a client. `base_url` is the project URL (trailing slash tolerated);
    /// `publishable_key` is the anon/publishable API key sent as `apikey`.
    pub fn new(base_url: &str, publishable_key: &str) -> Self {
        // Match the password path's 10s budget; a hung IdP must not pin a worker.
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            publishable_key: publishable_key.to_string(),
            http,
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/auth/v1/{}", self.base_url, path.trim_start_matches('/'))
    }

    /// Send a magic link + OTP over `channel`. `allow_signup` maps to Supabase's
    /// `should_create_user`: true lets an unknown identifier self-register (the
    /// signup path), false restricts to existing accounts.
    pub async fn send_otp(
        &self,
        channel: OtpChannel,
        identifier: &str,
        allow_signup: bool,
    ) -> Result<(), SupabaseAuthError> {
        let identifier = normalize_identifier(channel, identifier)?;
        let body = otp_request_body(channel, &identifier, allow_signup);
        let response = self
            .http
            .post(self.endpoint("otp"))
            .header("apikey", &self.publishable_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        self.expect_success(response).await.map(|_| ())
    }

    /// Redeem a 6-digit OTP (email or SMS) for a session.
    pub async fn verify_otp(
        &self,
        channel: OtpChannel,
        identifier: &str,
        token: &str,
    ) -> Result<SupabaseSession, SupabaseAuthError> {
        let identifier = normalize_identifier(channel, identifier)?;
        let token = normalize_otp_code(token)?;
        let body = verify_otp_body(channel, &identifier, &token);
        let response = self
            .http
            .post(self.endpoint("verify"))
            .header("apikey", &self.publishable_key)
            .json(&body)
            .send()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        let value = self.expect_success(response).await?;
        parse_session(&value)
    }

    /// List a user's registered factors (from `GET /auth/v1/user`).
    pub async fn list_factors(&self, access_token: &str) -> Result<Vec<Factor>, SupabaseAuthError> {
        let response = self
            .http
            .get(self.endpoint("user"))
            .header("apikey", &self.publishable_key)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        let value = self.expect_success(response).await?;
        Ok(parse_factors(&value))
    }

    /// Enroll a new TOTP factor; returns the QR + secret to display once.
    pub async fn enroll_totp(
        &self,
        access_token: &str,
        friendly_name: &str,
    ) -> Result<TotpEnrollment, SupabaseAuthError> {
        let body = enroll_totp_body(friendly_name);
        let response = self
            .http
            .post(self.endpoint("factors"))
            .header("apikey", &self.publishable_key)
            .bearer_auth(access_token)
            .json(&body)
            .send()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        let value = self.expect_success(response).await?;
        parse_enrollment(&value)
    }

    /// Open a challenge against an enrolled factor.
    pub async fn challenge(
        &self,
        access_token: &str,
        factor_id: &str,
    ) -> Result<TotpChallenge, SupabaseAuthError> {
        let response = self
            .http
            .post(self.endpoint(&format!("factors/{factor_id}/challenge")))
            .header("apikey", &self.publishable_key)
            .bearer_auth(access_token)
            .json(&json!({}))
            .send()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        let value = self.expect_success(response).await?;
        let challenge_id =
            value
                .get("id")
                .and_then(Value::as_str)
                .ok_or(SupabaseAuthError::Unavailable(
                    "challenge response missing id".to_string(),
                ))?;
        Ok(TotpChallenge {
            challenge_id: challenge_id.to_string(),
            factor_id: factor_id.to_string(),
        })
    }

    /// Verify a code against an open challenge; returns the stepped-up session.
    /// Used both to *activate* a freshly enrolled factor and to *step up* a login.
    pub async fn verify_factor(
        &self,
        access_token: &str,
        challenge: &TotpChallenge,
        code: &str,
    ) -> Result<SupabaseSession, SupabaseAuthError> {
        let code = normalize_otp_code(code)?;
        let body = verify_factor_body(&challenge.challenge_id, &code);
        let response = self
            .http
            .post(self.endpoint(&format!("factors/{}/verify", challenge.factor_id)))
            .header("apikey", &self.publishable_key)
            .bearer_auth(access_token)
            .json(&body)
            .send()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        let value = self.expect_success(response).await?;
        parse_session(&value)
    }

    /// Remove an enrolled factor.
    pub async fn unenroll(
        &self,
        access_token: &str,
        factor_id: &str,
    ) -> Result<(), SupabaseAuthError> {
        let response = self
            .http
            .delete(self.endpoint(&format!("factors/{factor_id}")))
            .header("apikey", &self.publishable_key)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        self.expect_success(response).await.map(|_| ())
    }

    /// Consume a response: 2xx → parsed JSON body (or `null` when empty); any
    /// other status → a `Rejected` error carrying Supabase's own message.
    async fn expect_success(
        &self,
        response: reqwest::Response,
    ) -> Result<Value, SupabaseAuthError> {
        let status = response.status();
        let text = response
            .text()
            .await
            .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()))?;
        if status.is_success() {
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text)
                .map_err(|error| SupabaseAuthError::Unavailable(error.to_string()));
        }
        Err(SupabaseAuthError::Rejected(extract_error_message(&text)))
    }
}

// ── Pure request/response shapers (unit-tested without a live Supabase) ────────

/// Trim + lightly validate an identifier for its channel. Email must contain a
/// single `@` with non-empty local/domain; phone must be E.164 (`+` then 7–15
/// digits). This is a fail-fast guard, not full validation — Supabase remains the
/// authority — but it stops obviously bad values from ever leaving the process.
pub fn normalize_identifier(
    channel: OtpChannel,
    identifier: &str,
) -> Result<String, SupabaseAuthError> {
    let trimmed = identifier.trim();
    match channel {
        OtpChannel::Email => {
            if is_plausible_email(trimmed) {
                Ok(trimmed.to_string())
            } else {
                Err(SupabaseAuthError::Invalid("email is not well-formed"))
            }
        }
        OtpChannel::Phone => {
            let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
            if is_e164_phone(&compact) {
                Ok(compact)
            } else {
                Err(SupabaseAuthError::Invalid(
                    "phone must be E.164, e.g. +14155550123",
                ))
            }
        }
    }
}

/// A one-time code is exactly 6–8 ASCII digits (Supabase default is 6; longer is
/// allowed for future length changes). Anything else never reaches the network.
pub fn normalize_otp_code(token: &str) -> Result<String, SupabaseAuthError> {
    let trimmed = token.trim();
    let digits = trimmed.len();
    if (6..=8).contains(&digits) && trimmed.bytes().all(|b| b.is_ascii_digit()) {
        Ok(trimmed.to_string())
    } else {
        Err(SupabaseAuthError::Invalid("code must be 6–8 digits"))
    }
}

fn is_plausible_email(value: &str) -> bool {
    if value.len() > 320 || value.contains(char::is_whitespace) {
        return false;
    }
    let mut parts = value.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return false;
    };
    !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.')
}

fn is_e164_phone(value: &str) -> bool {
    let Some(digits) = value.strip_prefix('+') else {
        return false;
    };
    let len = digits.len();
    (7..=15).contains(&len)
        && digits.bytes().all(|b| b.is_ascii_digit())
        && !digits.starts_with('0')
}

fn otp_request_body(channel: OtpChannel, identifier: &str, allow_signup: bool) -> Value {
    json!({ channel.field(): identifier, "should_create_user": allow_signup })
}

fn verify_otp_body(channel: OtpChannel, identifier: &str, token: &str) -> Value {
    json!({ "type": channel.verify_type(), channel.field(): identifier, "token": token })
}

fn enroll_totp_body(friendly_name: &str) -> Value {
    let name = friendly_name.trim();
    let mut body = json!({ "factor_type": "totp" });
    if !name.is_empty() {
        body["friendly_name"] = json!(name);
    }
    body
}

fn verify_factor_body(challenge_id: &str, code: &str) -> Value {
    json!({ "challenge_id": challenge_id, "code": code })
}

/// Parse a session out of a Supabase response. GoTrue returns the tokens at the
/// top level for grant/verify responses; MFA verify nests nothing extra, so a
/// top-level `access_token` is the contract.
fn parse_session(value: &Value) -> Result<SupabaseSession, SupabaseAuthError> {
    serde_json::from_value::<SupabaseSession>(value.clone())
        .ok()
        .filter(|session| !session.access_token.trim().is_empty())
        .ok_or(SupabaseAuthError::Unavailable(
            "session response missing access_token".to_string(),
        ))
}

/// Factors live under `user.factors` on the user object.
fn parse_factors(value: &Value) -> Vec<Factor> {
    value
        .get("factors")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| serde_json::from_value::<Factor>(item.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Enroll response shape: `{ id, type, totp: { qr_code, secret, uri } }`.
fn parse_enrollment(value: &Value) -> Result<TotpEnrollment, SupabaseAuthError> {
    let factor_id = value.get("id").and_then(Value::as_str).unwrap_or_default();
    let totp = value.get("totp");
    let qr_code = totp
        .and_then(|t| t.get("qr_code"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let secret = totp
        .and_then(|t| t.get("secret"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let uri = totp
        .and_then(|t| t.get("uri"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if factor_id.is_empty() || secret.is_empty() {
        return Err(SupabaseAuthError::Unavailable(
            "enroll response missing factor id or secret".to_string(),
        ));
    }
    Ok(TotpEnrollment {
        factor_id: factor_id.to_string(),
        qr_code: qr_code.to_string(),
        secret: secret.to_string(),
        uri: uri.to_string(),
    })
}

/// Best-effort extraction of GoTrue's error text (`error_description`, `msg`, or
/// `error`) for surfacing to logs; falls back to a truncated raw body.
fn extract_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        for key in ["error_description", "msg", "message", "error"] {
            if let Some(text) = value.get(key).and_then(Value::as_str) {
                if !text.is_empty() {
                    return text.to_string();
                }
            }
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "supabase returned a non-success status with no body".to_string()
    } else {
        trimmed.chars().take(200).collect()
    }
}

/// The login step-up decision: the id of the first verified TOTP factor, or
/// `None` when the primary factor already fully authenticates the user. Keeping
/// this pure makes the "who must show a second factor" rule directly testable.
pub fn required_totp_factor(factors: &[Factor]) -> Option<String> {
    factors
        .iter()
        .find(|factor| factor.is_verified_totp())
        .map(|factor| factor.id.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_maps_to_supabase_fields() {
        assert_eq!(OtpChannel::Email.field(), "email");
        assert_eq!(OtpChannel::Email.verify_type(), "email");
        assert_eq!(OtpChannel::Phone.field(), "phone");
        // Phone OTP verifies as "sms", not "phone" — a mismatch Supabase rejects.
        assert_eq!(OtpChannel::Phone.verify_type(), "sms");
    }

    #[test]
    fn email_validation_accepts_real_and_rejects_garbage() {
        assert!(normalize_identifier(OtpChannel::Email, "  user@fiducia.cloud ").is_ok());
        for bad in [
            "",
            "no-at",
            "a@b",
            "two@@at.com",
            "spaces in@x.com",
            "@x.com",
        ] {
            assert!(
                normalize_identifier(OtpChannel::Email, bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn phone_validation_is_e164_and_strips_spacing() {
        assert_eq!(
            normalize_identifier(OtpChannel::Phone, "+1 415 555 0123").unwrap(),
            "+14155550123"
        );
        for bad in ["4155550123", "+0155", "+", "+abc4155550", "+012345678"] {
            assert!(
                normalize_identifier(OtpChannel::Phone, bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn otp_code_must_be_six_to_eight_digits() {
        assert_eq!(normalize_otp_code(" 123456 ").unwrap(), "123456");
        for bad in ["12345", "1234567890", "12a456", "", "abcdef"] {
            assert!(normalize_otp_code(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn otp_request_body_carries_channel_field_and_signup_flag() {
        let email = otp_request_body(OtpChannel::Email, "u@x.com", true);
        assert_eq!(email["email"], "u@x.com");
        assert_eq!(email["should_create_user"], true);
        let phone = otp_request_body(OtpChannel::Phone, "+14155550123", false);
        assert_eq!(phone["phone"], "+14155550123");
        assert_eq!(phone["should_create_user"], false);
        assert!(phone.get("email").is_none());
    }

    #[test]
    fn verify_body_uses_sms_type_for_phone() {
        let body = verify_otp_body(OtpChannel::Phone, "+14155550123", "123456");
        assert_eq!(body["type"], "sms");
        assert_eq!(body["phone"], "+14155550123");
        assert_eq!(body["token"], "123456");
    }

    #[test]
    fn enroll_body_omits_blank_friendly_name() {
        assert!(enroll_totp_body("   ").get("friendly_name").is_none());
        assert_eq!(enroll_totp_body("iPhone")["friendly_name"], "iPhone");
        assert_eq!(enroll_totp_body("iPhone")["factor_type"], "totp");
    }

    #[test]
    fn parse_session_requires_access_token() {
        let ok = json!({ "access_token": "jwt", "refresh_token": "r", "expires_in": 3600 });
        assert_eq!(parse_session(&ok).unwrap().access_token, "jwt");
        assert!(parse_session(&json!({ "access_token": "" })).is_err());
        assert!(parse_session(&json!({ "nope": 1 })).is_err());
    }

    #[test]
    fn parse_enrollment_pulls_qr_secret_uri() {
        let value = json!({
            "id": "factor-1",
            "type": "totp",
            "totp": { "qr_code": "<svg/>", "secret": "JBSWY3DP", "uri": "otpauth://totp/x" }
        });
        let enrollment = parse_enrollment(&value).unwrap();
        assert_eq!(enrollment.factor_id, "factor-1");
        assert_eq!(enrollment.secret, "JBSWY3DP");
        assert!(enrollment.uri.starts_with("otpauth://"));
        // Missing secret is an unusable enrollment.
        assert!(parse_enrollment(&json!({ "id": "f", "totp": {} })).is_err());
    }

    #[test]
    fn step_up_requires_a_verified_totp_factor() {
        // No factors → no step-up.
        assert!(required_totp_factor(&[]).is_none());

        let unverified = vec![Factor {
            id: "f1".into(),
            factor_type: Some("totp".into()),
            status: Some("unverified".into()),
            friendly_name: None,
        }];
        // A half-enrolled factor must not lock the user into a challenge.
        assert!(required_totp_factor(&unverified).is_none());

        let verified = vec![Factor {
            id: "f2".into(),
            factor_type: Some("totp".into()),
            status: Some("verified".into()),
            friendly_name: Some("Authy".into()),
        }];
        assert_eq!(required_totp_factor(&verified).as_deref(), Some("f2"));
    }

    #[test]
    fn error_message_extraction_prefers_gotrue_fields() {
        assert_eq!(
            extract_error_message(r#"{"error_description":"Invalid OTP"}"#),
            "Invalid OTP"
        );
        assert_eq!(
            extract_error_message(r#"{"msg":"otp expired"}"#),
            "otp expired"
        );
        assert_eq!(
            extract_error_message("   "),
            "supabase returned a non-success status with no body"
        );
    }

    #[test]
    fn endpoint_join_is_slash_safe() {
        let auth = SupabaseAuth::new("https://proj.supabase.co/", "anon-key");
        assert_eq!(auth.endpoint("otp"), "https://proj.supabase.co/auth/v1/otp");
        assert_eq!(
            auth.endpoint("/factors/abc/verify"),
            "https://proj.supabase.co/auth/v1/factors/abc/verify"
        );
    }
}
