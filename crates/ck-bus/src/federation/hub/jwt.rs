//! The hub's JWTs (nats-io/jwt v2), each signed through the vault.
//!
//! The hub has its own operator, separate from the local server's: the vault key
//! `signing:bus-hub-operator:1` is the operator identity. It self-signs the operator
//! JWT, signs the hub system account and the user's hub account, and, listed as the
//! system account's signing key, signs ck-bus's own system user on the hub. The hub
//! account lists `signing:bus-hub-account:1` as its signing key, which signs every leaf
//! user JWT and ck-bus's own admin user in that account.
//!
//! The account identity keys are generated in memory once, at the first setup; only
//! their public halves are kept (in the hub record), because every JWT in an account is
//! signed by the operator or by a signing key the account lists.

use std::collections::BTreeMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use data_encoding::BASE32_NOPAD;
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::{
    bootstrap::account_jwt::AccountClaims,
    credentials::{
        jwt::JWT_HEADER,
        nkey::{encode_public, NkeyRole},
        vault::VaultSigning,
        wire::VaultPublicKey,
    },
};

/// The hub operator's name in its JWT.
pub const HUB_OPERATOR_NAME: &str = "cortexkit-hub";
/// The hub system account's name.
pub const HUB_SYSTEM_ACCOUNT_NAME: &str = "SYS";
/// The user's hub account's name.
pub const HUB_ACCOUNT_NAME: &str = "hub";
/// A leaf user JWT lives 24 hours on every hub kind, so a missed revocation is bounded
/// the same way everywhere.
pub const LEAF_JWT_LIFETIME_SECS: i64 = 24 * 60 * 60;
/// ck-bus's own users on the hub sign in for one short job at a time; each connection
/// gets a fresh JWT.
pub const ADMIN_JWT_LIFETIME_SECS: i64 = 15 * 60;

/// A JWT signed through the vault, with the key it was signed under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signed {
    pub jwt: String,
    /// The signer's public key, as the vault reported it.
    pub signer: VaultPublicKey,
}

/// Signs the claims `build` makes for the signer's nkey (encoded with `role`) through
/// the vault. The public key is read first because the claims name it; a signature
/// under a different `key_id` means the key rotated in between, and the claims are
/// rebuilt and signed once more. The signature is checked before the JWT is returned.
pub async fn sign_claims(
    vault: &dyn VaultSigning,
    credential_id: &str,
    role: NkeyRole,
    build: impl Fn(&str) -> Value,
) -> Result<Signed, String> {
    for _ in 0..2 {
        let signer = vault
            .public_key(credential_id)
            .await
            .map_err(|error| format!("{credential_id}: {error}"))?;
        let issuer = encode_public(role, &signer.public);
        let mut claims = build(&issuer);
        claims["jti"] = Value::String(String::new());
        // The jti only has to be unique per token; a digest of the claims with an empty
        // jti makes it so without a random source.
        let jti = BASE32_NOPAD.encode(&Sha256::digest(claims.to_string().as_bytes()));
        claims["jti"] = Value::String(jti);
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes()),
            URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
        );
        let signature = vault
            .sign(credential_id, input.as_bytes())
            .await
            .map_err(|error| format!("{credential_id}: {error}"))?;
        if signature.key_id != signer.key_id {
            continue;
        }
        KeyPair::from_public_key(&issuer)
            .ok()
            .and_then(|pair| pair.verify(input.as_bytes(), &signature.signature).ok())
            .ok_or_else(|| {
                format!("the vault's signature under {credential_id} does not verify")
            })?;
        return Ok(Signed {
            jwt: format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.signature)),
            signer,
        });
    }
    Err(format!(
        "{credential_id} changed key_id between its public-key read and its signature twice \
         in a row"
    ))
}

/// The operator JWT, self-signed by the hub operator key.
pub fn operator_claims(issuer: &str, system_account: &str, issued_at: i64) -> Value {
    json!({
        "iat": issued_at,
        "iss": issuer,
        "name": HUB_OPERATOR_NAME,
        "sub": issuer,
        "nats": {
            "type": "operator",
            "version": 2,
            "system_account": system_account,
        },
    })
}

/// The hub system account, signed by the operator. It lists the operator key itself
/// (as an account nkey) as its signing key, so ck-bus can issue its own system user
/// without a key of the system account's. JetStream stays off: the server refuses it
/// on the system account.
pub fn system_account_claims(
    issuer: &str,
    account_public: &str,
    operator_as_account: &str,
    issued_at: i64,
) -> Value {
    json!({
        "iat": issued_at,
        "iss": issuer,
        "name": HUB_SYSTEM_ACCOUNT_NAME,
        "sub": account_public,
        "nats": {
            "type": "account",
            "version": 2,
            "limits": {
                "subs": -1, "data": -1, "payload": -1, "imports": -1, "exports": -1,
                "wildcards": true, "conn": -1, "leaf": -1,
            },
            "signing_keys": [operator_as_account],
            "default_permissions": {"pub": {}, "sub": {}},
        },
    })
}

/// The user's hub account, signed by the operator: JetStream on with every limit
/// explicit, the hub account key as its signing key, and the revoked leaf keys.
pub fn hub_account_claims(
    issuer: &str,
    account_public: &str,
    account_signing_key: &str,
    revocations: &BTreeMap<String, i64>,
    issued_at: i64,
) -> Value {
    AccountClaims {
        account_public: account_public.to_string(),
        name: HUB_ACCOUNT_NAME.to_string(),
        signing_keys: vec![account_signing_key.to_string()],
        revocations: revocations.clone(),
        issued_at,
    }
    .claims(issuer)
}

/// One user JWT with explicit allow lists. `issuer_account` names the account whose
/// signing key `issuer` is.
pub struct UserJwt<'a> {
    pub user_public: &'a str,
    pub issuer_account: &'a str,
    pub name: &'a str,
    pub issued_at: i64,
    pub expires_at: i64,
    pub publish: &'a [String],
    pub subscribe: &'a [String],
}

impl UserJwt<'_> {
    pub fn claims(&self, issuer: &str) -> Value {
        json!({
            "iat": self.issued_at,
            "exp": self.expires_at,
            "iss": issuer,
            "name": self.name,
            "sub": self.user_public,
            "nats": {
                "type": "user",
                "version": 2,
                "pub": { "allow": self.publish },
                "sub": { "allow": self.subscribe },
                "subs": -1,
                "data": -1,
                "payload": -1,
                "issuer_account": self.issuer_account,
            },
        })
    }
}

/// The operator key's public half as an account nkey, the form an account's
/// `signing_keys` takes.
pub fn as_account_key(public: &VaultPublicKey) -> String {
    encode_public(NkeyRole::Account, &public.public)
}

/// A new account identity: only the public half leaves this function.
pub fn new_account_identity() -> String {
    KeyPair::new_account().public_key()
}

/// ck-bus's own system user's allow lists on the hub: the claims update and lookup, the
/// leaf report and the kick, with replies on the user's own inbox.
pub fn system_user_permissions(user_public: &str) -> (Vec<String>, Vec<String>) {
    (
        [
            "$SYS.REQ.CLAIMS.UPDATE",
            "$SYS.REQ.CLAIMS.LIST",
            "$SYS.REQ.ACCOUNT.*.CLAIMS.LOOKUP",
            "$SYS.REQ.SERVER.*.LEAFZ",
            "$SYS.REQ.SERVER.*.KICK",
        ]
        .map(str::to_string)
        .to_vec(),
        vec![format!("_INBOX.{user_public}.>")],
    )
}

/// ck-bus's own admin user in the hub account: the JetStream API, to create the hub
/// streams, with replies on the user's own inbox.
pub fn admin_user_permissions(user_public: &str) -> (Vec<String>, Vec<String>) {
    (
        vec!["$JS.API.>".to_string()],
        vec![format!("_INBOX.{user_public}.>")],
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        bootstrap::account_jwt::{decode_claims, verify_self_named_issuer},
        federation::test_support::KeyVault,
    };

    #[tokio::test]
    async fn the_operator_jwt_is_self_signed_and_an_account_names_the_operator() {
        let vault = KeyVault::default();
        vault.mint("signing:op", KeyPair::new_operator());
        let system = new_account_identity();
        let operator = sign_claims(&vault, "signing:op", NkeyRole::Operator, |issuer| {
            operator_claims(issuer, &system, 100)
        })
        .await
        .unwrap();
        let claims = verify_self_named_issuer(&operator.jwt).unwrap();
        assert_eq!(claims["iss"], claims["sub"]);
        assert!(claims["iss"].as_str().unwrap().starts_with('O'));
        assert_eq!(claims["nats"]["system_account"], json!(system));
        assert!(!claims["jti"].as_str().unwrap().is_empty());

        let sys = sign_claims(&vault, "signing:op", NkeyRole::Operator, |issuer| {
            system_account_claims(issuer, &system, &as_account_key(&operator.signer), 100)
        })
        .await
        .unwrap();
        let claims = decode_claims(&sys.jwt).unwrap();
        assert_eq!(claims["iss"], decode_claims(&operator.jwt).unwrap()["iss"]);
        let signing_key = claims["nats"]["signing_keys"][0].as_str().unwrap();
        assert!(signing_key.starts_with('A'));
        assert_eq!(
            nkeys::from_public_key(signing_key).unwrap().1,
            operator.signer.public
        );
    }
}
