//! The direct hub rig, connected using a credential issued by ck-bus itself.

#![allow(dead_code)] // Production modules are included without the module executable.

#[path = "../../src/bootstrap/mod.rs"]
mod bootstrap;
#[path = "../../src/credentials/mod.rs"]
mod credentials;
#[path = "../../src/grants/mod.rs"]
mod grants;
mod hub_module_imports;
#[path = "../../src/issuance/mod.rs"]
mod issuance;
#[path = "../../src/membership/mod.rs"]
mod membership;
#[path = "../../src/revocation/mod.rs"]
mod revocation;
#[path = "../../src/runtime/seams.rs"]
mod runtime;
#[path = "../../src/federation/test_support.rs"]
mod test_support;

use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use async_trait::async_trait;
use bootstrap::{
    account_jwt::decode_claims,
    plane::{Broker, NatsBroker, NatsSystem},
    store::{AccountRecord, Store},
    Ready,
};
use cortexkit_bus_naming::{
    federation::{BoxId, FEDOUT_STREAM},
    AccountNames,
};
use credentials::{
    issue::{sign_user_jwt, UserJwtRequest},
    roots::RootCredential,
    Credentials,
};
use hub_module_imports::{prove_memory, shared_modules, Topology};
use issuance::{
    handler::{IssuanceHandler, ReadyHub},
    Issuance, LiveGenerations, Plane, PlaneSource,
};
use serde_json::{json, Value};
use subc_client_rs::HandlerOutcome;
use subc_protocol::Principal;

struct Generation;

#[async_trait]
impl LiveGenerations for Generation {
    async fn live_generation(&self, _module_id: &str) -> Result<Option<u64>, String> {
        Ok(Some(7))
    }
}

struct FixedPlane(Option<Plane>);

impl PlaneSource for FixedPlane {
    fn current(&self) -> Option<Plane> {
        self.0.clone()
    }
}

async fn request(
    handler: &IssuanceHandler<()>,
    module_id: &str,
    method: &str,
    params: Value,
) -> HandlerOutcome {
    handler
        .answer(
            Some(&Principal::Reserved {
                module_id: module_id.to_owned(),
            }),
            &serde_json::to_vec(&json!({"method": method, "params": params})).unwrap(),
        )
        .await
        .expect("operation is served")
}

fn result(outcome: HandlerOutcome) -> Value {
    match outcome {
        HandlerOutcome::Response(body) => {
            serde_json::from_slice::<Value>(&body).unwrap()["result"].clone()
        }
        HandlerOutcome::Error { code, message } => panic!("unexpected {code}: {message}"),
        _ => panic!("expected a single reply"),
    }
}

fn refused(outcome: HandlerOutcome, expected: &str) {
    match outcome {
        HandlerOutcome::Error { code, .. } => assert_eq!(code, expected),
        _ => panic!("expected refusal {expected}"),
    }
}

#[tokio::test]
async fn hub_credential_refuses_non_hub_attested_caller_even_if_it_claims_a_hub_module() {
    let dir = tempfile::tempdir().unwrap();
    let creds = Arc::new(Credentials::new(
        Arc::new(test_support::KeyVault::default()),
    ));
    let issued = Arc::new(Issuance::new(
        creds,
        dir.path(),
        Arc::new(Generation),
        Arc::new(FixedPlane(None)),
    ));
    let handler = IssuanceHandler::new((), issued);
    refused(
        request(
            &handler,
            "non-hub",
            issuance::HUB_CREDENTIAL_OP,
            json!({"module_id": "claustrum"}),
        )
        .await,
        "hub_not_granted",
    );
    refused(
        request(
            &handler,
            "non-hub",
            issuance::HUB_CREDENTIAL_RENEW_OP,
            json!({"credential_public": "a key", "module_id": "plexus"}),
        )
        .await,
        "hub_not_granted",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn hub_credential_issued_by_ckbus_runs_the_real_memory_probes() {
    let box_id = BoxId::parse("11111111111111111111111111111111").unwrap();
    let modules = shared_modules(&box_id);
    let (rig, fixture) = Topology::start_issuance(&box_id, &modules).await;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::new(dir.path().to_owned());
    let vault = Arc::new(test_support::KeyVault::default());
    let box_root = RootCredential::BoxAccount.credential_id().unwrap();
    vault.mint(&box_root, fixture.box_signer);
    vault.mint(
        &RootCredential::OperatorSigner.credential_id().unwrap(),
        fixture.operator,
    );
    let credentials = Arc::new(Credentials::new(vault.clone()));
    let server_url = rig.local_url();
    let broker = NatsBroker::new(server_url.clone(), credentials.clone());
    let names = AccountNames::derive(&format!("box_{box_id}")).unwrap();
    let box_user = credentials.custody.generate_user();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let box_jwt = sign_user_jwt(
        vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: &box_root,
            user_public: &box_user,
            issuer_account: None,
            name: "ckbus-box",
            issued_at: now,
            expires_at: now + 900,
            grant: &grants::bus_module_grant(&names, &box_user).unwrap(),
        },
    )
    .await
    .unwrap();
    let box_plane = broker.connect_box(&box_jwt.jwt, &box_user).await.unwrap();
    box_plane.ensure_census(&names).await.unwrap();
    let ready = Arc::new(Ready {
        orphaned_module_durables: 0,
        account: AccountRecord {
            machine_id: box_id.to_string(),
            acct: names.account().to_owned(),
            account_public: fixture.box_account.clone(),
        },
        system_user: String::new(),
        box_user,
        system: Arc::new(NatsSystem::from_client(fixture.system)),
        box_plane: box_plane.clone(),
        federation_account: Some(fixture.federation.clone()),
        fedout_account: tokio::sync::OnceCell::new(),
        renewals: Vec::new(),
    });
    let (_tx, ready_rx) = tokio::sync::watch::channel(Some(ready.clone()));
    let issuance = Arc::new(Issuance::new(
        credentials.clone(),
        dir.path(),
        Arc::new(Generation),
        Arc::new(FixedPlane(Some(Plane {
            names: names.clone(),
            account_public: fixture.box_account,
            server_url: server_url.clone(),
            box_plane: box_plane.clone(),
        }))),
    ));
    let linked = Arc::new(AtomicBool::new(false));
    let link_state = linked.clone();
    issuance.set_hub_source(Arc::new(ReadyHub {
        ready: ready_rx.clone(),
        server_url: server_url.clone(),
        credentials: credentials.clone(),
        store: store.clone(),
        linked: Arc::new(move || link_state.load(Ordering::Relaxed)),
    }));
    let revoker = Arc::new(revocation::Revoker::new(
        credentials,
        dir.path(),
        Arc::new(revocation::connections::Connections::default()),
    ));
    issuance.set_replacement_guard(Arc::new(revocation::handler::Area::new(
        revoker.clone(),
        ready_rx,
    )));
    let handler = IssuanceHandler::new((), issuance.clone());
    let issue = || {
        request(
            &handler,
            "magic-context",
            issuance::HUB_CREDENTIAL_OP,
            json!({"module_id": "not-the-attested-id"}),
        )
    };
    refused(issue().await, "hub_unavailable");
    linked.store(true, Ordering::Relaxed);
    refused(issue().await, "hub_unavailable");
    assert!(ready.fedout_account.get().is_none());
    assert!(box_plane
        .census_get(&names, "hub.magic-context")
        .await
        .unwrap()
        .is_none());

    let signer = vault.mint(
        "signing:bus-federation-account:1",
        nkeys::KeyPair::new_account(),
    );
    let box_issued = result(
        request(
            &handler,
            "magic-context",
            issuance::CREDENTIAL_OP,
            json!({}),
        )
        .await,
    );
    let answer = result(issue().await);
    for module_id in ["claustrum", "plexus"] {
        let issued = result(
            request(
                &handler,
                module_id,
                issuance::HUB_CREDENTIAL_OP,
                json!({"module_id": "not-the-attested-id"}),
            )
            .await,
        );
        assert_eq!(issued["account_public"], fixture.federation);
        let claims = decode_claims(issued["jwt"].as_str().unwrap()).unwrap();
        assert_eq!(claims["sub"], issued["credential_public"]);
        assert_eq!(claims["nats"]["issuer_account"], fixture.federation);
        assert_eq!(
            claims["nats"]["sub"]["allow"],
            json!([format!("ckdel.{box_id}.{module_id}.>")])
        );
        let publish = claims["nats"]["pub"]["allow"].as_array().unwrap();
        assert!(publish.contains(&json!("$JS.hub.API.DIRECT.GET.KV_vault-manifest-active")));
        assert_eq!(
            publish.contains(&json!(format!("$KV.vault-shared-logins.{box_id}.>"))),
            module_id == "claustrum"
        );
    }
    assert_eq!(answer["acct"], format!("fed_{box_id}"));
    assert_eq!(answer["account_public"], fixture.federation);
    assert_eq!(answer["box_id"], box_id.as_str());
    assert_eq!(answer["domain"], "hub");
    assert_eq!(
        answer["inbox_prefix"],
        format!("ckdel.{box_id}.magic-context")
    );
    assert_eq!(answer["server_url"], server_url);
    assert_eq!(ready.fedout_account.get(), Some(&fixture.federation));
    let account_claims = || async {
        decode_claims(
            &ready
                .system
                .lookup(&fixture.federation)
                .await
                .unwrap()
                .unwrap(),
        )
        .unwrap()
    };
    assert_eq!(
        account_claims().await["nats"]["signing_keys"],
        json!([signer])
    );
    let public = answer["credential_public"].as_str().unwrap().to_owned();
    let jwt = answer["jwt"].as_str().unwrap().to_owned();
    let options = async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
        let public = public.clone();
        let issuance = issuance.clone();
        let sign = tokio::spawn(async move {
            issuance
                .sign_nonce("magic-context", Some(&public), &nonce)
                .await
        });
        async move { sign.await.unwrap().map_err(async_nats::AuthError::new) }
    });
    let (memory, mut errors) = rig
        .connect_options(options, answer["inbox_prefix"].as_str().unwrap())
        .await;
    prove_memory(&memory, &mut errors, &box_id, &modules).await;
    let fedout = rig.fedout_config(FEDOUT_STREAM).await;
    assert_eq!(fedout.subjects, ["fedout.>"]);
    assert_eq!(fedout.max_message_size, 1024 * 1024);

    let renewed = result(
        request(
            &handler,
            "magic-context",
            issuance::HUB_CREDENTIAL_RENEW_OP,
            json!({"credential_public": answer["credential_public"]}),
        )
        .await,
    );
    assert_eq!(renewed["credential_public"], answer["credential_public"]);
    assert_eq!(renewed["credential_epoch"], answer["credential_epoch"]);
    let replacement = result(issue().await);
    assert_ne!(
        replacement["credential_public"],
        answer["credential_public"]
    );
    refused(
        request(
            &handler,
            "magic-context",
            issuance::HUB_CREDENTIAL_RENEW_OP,
            json!({"credential_public": answer["credential_public"]}),
        )
        .await,
        "ckbus_credential_revoked",
    );
    let plane = revocation::RevocationPlane::from_ready(&ready).unwrap();
    let completed = revoker.resume_all(&plane).await;
    assert_eq!(completed.len(), 1);
    assert!(completed[0].1.as_ref().unwrap().pushed);
    let old = answer["credential_public"].as_str().unwrap();
    assert!(account_claims().await["nats"]["revocations"][old].is_i64());
    for (key, current) in [
        ("hub.magic-context", replacement),
        ("magic-context", box_issued),
    ] {
        let record = box_plane.census_get(&names, key).await.unwrap().unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&record.value).unwrap()["credential_public"],
            current["credential_public"]
        );
    }
}
