//! The leaf remote the engine is given (`Remote` in the channel wire, v1), and its name.

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::credentials::wire::hex_lower;

/// Subjects the federation account keeps to itself in both directions. `fedlocal.>` is
/// where opened deliveries are handed to the box account, and `fedout.>` is the outbox
/// ck-bus seals into; neither may cross the leaf, whatever the hub's grant says.
pub const DENIED_SUBJECTS: [&str; 2] = ["fedlocal.>", "fedout.>"];

/// How the engine checks the hub's certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pin {
    /// `spki` for a self or peer hub: the hub certificate's own SubjectPublicKeyInfo.
    pub kind: &'static str,
    pub sha256_hex: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub name: String,
    pub account: String,
    pub urls: Vec<String>,
    pub deny_imports: Vec<String>,
    pub deny_exports: Vec<String>,
    pub pin: Pin,
}

impl Remote {
    /// A remote for `account` dialling `urls`, pinned to the SPKI hash `spki_sha256`,
    /// named from everything the server cannot change in place.
    pub fn spki(account: &str, urls: Vec<String>, spki_sha256: &str) -> Self {
        let deny: Vec<String> = DENIED_SUBJECTS.iter().map(|s| s.to_string()).collect();
        let pin = Pin {
            kind: "spki",
            sha256_hex: spki_sha256.to_string(),
        };
        Self {
            name: remote_name(account, &deny, &deny, &pin),
            account: account.to_string(),
            urls,
            deny_imports: deny.clone(),
            deny_exports: deny,
            pin,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "account": self.account,
            "urls": self.urls,
            "deny_imports": self.deny_imports,
            "deny_exports": self.deny_exports,
            "pin": { "kind": self.pin.kind, "sha256_hex": self.pin.sha256_hex },
        })
    }
}

/// A remote's name. `nats-server` refuses to reload a remote whose account, deny lists
/// or pin changed in place, so those are hashed into the name: a change to any of them
/// gives a new name, which the engine applies as remove-and-add. The URLs are left out,
/// because they change in place (a new candidate, the relay bridge) and the link should
/// survive that. Fits the wire's `[a-z0-9_-]{1,64}`.
pub fn remote_name(
    account: &str,
    deny_imports: &[String],
    deny_exports: &[String],
    pin: &Pin,
) -> String {
    let identity = json!({
        "account": account,
        "deny_imports": deny_imports,
        "deny_exports": deny_exports,
        "pin": { "kind": pin.kind, "sha256_hex": pin.sha256_hex },
    });
    let digest = Sha256::digest(identity.to_string().as_bytes());
    format!("hub-{}", hex_lower(&digest[..8]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(account: &str, pin: &str) -> Remote {
        Remote::spki(account, vec!["tls://10.0.0.1:7422".to_string()], pin)
    }

    #[test]
    fn binds_one_account_and_denies_both_federation_families() {
        let remote = remote("AFED", &"ab".repeat(32));
        assert_eq!(
            remote.to_json(),
            json!({
                "name": remote.name,
                "account": "AFED",
                "urls": ["tls://10.0.0.1:7422"],
                "deny_imports": ["fedlocal.>", "fedout.>"],
                "deny_exports": ["fedlocal.>", "fedout.>"],
                "pin": {"kind": "spki", "sha256_hex": "ab".repeat(32)},
            })
        );
        let name = &remote.name;
        assert!(name.len() <= 64);
        assert!(name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_'));
    }

    #[test]
    fn a_new_pin_or_account_renames_and_new_urls_do_not() {
        let base = remote("AFED", &"ab".repeat(32));
        assert_ne!(base.name, remote("AFED", &"cd".repeat(32)).name);
        assert_ne!(base.name, remote("AOTHER", &"ab".repeat(32)).name);
        let moved = Remote::spki(
            "AFED",
            vec![
                "tls://10.0.0.2:7422".to_string(),
                "tls://127.0.0.1:49152".to_string(),
            ],
            &"ab".repeat(32),
        );
        assert_eq!(base.name, moved.name);
        let pin = base.pin.clone();
        let fewer = vec!["fedout.>".to_string()];
        assert_ne!(
            base.name,
            remote_name("AFED", &fewer, &base.deny_exports, &pin)
        );
    }
}
