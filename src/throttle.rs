//! Abuse throttling for the unauthenticated login surface.
//!
//! The CSRF/origin machinery on `/login*` is a *cross-site* control, not an
//! abuse control: an attacker driving curl mints their own nonce + matching
//! token from a single `GET /login` and replays it indefinitely. Without a rate
//! limit that leaves three unauthenticated money/takeover paths open:
//!
//!   * **SMS pumping** — `POST /login/otp` with `method=phone` dispatches an SMS
//!     to any attacker-chosen E.164 number, one per request, billed to us.
//!   * **OTP brute force** — a 6-digit code is 10^6; `POST /login/verify` had no
//!     attempt cap, and the success/failure oracle is unambiguous (303 vs 401).
//!   * **TOTP brute force** — `POST /login/mfa` deliberately keeps the challenge
//!     alive across wrong codes so a user can retry, which equally lets an
//!     attacker holding the primary factor grind the second one.
//!
//! Scope, stated honestly: this is a **per-process** sliding-window limiter. The
//! deployment runs an HPA, so N replicas each admit the configured budget and
//! the effective fleet limit is N×. That is a large constant-factor reduction in
//! blast radius (and it makes single-source grinding useless), not a hard global
//! cap. The durable fix is to key these buckets in a shared store — fiducia's own
//! data plane already exposes a distributed rate-limit primitive
//! (`fiducia-load-balance` → `fiducia-node`), which is the natural home once the
//! login path is allowed to depend on it. Until then, this closes the gap that
//! matters most: an attacker with one IP and one script.
//!
//! Fail-closed is deliberate on the *counting* side and fail-open on the *clock*
//! side: a poisoned mutex is recovered rather than propagated, because an
//! internal bug must not lock every user out of signing in.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// A distinct budget. Keeping these as separate buckets means a user grinding
/// their own TOTP cannot exhaust the SMS budget, and vice versa.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum Bucket {
    /// Codes dispatched *to one identifier* — protects a victim from being
    /// spammed with texts/mail they did not ask for.
    OtpDispatchPerIdentifier,
    /// Codes dispatched *by one client* — caps total spend when the attacker
    /// varies the destination on every request (the toll-fraud shape).
    OtpDispatchPerClient,
    /// Redemption attempts against one identifier — the 10^6 keyspace guard.
    OtpVerifyPerIdentifier,
    /// TOTP step-up attempts from one client.
    MfaVerifyPerClient,
    /// Password grants against one account.
    PasswordPerIdentifier,
}

impl Bucket {
    /// `(max_events, window)`. Windows are deliberately long relative to the
    /// limit: a human needs a handful of tries, a grinder needs thousands.
    const fn budget(self) -> (u32, Duration) {
        match self {
            // A real user asks for a resend once or twice; 3 per 15 min is
            // generous while making bulk dispatch to one number pointless.
            Bucket::OtpDispatchPerIdentifier => (3, Duration::from_secs(15 * 60)),
            // The toll-fraud cap. One client cannot bill us for more than this
            // many messages an hour no matter how many numbers it rotates.
            Bucket::OtpDispatchPerClient => (10, Duration::from_secs(60 * 60)),
            // 5 tries per 10 min against 10^6 is ~1.4M years of expected grind.
            Bucket::OtpVerifyPerIdentifier => (5, Duration::from_secs(10 * 60)),
            Bucket::MfaVerifyPerClient => (5, Duration::from_secs(10 * 60)),
            // Higher: fat-fingered passwords are common and this path has no
            // out-of-band cost. Still bounds credential stuffing per replica.
            Bucket::PasswordPerIdentifier => (10, Duration::from_secs(15 * 60)),
        }
    }
}

/// Outcome of a throttle check. `retry_after_secs` is how long until the oldest
/// event in the window ages out — surfaced to the user, never zero when denied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Decision {
    pub allowed: bool,
    pub retry_after_secs: u64,
}

impl Decision {
    const fn allow() -> Self {
        Self { allowed: true, retry_after_secs: 0 }
    }
}

/// Sliding-window event log per (bucket, key).
type Windows = HashMap<(Bucket, String), Vec<Instant>>;

/// Bound on distinct tracked keys. An attacker rotating identifiers would
/// otherwise grow this map without limit — that is the same unauthenticated
/// request that the limiter exists to bound, so it must not become a memory
/// exhaustion primitive. On overflow we drop fully-expired entries first and,
/// failing that, refuse to admit a NEW key rather than evicting a live one
/// (evicting live keys is exactly what an attacker would farm for).
const MAX_TRACKED_KEYS: usize = 100_000;

fn windows() -> &'static Mutex<Windows> {
    static W: OnceLock<Mutex<Windows>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Record an attempt and decide whether it may proceed.
///
/// `key` is lower-cased so `User@x.com` and `user@x.com` share a budget.
pub fn check(bucket: Bucket, key: &str) -> Decision {
    let mut guard = match windows().lock() {
        Ok(guard) => guard,
        // A panic in another handler poisoned the lock. Recover the data rather
        // than propagate: losing the limiter degrades to today's behaviour, but
        // propagating would 500 every login attempt.
        Err(poisoned) => poisoned.into_inner(),
    };
    check_in(&mut guard, bucket, key, Instant::now())
}

/// Testable core: all clock and state access is injected.
fn check_in(state: &mut Windows, bucket: Bucket, key: &str, now: Instant) -> Decision {
    let (limit, window) = bucket.budget();
    let entry_key = (bucket, key.trim().to_ascii_lowercase());

    if state.len() >= MAX_TRACKED_KEYS && !state.contains_key(&entry_key) {
        prune_expired(state, now);
        if state.len() >= MAX_TRACKED_KEYS {
            // Refuse the new key instead of evicting a live one.
            return Decision { allowed: false, retry_after_secs: window.as_secs() };
        }
    }

    let events = state.entry(entry_key).or_default();
    // Drop everything that has aged out of the window.
    events.retain(|at| now.duration_since(*at) < window);

    if events.len() as u32 >= limit {
        // Oldest event governs when a slot frees up.
        let retry = events
            .first()
            .map(|oldest| window.saturating_sub(now.duration_since(*oldest)))
            .unwrap_or(window);
        // Never advertise 0s while denying.
        return Decision { allowed: false, retry_after_secs: retry.as_secs().max(1) };
    }

    events.push(now);
    Decision::allow()
}

/// Drop keys whose every event has aged out. Cheap amortised cleanup — only
/// called when the map hits its ceiling.
fn prune_expired(state: &mut Windows, now: Instant) {
    state.retain(|(bucket, _), events| {
        let (_, window) = bucket.budget();
        events.retain(|at| now.duration_since(*at) < window);
        !events.is_empty()
    });
}

/// The client identity used for per-client buckets.
///
/// Behind the nginx gateway `proxy_set_header X-Forwarded-For
/// $proxy_add_x_forwarded_for` APPENDS the peer nginx actually saw, so the
/// **last** element is the one hop a client cannot forge — any values it
/// supplies itself are pushed left. Taking the last element is therefore the
/// only sound read of this header here; taking the first would let an attacker
/// reset their own bucket at will by sending `X-Forwarded-For: <random>`.
///
/// Missing header (direct, in-cluster, or tests) collapses to one shared bucket
/// rather than a per-request one — an unattributable caller must not get an
/// unlimited budget.
pub fn client_key(forwarded_for: Option<&str>) -> String {
    forwarded_for
        .and_then(|value| value.rsplit(',').next())
        .map(str::trim)
        .filter(|last| !last.is_empty())
        .unwrap_or("unattributed")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> Windows {
        HashMap::new()
    }

    #[test]
    fn allows_up_to_the_budget_then_denies() {
        let mut s = state();
        let now = Instant::now();
        // OtpDispatchPerIdentifier = 3 per 15 min.
        for attempt in 1..=3 {
            assert!(
                check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", now).allowed,
                "attempt {attempt} should be admitted"
            );
        }
        let denied = check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", now);
        assert!(!denied.allowed, "the 4th dispatch must be refused");
        assert!(denied.retry_after_secs > 0, "a denial must advertise a retry delay");
    }

    #[test]
    fn the_window_slides_so_a_real_user_recovers() {
        let mut s = state();
        let start = Instant::now();
        for _ in 0..3 {
            assert!(check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", start).allowed);
        }
        assert!(!check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", start).allowed);
        // Just past the 15-minute window the budget is whole again.
        let later = start + Duration::from_secs(15 * 60 + 1);
        assert!(
            check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", later).allowed,
            "events older than the window must age out"
        );
    }

    #[test]
    fn identifiers_are_case_and_space_insensitive() {
        let mut s = state();
        let now = Instant::now();
        for _ in 0..3 {
            assert!(check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", now).allowed);
        }
        // Case/whitespace variants must NOT hand out a fresh budget.
        assert!(!check_in(&mut s, Bucket::OtpDispatchPerIdentifier, " U@X.com ", now).allowed);
    }

    #[test]
    fn buckets_are_independent() {
        let mut s = state();
        let now = Instant::now();
        for _ in 0..3 {
            check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", now);
        }
        assert!(!check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "u@x.com", now).allowed);
        // Exhausting SMS dispatch must not lock the same user out of verifying.
        assert!(
            check_in(&mut s, Bucket::OtpVerifyPerIdentifier, "u@x.com", now).allowed,
            "a spent dispatch budget must not consume the verify budget"
        );
    }

    #[test]
    fn distinct_keys_do_not_share_a_budget() {
        let mut s = state();
        let now = Instant::now();
        for _ in 0..3 {
            check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "a@x.com", now);
        }
        assert!(!check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "a@x.com", now).allowed);
        assert!(check_in(&mut s, Bucket::OtpDispatchPerIdentifier, "b@x.com", now).allowed);
    }

    #[test]
    fn brute_force_budgets_are_tight_enough_to_matter() {
        // 5 tries per 10 min against a 6-digit space: the guard that makes the
        // second factor meaningful rather than delegated.
        let (limit, window) = Bucket::OtpVerifyPerIdentifier.budget();
        assert_eq!(limit, 5);
        assert_eq!(window, Duration::from_secs(600));
        let (mfa_limit, _) = Bucket::MfaVerifyPerClient.budget();
        assert_eq!(mfa_limit, 5);
    }

    #[test]
    fn client_key_takes_the_last_forwarded_hop() {
        // The gateway appends the peer it saw; client-supplied values sit left
        // of it, so only the last element is trustworthy.
        assert_eq!(client_key(Some("1.2.3.4")), "1.2.3.4");
        assert_eq!(client_key(Some("evil-spoof, 203.0.113.9")), "203.0.113.9");
        assert_eq!(client_key(Some(" 10.0.0.1 , 198.51.100.7 ")), "198.51.100.7");
    }

    #[test]
    fn unattributable_clients_share_one_bucket_not_unlimited_ones() {
        assert_eq!(client_key(None), "unattributed");
        assert_eq!(client_key(Some("   ")), "unattributed");
        assert_eq!(client_key(Some("")), "unattributed");
    }

    #[test]
    fn a_spoofed_forwarded_prefix_cannot_reset_the_bucket() {
        let mut s = state();
        let now = Instant::now();
        // Attacker rotates the part of XFF they control; the real hop is last.
        for spoof in ["a", "b", "c"] {
            let key = client_key(Some(&format!("{spoof}, 203.0.113.9")));
            check_in(&mut s, Bucket::MfaVerifyPerClient, &key, now);
        }
        for spoof in ["d", "e"] {
            let key = client_key(Some(&format!("{spoof}, 203.0.113.9")));
            check_in(&mut s, Bucket::MfaVerifyPerClient, &key, now);
        }
        let key = client_key(Some("f, 203.0.113.9"));
        assert!(
            !check_in(&mut s, Bucket::MfaVerifyPerClient, &key, now).allowed,
            "rotating the spoofable prefix must not mint a fresh budget"
        );
    }
}
