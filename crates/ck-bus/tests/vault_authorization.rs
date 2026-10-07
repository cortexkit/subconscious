//! Checks whether the real Claustrum grants vault operations to reserved:ckbus.
//!
//! The fixture vault gets the operator ceremony with the placed `ck auth`:
//! `mint-signing-key --id signing:ck-bus-account:1`, then exact `sign` and `read` grants
//! to `reserved:ckbus`. A second key receives only the `sign` grant.
//! - A supervised test relay registered as `ckbus` uses its own identity, read
//!   through the shared accessor, to get `credential.sign` and
//!   `credential.public_key` answered.
//! - The same open without the identity arrives `Direct` and gets `not_found` for both.
//! - With only the `sign` grant, `credential.public_key` answers `not_found` and
//!   `credential.sign` succeeds.
//!
//! This proves Claustrum's authorization of reserved:ckbus, not production
//! ck-bus's own vault calls or key handling.
//! No test constructs `Principal::Reserved`: the daemon stamps it from the presented
//! identity. Without `CK_CLAUSTRUM_BIN` and `CK_CK_BIN` this test records
//! `claustrum-binary-absent` loudly and reports SKIP; it never passes and never falls
//! back to the harness signer.

#[allow(dead_code)]
#[path = "../src/credentials/mod.rs"]
mod credentials;
#[allow(dead_code)]
#[path = "../src/grants/mod.rs"]
mod grants;
#[allow(dead_code)]
mod harness;
#[allow(dead_code)]
#[path = "../src/runtime/seams.rs"]
mod runtime;

use std::collections::BTreeSet;

use credentials::{
    nkey::{encode_public, NkeyRole},
    vault::{ClaustrumRoute, VaultError, VaultSigning},
    wire,
};
use harness::{
    report::{Row, RowReport, ServedBy},
    signer::{
        claustrum::{RealClaustrum, CLAUSTRUM_BINARY_ABSENT},
        run::{ClaustrumSide, SignerRun},
    },
};
use nkeys::KeyPair;
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::manifest::ProviderRole;

/// The credential id the row text names for the fixture vault's ceremony.
const ACCOUNT_ROOT: &str = "signing:ck-bus-account:1";
const SIGN_ONLY_ROOT: &str = "signing:harness-sign-only:1";

async fn claustrum_vocabulary(run: &SignerRun) -> BTreeSet<String> {
    let response = harness::control::response(
        &run.connection_file,
        ClientControlRequest::CatalogList {
            module_id: Some("claustrum".to_string()),
        },
    )
    .await;
    let ClientControlResponse::CatalogList { modules, .. } = response else {
        panic!("catalog.list must return its matching response variant");
    };
    modules
        .iter()
        .filter(|entry| entry.module_id == "claustrum")
        .flat_map(|entry| entry.roles.iter())
        .flat_map(|role| match role {
            ProviderRole::ManagementSurface { operations, .. } => {
                operations.iter().map(|op| op.name.clone()).collect()
            }
            _ => Vec::new(),
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn vault_authorization_against_the_real_claustrum() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let tree = SignerRun::tree();
    let real = match RealClaustrum::discover(&SignerRun::data_home(&tree), &tree.join("keys")) {
        Ok(real) => real,
        Err(observation) => {
            RowReport::skipped(
                Row::VaultAuthorization,
                CLAUSTRUM_BINARY_ABSENT,
                observation,
            )
            .served_by(ServedBy::ClaustrumBinary)
            .emit(&BTreeSet::new());
            return;
        }
    };
    std::fs::create_dir_all(tree.join("keys")).unwrap();
    let (account_public, account_key_id) = real.bootstrap_and_mint(ACCOUNT_ROOT);
    real.grant(ACCOUNT_ROOT, "sign");
    real.grant(ACCOUNT_ROOT, "read");
    real.ck_auth(&["mint-signing-key", "--id", SIGN_ONLY_ROOT]);
    real.grant(SIGN_ONLY_ROOT, "sign");

    // The relay tests Claustrum's reserved:ckbus grants, not the production
    // ck-bus binary's own calls or key handling.
    let run = SignerRun::start_vault_relay(tree, ClaustrumSide::Binary(&real)).await;
    let vocabulary = claustrum_vocabulary(&run).await;
    let payload = b"vault authorization row";

    // Reserved: ck-bus's identity on the route.
    let reserved = vault_relay::RelayVault(run.connection_file.clone());
    let public = reserved
        .public_key(ACCOUNT_ROOT)
        .await
        .expect("reserved:ckbus with a read grant reads the public key");
    assert_eq!(wire::hex_lower(&public.public), account_public);
    assert_eq!(public.key_id, account_key_id);
    let signature = reserved
        .sign(ACCOUNT_ROOT, payload)
        .await
        .expect("reserved:ckbus with a sign grant signs");
    assert_eq!(signature.key_id, account_key_id);
    KeyPair::from_public_key(&encode_public(NkeyRole::Account, &public.public))
        .unwrap()
        .verify(payload, &signature.signature)
        .expect("the vault's signature verifies under its public key");

    // Direct twin: the same open without the identity.
    let direct = ClaustrumRoute::new(run.connection_file.clone(), None);
    for result in [
        direct.public_key(ACCOUNT_ROOT).await.map(|_| ()),
        direct.sign(ACCOUNT_ROOT, payload).await.map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(VaultError::RootKeyUnreachable { .. })),
            "a Direct caller must get not_found: {result:?}"
        );
    }

    // Sign grant only: the public key is not_found, the signature succeeds.
    let public_only = reserved.public_key(SIGN_ONLY_ROOT).await;
    assert!(
        matches!(public_only, Err(VaultError::RootKeyUnreachable { .. })),
        "a sign grant alone must not authorize credential.public_key: {public_only:?}"
    );
    reserved
        .sign(SIGN_ONLY_ROOT, payload)
        .await
        .expect("the sign grant alone authorizes credential.sign");

    run.shutdown().await;
    eprintln!(
        "claustrum-binary: {} ({}), ck: {} ({})",
        real.claustrum_bin.display(),
        real.claustrum_version,
        real.ck_bin.display(),
        real.ck_version
    );
    RowReport::passed(Row::VaultAuthorization)
        .served_by(ServedBy::ClaustrumBinary)
        .asserts_authorization()
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary);
}

#[path = "harness/signer/vault_relay.rs"]
mod vault_relay;

#[test]
fn vault_relay_child() {
    harness::issuance::participant_child_entry();
}
