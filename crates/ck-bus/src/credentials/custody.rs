//! Per-process user keys, held in ck-bus's memory only.
//!
//! Each participant's user key (and each of ck-bus's own users) is an Ed25519 nkey
//! generated here on issue. Its seed is never written to env, argv, disk, the store, a
//! log, a report or the vault, and it never leaves this process: a child that must answer
//! the server's connect nonce asks ck-bus to sign it. A copied seed held by a child would
//! work anywhere until expiry; a seed held here signs only for a caller that reaches
//! ck-bus as that module.

use std::{
    collections::HashMap,
    fmt,
    sync::{Mutex, MutexGuard},
};

use nkeys::KeyPair;

/// The refusal a caller gets when asking to sign for a key ck-bus does not hold: the key
/// was issued by an earlier ck-bus process (whose memory died with it) or was dropped.
pub const CREDENTIAL_SUPERSEDED: &str = "ckbus_credential_superseded";
/// The refusal a renewal gets when this process has recorded the key's revocation.
pub const CREDENTIAL_REVOKED: &str = "ckbus_credential_revoked";

/// Signing for a key this process does not hold.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Superseded {
    pub user_public: String,
}

impl fmt::Display for Superseded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{CREDENTIAL_SUPERSEDED}: ck-bus holds no key for {}; fetch a fresh credential",
            self.user_public
        )
    }
}

/// The in-memory key store. It deliberately offers no way to read a seed back out.
#[derive(Default)]
pub struct KeyCustody {
    keys: Mutex<HashMap<String, KeyPair>>,
    /// Every key whose revocation this process has recorded. Its key pair is dropped at
    /// the same moment, so nothing signs for it again. The set lives as long as the
    /// process: a later process knows none of them, and answers them as superseded.
    revoked: Mutex<std::collections::HashSet<String>>,
}

impl fmt::Debug for KeyCustody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Public keys only; the key pairs themselves are never formatted.
        f.debug_struct("KeyCustody")
            .field("users", &self.lock().keys().collect::<Vec<_>>())
            .finish()
    }
}

impl KeyCustody {
    pub fn new() -> Self {
        Self::default()
    }

    /// Generates a user key in memory and returns its public nkey (`U...`).
    pub fn generate_user(&self) -> String {
        let pair = KeyPair::new_user();
        let public = pair.public_key();
        self.lock().insert(public.clone(), pair);
        public
    }

    /// Signs a connect nonce with the held key for `user_public`: the path
    /// `ckbus.nonce_sign` answers through. Returns the raw 64 signature bytes.
    pub fn sign_nonce(&self, user_public: &str, nonce: &[u8]) -> Result<Vec<u8>, Superseded> {
        let keys = self.lock();
        let pair = keys.get(user_public).ok_or_else(|| Superseded {
            user_public: user_public.to_string(),
        })?;
        Ok(pair
            .sign(nonce)
            .expect("a key pair generated in this process always holds its seed"))
    }

    /// Drops a held key. Its next nonce signature is refused as superseded. Returns
    /// whether a key was held.
    pub fn forget(&self, user_public: &str) -> bool {
        self.lock().remove(user_public).is_some()
    }

    pub fn holds(&self, user_public: &str) -> bool {
        self.lock().contains_key(user_public)
    }

    /// Records that `user_public` is being revoked and drops its key. Called before the
    /// revocation's first step, so a renewal that checks `is_revoked` after signing
    /// either sees the mark or signed with an `iat` no later than the revocation's
    /// timestamp, which nats-server's revocation list then covers.
    pub fn mark_revoked(&self, user_public: &str) {
        self.revoked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .insert(user_public.to_string());
        self.forget(user_public);
    }

    pub fn is_revoked(&self, user_public: &str) -> bool {
        self.revoked
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .contains(user_public)
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, KeyPair>> {
        // A panic while holding the lock cannot leave a half-written entry (every
        // mutation is one insert or remove), so the map is still usable.
        self.keys
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
