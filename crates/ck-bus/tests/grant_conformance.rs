//! Ladder row "A9 grant conformance" (slice 5 of `docs/specs/ck-bus-module.md`),
//! live-server half. served-by: harness-signer (every user JWT here is built by ck-bus's
//! own code with the generated grant and signed by the harness signer's roots), against
//! a real nats-server whose box account, census bucket and five streams a supervised
//! ck-bus created.
//!
//! Under generated grants on the live server:
//! - the per-process (participant) user, whose grant names no agent (spec ruling R15),
//!   may pull and ack any agent's durable on the agent streams, get and watch the census
//!   (the watch's ordered-consumer create, and a consumer delete, under
//!   `KV_CK_{ACCT}_CENSUS`), and publish to `ck.{acct}.effect.dead` and a bound room; it
//!   is refused server-side on every workload publish (wake, peer delivery, effect
//!   intent), on a consumer create on each of the four workload streams, on a census
//!   write, on an unbound room, and on `$SYS` and sentinel subjects;
//! - the delivery-authority user (prefrontal-core's grant) may publish wakes, peer
//!   deliveries and effect intents for any agent, and is refused a consumer create, a
//!   census write and `$SYS`;
//! - the bus-module user may get, watch, put and delete the census and manage the five
//!   streams, and is refused every workload publish;
//! - the system user may send the kick, the claims update and the claims lookup (the
//!   subjects the vendored golden records), and nothing else;
//! - a client using the library's default `_INBOX` connects and is refused at subscribe.
//!
//! The participant is bound to a fixture room here, through the grant generator
//! directly: issuance binds no room yet, because no contract names where a module's
//! room bindings come from. A refusal is observed as the server's
//! permissions violation for the exact subject, never as a silent drop, and every
//! allowed act is checked by its effect as well as by the absence of a violation.
//!
//! The golden-commit half (regenerating prefrontal's `permission_golden.txt` from this
//! generator) records `prefrontal-seat-unnamed`.

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

use async_nats::jetstream::{self, consumer::pull};
use async_trait::async_trait;
use bootstrap::plane::{Broker, NatsBroker};
use cortexkit_bus_naming::AccountNames;
use credentials::{
    issue::{sign_user_jwt, UserJwtRequest},
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use futures_util::StreamExt;
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
    issuance::VerdictClient,
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::{nats_server_bin, unix_now},
        run::{ClaustrumSide, RunOptions, SignerRun},
        HarnessSigner, SIGNER_OPERATIONS,
    },
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_client_rs::HandlerOutcome;

const BOOT_LIMIT: Duration = Duration::from_secs(60);
const AGENT: &str = "agent_conf_a";
const FOREIGN_AGENT: &str = "agent_conf_b";
const ROOM: &str = "room_conf_bound";
const UNBOUND_ROOM: &str = "room_conf_unbound";

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

/// The harness signer answered in-process through ck-bus's own vault wire.
struct InProcessSigner(HarnessSigner);

impl InProcessSigner {
    fn answer(&self, body: &[u8]) -> Result<Vec<u8>, VaultError> {
        match self.0.answer(body) {
            HandlerOutcome::Response(bytes) => Ok(bytes),
            other => Err(VaultError::Malformed(format!("{other:?}"))),
        }
    }
}

#[async_trait]
impl VaultSigning for InProcessSigner {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let body = wire::sign_request(credential_id, payload)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))?;
        wire::parse_sign_reply(&self.answer(&body)?)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        wire::parse_public_key_reply(&self.answer(&wire::public_key_request(credential_id))?)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))
    }
}

/// A user ck-bus's code would issue: key generated in custody, JWT built with `grant`
/// and signed by `root_id` through the harness signer.
struct Minted {
    credentials: Arc<Credentials>,
    user: String,
    jwt: String,
}

async fn mint(
    trust: &TrustChain,
    root_id: &str,
    issuer_account: &str,
    grant: impl FnOnce(&str) -> grants::Grant,
) -> Minted {
    let credentials = Arc::new(Credentials::new(Arc::new(InProcessSigner(
        trust.signer.clone(),
    ))));
    let user = credentials.custody.generate_user();
    let grant = grant(&user);
    let jwt = sign_user_jwt(
        credentials.vault.as_ref(),
        &credentials.key_ids,
        &UserJwtRequest {
            root_credential_id: root_id,
            user_public: &user,
            issuer_account: Some(issuer_account),
            name: "grant-conformance",
            issued_at: unix_now() - 60,
            expires_at: unix_now() - 60 + credentials::lifetime::USER_JWT_LIFETIME.as_secs() as i64,
            grant: &grant,
        },
    )
    .await
    .expect("ck-bus builds and the harness signer signs the user JWT")
    .jwt;
    Minted {
        credentials,
        user,
        jwt,
    }
}

async fn connect(
    server: &BusServer,
    minted: &Minted,
    custom_inbox: bool,
) -> Result<VerdictClient, async_nats::ConnectError> {
    let credentials = minted.credentials.clone();
    let user = minted.user.clone();
    VerdictClient::connect(
        &server.url,
        &minted.jwt,
        Arc::new(move |nonce: Vec<u8>| {
            let signed = credentials
                .custody
                .sign_nonce(&user, &nonce)
                .map_err(|error| error.to_string());
            Box::pin(async move { signed })
                as futures_util::future::BoxFuture<'static, Result<Vec<u8>, String>>
        }),
        custom_inbox.then(|| format!("_INBOX.{}", minted.user)),
    )
    .await
}

async fn stored(observer: &jetstream::Context, stream: &str) -> u64 {
    observer
        .get_stream(stream.to_string())
        .await
        .expect("stream exists")
        .info()
        .await
        .expect("stream info")
        .state
        .messages
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn generated_grants_hold_on_a_live_server() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::GrantConformance, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary());
            return;
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
    let acct = ready["acct"].as_str().unwrap().to_string();
    let account_public = ready["account_public"].as_str().unwrap().to_string();
    let names = grants::derive_account(&acct).unwrap();
    let streams = names.streams().clone();
    let census_bucket = names.buckets().census.clone();
    let census_stream = names.buckets().census_stream.clone();
    let observer = jetstream::new(bus::box_client(&trust, &server, &account_public).await);

    // The bus-module user, through ck-bus's broker code, creates the participant's and
    // a foreign identity's durables exactly as prefrontal's `ckbus.agent_durable_bind`
    // does (R15 moved agent durables out of issuance).
    let bus_user = mint(&trust, &bus::box_root_id(), &account_public, |user| {
        grants::bus_module_grant(&names, user).unwrap()
    })
    .await;
    let bus_plane = NatsBroker::new(server.url.clone(), bus_user.credentials.clone())
        .connect_box(&bus_user.jwt, &bus_user.user)
        .await
        .expect("the bus-module user connects");
    let plane = issuance::Plane {
        names: names.clone(),
        account_public: account_public.clone(),
        server_url: server.url.clone(),
        box_plane: bus_plane,
    };
    for agent in [AGENT, FOREIGN_AGENT] {
        membership::bind(&names, plane.box_plane.as_ref(), agent)
            .await
            .unwrap_or_else(|refusal| panic!("durables for {agent}: {refusal}"));
    }

    // ---- The per-process user ----
    let participant = mint(&trust, &bus::box_root_id(), &account_public, |user| {
        grants::participant_grant(&names, user, &[ROOM]).unwrap()
    })
    .await;
    let p = connect(&server, &participant, true)
        .await
        .expect("the participant connects");
    let pjs = jetstream::new(p.client.clone());

    // ---- The delivery-authority user ----
    let authority = mint(&trust, &bus::box_root_id(), &account_public, |user| {
        grants::delivery_authority_grant(&names, user, &[ROOM]).unwrap()
    })
    .await;
    let a = connect(&server, &authority, true)
        .await
        .expect("the delivery-authority user connects");
    // Allowed: a workload message for each agent stream, each checked by its storage.
    for (stream, subject) in [
        (
            &streams.peer,
            names.peer_delivery(AGENT, "sess_conf").unwrap(),
        ),
        (
            &streams.effect,
            names.effect_intent(AGENT, "sess_conf").unwrap(),
        ),
    ] {
        let before = stored(&observer, stream).await;
        a.publish(&subject, b"delivered").await;
        a.expect_allowed(&subject).await;
        assert_eq!(
            stored(&observer, stream).await,
            before + 1,
            "{subject} stored"
        );
    }
    // Refused: it creates no consumer, writes no census key and reaches no `$SYS`.
    let mut authority_refused = vec![
        format!("$KV.{census_bucket}.authority_probe"),
        "$SYS.REQ.CLAIMS.UPDATE".to_string(),
    ];
    for stream in [&streams.room, &streams.wake, &streams.peer, &streams.effect] {
        authority_refused.push(format!(
            "$JS.API.CONSUMER.CREATE.{stream}.{}",
            AccountNames::consumer_name(AGENT).unwrap()
        ));
    }
    for subject in &authority_refused {
        a.publish(subject, b"{}").await;
        a.expect_denied(subject).await;
    }

    // Allowed: the delivery authority's wake, then the participant's pull and ack on
    // the agent's durable. The grant names no agent, so this is any agent's durable.
    let fire = names.wake_fire(AGENT).unwrap();
    a.publish(&fire, b"wake").await;
    a.expect_allowed(&fire).await;
    let own = AccountNames::consumer_name(AGENT).unwrap();
    let consumer: jetstream::consumer::Consumer<pull::Config> = pjs
        .get_consumer_from_stream(own.clone(), streams.wake.clone())
        .await
        .expect("info on its own consumer");
    let mut batch = consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(3))
        .messages()
        .await
        .expect("pull on its own consumer");
    let message = batch
        .next()
        .await
        .expect("the wake arrives")
        .expect("a delivered message");
    assert_eq!(message.payload.as_ref(), b"wake");
    let ack_subject = message.reply.clone().expect("an ack subject").to_string();
    assert!(ack_subject.starts_with(&format!("$JS.ACK.{}.{own}.", streams.wake)));
    message.ack().await.expect("ack sent");
    p.expect_allowed(&ack_subject).await;
    for subject in [
        format!("$JS.API.CONSUMER.INFO.{}.{own}", streams.wake),
        format!("$JS.API.CONSUMER.MSG.NEXT.{}.{own}", streams.wake),
    ] {
        p.expect_allowed(&subject).await;
    }
    let mut info = consumer.clone();
    assert_eq!(
        info.info().await.unwrap().ack_floor.stream_sequence,
        1,
        "the ack was applied"
    );

    // Allowed: census get and watch.
    let census_key = "conformance_probe";
    plane
        .box_plane
        .census_put(
            &names.census_subject(census_key).unwrap(),
            b"{\"probe\":1}".to_vec(),
        )
        .await
        .expect("the bus-module user writes the census");
    let pkv = pjs
        .get_key_value(census_bucket.clone())
        .await
        .expect("census bucket info");
    assert_eq!(
        pkv.get(census_key).await.expect("census get").as_deref(),
        Some(&b"{\"probe\":1}"[..])
    );
    let mut watch = pkv
        .watch_with_history(census_key)
        .await
        .expect("census watch");
    let seen = tokio::time::timeout(Duration::from_secs(5), watch.next())
        .await
        .unwrap_or_else(|_| panic!("the participant's watch delivers; events {:?}", p.events()))
        .expect("a watch entry")
        .expect("a watch entry");
    assert_eq!(seen.key, census_key);
    drop(watch);
    // The watch's ordered consumer is created under the census stream; a delete there
    // is permitted too (the server answers "not found" for a name that does not exist).
    let delete = format!("$JS.API.CONSUMER.DELETE.{census_stream}.conformance_gone");
    let reply = p
        .client
        .request(delete.clone(), "".into())
        .await
        .expect("the delete is answered");
    assert!(String::from_utf8_lossy(&reply.payload).contains("error"));
    p.expect_allowed(&delete).await;
    let census_violations: Vec<String> = p
        .events()
        .into_iter()
        .filter(|event| {
            event.to_ascii_lowercase().contains("permissions violation")
                && event.contains(&census_stream)
        })
        .collect();
    assert!(
        census_violations.is_empty(),
        "the watch's consumer create and the delete under the census stream are allowed: \
         {census_violations:?}"
    );

    // Allowed: the dead-letter subject and a bound room.
    let dead = names.effect_dead();
    let room = names.room_post(ROOM).unwrap();
    let dead_before = stored(&observer, &streams.effect_dead).await;
    let room_before = stored(&observer, &streams.room).await;
    p.publish(&dead, b"dead").await;
    p.publish(&room, b"post").await;
    p.expect_allowed(&dead).await;
    p.expect_allowed(&room).await;
    assert_eq!(
        stored(&observer, &streams.effect_dead).await,
        dead_before + 1
    );
    assert_eq!(stored(&observer, &streams.room).await, room_before + 1);

    // Allowed: another agent's durable too, with no reissue (R15).
    let foreign_fire = names.wake_fire(FOREIGN_AGENT).unwrap();
    a.publish(&foreign_fire, b"foreign wake").await;
    a.expect_allowed(&foreign_fire).await;
    let foreign = AccountNames::consumer_name(FOREIGN_AGENT).unwrap();
    let foreign_consumer: jetstream::consumer::Consumer<pull::Config> = pjs
        .get_consumer_from_stream(foreign.clone(), streams.wake.clone())
        .await
        .expect("info on another agent's durable");
    let foreign_message = foreign_consumer
        .fetch()
        .max_messages(1)
        .expires(Duration::from_secs(3))
        .messages()
        .await
        .expect("pull on another agent's durable")
        .next()
        .await
        .expect("the foreign wake arrives")
        .expect("a delivered message");
    assert_eq!(foreign_message.payload.as_ref(), b"foreign wake");
    let foreign_ack = foreign_message
        .reply
        .clone()
        .expect("an ack subject")
        .to_string();
    foreign_message.ack().await.expect("ack sent");
    for subject in [
        foreign_ack,
        format!("$JS.API.CONSUMER.MSG.NEXT.{}.{foreign}", streams.wake),
        format!("$JS.API.CONSUMER.INFO.{}.{foreign}", streams.wake),
    ] {
        p.expect_allowed(&subject).await;
    }

    // Refused, server-side: every workload publish, its own agent's included.
    let mut refused = vec![
        names.wake_fire(AGENT).unwrap(),
        names.peer_delivery(AGENT, "sess_conf").unwrap(),
        names.effect_intent(AGENT, "sess_conf").unwrap(),
        format!("$KV.{census_bucket}.{census_key}"),
        names.room_post(UNBOUND_ROOM).unwrap(),
        "$SYS.REQ.CLAIMS.UPDATE".to_string(),
        names.sentinel_ping(),
    ];
    for stream in [&streams.room, &streams.wake, &streams.peer, &streams.effect] {
        refused.push(format!("$JS.API.CONSUMER.CREATE.{stream}.{own}"));
    }
    for subject in &refused {
        p.publish(subject, b"{}").await;
        p.expect_denied(subject).await;
    }

    // ---- The bus-module user ----
    let b = connect(&server, &bus_user, true)
        .await
        .expect("a second bus-module connection");
    let bjs = jetstream::new(b.client.clone());
    let bkv = bjs
        .get_key_value(census_bucket.clone())
        .await
        .expect("census bucket info");
    bkv.put("conformance_bus", "one".into())
        .await
        .expect("census put");
    assert_eq!(
        bkv.get("conformance_bus").await.unwrap().as_deref(),
        Some(&b"one"[..])
    );
    let mut watch = bkv
        .watch_with_history("conformance_bus")
        .await
        .expect("census watch");
    tokio::time::timeout(Duration::from_secs(5), watch.next())
        .await
        .expect("the watch delivers")
        .expect("a watch entry")
        .expect("a watch entry");
    drop(watch);
    bkv.delete("conformance_bus").await.expect("census delete");
    assert_eq!(bkv.get("conformance_bus").await.unwrap(), None);
    let probes = [
        (
            streams.room.clone(),
            names.room_post("room_conf_probe").unwrap(),
        ),
        (
            streams.wake.clone(),
            names.wake_fire("agent_conf_probe").unwrap(),
        ),
        (
            streams.peer.clone(),
            names.peer_filter("agent_conf_probe").unwrap(),
        ),
        (
            streams.effect.clone(),
            names.effect_filter("agent_conf_probe").unwrap(),
        ),
        (streams.effect_dead.clone(), names.effect_dead()),
    ];
    for (stream, filter) in probes {
        bjs.get_stream(stream.clone())
            .await
            .unwrap_or_else(|error| panic!("stream info {stream}: {error}"));
        bjs.create_consumer_on_stream(
            pull::Config {
                durable_name: Some("c_conformance_probe".to_string()),
                filter_subject: filter,
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                max_ack_pending: membership::DURABLE_MAX_ACK_PENDING,
                ..Default::default()
            },
            stream.clone(),
        )
        .await
        .unwrap_or_else(|error| panic!("consumer create on {stream}: {error}"));
        bjs.delete_consumer_from_stream("c_conformance_probe", stream.clone())
            .await
            .unwrap_or_else(|error| panic!("consumer delete on {stream}: {error}"));
    }
    for subject in [
        names.room_post(ROOM).unwrap(),
        names.wake_fire(AGENT).unwrap(),
        names.effect_intent(AGENT, "sess_conf").unwrap(),
        names.peer_delivery(AGENT, "sess_conf").unwrap(),
    ] {
        b.publish(&subject, b"workload").await;
        b.expect_denied(&subject).await;
    }

    // ---- The system user ----
    let golden = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/foundation/permission_golden.txt"),
    )
    .unwrap();
    for pattern in [
        "$SYS.REQ.CLAIMS.UPDATE",
        "$SYS.REQ.ACCOUNT.*.CLAIMS.LOOKUP",
        "$SYS.REQ.SERVER.*.KICK",
    ] {
        assert!(
            golden.contains(&format!("allow system publish {pattern}\n")),
            "the golden records {pattern}"
        );
    }
    let system = mint(
        &trust,
        &bus::system_root_id(),
        &trust.system_account,
        |user| grants::system_account_grant(&names, user).unwrap(),
    )
    .await;
    let s = connect(&server, &system, true)
        .await
        .expect("the system user connects");
    let lookup = format!("$SYS.REQ.ACCOUNT.{account_public}.CLAIMS.LOOKUP");
    let account_jwt = s
        .client
        .request(lookup.clone(), "".into())
        .await
        .expect("claims lookup answered");
    assert!(
        !account_jwt.payload.is_empty(),
        "the lookup returns the account JWT"
    );
    let update = s
        .client
        .request(
            "$SYS.REQ.CLAIMS.UPDATE".to_string(),
            account_jwt.payload.clone(),
        )
        .await
        .expect("claims update answered");
    let update: Value = serde_json::from_slice(&update.payload).unwrap();
    assert!(update.get("error").is_none(), "claims update: {update}");
    let kicked = bus::box_client(&trust, &server, &account_public).await;
    let info = kicked.server_info();
    let kick = format!("$SYS.REQ.SERVER.{}.KICK", info.server_id);
    let reply = s
        .client
        .request(
            kick.clone(),
            serde_json::to_vec(&json!({"cid": info.client_id}))
                .unwrap()
                .into(),
        )
        .await
        .expect("kick answered");
    let reply: Value = serde_json::from_slice(&reply.payload).unwrap();
    assert!(reply.get("error").is_none(), "kick: {reply}");
    for subject in [&lookup, &kick, &"$SYS.REQ.CLAIMS.UPDATE".to_string()] {
        s.expect_allowed(subject).await;
    }
    for subject in [
        "$SYS.REQ.SERVER.PING".to_string(),
        format!("$SYS.REQ.ACCOUNT.{account_public}.CONNZ"),
        names.room_post(ROOM).unwrap(),
    ] {
        s.publish(&subject, b"{}").await;
        s.expect_denied(&subject).await;
    }
    let _sys_everything = s.client.subscribe("$SYS.>".to_string()).await.unwrap();
    s.client.flush().await.unwrap();
    s.expect_denied("$SYS.>").await;

    // ---- The default-inbox client ----
    let default_inbox = mint(&trust, &bus::box_root_id(), &account_public, |user| {
        grants::participant_grant(&names, user, &[ROOM]).unwrap()
    })
    .await;
    let d = connect(&server, &default_inbox, false)
        .await
        .expect("a default-inbox client still connects");
    let inbox = d.client.new_inbox();
    assert!(
        inbox.starts_with("_INBOX.")
            && !inbox.starts_with(&format!("_INBOX.{}", default_inbox.user))
    );
    let _sub = d.client.subscribe(inbox.clone()).await.unwrap();
    d.client.flush().await.unwrap();
    d.expect_denied(&inbox).await;

    RowReport::passed(Row::GrantConformance)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary());
    drop((p, a, b, s, d, kicked));
    server.stop().await;
    run.shutdown().await;
}

/// The golden-commit half: the committed permission golden lives in the prefrontal
/// repository, so regenerating it from this generator is a change there, which this
/// repository's tests never make.
#[test]
fn the_golden_commit_half_waits_for_the_prefrontal_seat() {
    RowReport::skipped(
        Row::GrantConformance,
        "prefrontal-seat-unnamed",
        "regenerating prefrontal's tests/grants/permission_golden.txt is a prefrontal commit; \
         no owner has named the prefrontal seat, campaign or path",
    )
    .served_by(ServedBy::HarnessSigner)
    .emit(&vocabulary());
}
