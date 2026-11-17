//! The bound on `ckbus.hub_tls_sign`: a token bucket of 20 per second with a burst of 40.
//!
//! The hub's leaf port faces the network, and any client can open TLS handshakes in a
//! loop. Each handshake costs one vault signature, so a flood is refused here, at the
//! hub, instead of queueing on the vault.

use std::time::Duration;

use tokio::time::Instant;

pub const RATE_PER_SECOND: u32 = 20;
pub const BURST: u32 = 40;

#[derive(Debug)]
pub struct TokenBucket {
    rate_per_second: f64,
    capacity: f64,
    tokens: f64,
    refilled_at: Instant,
}

impl TokenBucket {
    /// A full bucket.
    pub fn new(rate_per_second: u32, burst: u32, now: Instant) -> Self {
        Self {
            rate_per_second: f64::from(rate_per_second),
            capacity: f64::from(burst),
            tokens: f64::from(burst),
            refilled_at: now,
        }
    }

    /// The hub listener's bucket.
    pub fn hub_tls(now: Instant) -> Self {
        Self::new(RATE_PER_SECOND, BURST, now)
    }

    /// Takes one token, or refuses when none is left.
    pub fn try_take(&mut self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.refilled_at);
        self.refilled_at = now;
        self.tokens =
            (self.tokens + elapsed.as_secs_f64() * self.rate_per_second).min(self.capacity);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// How long until the next token, for the refusal text.
    pub fn next_token_in(&self) -> Duration {
        let missing = (1.0 - self.tokens).max(0.0);
        Duration::from_secs_f64(missing / self.rate_per_second)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_forty_first_immediate_take_is_refused_and_tokens_return_at_twenty_a_second() {
        let start = Instant::now();
        let mut bucket = TokenBucket::hub_tls(start);
        assert!((0..40).all(|_| bucket.try_take(start)));
        assert!(!bucket.try_take(start));
        assert!(bucket.next_token_in() <= Duration::from_millis(50));
        // 50 ms is one token at 20 per second.
        assert!(bucket.try_take(start + Duration::from_millis(50)));
        assert!(!bucket.try_take(start + Duration::from_millis(50)));
        // A long pause refills to the burst, never beyond it.
        let later = start + Duration::from_secs(60);
        assert!((0..40).all(|_| bucket.try_take(later)));
        assert!(!bucket.try_take(later));
    }
}
