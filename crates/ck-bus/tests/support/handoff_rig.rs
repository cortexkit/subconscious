//! Run with a real `nats-server` on PATH or in `CK_NATS_SERVER_BIN`.

#[allow(dead_code)]
#[path = "nats_federation.rs"]
mod fixture;

use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

use async_nats::{
    header::NATS_MESSAGE_ID,
    jetstream::{self, message::PublishMessage},
};
use async_trait::async_trait;
use cortexkit_bus_naming::{
    federation::{
        fedout_stream, hub_inbox_stream, inbox_stream_name, leaf_user_grant, FEDOUT_STREAM,
    },
    shipped_streams, AccountNames,
};
use cortexkit_bus_testkit::{nats_server_bin, signer::HarnessSigner, RigError};
use fixture::{connect, http_get, Account, Operator, Resolver, Server};
use hpke::{kem::X25519HkdfSha256 as Kem, Kem as _, Serializable as _};
use serde_json::Value;
use tokio::task::JoinHandle;

use crate::{
    bootstrap::{account_jwt::HandoffService, plane::stream_config},
    credentials::{
        vault::{VaultError, VaultSigning},
        wire::{VaultPublicKey, VaultSignature},
    },
    federation::handoff::{
        in_durable,
        mover::{Inputs, Mover},
        out_durable,
        production::{ensure_inbox, mover_grant},
        tests::{hpke_open, rig_seal, rig_stamp, A, B},
        Health, OpenError, Opener, Peer, PeerKeys, Peers,
    },
    grants,
    runtime::{GrantGeneration, OwnUser},
};
use cortexkit_bus_naming::federation::BoxId;

const SIGNING_ID: &str = "signing:msgsig";
const C: &str = "cccccccccccccccccccccccccccccccc";
const UNKNOWN: &str = "dddddddddddddddddddddddddddddddd";
const FRAME_ID: &str = "delivery-a-1";
const BODY: &str = "handoff payload";
const DEADLINE: Duration = Duration::from_secs(15);

struct Vault {
    signing: HarnessSigner,
    seal: <Kem as hpke::Kem>::PrivateKey,
    signatures: AtomicU64,
    rate_limit: AtomicBool,
}

impl Vault {
    fn new(signing: HarnessSigner, seal: <Kem as hpke::Kem>::PrivateKey) -> Self {
        Self {
            signing,
            seal,
            signatures: AtomicU64::new(0),
            rate_limit: AtomicBool::new(false),
        }
    }
}

#[async_trait]
impl VaultSigning for Vault {
    async fn sign(&self, id: &str, payload: &[u8]) -> Result<VaultSignature, VaultError> {
        assert_eq!(id, SIGNING_ID);
        self.signatures.fetch_add(1, Ordering::Relaxed);
        let root = self.signing.root(id);
        Ok(VaultSignature {
            signature: root.pair.sign(payload).unwrap().try_into().unwrap(),
            key_id: root.key_id(),
        })
    }

    async fn public_key(&self, _: &str) -> Result<VaultPublicKey, VaultError> {
        panic!("the hand-off must not read A's own signing key")
    }
}

#[async_trait]
impl Opener for Vault {
    async fn open(&self, enc: &[u8], ciphertext: &[u8], info: &[u8]) -> Result<Vec<u8>, OpenError> {
        assert!(info.starts_with(b"ck-bus-seal/v1/ckbus/"));
        if self.rate_limit.load(Ordering::Relaxed) {
            return Err(OpenError::RateLimited);
        }
        hpke_open(&self.seal, enc, ciphertext, info)
            .map_err(|_| OpenError::Refused("open_failed".into()))
    }
}

struct Roster(Vec<(BoxId, PeerKeys)>);
#[async_trait]
impl Peers for Roster {
    async fn lookup(&self, peer: &BoxId) -> Result<Peer, String> {
        Ok(self
            .0
            .iter()
            .find(|(known, _)| known == peer)
            .map_or(Peer::Unpaired, |(_, keys)| Peer::Live(*keys)))
    }
}

struct BoxServer {
    id: BoxId,
    names: AccountNames,
    _server: Server,
    admin: jetstream::Context,
    fed_admin: jetstream::Context,
    ckbus: jetstream::Context,
    fed: jetstream::Context,
    publisher: async_nats::Client,
}

impl BoxServer {
    async fn start(
        bin: &Path,
        root: &Path,
        hub_port: u16,
        hub_account: &Account,
        id: BoxId,
    ) -> Self {
        let names = AccountNames::derive(&format!("box_{id}")).unwrap();
        let operator = Operator::new(&format!("operator-{id}"));
        let mut local = Account::new(names.account(), true);
        let mut fed = Account::new(&format!("fed_{id}"), true);
        local.exports = service_claims(HandoffService::Export {
            box_name: names.account().into(),
        });
        fed.imports = service_claims(HandoffService::Import {
            box_name: names.account().into(),
            box_account: local.public_key(),
        });
        let leaf = leaf_user_grant(&id);
        let leaf_user = hub_account.user_with_permissions(
            &format!("leaf-{id}"),
            &leaf.publish,
            &leaf.subscribe,
        );
        let creds = root.join(format!("leaf-{id}.creds"));
        std::fs::write(&creds, &leaf_user.creds).unwrap();
        let extra = format!(
            "max_control_line: 65536\nleafnodes {{ remotes: [{{ url: \"nats-leaf://127.0.0.1:{hub_port}\", credentials: {:?}, account: {:?}, deny_imports: [\"fedlocal.>\", \"fedout.>\"], deny_exports: [\"fedlocal.>\", \"fedout.>\"] }}] }}",
            creds.display().to_string(), fed.public_key(),
        );
        let tag = if id.as_str() == A { "box-a" } else { "box-b" };
        let mut server = Server::configure(
            bin,
            root,
            tag,
            "",
            &operator,
            &[&local, &fed],
            Resolver::Memory,
            &extra,
        );
        server.start().await;
        let admin = jetstream::new(connect(&server.url(), &local.user("admin")).await);
        let fed_admin = jetstream::new(connect(&server.url(), &fed.user("admin")).await);
        for spec in shipped_streams(&names) {
            if [
                names.streams().fedsend.as_str(),
                names.streams().fedsend_dead.as_str(),
                names.streams().peer.as_str(),
                names.streams().wake.as_str(),
            ]
            .contains(&spec.name.as_str())
            {
                admin.create_stream(stream_config(&spec)).await.unwrap();
            }
        }
        fed_admin
            .create_stream(stream_config(&fedout_stream()))
            .await
            .unwrap();
        let ckbus = scoped_client(&server, &local, "ckbus", |user| {
            let permissions = grants::GrantSeam
                .own_user_grant(OwnUser::BusModule, &names, user)
                .unwrap();
            grants::Grant::from_generated(grants::GrantRole::BusModule, &names, &permissions)
                .unwrap()
                .jwt_permissions()
        })
        .await;
        let publisher = scoped_client(&server, &local, "prefrontal", |user| {
            grants::issued_grant(&names, "prefrontal", user, &[])
                .unwrap()
                .jwt_permissions()
        })
        .await;
        let mover = scoped_client(&server, &fed, "mover", |user| {
            mover_grant(&id, user).jwt_permissions()
        })
        .await;
        let mover = jetstream::new(mover);
        ensure_inbox(&mover, &id).await.unwrap();
        Self {
            id,
            names,
            _server: server,
            admin,
            fed_admin,
            ckbus: jetstream::new(ckbus),
            fed: mover,
            publisher,
        }
    }

    fn move_frames(
        &self,
        vault: &Arc<Vault>,
        peers: &[(BoxId, PeerKeys)],
        health: &Arc<Health>,
        journal: &Path,
    ) -> JoinHandle<()> {
        let mover = Mover::new(
            Inputs {
                machine: self.id.clone(),
                names: self.names.clone(),
                box_js: self.ckbus.clone(),
                fed_js: self.fed.clone(),
                peers: Arc::new(Roster(peers.to_vec())),
                signing: vault.clone(),
                opener: vault.clone(),
                health: health.clone(),
            },
            journal,
        )
        .unwrap()
        .with_test_rate_limit_delay(Duration::from_secs(2));
        tokio::spawn(async move { mover.run().await })
    }
}

fn service_claims(handoff: HandoffService) -> Vec<Value> {
    let Value::Array(claims) = handoff.claims().1 else {
        panic!("handoff claims must be an array");
    };
    claims
}

async fn scoped_client(
    server: &Server,
    account: &Account,
    name: &str,
    permissions: impl FnOnce(&str) -> Value,
) -> async_nats::Client {
    // NATS subjects for the response inbox include the same nkey that signed CONNECT.
    let user = account.user_with_claims_for_key(name, permissions);
    async_nats::ConnectOptions::with_credentials(&user.creds)
        .unwrap()
        .custom_inbox_prefix(format!("_INBOX.{}", user.public_key()))
        .connect(server.url())
        .await
        .unwrap()
}

async fn publish(
    js: &jetstream::Context,
    subject: &str,
    id: &str,
    body: impl Into<Vec<u8>>,
) -> jetstream::publish::PublishAck {
    js.send_publish(
        subject.to_owned(),
        PublishMessage::build()
            .payload(body.into().into())
            .message_id(id),
    )
    .await
    .unwrap()
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn handoff_rig() {
    let (bin, _) = match nats_server_bin() {
        Ok(binary) => binary,
        Err(RigError::MissingBinary { .. })
            if std::env::var_os("CK_BUS_TESTKIT_REQUIRE").is_none() =>
        {
            eprintln!("handoff rig skipped: nats-server is missing");
            return;
        }
        Err(error) => panic!("real nats-server: {error}"),
    };
    let root = tempfile::tempdir().unwrap();
    let a_id = BoxId::parse(A).unwrap();
    let b_id = BoxId::parse(B).unwrap();
    let hub_operator = Operator::new("hub-operator");
    let hub_account = Account::new("hub-account", true);
    let mut hub = Server::configure(
        &bin,
        root.path(),
        "hub",
        "hub",
        &hub_operator,
        &[&hub_account],
        Resolver::Memory,
        "leafnodes { listen: \"127.0.0.1:-1\", no_advertise: true }",
    );
    hub.start().await;
    let hub_port = serde_json::from_str::<Value>(&http_get(hub.http_port, "/varz").await.unwrap())
        .unwrap()["leaf"]["port"]
        .as_u64()
        .unwrap() as u16;
    let hub_js = jetstream::new(connect(&hub.url(), &hub_account.user("hub-client")).await);
    for id in [&a_id, &b_id] {
        let mut config = stream_config(&hub_inbox_stream(id).stream);
        // Let the exact same envelope and delivery ID reach the mover twice, beyond
        // the hub's dedupe window, to test the receiver's independent receipt set.
        config.duplicate_window = Duration::from_millis(100);
        hub_js.create_stream(config).await.unwrap();
    }
    let a = BoxServer::start(&bin, root.path(), hub_port, &hub_account, a_id.clone()).await;
    let b = BoxServer::start(&bin, root.path(), hub_port, &hub_account, b_id.clone()).await;
    hub.wait_leaf_names(&["box-a", "box-b"], DEADLINE).await;
    let (a_kem, a_pub) = Kem::derive_keypair(&[2; 32]);
    let (b_kem, b_pub) = Kem::derive_keypair(&[3; 32]);
    let a_vault = Arc::new(Vault::new(HarnessSigner::fixed(SIGNING_ID, [4; 32]), a_kem));
    let b_vault = Arc::new(Vault::new(HarnessSigner::fixed(SIGNING_ID, [5; 32]), b_kem));
    let a_keys = PeerKeys {
        seal: a_pub.to_bytes().into(),
        sign: a_vault.signing.root(SIGNING_ID).public,
    };
    let b_keys = PeerKeys {
        seal: b_pub.to_bytes().into(),
        sign: b_vault.signing.root(SIGNING_ID).public,
    };
    let a_health = Arc::new(Health::default());
    let b_health = Arc::new(Health::default());
    let a_path = root.path().join("a.sqlite3");
    let b_path = root.path().join("b.sqlite3");
    let c_id = BoxId::parse(C).unwrap();
    let a_peers = [(b_id.clone(), b_keys), (c_id.clone(), b_keys)];
    let b_peers = [(a_id.clone(), a_keys)];
    let a_task = a.move_frames(&a_vault, &a_peers, &a_health, &a_path);
    let b_task = b.move_frames(&b_vault, &b_peers, &b_health, &b_path);

    let original = format!("ck.box_{A}.fedsend.{B}.peer.agent_x.handoff");
    let on_hub = format!("ck.{B}.peer.agent_x.handoff");
    let on_box = format!("ck.box_{B}.peer.fed.{A}.agent_x.handoff");
    let publisher = jetstream::new(a.publisher.clone());
    assert_eq!(
        publish(&publisher, &original, FRAME_ID, BODY.as_bytes())
            .await
            .stream,
        a.names.streams().fedsend
    );
    let peer = b.admin.get_stream(&b.names.streams().peer).await.unwrap();
    let received = tokio::time::timeout(DEADLINE, async {
        loop {
            if let Ok(frame) = peer.get_raw_message(1).await {
                break frame;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "no box PubAck: A {:?}; B {:?}",
            a_health.metrics(),
            b_health.metrics()
        )
    });
    assert_eq!(received.subject.as_str(), on_box);
    assert_eq!(&received.payload[..], BODY.as_bytes());
    assert_eq!(received.headers.iter().count(), 1);
    assert_eq!(
        received.headers.get(NATS_MESSAGE_ID).unwrap().as_str(),
        FRAME_ID
    );
    assert_eq!(b_health.metrics()["fed.inbound_gaps"], 0);

    let hub_inbox = hub_js
        .get_stream(hub_inbox_stream(&b_id).stream.name)
        .await
        .unwrap();
    let sealed = hub_inbox.get_raw_message(1).await.unwrap();
    assert_eq!(sealed.subject.as_str(), on_hub);
    assert_ne!(&sealed.payload[..], BODY.as_bytes());
    assert_eq!(
        sealed.headers.get(NATS_MESSAGE_ID).unwrap().as_str(),
        FRAME_ID
    );
    let (epoch, sequence) = rig_stamp(
        &a_id,
        &b_id,
        &b_vault.seal,
        &sealed.payload,
        "peer.agent_x.handoff",
        FRAME_ID,
    );
    assert_ne!(epoch, 0);
    assert_eq!(sequence, 1);
    let inbox = inbox_stream_name(&b_id);
    let consumer: jetstream::consumer::PullConsumer = b
        .fed_admin
        .get_consumer_from_stream(in_durable("peer"), &inbox)
        .await
        .unwrap();
    fixture::wait_for("first receipt settled", DEADLINE, || async {
        consumer
            .clone()
            .info()
            .await
            .unwrap()
            .ack_floor
            .stream_sequence
            == 1
    })
    .await;
    stop(b_task).await;
    let b_task = b.move_frames(&b_vault, &b_peers, &b_health, &b_path);
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !publish(&hub_js, &on_hub, FRAME_ID, sealed.payload.to_vec())
            .await
            .duplicate
    );
    wait_metric(&b_health, "fed.inbound_duplicate").await;
    assert!(peer.get_raw_message(2).await.is_err());

    let unknown = format!("ck.box_{A}.fedsend.{UNKNOWN}.peer.agent_x.handoff");
    publish(&publisher, &unknown, "delivery-unknown", b"never routed").await;
    wait_metric(&a_health, "fed.fedsend_dead").await;
    let dead = a
        .admin
        .get_stream(&a.names.streams().fedsend_dead)
        .await
        .unwrap()
        .get_raw_message(1)
        .await
        .unwrap();
    assert_eq!(
        dead.subject.as_str(),
        format!("ck.box_{A}.fedsend_dead.unknown_or_removed_computer.2")
    );
    assert_eq!(&dead.payload[..], b"never routed");

    let mut tampered = sealed.payload.to_vec();
    *tampered.last_mut().unwrap() ^= 1;
    publish(&hub_js, &on_hub, "tampered", tampered.clone()).await;
    wait_metric(&b_health, "fed.inbound_quarantined").await;
    assert!(peer.get_raw_message(2).await.is_err());
    fixture::wait_for("quarantine was recorded and settled", DEADLINE, || async {
        consumer
            .clone()
            .info()
            .await
            .unwrap()
            .ack_floor
            .stream_sequence
            == 3
    })
    .await;
    assert_quarantine(&b_path, &on_hub, &tampered);

    // Only the federation service import can place this exact subject on PEER.
    let illegal = jetstream::new(b.publisher.clone())
        .publish(on_box, "forgery".into())
        .await
        .unwrap();
    assert!(illegal.await.is_err());
    assert!(peer.get_raw_message(2).await.is_err());

    // C is paired but has no HUB_INBOX. Its durable must not block B's.
    let to_c = format!("ck.box_{A}.fedsend.{C}.peer.agent_x.handoff");
    publish(
        &publisher,
        &to_c,
        "delivery-c",
        b"waiting for C's hub stream",
    )
    .await;
    wait_metric(&a_health, "fed.hub_unavailable").await;
    let outbox = a.fed_admin.get_stream(FEDOUT_STREAM).await.unwrap();
    let c_consumer: jetstream::consumer::PullConsumer = a
        .fed_admin
        .get_consumer_from_stream(out_durable(&c_id), FEDOUT_STREAM)
        .await
        .unwrap();
    assert_eq!(c_consumer.cached_info().num_ack_pending, 1);

    let mut full = hub_inbox.cached_info().config.clone();
    let original_limit = full.max_messages;
    full.max_messages = 3;
    hub_js.update_stream(&full).await.unwrap();
    let next_id = "delivery-a-2";
    let boundary_body =
        vec![b'x'; 1024 * 1024 - 252 - "peer.agent_x.handoff".len() - next_id.len() - 53];
    // Crash A after saving the seal but before it can store in FEDOUT. The retry
    // must read the journal, not sign again or consume a second sequence.
    let mut outbox_config = outbox.cached_info().config.clone();
    let outbox_limit = outbox_config.max_messages;
    outbox_config.max_messages = 2;
    a.fed_admin.update_stream(&outbox_config).await.unwrap();
    publish(&publisher, &original, next_id, boundary_body.clone()).await;
    fixture::wait_for(
        "sealed bytes on disk while FEDOUT is full",
        DEADLINE,
        || async {
            recorded_seal(&a_path, 4).is_some()
                && a_health.metrics().contains_key("fed.fedsend_waiting")
        },
    )
    .await;
    let before_restart = recorded_seal(&a_path, 4).unwrap();
    assert_eq!(a_vault.signatures.load(Ordering::Relaxed), 3);
    stop(a_task).await;
    outbox_config.max_messages = outbox_limit;
    a.fed_admin.update_stream(&outbox_config).await.unwrap();
    let a_task = a.move_frames(&a_vault, &a_peers, &a_health, &a_path);
    wait_metric(&a_health, "fed.hub_full").await;
    assert_eq!(a_vault.signatures.load(Ordering::Relaxed), 3);
    let before_retry = outbox.get_raw_message(3).await.unwrap();
    assert_eq!(before_retry.payload.as_ref(), before_restart);
    assert!(hub_inbox.get_raw_message(4).await.is_err());

    full.max_messages = original_limit;
    hub_js.update_stream(&full).await.unwrap();
    fixture::wait_for("retry reached the box", DEADLINE, || async {
        peer.get_raw_message(2).await.is_ok()
    })
    .await;
    let retried = hub_inbox.get_raw_message(4).await.unwrap();
    assert_eq!(retried.payload, before_retry.payload);
    assert_eq!(
        rig_stamp(
            &a_id,
            &b_id,
            &b_vault.seal,
            &retried.payload,
            "peer.agent_x.handoff",
            next_id
        ),
        (epoch, 2)
    );
    assert_eq!(
        retried.headers.get(NATS_MESSAGE_ID).unwrap().as_str(),
        next_id
    );
    assert_eq!(
        &peer.get_raw_message(2).await.unwrap().payload[..],
        &boundary_body
    );
    assert_eq!(b_health.metrics()["fed.inbound_gaps"], 0);
    assert_eq!(c_consumer.clone().info().await.unwrap().num_ack_pending, 1);

    let other_epoch = if epoch == 1 { 2 } else { 1 };
    let a_signer = &a_vault.signing.root(SIGNING_ID).pair;
    let wake = b.admin.get_stream(&b.names.streams().wake).await.unwrap();
    let mut full_wake = wake.cached_info().config.clone();
    let wake_limit = full_wake.max_messages;
    full_wake.max_messages = 1;
    b.admin.update_stream(&full_wake).await.unwrap();
    publish(
        &b.admin,
        &format!("ck.box_{B}.wake.agent_x.fire"),
        "prefill",
        b"fill WAKE",
    )
    .await;
    let wake_tail = "wake.agent_x.handoff";
    let wake_id = "wake-waiting";
    publish(
        &hub_js,
        &format!("ck.{B}.{wake_tail}"),
        wake_id,
        rig_seal(
            &a_id,
            &b_id,
            &b_keys.seal,
            a_signer,
            wake_tail,
            wake_id,
            (other_epoch, 1),
        ),
    )
    .await;
    fixture::wait_for("WAKE is blocked", DEADLINE, || async {
        b_health.metrics().contains_key("fed.receive_waiting.wake")
    })
    .await;
    let peer_tail = "peer.agent_x.handoff";
    let peer_id = "peer-passes-wake";
    publish(
        &hub_js,
        &on_hub,
        peer_id,
        rig_seal(
            &a_id,
            &b_id,
            &b_keys.seal,
            a_signer,
            peer_tail,
            peer_id,
            (other_epoch, 2),
        ),
    )
    .await;
    fixture::wait_for("PEER passes blocked WAKE", DEADLINE, || async {
        peer.get_raw_message(3).await.is_ok()
    })
    .await;
    assert!(wake.get_raw_message(2).await.is_err());
    wait_metric(&b_health, "fed.inbound_gaps").await;
    full_wake.max_messages = wake_limit;
    b.admin.update_stream(&full_wake).await.unwrap();
    fixture::wait_for("late unseen sequence is delivered", DEADLINE, || async {
        wake.get_raw_message(2).await.is_ok()
    })
    .await;

    let limited_id = "open-then-retry";
    let limited = rig_seal(
        &a_id,
        &b_id,
        &b_keys.seal,
        a_signer,
        peer_tail,
        limited_id,
        (other_epoch, 3),
    );
    b_vault.rate_limit.store(true, Ordering::Relaxed);
    let limited_seq = publish(&hub_js, &on_hub, limited_id, limited)
        .await
        .sequence;
    fixture::wait_for("vault open is rate limited", DEADLINE, || async {
        waiting_on(&b_health, "peer", "open_rate_limited")
    })
    .await;
    let pending = consumer.clone().info().await.unwrap().clone();
    assert_eq!(pending.num_ack_pending, 1);
    assert!(pending.ack_floor.stream_sequence < limited_seq);
    assert!(peer.get_raw_message(4).await.is_err());
    assert_eq!(b_health.metrics()["fed.inbound_quarantined"], 1);
    b_vault.rate_limit.store(false, Ordering::Relaxed);
    fixture::wait_for(
        "rate-limited frame retried and delivered",
        DEADLINE,
        || async {
            peer.get_raw_message(4).await.is_ok()
                && consumer
                    .clone()
                    .info()
                    .await
                    .unwrap()
                    .ack_floor
                    .stream_sequence
                    == limited_seq
        },
    )
    .await;

    // C knows B's public seal key and builds a valid envelope claiming to be A,
    // but only A's signing key can authorize that origin.
    let c_signer = HarnessSigner::fixed(SIGNING_ID, [6; 32]);
    let forged_id = "origin-forged-by-c";
    let forgery = rig_seal(
        &a_id,
        &b_id,
        &b_keys.seal,
        &c_signer.root(SIGNING_ID).pair,
        peer_tail,
        forged_id,
        (other_epoch, 4),
    );
    let forged_seq = publish(&hub_js, &on_hub, forged_id, forgery).await.sequence;
    fixture::wait_for("signature mismatch waits on peer key", DEADLINE, || async {
        waiting_on(&b_health, "peer", "invalid_signature")
    })
    .await;
    let waiting = consumer.clone().info().await.unwrap().clone();
    assert_eq!(waiting.num_ack_pending, 1);
    assert!(waiting.ack_floor.stream_sequence < forged_seq);
    assert!(peer.get_raw_message(5).await.is_err());
    assert_eq!(b_health.metrics()["fed.inbound_quarantined"], 1);
    stop(a_task).await;
    stop(b_task).await;
}

async fn stop(task: JoinHandle<()>) {
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}

fn recorded_seal(path: &Path, source: u64) -> Option<Vec<u8>> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .ok()?;
    connection
        .query_row(
            "SELECT blob FROM sealed WHERE source = ?",
            [source.to_be_bytes()],
            |row| row.get(0),
        )
        .ok()
}

fn assert_quarantine(path: &Path, subject: &str, blob: &[u8]) {
    use sha2::{Digest as _, Sha256};

    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let record: (String, String, String, usize, Vec<u8>) = connection
        .query_row(
            "SELECT subject, origin, reason, size, digest FROM quarantined",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        record,
        (
            subject.into(),
            A.into(),
            "open_failed".into(),
            blob.len(),
            Sha256::digest(blob).to_vec()
        )
    );
}

async fn wait_metric(health: &Health, name: &str) {
    fixture::wait_for(name, DEADLINE, || async {
        health.metrics()[name].as_u64().unwrap_or_default() > 0
    })
    .await;
}

fn waiting_on(health: &Health, family: &str, reason: &str) -> bool {
    health
        .metrics()
        .get(&format!("fed.receive_waiting.{family}"))
        .and_then(Value::as_str)
        == Some(reason)
}
