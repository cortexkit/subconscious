//! Test doubles shared by the federation unit tests.

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicBool, Ordering},
        Mutex,
    },
};

use async_trait::async_trait;
use nkeys::KeyPair;

use crate::credentials::{
    vault::{Retry, VaultError, VaultSigning},
    wire::{key_id_for, VaultPublicKey, VaultSignature},
};

/// A vault holding throwaway Ed25519 keys by credential id.
#[derive(Default)]
pub struct KeyVault {
    keys: Mutex<HashMap<String, KeyPair>>,
    pub down: AtomicBool,
    pub calls: Mutex<Vec<String>>,
}

impl KeyVault {
    /// Creates, or replaces, the key under `credential_id` and returns its nkey string.
    pub fn mint(&self, credential_id: &str, pair: KeyPair) -> String {
        let public = pair.public_key();
        self.keys
            .lock()
            .unwrap()
            .insert(credential_id.to_string(), pair);
        public
    }

    pub fn raw_public(&self, credential_id: &str) -> [u8; 32] {
        let keys = self.keys.lock().unwrap();
        nkeys::from_public_key(&keys[credential_id].public_key())
            .unwrap()
            .1
    }

    fn unavailable(&self) -> Result<(), VaultError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(VaultError::Route {
                code: Some("target_unavailable".to_string()),
                retry: Retry::Retryable,
                message: "the test vault is down".to_string(),
            });
        }
        Ok(())
    }
}

#[async_trait]
impl VaultSigning for KeyVault {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("sign {credential_id}"));
        self.unavailable()?;
        let keys = self.keys.lock().unwrap();
        let pair = keys
            .get(credential_id)
            .ok_or_else(|| VaultError::RootKeyUnreachable {
                credential_id: credential_id.to_string(),
            })?;
        let signature: [u8; 64] = pair.sign(payload).unwrap().try_into().unwrap();
        let public = nkeys::from_public_key(&pair.public_key()).unwrap().1;
        Ok(VaultSignature {
            signature,
            key_id: key_id_for(&public),
        })
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("public_key {credential_id}"));
        self.unavailable()?;
        let keys = self.keys.lock().unwrap();
        let pair = keys
            .get(credential_id)
            .ok_or_else(|| VaultError::RootKeyUnreachable {
                credential_id: credential_id.to_string(),
            })?;
        let public = nkeys::from_public_key(&pair.public_key()).unwrap().1;
        Ok(VaultPublicKey {
            public,
            key_id: key_id_for(&public),
        })
    }
}
