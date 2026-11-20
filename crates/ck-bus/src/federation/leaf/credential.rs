//! The leaf credential: the leaf key's public half and the hub-issued user JWT for it.

use std::time::Duration;

use crate::{
    bootstrap::account_jwt::decode_claims,
    credentials::{
        nkey::{encode_public, NkeyRole},
        wire::VaultPublicKey,
    },
};

/// The leaf key as the vault last reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafKey {
    /// `U...`: the nkey the hub's JWT must name as its subject.
    pub nkey: String,
    /// The vault's id for the key's current public half. It changes exactly when the
    /// operator replaces the key.
    pub key_id: String,
}

impl LeafKey {
    pub fn from_vault(public: &VaultPublicKey) -> Self {
        Self {
            nkey: encode_public(NkeyRole::User, &public.public),
            key_id: public.key_id.clone(),
        }
    }
}

/// A leaf user JWT the hub issued, and when to replace it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafCredential {
    pub jwt: String,
    pub issued_at: i64,
    pub expires_at: i64,
    /// The leaf key the JWT names. A signature by any other key would not match it.
    pub key_id: String,
    /// The hub (fingerprint) that issued it.
    pub hub: String,
}

impl LeafCredential {
    /// Checks a JWT the hub returned for `key` and reads its lifetime. A JWT for another
    /// key, without an expiry, or already expired is refused: presenting it would only
    /// fail at the hub.
    pub fn accept(jwt: &str, key: &LeafKey, hub: &str, now: i64) -> Result<Self, String> {
        let jwt = jwt.trim();
        let claims = decode_claims(jwt).ok_or("the issued leaf JWT does not decode")?;
        if claims["sub"].as_str() != Some(key.nkey.as_str()) {
            return Err(format!(
                "the issued leaf JWT names {:?}, not this machine's leaf key {}",
                claims["sub"], key.nkey
            ));
        }
        let expires_at = claims["exp"]
            .as_i64()
            .ok_or("the issued leaf JWT has no expiry")?;
        let issued_at = claims["iat"].as_i64().unwrap_or(now).min(now);
        if expires_at <= now {
            return Err(format!("the issued leaf JWT expired at {expires_at}"));
        }
        Ok(Self {
            jwt: jwt.to_string(),
            issued_at,
            expires_at,
            key_id: key.key_id.clone(),
            hub: hub.to_string(),
        })
    }

    /// Seconds until the wall-clock renewal point. Renew at half-life or when fewer
    /// than ten minutes remain, whichever comes first.
    pub fn renew_in(&self, now: i64) -> u64 {
        let midpoint = self
            .issued_at
            .saturating_add(self.expires_at.saturating_sub(self.issued_at) / 2);
        let expiry_margin = self.expires_at.saturating_sub(599);
        u64::try_from(midpoint.min(expiry_margin).saturating_sub(now)).unwrap_or(0)
    }
}

/// Retry spacing after a failed issue or vault read: 1 s doubling to 5 min.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Backoff {
    next: Duration,
}

pub const BACKOFF_FIRST: Duration = Duration::from_secs(1);
pub const BACKOFF_CAP: Duration = Duration::from_secs(300);

impl Default for Backoff {
    fn default() -> Self {
        Self {
            next: BACKOFF_FIRST,
        }
    }
}

impl Backoff {
    /// The wait before the next attempt, doubling the one after.
    pub fn failed(&mut self) -> Duration {
        let wait = self.next;
        self.next = (self.next * 2).min(BACKOFF_CAP);
        wait
    }

    pub fn reset(&mut self) {
        self.next = BACKOFF_FIRST;
    }
}

#[cfg(test)]
mod tests {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
    use serde_json::json;

    use super::*;

    fn jwt(claims: serde_json::Value) -> String {
        format!(
            "{}.{}.sig",
            URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ed25519-nkey"}"#),
            URL_SAFE_NO_PAD.encode(claims.to_string())
        )
    }

    fn key() -> LeafKey {
        LeafKey {
            nkey: "UKEY".to_string(),
            key_id: "k1".to_string(),
        }
    }

    #[test]
    fn renews_at_half_life() {
        let credential = LeafCredential::accept(
            &jwt(json!({"sub": "UKEY", "iat": 1_000, "exp": 1_000 + 86_400})),
            &key(),
            "hub",
            1_000,
        )
        .unwrap();
        assert_eq!(credential.renew_in(1_000), 43_200);
        assert_eq!(credential.renew_in(1_000 + 43_199), 1);
        assert_eq!(credential.renew_in(1_000 + 80_000), 0);
        let short = LeafCredential {
            expires_at: 1_800,
            ..credential
        };
        assert_eq!(short.renew_in(1_200), 1);
        assert_eq!(short.renew_in(1_201), 0);
    }

    #[test]
    fn refuses_a_jwt_for_another_key_or_already_expired() {
        assert!(LeafCredential::accept(
            &jwt(json!({"sub": "UOTHER", "iat": 1, "exp": 10_000})),
            &key(),
            "hub",
            5
        )
        .is_err());
        assert!(LeafCredential::accept(
            &jwt(json!({"sub": "UKEY", "iat": 1, "exp": 4})),
            &key(),
            "hub",
            5
        )
        .is_err());
        assert!(LeafCredential::accept(&jwt(json!({"sub": "UKEY"})), &key(), "hub", 5).is_err());
    }

    #[test]
    fn backoff_doubles_from_one_second_to_five_minutes() {
        let mut backoff = Backoff::default();
        let waits: Vec<u64> = (0..11).map(|_| backoff.failed().as_secs()).collect();
        assert_eq!(waits, [1, 2, 4, 8, 16, 32, 64, 128, 256, 300, 300]);
        backoff.reset();
        assert_eq!(backoff.failed(), Duration::from_secs(1));
    }
}
