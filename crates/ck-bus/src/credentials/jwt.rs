//! NATS user JWTs (nats-io/jwt v2), built by ck-bus and signed by a vault root.
//!
//! A token is `<header>.<claims>.<signature>`, each part unpadded base64url. The header is
//! fixed; the signature is Ed25519 by the issuer over the ASCII of `<header>.<claims>`.
//! ck-bus sends exactly those ASCII bytes to `credential.sign` and re-encodes the standard
//! base64 signature it gets back as unpadded base64url.
//!
//! Every token carries `exp` (R16: 15 minutes after `iat`, see `lifetime`), so a user
//! whose revocation was lost still stops working once its last JWT expires.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use data_encoding::BASE32_NOPAD;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::grants::Grant;

/// The header ck-bus writes on every user JWT: a nats-io/jwt v2 token signed with an
/// Ed25519 nkey.
pub const JWT_HEADER: &str = r#"{"alg":"ed25519-nkey","typ":"JWT"}"#;

/// The claims of one user JWT.
#[derive(Debug, Clone)]
pub struct UserClaims<'a> {
    /// The user's public nkey (`U...`): the JWT subject.
    pub user_public: &'a str,
    /// The signing root's public nkey (`A...`): the JWT issuer.
    pub issuer: &'a str,
    /// The account's identity key, when the issuer is one of the account's signing keys
    /// rather than the identity key itself. The server then checks the issuer against the
    /// account JWT's `signing_keys`.
    pub issuer_account: Option<&'a str>,
    pub name: &'a str,
    /// Seconds since the Unix epoch.
    pub issued_at: i64,
    /// The `exp` claim, seconds since the Unix epoch.
    pub expires_at: i64,
    /// The generated permission set; only its allow lists are written.
    pub grant: &'a Grant,
}

/// A user JWT waiting for its signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsignedJwt {
    signing_input: String,
    jti: String,
}

impl UserClaims<'_> {
    pub fn unsigned(&self) -> UnsignedJwt {
        let permissions = self.grant.jwt_permissions();
        let mut nats = json!({
            "type": "user",
            "version": 2,
            "pub": permissions["pub"],
            "sub": permissions["sub"],
            "subs": -1,
            "data": -1,
            "payload": -1,
        });
        if let Some(account) = self.issuer_account {
            nats["issuer_account"] = Value::String(account.to_string());
        }
        let mut claims = json!({
            "jti": "",
            "iat": self.issued_at,
            "exp": self.expires_at,
            "iss": self.issuer,
            "name": self.name,
            "sub": self.user_public,
            "nats": nats,
        });
        // The jti is the census's revocation handle, so it must be unique per token; a
        // digest of the claims with an empty jti makes it so without a random source.
        let jti = BASE32_NOPAD.encode(&Sha256::digest(claims.to_string().as_bytes()));
        claims["jti"] = Value::String(jti.clone());
        let signing_input = format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(JWT_HEADER.as_bytes()),
            URL_SAFE_NO_PAD.encode(claims.to_string().as_bytes())
        );
        UnsignedJwt { signing_input, jti }
    }
}

impl UnsignedJwt {
    /// The exact bytes the issuer signs: the ASCII of `<header>.<claims>`.
    pub fn signing_input(&self) -> &[u8] {
        self.signing_input.as_bytes()
    }

    /// The token's `jti`, which the census records as `user_jwt_id`.
    pub fn jti(&self) -> &str {
        &self.jti
    }

    /// The finished token, with the signature re-encoded as unpadded base64url.
    pub fn assemble(&self, signature: &[u8; 64]) -> String {
        format!(
            "{}.{}",
            self.signing_input,
            URL_SAFE_NO_PAD.encode(signature)
        )
    }
}
