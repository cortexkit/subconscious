//! Building and signing one user JWT under a vault root.

use std::fmt;

use nkeys::KeyPair;

use super::{
    jwt::UserClaims,
    nkey::{encode_public, NkeyRole},
    roots::{KeyIdLedger, KeyIdObservation},
    vault::{VaultError, VaultSigning},
};
use crate::grants::Grant;

/// What to sign: everything a user JWT carries except the issuer, which comes from the
/// root's public key as the vault reports it.
#[derive(Debug, Clone)]
pub struct UserJwtRequest<'a> {
    pub root_credential_id: &'a str,
    pub user_public: &'a str,
    pub issuer_account: Option<&'a str>,
    pub name: &'a str,
    pub issued_at: i64,
    /// The `exp` claim; production callers take it from `lifetime::JwtLifetime`.
    pub expires_at: i64,
    pub grant: &'a Grant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedUserJwt {
    pub jwt: String,
    /// The census's `user_jwt_id`.
    pub jti: String,
    /// The token's `exp` claim, as requested.
    pub exp: i64,
    /// The root's public nkey, which the token names as issuer.
    pub issuer: String,
    pub root_key_id: String,
    /// Set when this issue found the root rotated since the last one: every JWT signed
    /// under this earlier `key_id` must be re-issued.
    pub rotated_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssueError {
    Vault(VaultError),
    /// The root was rotated again between reading its public key and signing, twice in
    /// a row. Nothing was issued.
    RootUnstable {
        credential_id: String,
    },
    /// The vault's signature does not verify under the root's public key over the bytes
    /// ck-bus sent. Nothing was issued: the server would refuse the token anyway.
    SignatureDoesNotVerify {
        credential_id: String,
        key_id: String,
    },
}

impl fmt::Display for IssueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vault(error) => error.fmt(f),
            Self::RootUnstable { credential_id } => write!(
                f,
                "root {credential_id} changed key_id between its public-key read and its \
                 signature twice in a row; nothing issued"
            ),
            Self::SignatureDoesNotVerify {
                credential_id,
                key_id,
            } => write!(
                f,
                "the vault's signature under {credential_id} (key_id {key_id}) does not \
                 verify over the JWT signing input; nothing issued"
            ),
        }
    }
}

impl From<VaultError> for IssueError {
    fn from(value: VaultError) -> Self {
        Self::Vault(value)
    }
}

/// Signs one user JWT through the vault.
///
/// The root's public key is read first, because the token names it as issuer. When the
/// signature comes back under a different `key_id` the root was rotated in between: the
/// public key is re-read and the token rebuilt and signed once more. The signature is
/// checked against the public key before the token is returned, so a signer that hashes
/// first or signs the base64 text instead of the bytes is caught here.
pub async fn sign_user_jwt(
    vault: &dyn VaultSigning,
    ledger: &KeyIdLedger,
    request: &UserJwtRequest<'_>,
) -> Result<SignedUserJwt, IssueError> {
    let credential_id = request.root_credential_id;
    let mut rotated_from = None;
    for _ in 0..2 {
        let root = vault.public_key(credential_id).await?;
        if let KeyIdObservation::Rotated { previous } = ledger.observe(credential_id, &root.key_id)
        {
            rotated_from.get_or_insert(previous);
        }
        let issuer = encode_public(NkeyRole::Account, &root.public);
        let unsigned = UserClaims {
            user_public: request.user_public,
            issuer: &issuer,
            issuer_account: request.issuer_account,
            name: request.name,
            issued_at: request.issued_at,
            expires_at: request.expires_at,
            grant: request.grant,
        }
        .unsigned();
        let signature = vault.sign(credential_id, unsigned.signing_input()).await?;
        if signature.key_id != root.key_id {
            if let KeyIdObservation::Rotated { previous } =
                ledger.observe(credential_id, &signature.key_id)
            {
                rotated_from.get_or_insert(previous);
            }
            continue;
        }
        verify(&issuer, unsigned.signing_input(), &signature.signature).map_err(|()| {
            IssueError::SignatureDoesNotVerify {
                credential_id: credential_id.to_string(),
                key_id: root.key_id.clone(),
            }
        })?;
        return Ok(SignedUserJwt {
            jwt: unsigned.assemble(&signature.signature),
            jti: unsigned.jti().to_string(),
            exp: request.expires_at,
            issuer,
            root_key_id: root.key_id,
            rotated_from,
        });
    }
    Err(IssueError::RootUnstable {
        credential_id: credential_id.to_string(),
    })
}

/// Verifies with the issuer nkey built from the vault's public key, so the check covers
/// the same key the server will use.
fn verify(issuer: &str, message: &[u8], signature: &[u8; 64]) -> Result<(), ()> {
    let verifier = KeyPair::from_public_key(issuer).map_err(|_| ())?;
    verifier.verify(message, signature).map_err(|_| ())
}
