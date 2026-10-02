//! Vault calls made by a supervised fixture registered as reserved `ckbus`.
//! The test receives replies, never the relay's launch secret. These checks prove
//! Claustrum's authorization of that identity, not production ck-bus's own calls.

use std::path::PathBuf;

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    credentials::{
        vault::{VaultError, VaultSigning},
        wire::{self, ReplyError, VaultPublicKey, VaultSignature},
    },
    harness::issuance,
};

pub struct RelayVault(pub PathBuf);

impl RelayVault {
    pub async fn call(&self, request: Value) -> Vec<u8> {
        let reply = issuance::relay_at(&self.0, "ckbus", json!({"attest": true, "body": request}))
            .await
            .expect("the supervised vault relay answers");
        let reply = reply
            .get("ok")
            .cloned()
            .unwrap_or_else(|| json!({"error": reply["error"]}));
        serde_json::to_vec(&reply).unwrap()
    }
}

fn from_reply(credential_id: &str, error: ReplyError) -> VaultError {
    match error {
        ReplyError::Refused { code, .. } if code == "not_found" => VaultError::RootKeyUnreachable {
            credential_id: credential_id.to_string(),
        },
        ReplyError::Refused { code, class } => VaultError::Refused { code, class },
        ReplyError::Malformed(detail) => VaultError::Malformed(detail),
    }
}

#[async_trait]
impl VaultSigning for RelayVault {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let request = wire::sign_request(credential_id, payload)
            .map_err(|error| from_reply(credential_id, error))?;
        let reply = self.call(serde_json::from_slice(&request).unwrap()).await;
        wire::parse_sign_reply(&reply).map_err(|error| from_reply(credential_id, error))
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        let reply = self
            .call(serde_json::from_slice(&wire::public_key_request(credential_id)).unwrap())
            .await;
        wire::parse_public_key_reply(&reply).map_err(|error| from_reply(credential_id, error))
    }
}
