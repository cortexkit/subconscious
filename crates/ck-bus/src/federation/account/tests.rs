use std::{collections::BTreeMap, sync::Mutex};

use async_trait::async_trait;
use nkeys::KeyPair;
use serde_json::json;
use tokio::sync::mpsc;

use super::*;
use crate::{
    bootstrap::{
        account_jwt::verify_self_named_issuer,
        plane::{ConnectionEvent, PlaneError},
    },
    federation::test_support::KeyVault,
};

const MACHINE: &str = "0123456789abcdef0123456789abcdef";

/// A directory resolver: stores every pushed account JWT by its subject.
#[derive(Default)]
struct Resolver {
    accounts: Mutex<BTreeMap<String, String>>,
    updates: Mutex<usize>,
}

#[async_trait]
impl SystemPlane for Resolver {
    async fn list_accounts(&self) -> Result<Vec<String>, PlaneError> {
        Ok(self.accounts.lock().unwrap().keys().cloned().collect())
    }

    async fn lookup(&self, account_public: &str) -> Result<Option<String>, PlaneError> {
        Ok(self.accounts.lock().unwrap().get(account_public).cloned())
    }

    async fn update(&self, jwt: &str) -> Result<(), PlaneError> {
        let claims = decode_claims(jwt).unwrap();
        *self.updates.lock().unwrap() += 1;
        self.accounts
            .lock()
            .unwrap()
            .insert(claims["sub"].as_str().unwrap().to_string(), jwt.to_string());
        Ok(())
    }

    async fn kick(&self, _server_id: &str, _client_id: u64) -> Result<(), PlaneError> {
        unreachable!("the federation account never kicks")
    }

    async fn watch_connections(
        &self,
        _account_public: &str,
    ) -> Result<mpsc::UnboundedReceiver<ConnectionEvent>, PlaneError> {
        unreachable!("the federation account never watches connections")
    }
}

fn credentials() -> (Credentials, KeyPair) {
    let vault = KeyVault::default();
    let signer = KeyPair::new_operator();
    let signer_public = signer.public_key();
    vault.mint(
        &RootCredential::OperatorSigner.credential_id().unwrap(),
        KeyPair::from_seed(&signer.seed().unwrap()).unwrap(),
    );
    (
        Credentials::new(std::sync::Arc::new(vault)),
        KeyPair::from_public_key(&signer_public).unwrap(),
    )
}

#[tokio::test]
async fn creates_the_account_once_with_jetstream_and_no_signing_key() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let resolver = Resolver::default();
    let (credentials, signer) = credentials();

    let account = ensure_federation_account(&resolver, &credentials, &store, MACHINE)
        .await
        .unwrap();
    assert!(account.starts_with('A'));
    assert_eq!(
        store.read_federation_account(),
        AccountFile::Present(AccountRecord {
            machine_id: MACHINE.to_string(),
            acct: format!("fed_{MACHINE}"),
            account_public: account.clone(),
        })
    );
    let jwt = resolver.lookup(&account).await.unwrap().unwrap();
    let claims = verify_self_named_issuer(&jwt).unwrap();
    assert_eq!(
        claims["iss"],
        signer.public_key(),
        "signed by the operator signer"
    );
    assert_eq!(claims["name"], format!("fed_{MACHINE}"));
    assert_eq!(claims["nats"]["signing_keys"], json!([]));
    for limit in ["mem_storage", "disk_storage", "streams", "consumer", "leaf"] {
        assert_eq!(claims["nats"]["limits"][limit], -1, "{limit}");
    }

    // A later process re-signs the same account and never re-keys it.
    let again = ensure_federation_account(&resolver, &credentials, &store, MACHINE)
        .await
        .unwrap();
    assert_eq!(again, account);
    assert_eq!(resolver.accounts.lock().unwrap().len(), 1);
    assert_eq!(*resolver.updates.lock().unwrap(), 2);
}

#[tokio::test]
async fn a_lost_record_adopts_the_servers_account_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    let resolver = Resolver::default();
    let (credentials, _) = credentials();
    let first = ensure_federation_account(&resolver, &credentials, &store, MACHINE)
        .await
        .unwrap();
    std::fs::remove_file(store.federation_account_path()).unwrap();

    let adopted = ensure_federation_account(&resolver, &credentials, &store, MACHINE)
        .await
        .unwrap();
    assert_eq!(adopted, first);
    assert_eq!(resolver.accounts.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_damaged_record_is_named_and_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_path_buf());
    std::fs::write(store.federation_account_path(), b"{not json").unwrap();
    let (credentials, _) = credentials();
    let refusal = ensure_federation_account(&Resolver::default(), &credentials, &store, MACHINE)
        .await
        .unwrap_err();
    assert!(refusal.contains("fed_account.json is damaged"), "{refusal}");
    assert_eq!(
        std::fs::read(store.federation_account_path()).unwrap(),
        b"{not json"
    );
}
