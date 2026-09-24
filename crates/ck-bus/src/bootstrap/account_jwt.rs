//! The box account JWT (nats-io/jwt v2), built by ck-bus and signed by the operator
//! signer through the vault; and the small JWT reading the rest of bootstrap needs.
//!
//! Every limit is written explicitly: a limit missing from an account JWT decodes as 0,
//! which the server enforces as "none allowed". JetStream is enabled by the storage
//! limits. `signing_keys` lists the box account root, which signs every user JWT in the
//! account with `issuer_account` set to the account id.

use std::collections::BTreeMap;

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use data_encoding::BASE32_NOPAD;
use nkeys::KeyPair;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::credentials::{
    jwt::JWT_HEADER,
    nkey::{encode_public, NkeyRole},
    roots::KeyIdLedger,
    vault::{VaultError, VaultSigning},
};

/// The claims of one box account JWT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountClaims {
    /// The account identity public key (`A...`): the JWT subject and the account id.
    pub account_public: String,
    /// `box_<machine id>`.
    pub name: String,
    /// The keys allowed to sign user JWTs for the account (`A...`).
    pub signing_keys: Vec<String>,
    /// Revoked user public keys, each with the time at or before which its JWTs are
    /// revoked.
    pub revocations: BTreeMap<String, i64>,
    pub issued_at: i64,
}

impl AccountClaims {
    fn claims(&self, issuer: &str) -> Value {
        let mut nats = json!({
            "type": "account",
            "version": 2,
            "limits": {
                "subs": -1, "data": -1, "payload": -1, "imports": -1, "exports": -1,
                "wildcards": true, "conn": -1, "leaf": -1,
                "mem_storage": -1, "disk_storage": -1, "streams": -1, "consumer": -1,
            },
            "signing_keys": self.signing_keys,
            "default_permissions": {"pub": {}, "sub": {}},
        });
        if !self.revocations.is_empty() {
            nats["revocations"] = json!(self.revocations);
        }
        let mut claims = json!({
            "jti": "",
            "iat": self.issued_at,
            "iss": issuer,
            "name": self.name,
            "sub": self.account_public,
            "nats": nats,
        });
        // The jti only has to be unique per token; a digest of the claims with an empty
        // jti makes it so without a random source.
        let jti = BASE32_NOPAD.encode(&Sha256::digest(claims.to_string().as_bytes()));
        claims["jti"] = Value::String(jti);
        claims
    }
}

/// A decoded JWT's claims. The signature is not checked here: callers that need trust
/// check the issuer themselves.
pub fn decode_claims(jwt: &str) -> Option<Value> {
    let mut parts = jwt.trim().split('.');
    let (_, claims, _) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(claims).ok()?).ok()
}

/// Verifies a JWT's signature under the key it names as issuer.
pub fn verify_self_named_issuer(jwt: &str) -> Result<Value, String> {
    let claims = decode_claims(jwt).ok_or("not a three-part JWT with JSON claims")?;
    let issuer = claims["iss"].as_str().ok_or("the JWT names no issuer")?;
    let (input, signature) = jwt
        .trim()
        .rsplit_once('.')
        .ok_or("the JWT has no signature")?;
    let signature = URL_SAFE_NO_PAD
        .decode(signature)
        .map_err(|_| "the signature is not base64url")?;
    KeyPair::from_public_key(issuer)
        .map_err(|_| "the issuer is not an nkey")?
        .verify(input.as_bytes(), &signature)
        .map_err(|_| "the signature does not verify under the issuer")?;
    Ok(claims)
}

/// The revocations an account JWT carries.
pub fn revocations(claims: &Value) -> BTreeMap<String, i64> {
    claims["nats"]["revocations"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(key, at)| Some((key.clone(), at.as_i64()?)))
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountSignError {
    Vault(VaultError),
    /// The signer's `key_id` changed between its public-key read and its signature,
    /// twice in a row.
    SignerUnstable {
        credential_id: String,
    },
    SignatureDoesNotVerify {
        credential_id: String,
    },
}

impl std::fmt::Display for AccountSignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Vault(error) => error.fmt(f),
            Self::SignerUnstable { credential_id } => write!(
                f,
                "operator signer {credential_id} changed key_id between its public-key read \
                 and its signature twice in a row; nothing pushed"
            ),
            Self::SignatureDoesNotVerify { credential_id } => write!(
                f,
                "the vault's signature under {credential_id} does not verify over the \
                 account JWT; nothing pushed"
            ),
        }
    }
}

/// Signs the box account JWT with the operator signer. The signer's public key is read
/// first because the token names it (`O...`) as issuer; a changed `key_id` on the
/// signature means the signer rotated in between, and the token is rebuilt once.
pub async fn sign_account_jwt(
    vault: &dyn VaultSigning,
    ledger: &KeyIdLedger,
    signer_credential_id: &str,
    claims: &AccountClaims,
) -> Result<String, AccountSignError> {
    for _ in 0..2 {
        let signer = vault
            .public_key(signer_credential_id)
            .await
            .map_err(AccountSignError::Vault)?;
        let _ = ledger.observe(signer_credential_id, &signer.key_id);
        let issuer = encode_public(NkeyRole::Operator, &signer.public);
        let input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes()),
            URL_SAFE_NO_PAD.encode(claims.claims(&issuer).to_string().as_bytes())
        );
        let signature = vault
            .sign(signer_credential_id, input.as_bytes())
            .await
            .map_err(AccountSignError::Vault)?;
        if signature.key_id != signer.key_id {
            let _ = ledger.observe(signer_credential_id, &signature.key_id);
            continue;
        }
        let verified = KeyPair::from_public_key(&issuer)
            .ok()
            .and_then(|pair| pair.verify(input.as_bytes(), &signature.signature).ok());
        if verified.is_none() {
            return Err(AccountSignError::SignatureDoesNotVerify {
                credential_id: signer_credential_id.to_string(),
            });
        }
        return Ok(format!(
            "{input}.{}",
            URL_SAFE_NO_PAD.encode(signature.signature)
        ));
    }
    Err(AccountSignError::SignerUnstable {
        credential_id: signer_credential_id.to_string(),
    })
}
