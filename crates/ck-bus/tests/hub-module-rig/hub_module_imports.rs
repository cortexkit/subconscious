//! Two-server proof for the box/federation account boundary. The standalone tests
//! use independent grants; the ck-bus issuance test reuses the same server and probes.

#[allow(dead_code)] // The original federation rigs use the restart and revocation helpers.
#[path = "../support/nats_federation.rs"]
mod fixture;

use std::time::Duration;

use async_nats::{
    jetstream::{
        self,
        consumer::{push, AckPolicy, DeliverPolicy},
        context::PublishErrorKind,
        kv::UpdateErrorKind,
        message::PublishMessage,
        object_store::ObjectMetadata,
        stream,
    },
    Event, RequestErrorKind, ServerError,
};
use cortexkit_bus_naming::federation::{
    hub_inbox_stream, hub_memory_subject, hub_room_subject, hub_streams, leaf_user_grant,
    module_deliver_prefix, BoxId, HubKind, HubName, CUTS_BUCKET, CUTS_STREAM, HUB_API_PREFIX,
    HUB_DOMAIN, HUB_IDLOG_STREAM, HUB_MEMORY_STREAM, HUB_MEMORY_SUBJECT, HUB_ROOMS_STREAM,
    HUB_ROOMS_SUBJECT, IDLOG_SUBJECT, MEMORY_KIND_GLOBAL, RESIDENCE_BUCKET, RESIDENCE_STREAM,
};
use cortexkit_bus_naming::DiscardPolicy;
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::{io::AsyncReadExt, sync::mpsc, time::timeout};

use fixture::*;

const BOX_ID: &str = "11111111111111111111111111111111";
const LIMIT: Duration = Duration::from_secs(10);
const FEDERATION_ONLY: [&str; 3] = ["fedlocal.>", "fedout.>", "fedsend.>"];

fn hub_name() -> HubName {
    HubName::parse("0123456789abcdef").unwrap()
}

pub(crate) struct Module {
    name: &'static str,
    inbox: String,
    streams: &'static [&'static str],
    publish: Vec<String>,
}

impl Module {
    fn new(
        box_id: &BoxId,
        name: &'static str,
        streams: &'static [&'static str],
        mut publish: Vec<String>,
    ) -> Self {
        for &stream in streams {
            for endpoint in ["STREAM.INFO", "DIRECT.GET", "CONSUMER.CREATE"] {
                publish.push(api(endpoint, stream));
            }
            for endpoint in [
                "DIRECT.GET",
                "CONSUMER.CREATE",
                "CONSUMER.INFO",
                "CONSUMER.DELETE",
            ] {
                publish.push(api(endpoint, &format!("{stream}.>")));
            }
        }
        Self {
            name,
            inbox: module_deliver_prefix(box_id, name).unwrap(),
            streams,
            publish,
        }
    }

    fn deliveries(&self) -> String {
        format!("{}.>", self.inbox)
    }
}

fn api(endpoint: &str, stream: &str) -> String {
    format!("{HUB_API_PREFIX}.{endpoint}.{stream}")
}

fn durable_grant(subject: &str, stream: &str) -> Vec<String> {
    vec![subject.to_owned(), format!("$JS.ACK.{stream}.>")]
}

fn import(fed: &mut Account, local: &mut Account, kind: &str, subject: &str) {
    let mut export = json!({"type": kind, "subject": subject});
    if kind == "service" {
        export["response_type"] = json!("Singleton");
    }
    fed.exports.push(export);
    local.imports.push(json!({
        "account": fed.public_key(), "type": kind, "subject": subject,
    }));
}

pub(crate) struct Topology {
    local: Server,
    hub: Server,
    proxy: Proxy,
    hub_js: jetstream::Context,
    fed_control: User,
    box_users: Vec<User>,
    users: Vec<User>,
    box_account: String,
    _root: tempfile::TempDir,
}

#[allow(dead_code)] // Used by the workspace's issuance test, not the standalone rig.
pub(crate) struct IssuanceFixture {
    pub(crate) federation: String,
    pub(crate) box_account: String,
    pub(crate) box_signer: nkeys::KeyPair,
    pub(crate) operator: nkeys::KeyPair,
    pub(crate) system: async_nats::Client,
}

impl Topology {
    async fn start(box_id: &BoxId, modules: &[Module], direct_import: bool) -> Self {
        Self::start_with_resolver(box_id, modules, direct_import, false)
            .await
            .0
    }

    #[allow(dead_code)] // The standalone rig never pushes production claims.
    pub(crate) async fn start_issuance(
        box_id: &BoxId,
        modules: &[Module],
    ) -> (Self, IssuanceFixture) {
        let (rig, issuance) = Self::start_with_resolver(box_id, modules, false, true).await;
        (rig, issuance.expect("issuance fixture requested"))
    }

    async fn start_with_resolver(
        box_id: &BoxId,
        modules: &[Module],
        direct_import: bool,
        writable: bool,
    ) -> (Self, Option<IssuanceFixture>) {
        let bin = nats_server_bin();
        let root = tempfile::tempdir().unwrap();
        let hub_operator = Operator::new("hub-operator");
        let hub_account = Account::new("user-u", true);
        let grant = leaf_user_grant(box_id, &hub_name());
        let leaf = hub_account.user_with_permissions("leaf", &grant.publish, &grant.subscribe);
        let mut hub = Server::configure(
            &bin,
            root.path(),
            "hub",
            HUB_DOMAIN,
            &hub_operator,
            &[&hub_account],
            Resolver::Memory,
            if direct_import {
                "trace: true\nleafnodes { listen: \"127.0.0.1:-1\", no_advertise: true }"
            } else {
                "trace: true\nmax_traced_msg_len: 128\nleafnodes { listen: \"127.0.0.1:-1\", no_advertise: true }"
            },
        );
        hub.start().await;
        let varz = http_get(hub.http_port, "/varz").await.expect("hub /varz");
        let varz: Value = serde_json::from_str(&varz).unwrap();
        let leaf_port = u16::try_from(varz["leaf"]["port"].as_u64().unwrap()).unwrap();
        let proxy = Proxy::start(leaf_port).await;
        let hub_js = jetstream::new(connect(&hub.url(), &hub_account.user("hub-admin")).await);
        for spec in hub_streams(box_id, &hub_name(), HubKind::SelfHosted) {
            let base = spec.stream;
            hub_js
                .create_stream(stream::Config {
                    name: base.name,
                    subjects: base.subjects,
                    storage: stream::StorageType::File,
                    discard: match base.discard {
                        DiscardPolicy::Old => stream::DiscardPolicy::Old,
                        DiscardPolicy::New => stream::DiscardPolicy::New,
                    },
                    max_age: base.max_age,
                    max_bytes: i64::try_from(base.max_bytes).unwrap(),
                    max_messages_per_subject: base.max_msgs_per_subject.unwrap_or(-1),
                    max_message_size: base.max_message_size.unwrap_or(-1),
                    duplicate_window: base.duplicate_window.unwrap_or_default(),
                    num_replicas: spec.replicas.unwrap_or(1),
                    allow_rollup: spec.allow_rollup_hdrs,
                    allow_direct: spec.allow_direct,
                    deny_delete: spec.deny_delete,
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let operator = Operator::new("box-operator");
        let mut local = Account::new("box-local", true);
        let mut fed = Account::new(&format!("fed_{box_id}"), true);
        let mut users = Vec::new();
        let mut box_users = Vec::new();
        for module in modules {
            if direct_import {
                for subject in [
                    HUB_MEMORY_SUBJECT.to_owned(),
                    api("STREAM.INFO", HUB_MEMORY_STREAM),
                ] {
                    import(&mut fed, &mut local, "service", &subject);
                }
                import(&mut fed, &mut local, "stream", &module.deliveries());
                users.push(local.user_with_permissions(
                    module.name,
                    &module.publish,
                    &[module.deliveries()],
                ));
            } else {
                let local_subject = format!("rig.local.{}", module.name);
                box_users.push(local.user_with_permissions(
                    module.name,
                    std::slice::from_ref(&local_subject),
                    std::slice::from_ref(&local_subject),
                ));
                users.push(fed.user_with_permission_claims(
                    module.name,
                    json!({"allow": module.publish, "deny": FEDERATION_ONLY}),
                    json!({"allow": [module.deliveries()], "deny": FEDERATION_ONLY}),
                ));
            }
        }
        let box_account = local.public_key();
        let local_signer = nkeys::KeyPair::from_seed(&local.kp.seed().unwrap()).unwrap();
        let fed_control = fed.user("fed-control");
        let creds = root.path().join("leaf.creds");
        std::fs::write(&creds, &leaf.creds).unwrap();
        let mut local = Server::configure(
            &bin,
            root.path(),
            "box",
            "local",
            &operator,
            &[&local, &fed],
            if writable { Resolver::Full } else { Resolver::Memory },
            &format!(
                "max_control_line: 65536\nleafnodes {{ remotes: [{{ url: \"nats-leaf://127.0.0.1:{}\", credentials: \"{}\", account: \"{}\" }}] }}",
                proxy.port,
                creds.display(),
                fed.public_key(),
            ),
        );
        local.start().await;
        hub.wait_leaf_names(&["box"], LIMIT).await;
        let issuance = if writable {
            Some(IssuanceFixture {
                federation: fed.public_key(),
                box_account: box_account.clone(),
                box_signer: local_signer,
                operator: nkeys::KeyPair::from_seed(&operator.kp.seed().unwrap()).unwrap(),
                system: connect(&local.url(), &operator.sys_user).await,
            })
        } else {
            None
        };
        (
            Self {
                local,
                hub,
                proxy,
                hub_js,
                fed_control,
                box_users,
                users,
                box_account,
                _root: root,
            },
            issuance,
        )
    }

    #[allow(dead_code)] // Only the workspace issuance arm uses this URL directly.
    pub(crate) fn local_url(&self) -> String {
        self.local.url()
    }

    #[allow(dead_code)] // Only ck-bus provisions FEDOUT.
    pub(crate) async fn fedout_config(&self, stream: &str) -> stream::Config {
        let client = connect(&self.local.url(), &self.fed_control).await;
        jetstream::new(client)
            .get_stream(stream)
            .await
            .unwrap()
            .cached_info()
            .config
            .clone()
    }

    async fn connect_module(
        &self,
        index: usize,
        module: &Module,
    ) -> (async_nats::Client, mpsc::UnboundedReceiver<String>) {
        self.connect_options(
            async_nats::ConnectOptions::with_credentials(&self.users[index].creds).unwrap(),
            &module.inbox,
        )
        .await
    }

    pub(crate) async fn connect_options(
        &self,
        options: async_nats::ConnectOptions,
        inbox: &str,
    ) -> (async_nats::Client, mpsc::UnboundedReceiver<String>) {
        let (errors, rx) = mpsc::unbounded_channel();
        let client = options
            .custom_inbox_prefix(inbox)
            .request_timeout(Some(LIMIT))
            .event_callback(move |event| {
                let errors = errors.clone();
                async move {
                    if let Event::ServerError(ServerError::Other(error)) = event {
                        let _ = errors.send(error);
                    }
                }
            })
            .connect(self.local.url())
            .await
            .expect("module connects to the local server");
        (client, rx)
    }
}

async fn durable_roundtrip(client: &async_nats::Client, stream_name: &str, subject: String) {
    let js = jetstream::with_domain(client.clone(), HUB_DOMAIN);
    let stream = js
        .get_stream(stream_name)
        .await
        .expect("module STREAM.INFO");
    let mut durable = stream
        .create_consumer(push::Config {
            durable_name: Some("rig_durable".into()),
            deliver_subject: client.new_inbox(),
            ack_policy: AckPolicy::Explicit,
            ..Default::default()
        })
        .await
        .expect("create durable push consumer");
    let mut messages = durable.messages().await.expect("subscribe to durable");
    let ack = js
        .publish(subject.clone(), "stored".into())
        .await
        .unwrap()
        .await
        .unwrap();
    assert_eq!(
        (ack.stream.as_str(), ack.domain.as_str(), ack.sequence),
        (stream_name, HUB_DOMAIN, 1),
        "module PubAck"
    );
    let message = tokio::time::timeout(LIMIT, messages.next())
        .await
        .expect("durable delivery reaches box account")
        .unwrap()
        .unwrap();
    assert_eq!(message.payload.as_ref(), b"stored", "durable payload");
    let reply = message.reply.as_ref().expect("ack subject");
    assert!(
        reply
            .as_str()
            .starts_with(&format!("$JS.ACK.{stream_name}.")),
        "expected stock v1 ack subject, got {reply}"
    );
    note(9, format!("{stream_name} ack reply {reply}"));
    message
        .double_ack()
        .await
        .expect("ack confirmation comes back over the leaf");
    let info = durable.info().await.expect("consumer info after ack");
    assert_eq!(
        info.ack_floor.stream_sequence, ack.sequence,
        "ack floor advanced"
    );
    assert_eq!(info.num_ack_pending, 0, "no unacked deliveries remain");
    let direct = stream
        .direct_get(ack.sequence)
        .await
        .expect("module direct get");
    assert_eq!(direct.subject.as_str(), subject, "direct get subject");
    assert_eq!(direct.payload.as_ref(), b"stored", "direct get payload");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig9_box_service_import_cannot_resolve_hub_api_account() {
    let _serial = RIG_LOCK.lock().await;
    let box_id = BoxId::parse(BOX_ID).unwrap();
    let modules = [Module::new(
        &box_id,
        "memory",
        &[HUB_MEMORY_STREAM],
        durable_grant(HUB_MEMORY_SUBJECT, HUB_MEMORY_STREAM),
    )];
    let rig = Topology::start(&box_id, &modules, true).await;
    let (box_user, _) = rig.connect_module(0, &modules[0]).await;
    let fed = connect(&rig.local.url(), &rig.fed_control).await;
    let endpoint = api("STREAM.INFO", HUB_MEMORY_STREAM);
    let info = fed
        .request(endpoint.clone(), "{}".into())
        .await
        .expect("FED control reaches the same hub endpoint over the same leaf");
    let info: Value = serde_json::from_slice(&info.payload).unwrap();
    assert_eq!(
        info["config"]["name"], HUB_MEMORY_STREAM,
        "FED control receives actual stream info"
    );
    assert_eq!(info["domain"], HUB_DOMAIN, "FED control reached hub");
    let subject = hub_memory_subject(MEMORY_KIND_GLOBAL, "all").unwrap();
    let box_js = jetstream::with_domain(box_user.clone(), HUB_DOMAIN);
    let pub_ack = box_js
        .publish(subject.clone(), "imported".into())
        .await
        .unwrap()
        .await;
    assert_eq!(
        pub_ack
            .expect_err("BOX publish is stored but receives no PubAck")
            .kind(),
        PublishErrorKind::TimedOut
    );
    note(
        9,
        "BOX service-import data publish timed out waiting for PubAck",
    );
    let stored = rig
        .hub_js
        .get_stream(HUB_MEMORY_STREAM)
        .await
        .unwrap()
        .direct_get(1)
        .await
        .unwrap();
    assert_eq!(
        stored.payload.as_ref(),
        b"imported",
        "BOX import stores data on the hub"
    );
    let error = box_user
        .request(endpoint, "{}".into())
        .await
        .expect_err("BOX service import cannot resolve the importing account on the hub");
    assert_eq!(
        error.kind(),
        RequestErrorKind::TimedOut,
        "BOX service import is reachable but the hub API sends no reply"
    );
    let endpoint_trace = format!("[HMSG {} _R_.", api("STREAM.INFO", HUB_MEMORY_STREAM));
    let data_trace = format!("[HMSG {subject} _R_.");
    for needle in [endpoint_trace.as_str(), data_trace.as_str()] {
        assert!(
            !rig.hub.log_lines(&[needle]).is_empty(),
            "the hub received the imported request with a rewritten reply: {needle}"
        );
    }
    let headers = rig.hub.log_lines(&["Nats-Request-Info:"]);
    assert!(
        headers
            .iter()
            .any(|line| line.contains(&rig.box_account) && line.contains("{}")),
        "the hub received BOX's account key in the API request header"
    );
    let fed_ack = jetstream::with_domain(fed, HUB_DOMAIN)
        .publish(subject, "fed control".into())
        .await
        .unwrap()
        .await
        .expect("FED data publish gets its PubAck back over the same leaf");
    assert_eq!(
        (
            fed_ack.stream.as_str(),
            fed_ack.domain.as_str(),
            fed_ack.sequence
        ),
        (HUB_MEMORY_STREAM, HUB_DOMAIN, 2)
    );
    note(9, "FED STREAM.INFO and PubAck succeeded; BOX requests reached the hub with rewritten reply subjects and timed out");
}

async fn identity_log(client: &async_nats::Client) {
    let js = jetstream::with_domain(client.clone(), HUB_DOMAIN);
    let stream = js.get_stream(HUB_IDLOG_STREAM).await.expect("IDLOG info");
    let ordered = stream
        .create_consumer(push::OrderedConfig {
            deliver_subject: client.new_inbox(),
            deliver_policy: DeliverPolicy::New,
            ..Default::default()
        })
        .await
        .expect("IDLOG ordered consumer creation");
    let mut tail = ordered
        .messages()
        .await
        .expect("IDLOG ordered tail subscription");
    for (expected, body) in [(0, "entry-1"), (1, "entry-2")] {
        let ack = js
            .send_publish(
                IDLOG_SUBJECT,
                PublishMessage::build()
                    .payload(body.into())
                    .expected_last_sequence(expected),
            )
            .await
            .unwrap()
            .await
            .expect("IDLOG CAS PubAck");
        assert_eq!(
            (ack.stream.as_str(), ack.domain.as_str(), ack.sequence),
            (HUB_IDLOG_STREAM, HUB_DOMAIN, expected + 1)
        );
        let message = timeout(LIMIT, tail.next())
            .await
            .expect("IDLOG tail delivery")
            .unwrap()
            .unwrap();
        assert_eq!(
            message.info().unwrap().stream_sequence,
            ack.sequence,
            "IDLOG tail sequence"
        );
        assert_eq!(
            message.payload.as_ref(),
            body.as_bytes(),
            "IDLOG tail payload"
        );
    }
    let error = js
        .send_publish(
            IDLOG_SUBJECT,
            PublishMessage::build()
                .payload("stale".into())
                .expected_last_sequence(1),
        )
        .await
        .unwrap()
        .await
        .expect_err("stale IDLOG append refused");
    assert_eq!(
        error.kind(),
        PublishErrorKind::WrongLastSequence,
        "IDLOG refusal came from JetStream CAS"
    );
    let direct = stream.direct_get(2).await.expect("IDLOG direct get");
    assert_eq!(direct.payload.as_ref(), b"entry-2");
}

async fn residence(client: &async_nats::Client, rig: &Topology) {
    let js = jetstream::with_domain(client.clone(), HUB_DOMAIN);
    let kv = js
        .get_key_value(RESIDENCE_BUCKET)
        .await
        .expect("module opens residence KV");
    let hub_kv = rig.hub_js.get_key_value(RESIDENCE_BUCKET).await.unwrap();
    let mut watch = kv.watch_all().await.expect("module opens residence watch");
    let first = kv
        .put("resident", "initial".into())
        .await
        .expect("initial KV put");
    let revision = kv
        .update("resident", "updated".into(), first)
        .await
        .expect("conditional KV put");
    assert_eq!(revision, first + 1, "KV CAS advanced the revision");
    let error = kv
        .update("resident", "stale".into(), first)
        .await
        .expect_err("stale KV revision refused");
    assert_eq!(error.kind(), UpdateErrorKind::WrongLastRevision);
    for (rev, value) in [(first, "initial"), (revision, "updated")] {
        let entry = timeout(LIMIT, watch.next())
            .await
            .expect("live watch delivery")
            .unwrap()
            .unwrap();
        assert_eq!(
            (entry.key.as_str(), entry.revision, entry.value.as_ref()),
            ("resident", rev, value.as_bytes())
        );
    }
    assert_eq!(
        kv.get("resident").await.unwrap().unwrap().as_ref(),
        b"updated",
        "KV subject-suffixed direct get"
    );

    rig.proxy.cut();
    rig.hub.wait_leaf_names(&[], LIMIT).await;
    // Keep the outage key distinct because the bucket keeps only one revision per key.
    let during = hub_kv.put("during", "leaf down".into()).await.unwrap();
    assert!(
        timeout(Duration::from_millis(250), watch.next())
            .await
            .is_err(),
        "no watch delivery while the hub confirms the leaf is absent"
    );
    rig.proxy.restore();
    rig.hub.wait_leaf_names(&["box"], LIMIT).await;
    let entry = timeout(LIMIT, watch.next())
        .await
        .expect("existing watch catches up after the leaf reconnects")
        .unwrap()
        .unwrap();
    assert_eq!(
        (entry.key.as_str(), entry.revision, entry.value.as_ref()),
        ("during", during, b"leaf down".as_slice())
    );
    let after = hub_kv.put("after", "leaf back".into()).await.unwrap();
    let entry = timeout(LIMIT, watch.next())
        .await
        .expect("existing watch receives new updates after recovery")
        .unwrap()
        .unwrap();
    assert_eq!(
        (entry.key.as_str(), entry.revision, entry.value.as_ref()),
        ("after", after, b"leaf back".as_slice())
    );
}

async fn cuts(client: &async_nats::Client, rig: &Topology) {
    let js = jetstream::with_domain(client.clone(), HUB_DOMAIN);
    let store = js
        .get_object_store(CUTS_BUCKET)
        .await
        .expect("module opens cuts object store");
    let hub_store = rig.hub_js.get_object_store(CUTS_BUCKET).await.unwrap();
    let mut backing_stream = rig.hub_js.get_stream(CUTS_STREAM).await.unwrap();
    let name = "opaque/0";
    let data: Vec<u8> = (0..(2 * 512 * 1024 + 17))
        .map(|i| (i % 251) as u8)
        .collect();
    let mut source = data.as_slice();
    let object = store
        .put(
            ObjectMetadata {
                name: name.into(),
                chunk_size: Some(512 * 1024),
                ..Default::default()
            },
            &mut source,
        )
        .await
        .expect("module puts object on hub");
    assert_eq!(object.chunks, 3, "real multi-chunk upload");
    let mut reader = store
        .get(name)
        .await
        .expect("module gets hub object metadata");
    let mut received = Vec::new();
    timeout(LIMIT, reader.read_to_end(&mut received))
        .await
        .expect("hub object chunks arrive at module deliver prefix")
        .unwrap();
    assert_eq!(received, data, "read bytes match uploaded object");
    wait_for(
        "the object reader's flow-control response reaches the hub leaf",
        LIMIT,
        || async {
            !rig.hub
                .log_lines(&[&format!("<<- [LMSG $JS.FC.{CUTS_STREAM}.")])
                .is_empty()
        },
    )
    .await;
    assert_eq!(
        backing_stream.info().await.unwrap().state.messages,
        4,
        "three chunks and metadata reached the hub before purge"
    );
    store
        .delete(name)
        .await
        .expect("module deletes object by filtered chunk purge");
    assert!(
        hub_store.info(name).await.unwrap().deleted,
        "hub retains a deletion tombstone"
    );
    assert_eq!(
        backing_stream.info().await.unwrap().state.messages,
        1,
        "hub purged all chunks and retained only the tombstone"
    );
}

async fn assert_denial(
    errors: &mut mpsc::UnboundedReceiver<String>,
    label: &str,
    operation: &str,
    subject: &str,
) {
    let error = timeout(LIMIT, errors.recv())
        .await
        .unwrap_or_else(|_| panic!("{label}: no server denial for {subject}"))
        .expect("server error callback closed");
    assert!(
        error.contains(operation) && error.contains(&format!("\"{subject}\"")),
        "{label}: expected {operation} on {subject}, got {error}"
    );
}

async fn deny_publish(
    client: &async_nats::Client,
    errors: &mut mpsc::UnboundedReceiver<String>,
    label: &str,
    subject: String,
) {
    client
        .publish(subject.clone(), "denied".into())
        .await
        .unwrap();
    assert_denial(errors, label, "Permissions Violation for Publish", &subject).await;
}

async fn deny_subscribe(
    client: &async_nats::Client,
    errors: &mut mpsc::UnboundedReceiver<String>,
    label: &str,
    subject: String,
) {
    let _sub = client.subscribe(subject.clone()).await.unwrap();
    assert_denial(
        errors,
        label,
        "Permissions Violation for Subscription",
        &subject,
    )
    .await;
}

async fn isolation(
    client: &async_nats::Client,
    errors: &mut mpsc::UnboundedReceiver<String>,
    module: &Module,
    modules: &[Module],
    streams: &[(String, Vec<String>)],
) {
    assert!(
        matches!(errors.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "{}: positive probes reported a server error",
        module.name
    );
    for (stream, subjects) in streams {
        if module.streams.contains(&stream.as_str()) {
            continue;
        }
        for subject in subjects {
            let label = format!("{} cannot publish data to {stream}", module.name);
            deny_publish(client, errors, &label, subject.clone()).await;
            let label = format!("{} cannot subscribe to data on {stream}", module.name);
            deny_subscribe(client, errors, &label, subject.clone()).await;
        }
        for subject in [
            format!("$JS.ACK.{stream}.probe"),
            format!("$JS.FC.{stream}.probe.probe"),
        ] {
            let label = format!("{} cannot ack or flow-control {stream}", module.name);
            deny_publish(client, errors, &label, subject).await;
        }
        for (endpoint, target) in [
            ("STREAM.INFO", stream.clone()),
            ("DIRECT.GET", stream.clone()),
            ("DIRECT.GET", format!("{stream}.probe")),
            ("STREAM.MSG.GET", stream.clone()),
            ("STREAM.PURGE", stream.clone()),
            ("CONSUMER.CREATE", stream.clone()),
            ("CONSUMER.CREATE", format!("{stream}.probe")),
            ("CONSUMER.INFO", format!("{stream}.probe")),
            ("CONSUMER.DELETE", format!("{stream}.probe")),
        ] {
            let label = format!("{} cannot {endpoint} on {stream}", module.name);
            deny_publish(client, errors, &label, api(endpoint, &target)).await;
        }
    }
    for other in modules.iter().filter(|other| other.name != module.name) {
        let label = format!(
            "{} cannot subscribe to {} deliveries",
            module.name, other.name
        );
        deny_subscribe(client, errors, &label, format!("{}.probe", other.inbox)).await;
        deny_subscribe(client, errors, &label, other.deliveries()).await;
        let label = format!(
            "{} cannot publish to {} deliveries",
            module.name, other.name
        );
        deny_publish(client, errors, &label, format!("{}.probe", other.inbox)).await;
    }
    for subject in FEDERATION_ONLY {
        let data = subject.replace('>', "probe");
        let label = format!(
            "{} cannot publish to internal federation traffic",
            module.name
        );
        deny_publish(client, errors, &label, data).await;
        let label = format!(
            "{} cannot subscribe to internal federation traffic",
            module.name
        );
        deny_subscribe(client, errors, &label, subject.to_owned()).await;
    }
    note(
        9,
        format!(
            "{}: all cross-stream, foreign delivery, and internal federation permissions refused",
            module.name
        ),
    );
}

pub(crate) fn shared_modules(box_id: &BoxId) -> [Module; 4] {
    [
        Module::new(
            box_id,
            "magic-context",
            &[HUB_MEMORY_STREAM],
            durable_grant(HUB_MEMORY_SUBJECT, HUB_MEMORY_STREAM),
        ),
        Module::new(
            box_id,
            "rooms",
            &[HUB_ROOMS_STREAM],
            durable_grant(HUB_ROOMS_SUBJECT, HUB_ROOMS_STREAM),
        ),
        Module::new(
            box_id,
            "identity",
            &[HUB_IDLOG_STREAM, RESIDENCE_STREAM],
            vec![
                IDLOG_SUBJECT.into(),
                format!("{HUB_API_PREFIX}.$KV.{RESIDENCE_BUCKET}.>"),
                format!("$JS.FC.{HUB_IDLOG_STREAM}.*.*"),
                format!("$JS.FC.{RESIDENCE_STREAM}.*.*"),
            ],
        ),
        Module::new(
            box_id,
            "objects",
            &[CUTS_STREAM],
            vec![
                format!("$O.{CUTS_BUCKET}.>"),
                api("STREAM.MSG.GET", CUTS_STREAM),
                api("STREAM.PURGE", CUTS_STREAM),
                format!("$JS.FC.{CUTS_STREAM}.*.*"),
            ],
        ),
    ]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the nats-server binary"]
async fn rig9_federation_users_reach_only_their_hub_streams() {
    let _serial = RIG_LOCK.lock().await;
    let box_id = BoxId::parse(BOX_ID).unwrap();
    let modules = shared_modules(&box_id);
    let rig = Topology::start(&box_id, &modules, false).await;

    let (memory, mut memory_errors) = rig.connect_module(0, &modules[0]).await;
    let (rooms, mut room_errors) = rig.connect_module(1, &modules[1]).await;
    let (identity, mut identity_errors) = rig.connect_module(2, &modules[2]).await;
    let (objects, mut object_errors) = rig.connect_module(3, &modules[3]).await;
    let memory_subject = hub_memory_subject(MEMORY_KIND_GLOBAL, "all").unwrap();
    let room_subject = hub_room_subject("rm_rig").unwrap();
    durable_roundtrip(&memory, HUB_MEMORY_STREAM, memory_subject.clone()).await;
    durable_roundtrip(&rooms, HUB_ROOMS_STREAM, room_subject.clone()).await;
    identity_log(&identity).await;
    residence(&identity, &rig).await;
    cuts(&objects, &rig).await;

    for (index, module) in modules.iter().enumerate() {
        let local = connect(&rig.local.url(), &rig.box_users[index]).await;
        let subject = format!("rig.local.{}", module.name);
        let mut messages = local.subscribe(subject.clone()).await.unwrap();
        local.publish(subject, "local only".into()).await.unwrap();
        let message = timeout(LIMIT, messages.next())
            .await
            .expect("separate BOX connection works")
            .unwrap();
        assert_eq!(
            message.payload.as_ref(),
            b"local only",
            "{} keeps a separate BOX user for local traffic",
            module.name
        );
    }

    let streams = shared_streams(&box_id, memory_subject, room_subject);
    for (index, (client, errors)) in [
        (&memory, &mut memory_errors),
        (&rooms, &mut room_errors),
        (&identity, &mut identity_errors),
        (&objects, &mut object_errors),
    ]
    .into_iter()
    .enumerate()
    {
        isolation(client, errors, &modules[index], &modules, &streams).await;
    }
    note(9, "PASS shared-stream operations and cross-module, per-machine, and internal federation denials");
}

fn shared_streams(
    box_id: &BoxId,
    memory_subject: String,
    room_subject: String,
) -> [(String, Vec<String>); 6] {
    let inbox = hub_inbox_stream(box_id, &hub_name());
    [
        (HUB_MEMORY_STREAM.into(), vec![memory_subject]),
        (HUB_ROOMS_STREAM.into(), vec![room_subject]),
        (HUB_IDLOG_STREAM.into(), vec![IDLOG_SUBJECT.into()]),
        (
            RESIDENCE_STREAM.into(),
            vec![
                format!("{HUB_API_PREFIX}.$KV.{RESIDENCE_BUCKET}.probe"),
                format!("$KV.{RESIDENCE_BUCKET}.probe"),
            ],
        ),
        (
            CUTS_STREAM.into(),
            vec![
                format!("$O.{CUTS_BUCKET}.M.probe"),
                format!("$O.{CUTS_BUCKET}.C.probe"),
            ],
        ),
        (
            inbox.stream.name,
            inbox
                .stream
                .subjects
                .iter()
                .map(|subject| subject.replace('>', "probe"))
                .collect(),
        ),
    ]
}

#[allow(dead_code)] // The standalone rig runs its own grants through the same probes.
pub(crate) async fn prove_memory(
    client: &async_nats::Client,
    errors: &mut mpsc::UnboundedReceiver<String>,
    box_id: &BoxId,
    modules: &[Module; 4],
) {
    let memory = hub_memory_subject(MEMORY_KIND_GLOBAL, "all").unwrap();
    durable_roundtrip(client, HUB_MEMORY_STREAM, memory.clone()).await;
    let streams = shared_streams(box_id, memory, hub_room_subject("rm_rig").unwrap());
    isolation(client, errors, &modules[0], modules, &streams).await;
}
