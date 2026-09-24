//! Ladder row "Membership lifecycle" (slice 10 of `docs/specs/ck-bus-module.md`), under
//! ruling R15: prefrontal owns agent residence and drives each agent's durables through
//! ck-bus's agent-durable ops. served-by: harness-signer (the box account root that
//! signs every issued credential), against a real nats-server and the acceptance daemon.
//!
//! Arms:
//! - every agent-durable op refuses any caller but `reserved:prefrontal-core`, a `Direct`
//!   one included, with `ckbus_caller_not_permitted`;
//! - bind is idempotent, and a durable with a different configuration is refused by
//!   name and left as it is; no consumer but an agent's `c_{agent_id}` sits on an agent
//!   stream, and none that ck-bus owns is placed on one;
//! - delete purges the agent's undelivered messages, and deleting an absent durable
//!   succeeds;
//! - the list reports each durable's undelivered count;
//! - the effect-pending read counts undelivered intents, so a dead-lettered one (kept
//!   in flight by the server) never holds a merge back;
//! - a participant credential issued before any bind pulls from a durable bound
//!   afterwards, with no reissue, while a `Direct` principal still gets no credential.
//!
//! Merge is prefrontal's (it needs a workload publish, which ck-bus never holds), so no
//! arm here merges. Rooms are not in R15's scope and record
//! `membership-contract-unpinned`.
//!
//! The daemon runs this executable twice as supervised modules, once as `participant`
//! and once as `prefrontal-core` (see `harness::issuance`), so each reaches ck-bus with
//! the principal the daemon stamps for a launch-nonce-attested route.

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

use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use async_nats::jetstream::{self, consumer::pull, AckKind};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use cortexkit_bus_naming::{shipped_streams, AccountNames};
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    control,
    issuance::{self as rows, VerdictClient, CHILD_TEST},
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::nats_server_bin,
        run::{ClaustrumSide, RunOptions, SignerRun},
        SIGNER_OPERATIONS,
    },
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::consumer::{CallOptions, ConsumerOptions, SubcConsumer};
use subc_control::ClientControlRequest;
use subc_protocol::{BindIdentity, Principal, RouteTarget};

const BOOT_LIMIT: Duration = Duration::from_secs(60);
const PARTICIPANT: &str = rows::PARTICIPANT;
const PREFRONTAL: &str = membership::AUTHORIZED_MODULE;
const RELAY_OP: &str = "participant.relay";

/// Runs only when the daemon starts this executable as a supervised module.
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

fn passed() {
    RowReport::passed(Row::Membership)
        .served_by(ServedBy::HarnessSigner)
        .emit(&vocabulary());
}

fn fresh_machine_id() -> String {
    let seed = format!("{:?}{}", std::time::SystemTime::now(), std::process::id());
    Sha256::digest(seed.as_bytes())[..16]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ---------------------------------------------------------------------------------
// Arms with no server.
// ---------------------------------------------------------------------------------

/// The pull grant is a whole-token `*` on each agent stream, so any consumer there is
/// readable by every participant. This holds ck-bus's own consumers off those streams
/// and keeps the durables bind creates on exactly them.
#[test]
fn ckbus_owned_durables_stay_off_the_agent_streams() {
    let names = grants::derive_account("box_membershipinvariant").unwrap();
    let agent_streams: Vec<String> = names
        .streams()
        .agent_streams()
        .iter()
        .map(|stream| stream.to_string())
        .collect();
    assert_eq!(membership::agent_streams(&names), agent_streams);
    let kinds: Vec<String> = membership::AGENT_STREAM_KINDS
        .iter()
        .map(|kind| membership::stream_name(&names, *kind))
        .collect();
    assert_eq!(
        kinds, agent_streams,
        "the kinds name the naming crate's list"
    );

    // Bootstrap creates the shipped streams; every agent stream is one of them.
    let bootstrap: BTreeSet<String> = shipped_streams(&names)
        .into_iter()
        .map(|spec| spec.name)
        .collect();
    for stream in &agent_streams {
        assert!(
            bootstrap.contains(stream),
            "{stream} is created by bootstrap"
        );
    }

    for (kind, durable) in membership::CKBUS_OWNED_DURABLES {
        let stream = membership::stream_name(&names, kind);
        assert!(
            bootstrap.contains(&stream),
            "{durable} sits on a bootstrap stream"
        );
        assert!(
            !agent_streams.contains(&stream),
            "ck-bus-owned durable {durable} is on agent stream {stream}, which every \
             participant may pull"
        );
    }

    let planned = membership::agent_durables(&names, "agent_invariant").unwrap();
    assert_eq!(
        planned
            .iter()
            .map(|durable| durable.stream.clone())
            .collect::<Vec<_>>(),
        agent_streams
    );
    for durable in &planned {
        assert_eq!(durable.durable, "c_agent_invariant");
    }
}

struct NoPlane;

impl issuance::PlaneSource for NoPlane {
    fn current(&self) -> Option<issuance::Plane> {
        None
    }
}

/// In process: the authorization check is the gate, not a side effect of an unready
/// plane. Every other principal is refused by name; prefrontal-core gets past it and
/// meets `ckbus_not_ready`.
#[tokio::test]
async fn only_prefrontal_core_passes_the_membership_gate() {
    let membership = membership::Membership::new(Arc::new(NoPlane));
    let refused = [
        None,
        Some(Principal::Direct),
        Some(Principal::Unverified),
        Some(Principal::Reserved {
            module_id: PARTICIPANT.to_string(),
        }),
        Some(Principal::Reserved {
            module_id: "prefrontal-core-shadow".to_string(),
        }),
    ];
    let prefrontal = Principal::Reserved {
        module_id: PREFRONTAL.to_string(),
    };
    for (op, _) in membership::OPERATIONS {
        let params = json!({"agent_id": "agent_gate"});
        for principal in &refused {
            let refusal = membership
                .answer(principal.as_ref(), op, &params)
                .await
                .expect("a membership op is answered")
                .expect_err("refused");
            assert_eq!(
                refusal.code,
                membership::code::CALLER_NOT_PERMITTED,
                "{op} as {principal:?}: {}",
                refusal.message
            );
        }
        let past_the_gate = membership
            .answer(Some(&prefrontal), op, &params)
            .await
            .expect("a membership op is answered")
            .expect_err("no plane yet");
        assert_eq!(past_the_gate.code, issuance::code::NOT_READY, "{op}");
    }
    assert!(membership
        .answer(Some(&prefrontal), issuance::CREDENTIAL_OP, &json!({}))
        .await
        .is_none());
}

#[test]
fn issuance_picks_the_grant_by_attested_module() {
    let names = grants::derive_account("box_membershipgrant").unwrap();
    let user = nkeys::KeyPair::new_user().public_key();
    let authority = grants::issued_grant(&names, PREFRONTAL, &user, &[]).unwrap();
    assert_eq!(authority.role(), grants::GrantRole::DeliveryAuthority);
    assert!(authority.publish_allow().contains(&names.wake_binding()));
    let participant = grants::issued_grant(&names, PARTICIPANT, &user, &[]).unwrap();
    assert_eq!(participant.role(), grants::GrantRole::Participant);
    assert_eq!(
        participant,
        grants::participant_grant(&names, &user, &[]).unwrap()
    );
    for stream in names.streams().agent_streams() {
        assert!(participant
            .publish_allow()
            .contains(&format!("$JS.API.CONSUMER.MSG.NEXT.{stream}.*")));
    }
    assert!(!participant.publish_allow().contains(&names.wake_binding()));
}

/// Rooms stay behind the foundation's membership contract.
#[test]
fn rooms_record_membership_contract_unpinned() {
    RowReport::skipped(
        Row::Membership,
        "membership-contract-unpinned",
        "R15 moves agent durables to prefrontal; room membership (re-issue, the room \
         consumer's filter_subjects, revocation within 10 s) waits for the foundation's \
         membership op, caller and event, which the vendored copy does not quote",
    )
    .served_by(ServedBy::HarnessSigner)
    .emit(&vocabulary());
}

// ---------------------------------------------------------------------------------
// Live server and daemon.
// ---------------------------------------------------------------------------------

struct Live {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    names: AccountNames,
    account_public: String,
}

impl Live {
    fn connection_file(&self) -> &Path {
        &self.run.connection_file
    }

    async fn observer(&self) -> jetstream::Context {
        jetstream::new(bus::box_client(&self.trust, &self.server, &self.account_public).await)
    }

    /// Calls a membership op as the attested prefrontal-core, panicking on a refusal.
    async fn prefrontal(&self, method: &str, params: Value) -> Value {
        relay_as(self.connection_file(), PREFRONTAL, true, method, params)
            .await
            .unwrap_or_else(|(code, message)| panic!("{method} refused: {code} {message}"))
    }

    async fn stop(self) {
        self.server.stop().await;
        self.run.shutdown().await;
    }
}

async fn start(modules: &[&str]) -> Option<Live> {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::Membership, gate, observation)
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
    for module in modules {
        register_module(&run, module).await;
    }
    Some(Live {
        names: grants::derive_account(ready["acct"].as_str().unwrap()).unwrap(),
        account_public: ready["account_public"].as_str().unwrap().to_string(),
        trust,
        server,
        run,
    })
}

/// Declares this executable as supervised module `module_id` and has the daemon start
/// it, as `harness::issuance::register_participant` does for `participant`.
async fn register_module(run: &SignerRun, module_id: &str) {
    let exe = std::env::current_exe().expect("the row's own executable");
    let mut value: Value =
        serde_json::from_slice(&fs::read(&run.config_file).expect("rendered config"))
            .expect("rendered config is JSON");
    value["modules"][module_id] = json!({
        "program": "/bin/sh",
        "args": [
            "-c",
            format!("CKBUS_PARTICIPANT_ARGV=\"$*\" exec \"$0\" --exact {CHILD_TEST} --nocapture --test-threads=1"),
            exe.display().to_string(),
        ],
        "enabled": true,
        "protocol": "subc",
        "drain_timeout_ms": 2000,
    });
    fs::write(
        &run.config_file,
        serde_json::to_vec_pretty(&value).expect("config encodes"),
    )
    .expect("rendered config writable");
    if let control::ControlReply::Error(error) = control::rpc(
        &run.connection_file,
        ClientControlRequest::SupervisorRescan { preview: false },
    )
    .await
    {
        panic!(
            "supervisor.rescan refused: {} {}",
            error.code, error.message
        );
    }
    run.wait_for_catalog_id(module_id).await;
    // The catalog can list the module before it serves; a relay answer proves it does.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match relay_raw(&run.connection_file, module_id, json!({"ping": true})).await {
            Ok(_) => return,
            Err(error) => assert!(
                Instant::now() < deadline,
                "{module_id} relay never answered: {error}"
            ),
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

type CkbusReply = Result<Value, (String, String)>;

/// Has supervised module `module_id` call ck-bus's `method`, attested or `Direct`.
async fn relay_as(
    connection_file: &Path,
    module_id: &str,
    attest: bool,
    method: &str,
    params: Value,
) -> CkbusReply {
    let reply = relay_raw(
        connection_file,
        module_id,
        json!({"attest": attest, "body": {"method": method, "params": params}}),
    )
    .await
    .unwrap_or_else(|error| panic!("the {module_id} relay failed: {error}"));
    if let Some(result) = reply.get("ok") {
        return Ok(result["result"].clone());
    }
    let error = &reply["error"];
    Err((
        error["code"].as_str().unwrap_or_default().to_string(),
        error["message"].as_str().unwrap_or_default().to_string(),
    ))
}

async fn relay_raw(
    connection_file: &Path,
    module_id: &str,
    params: Value,
) -> Result<Value, String> {
    let consumer = SubcConsumer::connect(connection_file, ConsumerOptions::default())
        .await
        .map_err(|error| error.to_string())?;
    let body = serde_json::to_vec(&json!({"method": RELAY_OP, "params": params})).unwrap();
    let reply = consumer
        .call(
            RouteTarget::ManagementSurface {
                module_id: module_id.to_string(),
            },
            BindIdentity::new(
                connection_file
                    .parent()
                    .map(PathBuf::from)
                    .unwrap_or_default(),
                "ck-bus-membership-row",
                "relay",
            ),
            body,
            CallOptions {
                timeout: Duration::from_secs(20),
                ..CallOptions::default()
            },
        )
        .await
        .map_err(|error| error.to_string());
    consumer.close().await;
    serde_json::from_slice(&reply?).map_err(|error| error.to_string())
}

/// Publishes through JetStream and waits for the stream's ack, so the message is stored
/// before the call returns.
async fn stored_publish(observer: &jetstream::Context, subject: String, payload: &'static [u8]) {
    observer
        .publish(subject.clone(), payload.into())
        .await
        .unwrap_or_else(|error| panic!("publish {subject}: {error}"))
        .await
        .unwrap_or_else(|error| panic!("{subject} not stored: {error}"));
}

async fn consumer_info(
    observer: &jetstream::Context,
    stream: &str,
    durable: &str,
) -> jetstream::consumer::Info {
    let mut consumer: jetstream::consumer::Consumer<pull::Config> = observer
        .get_consumer_from_stream(durable, stream)
        .await
        .unwrap_or_else(|error| panic!("consumer {durable} on {stream}: {error}"));
    consumer.info().await.expect("consumer info").clone()
}

async fn consumers_on(observer: &jetstream::Context, stream: &str) -> BTreeSet<String> {
    let stream_handle = observer
        .get_stream(stream.to_string())
        .await
        .expect("stream exists");
    let mut names = stream_handle.consumer_names();
    let mut listed = BTreeSet::new();
    while let Some(name) = names.next().await {
        listed.insert(name.expect("consumer name"));
    }
    listed
}

/// Polls `read` until `done` holds, failing with the last value after `limit`.
async fn wait_for<F, Fut>(
    what: &str,
    limit: Duration,
    mut read: F,
    done: impl Fn(&Value) -> bool,
) -> Value
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Value>,
{
    let deadline = Instant::now() + limit;
    loop {
        let value = read().await;
        if done(&value) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "{what} never held; last: {value}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn callers_other_than_prefrontal_core_are_refused_through_the_daemon() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(live) = start(&[PARTICIPANT, PREFRONTAL]).await else {
        return;
    };
    let cases = [
        (PARTICIPANT, true),
        (PARTICIPANT, false),
        (PREFRONTAL, false),
    ];
    for (op, _) in membership::OPERATIONS {
        for (module, attest) in cases {
            let (code, message) = relay_as(
                live.connection_file(),
                module,
                attest,
                op,
                json!({"agent_id": "agent_refused"}),
            )
            .await
            .expect_err("refused");
            assert_eq!(
                code,
                membership::code::CALLER_NOT_PERMITTED,
                "{op} from {module} (attested: {attest}): {message}"
            );
        }
    }
    // Nothing was bound on the way.
    let observer = live.observer().await;
    for stream in membership::agent_streams(&live.names) {
        assert!(
            consumers_on(&observer, &stream).await.is_empty(),
            "{stream}"
        );
    }
    // ck-bus's log names the principal it observed for each refusal.
    let answers = bus::events(live.run.root.path(), "ckbus.membership.answer");
    for principal in [format!("reserved:{PARTICIPANT}"), "direct".to_string()] {
        assert!(
            answers.iter().any(|event| event["principal"] == principal
                && event["outcome"] == membership::code::CALLER_NOT_PERMITTED),
            "no refusal logged for {principal}: {answers:?}"
        );
    }
    passed();
    live.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bind_is_idempotent_and_a_different_configuration_is_refused_and_kept() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(live) = start(&[PREFRONTAL]).await else {
        return;
    };
    let observer = live.observer().await;
    let agent = "agent_bind_a";

    let first = live
        .prefrontal(membership::BIND_OP, json!({"agent_id": agent}))
        .await;
    let second = live
        .prefrontal(membership::BIND_OP, json!({"agent_id": agent}))
        .await;
    let planned = membership::agent_durables(&live.names, agent).unwrap();
    for (reply, created) in [(&first, true), (&second, false)] {
        assert_eq!(reply["agent_id"], agent);
        let durables = reply["durables"].as_array().expect("durables");
        assert_eq!(durables.len(), planned.len(), "{reply}");
        for (row, plan) in durables.iter().zip(&planned) {
            assert_eq!(row["stream"], plan.stream);
            assert_eq!(row["durable"], plan.durable);
            assert_eq!(row["filter_subject"], plan.filter_subjects[0]);
            assert_eq!(row["created"], created, "{reply}");
        }
    }
    for plan in &planned {
        let info = consumer_info(&observer, &plan.stream, &plan.durable).await;
        // The server reports one filter either as `filter_subject` or as a one-entry
        // `filter_subjects`, depending on how it was created.
        let filters = if info.config.filter_subjects.is_empty() {
            vec![info.config.filter_subject.clone()]
        } else {
            info.config.filter_subjects.clone()
        };
        assert_eq!(filters, plan.filter_subjects);
        assert_eq!(info.config.max_deliver, plan.max_deliver);
        assert_eq!(info.config.ack_wait, plan.ack_wait);
        assert_eq!(info.config.max_ack_pending, plan.max_ack_pending);
    }

    // A durable of the same name with a different max_deliver: refused by name, kept.
    let conflicted = "agent_bind_b";
    let peer_plan = membership::agent_durables(&live.names, conflicted)
        .unwrap()
        .into_iter()
        .find(|plan| plan.stream == live.names.streams().peer)
        .unwrap();
    observer
        .create_consumer_on_stream(
            pull::Config {
                durable_name: Some(peer_plan.durable.clone()),
                filter_subject: peer_plan.filter_subjects[0].clone(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ack_wait: peer_plan.ack_wait,
                max_deliver: 3,
                max_ack_pending: peer_plan.max_ack_pending,
                ..Default::default()
            },
            peer_plan.stream.clone(),
        )
        .await
        .expect("the harness creates a differing durable");
    let (code, message) = relay_as(
        live.connection_file(),
        PREFRONTAL,
        true,
        membership::BIND_OP,
        json!({"agent_id": conflicted}),
    )
    .await
    .expect_err("a differing durable is refused");
    assert_eq!(code, membership::code::DURABLE_CONFLICT, "{message}");
    assert!(message.contains(&peer_plan.stream), "{message}");
    assert!(message.contains("max_deliver is 3"), "{message}");
    let kept = consumer_info(&observer, &peer_plan.stream, &peer_plan.durable).await;
    assert_eq!(
        kept.config.max_deliver, 3,
        "the differing durable was not replaced"
    );

    // Only agent durables sit on the agent streams.
    for stream in membership::agent_streams(&live.names) {
        for durable in consumers_on(&observer, &stream).await {
            assert!(
                durable == AccountNames::consumer_name(agent).unwrap()
                    || durable == AccountNames::consumer_name(conflicted).unwrap(),
                "{durable} on agent stream {stream} is not an agent durable"
            );
        }
    }
    passed();
    live.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn delete_drops_undelivered_messages_and_an_absent_durable_succeeds() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(live) = start(&[PREFRONTAL]).await else {
        return;
    };
    let observer = live.observer().await;
    let agent = "agent_delete_a";
    let bystander = "agent_delete_b";
    for bound in [agent, bystander] {
        live.prefrontal(membership::BIND_OP, json!({"agent_id": bound}))
            .await;
    }
    for _ in 0..2 {
        stored_publish(&observer, live.names.wake_fire(agent).unwrap(), b"wake").await;
    }
    stored_publish(
        &observer,
        live.names.peer_delivery(agent, "sess_delete").unwrap(),
        b"peer",
    )
    .await;
    stored_publish(&observer, live.names.wake_fire(bystander).unwrap(), b"kept").await;
    let streams = live.names.streams().clone();
    let durable = AccountNames::consumer_name(agent).unwrap();
    assert_eq!(
        consumer_info(&observer, &streams.wake, &durable)
            .await
            .num_pending,
        2
    );

    let deleted = live
        .prefrontal(membership::DELETE_OP, json!({"agent_id": agent}))
        .await;
    assert_eq!(
        deleted["deleted"],
        json!(membership::agent_streams(&live.names)),
        "{deleted}"
    );
    assert_eq!(deleted["purged"][&streams.wake], 2, "{deleted}");
    assert_eq!(deleted["purged"][&streams.peer], 1, "{deleted}");
    assert_eq!(deleted["purged"][&streams.effect], 0, "{deleted}");
    for stream in membership::agent_streams(&live.names) {
        assert!(!consumers_on(&observer, &stream).await.contains(&durable));
    }

    // Rebound, the agent's durable starts empty: the purge dropped the messages rather
    // than leaving them for the next bind to deliver.
    live.prefrontal(membership::BIND_OP, json!({"agent_id": agent}))
        .await;
    for stream in [&streams.wake, &streams.peer] {
        assert_eq!(
            consumer_info(&observer, stream, &durable).await.num_pending,
            0,
            "{stream}"
        );
    }
    // The purge was the agent's subject only.
    let bystander_durable = AccountNames::consumer_name(bystander).unwrap();
    assert_eq!(
        consumer_info(&observer, &streams.wake, &bystander_durable)
            .await
            .num_pending,
        1
    );

    // Deleting a durable that does not exist succeeds and reports nothing deleted.
    let absent = live
        .prefrontal(
            membership::DELETE_OP,
            json!({"agent_id": "agent_never_bound"}),
        )
        .await;
    assert_eq!(absent["deleted"], json!([]), "{absent}");
    passed();
    live.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_list_reports_each_durables_undelivered_count() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(live) = start(&[PREFRONTAL]).await else {
        return;
    };
    let observer = live.observer().await;
    let (a, b) = ("agent_list_a", "agent_list_b");
    for bound in [a, b] {
        live.prefrontal(membership::BIND_OP, json!({"agent_id": bound}))
            .await;
    }
    for _ in 0..3 {
        stored_publish(&observer, live.names.wake_fire(a).unwrap(), b"wake").await;
    }
    stored_publish(
        &observer,
        live.names.peer_delivery(b, "sess_list").unwrap(),
        b"peer",
    )
    .await;
    let streams = live.names.streams().clone();
    let pending = |rows: &Value, agent: &str, stream: &str| -> Option<u64> {
        rows.as_array()?
            .iter()
            .find(|row| row["agent_id"] == agent && row["stream"] == stream)?["pending"]
            .as_u64()
    };

    let rows = live.prefrontal(membership::LIST_OP, json!({})).await;
    assert_eq!(rows.as_array().map(Vec::len), Some(6), "{rows}");
    for row in rows.as_array().unwrap() {
        let agent = row["agent_id"]
            .as_str()
            .expect("every row is an agent durable");
        let plan = membership::agent_durables(&live.names, agent)
            .unwrap()
            .into_iter()
            .find(|plan| plan.stream == row["stream"])
            .expect("a planned stream");
        assert_eq!(row["durable"], plan.durable);
        assert_eq!(row["filter_subject"], plan.filter_subjects[0]);
    }
    assert_eq!(pending(&rows, a, &streams.wake), Some(3), "{rows}");
    assert_eq!(pending(&rows, a, &streams.peer), Some(0), "{rows}");
    assert_eq!(pending(&rows, b, &streams.peer), Some(1), "{rows}");
    assert_eq!(pending(&rows, b, &streams.wake), Some(0), "{rows}");

    // A delivery (unacked) leaves the undelivered count, so pending falls to 2.
    let consumer: jetstream::consumer::Consumer<pull::Config> = observer
        .get_consumer_from_stream(
            AccountNames::consumer_name(a).unwrap(),
            streams.wake.clone(),
        )
        .await
        .unwrap();
    let _held = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(3))
        .messages()
        .await
        .unwrap()
        .next()
        .await
        .expect("a wake")
        .expect("a delivered message");
    let wake = streams.wake.clone();
    wait_for(
        "the list's pending falls to 2 after one delivery",
        Duration::from_secs(5),
        || live.prefrontal(membership::LIST_OP, json!({})),
        |rows| pending(rows, a, &wake) == Some(2),
    )
    .await;
    passed();
    live.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dead_lettered_intent_is_not_counted_as_effects_pending() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(live) = start(&[PREFRONTAL]).await else {
        return;
    };
    let observer = live.observer().await;
    let agent = "agent_effect_a";
    let params = json!({"agent_id": agent});

    let unbound = live
        .prefrontal(membership::EFFECTS_PENDING_OP, params.clone())
        .await;
    assert_eq!(unbound["bound"], false, "{unbound}");
    assert_eq!(unbound["pending"], 0, "{unbound}");

    live.prefrontal(membership::BIND_OP, params.clone()).await;
    stored_publish(
        &observer,
        live.names.effect_intent(agent, "sess_poison").unwrap(),
        b"poisoned",
    )
    .await;
    let queued = live
        .prefrontal(membership::EFFECTS_PENDING_OP, params.clone())
        .await;
    assert_eq!(queued["undelivered"], 1, "{queued}");
    assert_eq!(
        queued["pending"], 1,
        "the counter sees a deliverable intent: {queued}"
    );

    // The intent is refused on every delivery until max_deliver (5) is exhausted.
    let consumer: jetstream::consumer::Consumer<pull::Config> = observer
        .get_consumer_from_stream(
            AccountNames::consumer_name(agent).unwrap(),
            live.names.streams().effect.clone(),
        )
        .await
        .unwrap();
    for delivery in 1..=membership::EFFECT_MAX_DELIVER {
        let message = consumer
            .fetch()
            .max_messages(1)
            .expires(Duration::from_secs(5))
            .messages()
            .await
            .unwrap()
            .next()
            .await
            .unwrap_or_else(|| panic!("delivery {delivery} arrives"))
            .expect("a delivered message");
        assert_eq!(message.info().unwrap().delivered, delivery);
        if delivery == 1 {
            // Delivered and unacked: reported in flight, and no longer undelivered.
            let in_flight = live
                .prefrontal(membership::EFFECTS_PENDING_OP, params.clone())
                .await;
            assert_eq!(in_flight["in_flight"], 1, "{in_flight}");
            assert_eq!(in_flight["undelivered"], 0, "{in_flight}");
        }
        message
            .ack_with(AckKind::Nak(None))
            .await
            .expect("nak sent");
    }
    // The server keeps an exhausted intent among the delivered-and-unacked (in flight)
    // until its ack wait passes, so the read must not count in-flight intents: this one
    // would otherwise hold a merge back for as long as it sits there.
    let exhausted = live
        .prefrontal(membership::EFFECTS_PENDING_OP, params.clone())
        .await;
    assert_eq!(exhausted["undelivered"], 0, "{exhausted}");
    assert_eq!(
        exhausted["pending"], 0,
        "a dead-lettered intent is not pending: {exhausted}"
    );
    let redelivered = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(2))
        .messages()
        .await
        .unwrap()
        .next()
        .await;
    assert!(
        redelivered.is_none(),
        "the exhausted intent is never delivered again"
    );

    // A new intent is counted again: the read is live, not stuck at zero.
    stored_publish(
        &observer,
        live.names.effect_intent(agent, "sess_fresh").unwrap(),
        b"fresh",
    )
    .await;
    let fresh = live
        .prefrontal(membership::EFFECTS_PENDING_OP, params)
        .await;
    assert_eq!(fresh["pending"], 1, "{fresh}");
    passed();
    live.stop().await;
}

/// A nonce signer relaying each connect nonce to `ckbus.nonce_sign` as `module_id`.
fn relayed_signer(
    connection_file: PathBuf,
    module_id: &'static str,
    credential_public: String,
) -> rows::NonceSigner {
    Arc::new(move |nonce: Vec<u8>| {
        let connection_file = connection_file.clone();
        let credential_public = credential_public.clone();
        Box::pin(async move {
            let reply = relay_as(
                &connection_file,
                module_id,
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
async fn a_credential_issued_before_a_bind_pulls_from_the_durable_bound_after() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(live) = start(&[PARTICIPANT, PREFRONTAL]).await else {
        return;
    };
    let connection_file = live.connection_file().to_path_buf();

    // A Direct principal still gets no credential.
    let (code, message) = relay_as(
        &connection_file,
        PARTICIPANT,
        false,
        issuance::CREDENTIAL_OP,
        json!({}),
    )
    .await
    .expect_err("a Direct caller is never issued");
    assert_eq!(code, issuance::code::PRINCIPAL_DIRECT, "{message}");

    // Both credentials are issued before any agent is bound.
    let participant = relay_as(
        &connection_file,
        PARTICIPANT,
        true,
        issuance::CREDENTIAL_OP,
        json!({}),
    )
    .await
    .unwrap_or_else(|(code, message)| panic!("participant credential: {code} {message}"));
    let authority = relay_as(
        &connection_file,
        PREFRONTAL,
        true,
        issuance::CREDENTIAL_OP,
        json!({}),
    )
    .await
    .unwrap_or_else(|(code, message)| panic!("prefrontal-core credential: {code} {message}"));
    let participant_public = participant["credential_public"]
        .as_str()
        .unwrap()
        .to_string();
    let authority_public = authority["credential_public"].as_str().unwrap().to_string();
    let participant_claims = bus::claims(participant["jwt"].as_str().unwrap());
    let authority_claims = bus::claims(authority["jwt"].as_str().unwrap());
    let expected = grants::participant_grant(&live.names, &participant_public, &[])
        .unwrap()
        .jwt_permissions();
    assert_eq!(participant_claims["nats"]["pub"], expected["pub"]);
    assert_eq!(participant_claims["nats"]["sub"], expected["sub"]);
    let expected = grants::delivery_authority_grant(&live.names, &authority_public, &[])
        .unwrap()
        .jwt_permissions();
    assert_eq!(authority_claims["nats"]["pub"], expected["pub"]);
    assert_eq!(authority_claims["nats"]["sub"], expected["sub"]);

    let participant_client = VerdictClient::connect(
        &live.server.url,
        participant["jwt"].as_str().unwrap(),
        relayed_signer(
            connection_file.clone(),
            PARTICIPANT,
            participant_public.clone(),
        ),
        Some(format!("_INBOX.{participant_public}")),
    )
    .await
    .unwrap_or_else(|error| panic!("participant connect: {error}\n{}", live.server.log_text()));
    let authority_client = VerdictClient::connect(
        &live.server.url,
        authority["jwt"].as_str().unwrap(),
        relayed_signer(
            connection_file.clone(),
            PREFRONTAL,
            authority_public.clone(),
        ),
        Some(format!("_INBOX.{authority_public}")),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "prefrontal-core connect: {error}\n{}",
            live.server.log_text()
        )
    });

    // Now prefrontal binds an agent and delivers it a wake under its own credential.
    let agent = "agent_after_issue";
    live.prefrontal(membership::BIND_OP, json!({"agent_id": agent}))
        .await;
    let fire = live.names.wake_fire(agent).unwrap();
    authority_client.publish(&fire, b"after issue").await;
    authority_client.expect_allowed(&fire).await;

    // The participant pulls and acks it with the credential it already held.
    let pjs = jetstream::new(participant_client.client.clone());
    let durable = AccountNames::consumer_name(agent).unwrap();
    let wake = live.names.streams().wake.clone();
    let consumer: jetstream::consumer::Consumer<pull::Config> = pjs
        .get_consumer_from_stream(durable.clone(), wake.clone())
        .await
        .unwrap_or_else(|error| {
            panic!(
                "info on the new durable: {error}; events {:?}",
                participant_client.events()
            )
        });
    let message = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(5))
        .messages()
        .await
        .expect("pull on the new durable")
        .next()
        .await
        .expect("the wake arrives")
        .expect("a delivered message");
    assert_eq!(message.payload.as_ref(), b"after issue");
    let ack = message.reply.clone().expect("an ack subject").to_string();
    message.ack().await.expect("ack sent");
    for subject in [
        ack,
        format!("$JS.API.CONSUMER.MSG.NEXT.{wake}.{durable}"),
        format!("$JS.API.CONSUMER.INFO.{wake}.{durable}"),
    ] {
        participant_client.expect_allowed(&subject).await;
    }
    let observer = live.observer().await;
    assert_eq!(
        consumer_info(&observer, &wake, &durable)
            .await
            .ack_floor
            .stream_sequence,
        1,
        "the participant's ack was applied"
    );
    // Still the credential issued before the bind: no reissue happened.
    let issued = bus::events(live.run.root.path(), "ckbus.issuance.issued");
    assert_eq!(
        issued
            .iter()
            .filter(|event| event["module_id"] == PARTICIPANT)
            .count(),
        1,
        "{issued:?}"
    );
    drop((participant_client, authority_client));
    RowReport::passed(Row::Membership)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary());
    live.stop().await;
}
