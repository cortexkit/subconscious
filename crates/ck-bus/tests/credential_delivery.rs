//! Ladder row "Credential delivery and attestation" (slice 5 of
//! `docs/specs/ck-bus-module.md`). served-by: harness-signer (the box account root that
//! signs the participant's JWT), against a real nats-server and the acceptance daemon.
//!
//! A participant spawned by the daemon as a supervised module (this row's own
//! executable, see `harness::issuance`) presents its consumer identity and calls
//! `ckbus.credential`. It gets a JWT whose subject is a fresh user key, whose grant is
//! the naming crate's participant grant for that key, and whose name is the attested
//! module id even though its request claims `ckbus`. The row then connects to the server
//! with that JWT, answering the nonce through `ckbus.nonce_sign`.
//!
//! Controls: the same child calling without its identity (arriving `Direct`) gets
//! `ckbus_principal_direct` for both ops and nothing else; a caller whose module has no
//! live generation gets `ckbus_generation_not_live`; ck-bus's log records the principal
//! it observed for every answer. `ckbus` is declared `reserved: true`, so a hand-started
//! impostor cannot register as `ckbus` to receive these calls; the declaration row
//! asserts the refused HELLO (`tests/module_declaration.rs`), and this row checks the
//! declaration it relies on.
//!
//! `spawn-stream-unlanded` (C) is not recorded: `supervisor.spawn_snapshot` answers in
//! this run.

#[allow(dead_code)]
#[path = "../src/bootstrap/mod.rs"]
mod bootstrap;
#[allow(dead_code)]
#[path = "../src/credentials/mod.rs"]
mod credentials;
#[allow(dead_code)]
#[path = "../src/grants/mod.rs"]
mod grants;
#[allow(dead_code)]
mod harness;
#[allow(dead_code)]
#[path = "../src/issuance/mod.rs"]
mod issuance;
#[allow(dead_code)]
#[path = "../src/membership/mod.rs"]
mod membership;
#[allow(dead_code)]
#[path = "../src/runtime/seams.rs"]
mod runtime;

use std::{collections::BTreeSet, path::Path, sync::Arc, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    issuance::{self as rows, VerdictClient, PARTICIPANT},
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::nats_server_bin,
        run::{ClaustrumSide, RunOptions, SignerRun},
        SIGNER_OPERATIONS,
    },
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_protocol::Principal;

const BOOT_LIMIT: Duration = Duration::from_secs(60);

/// Runs only when the daemon starts this executable as the participant.
#[test]
fn participant_child() {
    rows::participant_child_entry();
}

fn vocabulary() -> BTreeSet<String> {
    SIGNER_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn fresh_machine_id() -> String {
    let seed = format!("{:?}{}", std::time::SystemTime::now(), std::process::id());
    Sha256::digest(seed.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

struct Plane {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    ready: Value,
}

async fn start() -> Option<Plane> {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::CredentialDelivery, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary());
            return None;
        }
    };
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let run = SignerRun::start_with(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        ClaustrumSide::Signer(trust.signer.clone()),
        RunOptions {
            ckbus_env: server.ckbus_env(),
            machine_id: Some(fresh_machine_id()),
        },
    )
    .await;
    let ready = bus::wait_event(run.root.path(), "ckbus.bootstrap.ready", 1, BOOT_LIMIT).await;
    rows::register_participant(&run).await;
    Some(Plane {
        trust,
        server,
        run,
        ready,
    })
}

/// A nonce signer that relays every connect nonce to `ckbus.nonce_sign` as the
/// participant.
fn relayed_signer(
    connection_file: std::path::PathBuf,
    credential_public: String,
) -> rows::NonceSigner {
    Arc::new(move |nonce: Vec<u8>| {
        let connection_file = connection_file.clone();
        let credential_public = credential_public.clone();
        Box::pin(async move {
            let reply = rows::relay(
                &connection_file,
                true,
                issuance::NONCE_SIGN_OP,
                json!({"nonce_b64": STANDARD.encode(&nonce), "credential_public": credential_public}),
            )
            .await
            .map_err(|(code, message)| format!("{code}: {message}"))?;
            STANDARD
                .decode(reply["signature_b64"].as_str().unwrap_or_default())
                .map_err(|error| error.to_string())
        })
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_attested_participant_gets_its_credential_and_connects_through_nonce_sign() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start().await else {
        return;
    };
    let connection_file = plane.run.connection_file.clone();
    let generation = rows::live_generation(&connection_file, PARTICIPANT)
        .await
        .expect("the participant is live");

    // The request claims to be ck-bus; the answer is for the attested participant.
    let answer = rows::relay(
        &connection_file,
        true,
        issuance::CREDENTIAL_OP,
        json!({"module_id": "ckbus"}),
    )
    .await
    .unwrap_or_else(|(code, message)| panic!("ckbus.credential refused: {code} {message}"));
    let credential_public = answer["credential_public"].as_str().unwrap().to_string();
    assert!(credential_public.starts_with('U'), "{answer}");
    assert_ne!(
        credential_public,
        plane.ready["box_user"].as_str().unwrap(),
        "the participant's key is fresh, not ck-bus's own user"
    );
    assert_eq!(answer["acct"], plane.ready["acct"]);
    assert_eq!(answer["account_public"], plane.ready["account_public"]);
    assert_eq!(
        answer["inbox_prefix"],
        format!("_INBOX.{credential_public}")
    );
    assert_eq!(answer["server_url"], plane.server.url);
    assert_eq!(answer["spawn_generation"], generation);
    assert_eq!(answer["credential_epoch"], 0);

    let jwt = answer["jwt"].as_str().unwrap();
    let claims = bus::claims(jwt);
    assert_eq!(claims["sub"], credential_public);
    assert_eq!(claims["jti"], answer["user_jwt_id"]);
    assert_eq!(
        claims["name"], PARTICIPANT,
        "the JWT is for the attested module id, not the id the body claimed"
    );
    assert_eq!(
        claims["nats"]["issuer_account"],
        plane.ready["account_public"]
    );
    assert_eq!(
        claims["iss"],
        plane.trust.box_root_public(),
        "the harness signer's box account root signed it"
    );
    let names = grants::derive_account(plane.ready["acct"].as_str().unwrap()).unwrap();
    let expected = grants::participant_grant(&names, &credential_public, &[])
        .unwrap()
        .jwt_permissions();
    assert_eq!(claims["nats"]["pub"], expected["pub"], "publish grant");
    assert_eq!(claims["nats"]["sub"], expected["sub"], "subscribe grant");

    // The participant connects, each nonce signed by ck-bus over its subc route.
    let client = VerdictClient::connect(
        &plane.server.url,
        jwt,
        relayed_signer(connection_file.clone(), credential_public.clone()),
        Some(format!("_INBOX.{credential_public}")),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "the server refused the delivered credential: {error}\n{}",
            plane.server.log_text()
        )
    });
    let dead = names.effect_dead();
    client.publish(&dead, b"delivered").await;
    client.expect_allowed(&dead).await;
    let observer = bus::box_client(
        &plane.trust,
        &plane.server,
        plane.ready["account_public"].as_str().unwrap(),
    )
    .await;
    let mut stream = async_nats::jetstream::new(observer)
        .get_stream(names.streams().effect_dead.clone())
        .await
        .expect("the dead-letter stream exists");
    let stored = stream.info().await.expect("stream info").state.messages;
    assert_eq!(stored, 1, "the participant's publish was stored");

    // Direct: the same child, without its identity, gets the named refusal only.
    for (op, params) in [
        (issuance::CREDENTIAL_OP, json!({"module_id": PARTICIPANT})),
        (
            issuance::NONCE_SIGN_OP,
            json!({"nonce_b64": STANDARD.encode(b"nonce")}),
        ),
    ] {
        let refused = rows::relay(&connection_file, false, op, params).await;
        let (code, message) = refused.expect_err("a Direct caller is never answered");
        assert_eq!(code, issuance::code::PRINCIPAL_DIRECT, "{op}: {message}");
        assert!(message.contains("direct"), "{op}: {message}");
    }

    // ck-bus's log records the principal it observed for every answer.
    let answers = bus::events(plane.run.root.path(), "ckbus.issuance.answer");
    let credential_answers: Vec<_> = answers
        .iter()
        .filter(|event| event["op"] == issuance::CREDENTIAL_OP)
        .collect();
    assert!(credential_answers.iter().any(|event| event["principal"]
        == format!("reserved:{PARTICIPANT}")
        && event["outcome"] == "answered"
        && event["claimed_module_id"] == "ckbus"));
    assert!(credential_answers
        .iter()
        .any(|event| event["principal"] == "direct"
            && event["outcome"] == issuance::code::PRINCIPAL_DIRECT));
    assert!(answers
        .iter()
        .all(|event| event["principal"].as_str().is_some() && event["outcome"].is_string()));
    assert!(answers
        .iter()
        .any(|event| event["op"] == issuance::NONCE_SIGN_OP
            && event["principal"] == format!("reserved:{PARTICIPANT}")
            && event["outcome"] == "answered"));

    // The impostor control lives in the declaration row; the declaration it rests on:
    let config: Value =
        serde_json::from_slice(&std::fs::read(&plane.run.config_file).unwrap()).unwrap();
    assert_eq!(config["modules"]["ckbus"]["reserved"], true);
    assert_eq!(config["modules"]["ckbus"]["protocol"], "subc");

    drop(client);
    RowReport::passed(Row::CredentialDelivery)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary());
    plane.server.stop().await;
    plane.run.shutdown().await;
}

/// No live generation: the generation check itself, driven through ck-bus's issuance
/// handler against the acceptance daemon's real spawn snapshot. No supervised module can
/// present an attested identity once its process has no live generation (its
/// connection, and so its route, dies with it), so this control uses the handler seam
/// with an attested id the snapshot does not show.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_module_with_no_live_generation_is_refused() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(plane) = start().await else {
        return;
    };
    struct NoPlane;
    impl issuance::PlaneSource for NoPlane {
        fn current(&self) -> Option<issuance::Plane> {
            None
        }
    }
    let credentials = Arc::new(credentials::Credentials::new(Arc::new(
        credentials::vault::ClaustrumRoute::new(plane.run.connection_file.clone(), None),
    )));
    let store = tempfile::tempdir().unwrap();
    let issuing = Arc::new(issuance::Issuance::new(
        credentials,
        store.path(),
        Arc::new(issuance::handler::SpawnSnapshotGenerations::new(
            plane.run.connection_file.clone(),
        )),
        Arc::new(NoPlane),
    ));
    let handler = issuance::handler::IssuanceHandler::new(NoInner, issuing);
    for (op, module_id) in [
        (issuance::CREDENTIAL_OP, "ghost"),
        (issuance::NONCE_SIGN_OP, "ghost"),
    ] {
        let principal = Principal::Reserved {
            module_id: module_id.to_string(),
        };
        let body = serde_json::to_vec(&json!({
            "method": op,
            "params": {"nonce_b64": STANDARD.encode(b"nonce")},
        }))
        .unwrap();
        let outcome = handler
            .answer(Some(&principal), &body)
            .await
            .expect("an issuance op is answered by issuance");
        match outcome {
            subc_client_rs::HandlerOutcome::Error { code, message } => {
                assert_eq!(code, issuance::code::GENERATION_NOT_LIVE, "{op}: {message}")
            }
            other => panic!("{op}: a module with no live generation was answered: {other:?}"),
        }
    }
    // The same seam answers the live participant past the fence (it stops only at the
    // missing plane), so the refusal above is the generation check and not a blanket
    // refusal.
    let principal = Principal::Reserved {
        module_id: PARTICIPANT.to_string(),
    };
    let body = serde_json::to_vec(&json!({"method": issuance::CREDENTIAL_OP})).unwrap();
    match handler.answer(Some(&principal), &body).await {
        Some(subc_client_rs::HandlerOutcome::Error { code, .. }) => {
            assert_eq!(code, issuance::code::NOT_READY)
        }
        other => panic!("unexpected answer for the live participant: {other:?}"),
    }
    RowReport::passed(Row::CredentialDelivery)
        .served_by(ServedBy::HarnessSigner)
        .emit(&vocabulary());
    plane.server.stop().await;
    plane.run.shutdown().await;
}

struct NoInner;

#[async_trait::async_trait]
impl subc_client_rs::ModuleHandler for NoInner {
    async fn handle(
        &self,
        _ctx: subc_client_rs::RequestCtx,
        _body: Vec<u8>,
    ) -> subc_client_rs::HandlerOutcome {
        subc_client_rs::HandlerOutcome::Error {
            code: "not_issuance".to_string(),
            message: "not an issuance op".to_string(),
        }
    }
}
