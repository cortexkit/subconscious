//! How long a user JWT ck-bus issues lives, and when its holder renews it (R16).
//!
//! Every user JWT carries `exp` 15 minutes after its `iat`. Its holder renews it 10
//! minutes after issue, plus a few seconds of per-process jitter so the processes on one
//! machine do not all ask ck-bus to sign in the same instant. Renewal re-signs the same
//! user key with a fresh `iat` and `exp`; the connection keeps running on the old JWT
//! until nats-server disconnects it at that JWT's `exp`, and the reconnect that follows
//! presents the renewed one. The 15 minutes is also how long a user whose revocation was
//! lost to damage stays valid.
//!
//! The acceptance rows cannot wait 15 minutes, so a debug build (what `cargo test`
//! runs) reads a shorter lifetime from `CKBUS_TEST_USER_JWT_LIFETIME_MS` and
//! `CKBUS_TEST_USER_JWT_RENEW_AFTER_MS`. A release build never reads them: the lifetime
//! of a shipped ck-bus is always the one written here.

use std::time::Duration;

/// R16: a user JWT's `exp` is this long after its `iat`.
pub const USER_JWT_LIFETIME: Duration = Duration::from_secs(15 * 60);
/// R16: the holder renews this long after issue (before the jitter).
pub const USER_JWT_RENEW_AFTER: Duration = Duration::from_secs(10 * 60);
/// The most jitter one process adds to its renewal time.
pub const RENEW_JITTER_MAX: Duration = Duration::from_secs(5);

/// The test-only overrides, read by debug builds only.
pub const TEST_LIFETIME_ENV: &str = "CKBUS_TEST_USER_JWT_LIFETIME_MS";
pub const TEST_RENEW_AFTER_ENV: &str = "CKBUS_TEST_USER_JWT_RENEW_AFTER_MS";

/// One process's JWT lifetime, renewal point and jitter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JwtLifetime {
    pub lifetime: Duration,
    pub renew_after: Duration,
    /// This process's jitter, fixed for its life.
    pub jitter: Duration,
}

impl JwtLifetime {
    /// R16's values, with jitter derived from `seed` (see `with_jitter`).
    pub fn production(seed: u64) -> Self {
        Self::with_jitter(USER_JWT_LIFETIME, USER_JWT_RENEW_AFTER, seed)
    }

    /// `lifetime` and `renew_after` with a jitter below the smaller of
    /// `RENEW_JITTER_MAX` and half the gap between renewal and expiry, so the jitter can
    /// never push a renewal past `exp` however short a test makes the lifetime.
    pub fn with_jitter(lifetime: Duration, renew_after: Duration, seed: u64) -> Self {
        let gap = lifetime.saturating_sub(renew_after) / 2;
        let bound = RENEW_JITTER_MAX.min(gap).as_millis() as u64;
        let jitter = if bound == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(seed % bound)
        };
        Self {
            lifetime,
            renew_after,
            jitter,
        }
    }

    /// The lifetime for this process: R16's, or in a debug build the test overrides when
    /// both are set. An override that does not parse, is zero, or renews at or after
    /// expiry is refused rather than replaced by a guess.
    pub fn for_process() -> Result<Self, String> {
        let seed = process_seed();
        #[cfg(debug_assertions)]
        {
            let lifetime = test_override(TEST_LIFETIME_ENV)?;
            let renew_after = test_override(TEST_RENEW_AFTER_ENV)?;
            match (lifetime, renew_after) {
                (None, None) => {}
                (Some(lifetime), Some(renew_after)) if renew_after < lifetime => {
                    return Ok(Self::with_jitter(lifetime, renew_after, seed));
                }
                (Some(_), Some(_)) => {
                    return Err(format!(
                        "{TEST_RENEW_AFTER_ENV} must be less than {TEST_LIFETIME_ENV}"
                    ))
                }
                _ => {
                    return Err(format!(
                        "{TEST_LIFETIME_ENV} and {TEST_RENEW_AFTER_ENV} are set together or \
                         not at all"
                    ))
                }
            }
        }
        Ok(Self::production(seed))
    }

    /// The `exp` claim for a JWT issued at `issued_at` (seconds since the Unix epoch).
    /// A sub-second test lifetime still expires at least one second after issue.
    pub fn expires_at(&self, issued_at: i64) -> i64 {
        issued_at + (self.lifetime.as_secs() as i64).max(1)
    }

    /// How long after issue this process renews.
    pub fn renew_delay(&self) -> Duration {
        self.renew_after + self.jitter
    }
}

impl Default for JwtLifetime {
    fn default() -> Self {
        Self::production(process_seed())
    }
}

#[cfg(debug_assertions)]
fn test_override(name: &str) -> Result<Option<Duration>, String> {
    let Some(raw) = std::env::var_os(name) else {
        return Ok(None);
    };
    let millis = raw
        .to_str()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|millis| *millis > 0)
        .ok_or_else(|| format!("{name} must be a positive integer of milliseconds"))?;
    Ok(Some(Duration::from_millis(millis)))
}

/// A per-process value for the jitter: the pid and the start time mixed, so two
/// processes started together still differ.
fn process_seed() -> u64 {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    (u64::from(std::process::id()) << 32) ^ nanos
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_values_are_r16s() {
        let lifetime = JwtLifetime::production(123_456);
        assert_eq!(lifetime.lifetime, Duration::from_secs(900));
        assert_eq!(lifetime.renew_after, Duration::from_secs(600));
        assert!(lifetime.jitter < RENEW_JITTER_MAX);
        assert_eq!(lifetime.expires_at(1_000), 1_900);
    }

    #[test]
    fn jitter_never_reaches_expiry() {
        let lifetime =
            JwtLifetime::with_jitter(Duration::from_secs(4), Duration::from_secs(2), 999_999);
        assert!(lifetime.renew_delay() < Duration::from_secs(3));
    }
}
