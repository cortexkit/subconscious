//! Hub area tests: setup, the four ops and removal, against fakes for callosum, the hub
//! instance and the vault.

use std::{
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nkeys::KeyPair;
use serde_json::{json, Value};
use subc_client_rs::HandlerOutcome;
use subc_protocol::{manifest::ModuleManifest, Principal};
use tokio::{sync::mpsc, time::Instant};

use super::{
    area::{issued, HubArea, HubDeps, HubFiles, HubTiming},
    cert::{certificate_public_key, spki_sha256_hex},
    conf::HUB_LEAF_PORT,
    devices::Devices,
    grant::leaf_permissions,
    handler::{advertise, forwarded_computer, query, Caller},
    hub_share, issue_code,
    record::{self, HubRecord, RecordFile},
    seams::{HubServer, HubStream, OwnMachine, PairedDevices},
    tls_code,
};
use crate::{
    bootstrap::account_jwt::{decode_claims, verify_self_named_issuer},
    credentials::{
        nkey::{encode_public, NkeyRole},
        roots::RootCredential,
    },
    federation::{
        leaf::{
            hub::HubView,
            seams::{Callosum, LocalHub},
        },
        test_support::KeyVault,
    },
};

const HUB_FP: &str = "11111111111111111111111111111111";
const PEER_FP: &str = "648aa5c579fb30f38af744d97d6ec840";
const PEER_BOX: &str = "853ecdb6e1681c3dc8c1c578d14c23a2";
const PHONE_FP: &str = "72cd6e8422c407fb6d098690f1130b7d";
const OWN_BOX: &str = "0123456789abcdef0123456789abcdef";

struct NoCallosum;

#[async_trait]
impl Callosum for NoCallosum {
    async fn hub_watch(&self) -> Result<mpsc::Receiver<Vec<u8>>, String> {
        Err("no watch in these tests".into())
    }
    async fn hub_read(&self) -> Result<Vec<u8>, String> {
        Err("no read in these tests".into())
    }
    async fn leaf_bridge_open(&self, _fingerprint: &str) -> Result<u16, String> {
        Err("no bridge".into())
    }
}

struct FakeDevices {
    devices: Mutex<Value>,
    peers: Mutex<Value>,
}

impl FakeDevices {
    fn new() -> Self {
        Self {
            devices: Mutex::new(json!({
                "devices": [
                    {"class": "computer", "fingerprint": PEER_FP},
                    {"class": "phone", "fingerprint": PHONE_FP},
                ],
                "self": {"static_public_key_hex": "00"},
                "tombstones": [],
            })),
            peers: Mutex::new(json!({
                "peers": [
                    {"fingerprint": PEER_FP, "machine_id": PEER_BOX},
                    {"fingerprint": PHONE_FP, "machine_id": "fedcba9876543210fedcba9876543210"},
                ],
            })),
        }
    }

    fn snapshot(&self) -> Devices {
        super::devices::parse_devices(&serde_json::to_vec(&*self.devices.lock().unwrap()).unwrap())
            .unwrap()
    }
}

#[async_trait]
impl PairedDevices for FakeDevices {
    async fn watch(&self) -> Result<mpsc::Receiver<Vec<u8>>, String> {
        Err("no watch in these tests".into())
    }
    async fn read(&self) -> Result<Vec<u8>, String> {
        Ok(serde_json::to_vec(&*self.devices.lock().unwrap()).unwrap())
    }
    async fn peer_keys(&self) -> Result<Vec<u8>, String> {
        Ok(serde_json::to_vec(&*self.peers.lock().unwrap()).unwrap())
    }
}

#[derive(Default)]
struct FakeServer {
    down: AtomicBool,
    applied: Mutex<Vec<(String, String)>>,
    streams: Mutex<Vec<HubStream>>,
    leafs: AtomicUsize,
}

#[async_trait]
impl HubServer for FakeServer {
    async fn apply_account(
        &self,
        _record: &HubRecord,
        account_public: &str,
        jwt: &str,
    ) -> Result<(), String> {
        if self.down.load(Ordering::SeqCst) {
            return Err("the hub instance is not up".into());
        }
        self.applied
            .lock()
            .unwrap()
            .push((account_public.to_string(), jwt.to_string()));
        Ok(())
    }

    async fn ensure_streams(
        &self,
        _record: &HubRecord,
        streams: &[HubStream],
    ) -> Result<(), String> {
        if self.down.load(Ordering::SeqCst) {
            return Err("the hub instance is not up".into());
        }
        let mut known = self.streams.lock().unwrap();
        for stream in streams {
            if !known.contains(stream) {
                known.push(stream.clone());
            }
        }
        Ok(())
    }

    async fn leaf_count(&self, _record: &HubRecord) -> Result<usize, String> {
        Ok(self.leafs.load(Ordering::SeqCst))
    }
}

struct FakeOwn;

#[async_trait]
impl OwnMachine for FakeOwn {
    async fn machine_id(&self) -> Result<String, String> {
        Ok(OWN_BOX.to_string())
    }
}

struct Fixture {
    area: Arc<HubArea>,
    vault: Arc<KeyVault>,
    server: Arc<FakeServer>,
    devices: Arc<FakeDevices>,
    files: HubFiles,
    _dir: tempfile::TempDir,
}

fn key(root: RootCredential) -> String {
    root.credential_id().unwrap()
}

impl Fixture {
    fn new() -> Self {
        Self::with(HubTiming::default(), None)
    }

    fn with(timing: HubTiming, local_server_url: Option<&str>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let vault = Arc::new(KeyVault::default());
        vault.mint(
            &key(RootCredential::BusHubOperator),
            KeyPair::new_operator(),
        );
        vault.mint(&key(RootCredential::BusHubAccount), KeyPair::new_account());
        vault.mint(&key(RootCredential::BusHubTls), KeyPair::new_user());
        let server = Arc::new(FakeServer::default());
        let devices = Arc::new(FakeDevices::new());
        let files = HubFiles {
            store_root: dir.path().join("ckbus"),
            hub_dir: dir.path().join("nats-hub"),
        };
        let area = HubArea::new(
            HubDeps {
                callosum: Arc::new(NoCallosum),
                devices: devices.clone(),
                vault: vault.clone(),
                server: server.clone(),
                own: Arc::new(FakeOwn),
                files: files.clone(),
                local_server_url: local_server_url.map(str::to_string),
            },
            timing,
        );
        Self {
            area,
            vault,
            server,
            devices,
            files,
            _dir: dir,
        }
    }

    async fn become_hub(&self) {
        self.area.apply_hub(HubView::SelfHub {
            fingerprint: HUB_FP.to_string(),
        });
        self.area.step(Instant::now()).await;
    }

    fn record(&self) -> HubRecord {
        match record::read(&self.files.store_root) {
            RecordFile::Present(record) => *record,
            _ => panic!("no hub record"),
        }
    }

    fn metric(&self, name: &str) -> Value {
        self.area.health_metrics()[name].clone()
    }
}

fn leaf_nkey() -> String {
    KeyPair::new_user().public_key()
}

fn certificate_verify(hash_len: usize) -> Vec<u8> {
    let mut message = vec![b' '; 64];
    message.extend_from_slice(b"TLS 1.3, server CertificateVerify\0");
    message.extend(std::iter::repeat_n(7, hash_len));
    message
}

#[tokio::test]
async fn a_computer_that_is_not_the_hub_sends_no_listener_and_answers_no_hub() {
    let fixture = Fixture::new();
    fixture.area.step(Instant::now()).await;
    assert_eq!(fixture.area.current().listener, None);
    let state = fixture.area.current().to_json();
    assert!(state["generation"].as_u64().unwrap() > 0);
    assert_eq!(state.get("hub_listener"), None);
    assert_eq!(fixture.area.identity_json(), json!({ "hub": null }));
    assert_eq!(fixture.metric("fed.hub_setup"), json!("not_hub"));
    assert_eq!(
        fixture
            .area
            .leaf_issue(PEER_FP, &leaf_nkey())
            .await
            .unwrap_err()
            .code,
        issue_code::NOT_HUB
    );
    assert!(!fixture.files.server_conf().exists());
    assert!(
        fixture.vault.calls.lock().unwrap().is_empty(),
        "no key is read"
    );
}

#[tokio::test]
async fn setup_writes_the_conf_and_record_then_listens_and_brings_the_instance_up() {
    let fixture = Fixture::new();
    fixture.become_hub().await;

    let record = fixture.record();
    let tls_public = fixture.vault.raw_public(&key(RootCredential::BusHubTls));
    assert_eq!(certificate_public_key(&record.cert_der), Some(tls_public));
    assert_eq!(record.spki_sha256, spki_sha256_hex(&tls_public));

    // The operator JWT is self-signed by the vault's hub operator key and names the
    // system account; server.conf preloads the system and hub accounts.
    let operator = std::fs::read_to_string(fixture.files.operator_jwt()).unwrap();
    let claims = verify_self_named_issuer(&operator).unwrap();
    assert_eq!(claims["sub"], json!(record.operator_public));
    assert_eq!(
        nkeys::from_public_key(&record.operator_public).unwrap().1,
        fixture
            .vault
            .raw_public(&key(RootCredential::BusHubOperator))
    );
    assert_eq!(
        claims["nats"]["system_account"],
        json!(record.system_account)
    );
    let conf = std::fs::read_to_string(fixture.files.server_conf()).unwrap();
    assert!(conf.contains(&format!("system_account: {}", record.system_account)));
    assert!(conf.contains(&format!("  {}: \"", record.hub_account)));
    assert!(conf.contains("domain: hub"));

    // The listener is published with the stored certificate.
    let state = fixture.area.current().to_json();
    assert_eq!(state["hub_listener"]["port"], json!(HUB_LEAF_PORT));
    assert_eq!(
        URL_SAFE_NO_PAD
            .decode(state["hub_listener"]["cert_der_b64"].as_str().unwrap())
            .unwrap(),
        record.cert_der
    );
    assert_eq!(
        fixture.area.identity_json(),
        json!({ "spki_sha256": record.spki_sha256, "leaf_port": 7422 })
    );

    // The hub account JWT, signed by the operator, lists the hub account key.
    let applied = fixture.server.applied.lock().unwrap().clone();
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].0, record.hub_account);
    let account = verify_self_named_issuer(&applied[0].1).unwrap();
    assert_eq!(account["iss"], json!(record.operator_public));
    assert_eq!(account["sub"], json!(record.hub_account));
    assert_eq!(
        nkeys::from_public_key(account["nats"]["signing_keys"][0].as_str().unwrap())
            .unwrap()
            .1,
        fixture
            .vault
            .raw_public(&key(RootCredential::BusHubAccount))
    );
    assert_eq!(
        *fixture.server.streams.lock().unwrap(),
        [
            HubStream::IdLog,
            HubStream::Residence,
            HubStream::Cuts,
            HubStream::Rooms,
            HubStream::Memory
        ]
    );
    assert_eq!(fixture.metric("fed.hub_setup"), json!("ready"));
    assert_eq!(fixture.metric("fed.hub_setup_error"), Value::Null);

    // A restart reuses the record: no second setup, the same certificate bytes.
    let generation = fixture.area.current().generation;
    let restarted = HubArea::new(
        HubDeps {
            callosum: Arc::new(NoCallosum),
            devices: fixture.devices.clone(),
            vault: fixture.vault.clone(),
            server: fixture.server.clone(),
            own: Arc::new(FakeOwn),
            files: fixture.files.clone(),
            local_server_url: None,
        },
        HubTiming::default(),
    );
    restarted.apply_hub(HubView::SelfHub {
        fingerprint: HUB_FP.to_string(),
    });
    restarted.step(Instant::now()).await;
    assert_eq!(restarted.current().listener, Some(record.cert_der.clone()));
    assert!(restarted.current().generation >= generation);
    assert_eq!(fixture.record().operator_public, record.operator_public);
    assert_eq!(fixture.record().hub_account, record.hub_account);
}

#[tokio::test]
async fn setup_stays_setting_up_until_the_instance_answers() {
    let fixture = Fixture::new();
    fixture.server.down.store(true, Ordering::SeqCst);
    fixture.become_hub().await;
    // The listener goes out at once, because the instance starts only on it.
    assert!(fixture.area.current().listener.is_some());
    assert_eq!(fixture.metric("fed.hub_setup"), json!("setting_up"));
    assert!(fixture.metric("fed.hub_setup_error").is_string());
    assert_eq!(
        fixture
            .area
            .leaf_issue(PEER_FP, &leaf_nkey())
            .await
            .unwrap_err()
            .code,
        issue_code::HUB_NOT_READY
    );
}

#[tokio::test]
async fn setup_refuses_an_admin_port_the_local_server_holds() {
    let fixture = Fixture::with(HubTiming::default(), Some("nats://127.0.0.1:17422"));
    fixture.become_hub().await;
    assert_eq!(fixture.metric("fed.hub_setup"), json!("failed"));
    assert!(fixture
        .metric("fed.hub_setup_error")
        .as_str()
        .unwrap()
        .contains("17422"));
    assert!(!fixture.files.server_conf().exists());
    assert_eq!(fixture.area.current().listener, None);
}

#[tokio::test]
async fn leaving_the_hub_role_withdraws_the_listener_and_keeps_the_store() {
    let fixture = Fixture::new();
    fixture.become_hub().await;
    let generation = fixture.area.current().generation;
    fixture.area.apply_hub(HubView::None);
    fixture.area.step(Instant::now()).await;
    let state = fixture.area.current();
    assert_eq!(state.listener, None);
    assert!(state.generation > generation);
    assert_eq!(fixture.metric("fed.hub_setup"), json!("not_hub"));
    assert_eq!(fixture.area.identity_json(), json!({ "hub": null }));
    assert!(fixture.files.server_conf().exists());
}

#[tokio::test]
async fn a_live_paired_computer_gets_a_24_hour_jwt_with_exactly_its_boxs_grant() {
    let fixture = Fixture::new();
    fixture.become_hub().await;
    let record = fixture.record();
    let nkey = leaf_nkey();
    let jwt = fixture.area.leaf_issue(PEER_FP, &nkey).await.unwrap();
    let claims = verify_self_named_issuer(&jwt).unwrap();
    assert_eq!(claims["sub"], json!(nkey));
    assert_eq!(
        nkeys::from_public_key(claims["iss"].as_str().unwrap())
            .unwrap()
            .1,
        fixture
            .vault
            .raw_public(&key(RootCredential::BusHubAccount))
    );
    assert_eq!(claims["nats"]["issuer_account"], json!(record.hub_account));
    assert_eq!(
        claims["exp"].as_i64().unwrap() - claims["iat"].as_i64().unwrap(),
        86_400
    );
    let (publish, subscribe) = leaf_permissions(PEER_BOX).unwrap();
    assert_eq!(claims["nats"]["pub"], json!({ "allow": publish }));
    assert_eq!(claims["nats"]["sub"], json!({ "allow": subscribe }));
    assert!(fixture
        .server
        .streams
        .lock()
        .unwrap()
        .contains(&HubStream::Inbox {
            box_id: PEER_BOX.to_string()
        }));
    assert_eq!(
        issued(&fixture.area)[PEER_FP],
        (PEER_BOX.to_string(), nkey.clone())
    );
    assert_eq!(fixture.metric("fed.hub_issued"), json!(1));
    // Idempotent: a re-issue keeps one record.
    fixture.area.leaf_issue(PEER_FP, &nkey).await.unwrap();
    assert_eq!(fixture.record().issued.len(), 1);
}

#[tokio::test]
async fn issue_is_refused_for_anything_but_a_live_computer_with_a_valid_machine_id() {
    let fixture = Fixture::new();
    fixture.become_hub().await;
    let code = |result: Result<String, super::area::Refusal>| result.unwrap_err().code;
    // A phone, and a fingerprint callosum does not list.
    assert_eq!(
        code(fixture.area.leaf_issue(PHONE_FP, &leaf_nkey()).await),
        issue_code::NOT_PAIRED
    );
    assert_eq!(
        code(fixture.area.leaf_issue(&"a".repeat(32), &leaf_nkey()).await),
        issue_code::NOT_PAIRED
    );
    // A tombstoned computer, even while still listed.
    fixture.devices.devices.lock().unwrap()["tombstones"] =
        json!([{"fingerprint": PEER_FP, "removal_id": "1:permanent:1"}]);
    assert_eq!(
        code(fixture.area.leaf_issue(PEER_FP, &leaf_nkey()).await),
        issue_code::NOT_PAIRED
    );
    fixture.devices.devices.lock().unwrap()["tombstones"] = json!([]);
    // No machine id, or one in another spelling.
    fixture.devices.peers.lock().unwrap()["peers"][0]
        .as_object_mut()
        .unwrap()
        .remove("machine_id");
    assert_eq!(
        code(fixture.area.leaf_issue(PEER_FP, &leaf_nkey()).await),
        issue_code::NO_MACHINE_ID
    );
    fixture.devices.peers.lock().unwrap()["peers"][0]["machine_id"] =
        json!(PEER_BOX.to_uppercase());
    assert_eq!(
        code(fixture.area.leaf_issue(PEER_FP, &leaf_nkey()).await),
        issue_code::INVALID_MACHINE_ID
    );
    // Not a user nkey.
    assert_eq!(
        code(fixture.area.leaf_issue(PEER_FP, "UNOTAKEY").await),
        issue_code::BAD_REQUEST
    );
    assert!(fixture.record().issued.is_empty());
}

#[tokio::test]
async fn a_removed_computer_is_revoked_pushed_and_refused_afterwards() {
    let fixture = Fixture::new();
    fixture.become_hub().await;
    let nkey = leaf_nkey();
    fixture.area.leaf_issue(PEER_FP, &nkey).await.unwrap();
    // Its own machine's credential is never treated as a removed peer.
    fixture.area.issue(&leaf_nkey()).await.unwrap();
    fixture.area.apply_devices(fixture.devices.snapshot());
    fixture.area.step(Instant::now()).await;
    assert_eq!(fixture.record().issued.len(), 2, "both still live");

    // A local unpair shows as the fingerprint leaving the list.
    fixture.devices.devices.lock().unwrap()["devices"] =
        json!([{"class": "phone", "fingerprint": PHONE_FP}]);
    fixture.area.apply_devices(fixture.devices.snapshot());
    let pushed_before = fixture.server.applied.lock().unwrap().len();
    fixture.area.step(Instant::now()).await;

    let record = fixture.record();
    assert!(!record.issued.contains_key(PEER_FP));
    assert!(record.issued.contains_key(HUB_FP));
    assert_eq!(record.removed[PEER_FP].leaf_nkey, nkey);
    assert!(record.revocations.contains_key(&nkey));
    assert!(record.revocations_pushed);
    let applied = fixture.server.applied.lock().unwrap().clone();
    assert_eq!(applied.len(), pushed_before + 1);
    let account = decode_claims(&applied.last().unwrap().1).unwrap();
    assert!(account["nats"]["revocations"][&nkey].is_i64());

    // A tombstone keeps it refused even if it reappears in the list.
    fixture.devices.devices.lock().unwrap()["devices"] =
        json!([{"class": "computer", "fingerprint": PEER_FP}]);
    fixture.devices.devices.lock().unwrap()["tombstones"] =
        json!([{"fingerprint": PEER_FP, "removal_id": "1:permanent:1"}]);
    assert_eq!(
        fixture
            .area
            .leaf_issue(PEER_FP, &nkey)
            .await
            .unwrap_err()
            .code,
        issue_code::NOT_PAIRED
    );
}

#[tokio::test]
async fn a_revocation_waits_for_the_instance_and_is_pushed_when_it_returns() {
    let fixture = Fixture::new();
    fixture.become_hub().await;
    let nkey = leaf_nkey();
    fixture.area.leaf_issue(PEER_FP, &nkey).await.unwrap();
    fixture.server.down.store(true, Ordering::SeqCst);
    fixture.devices.devices.lock().unwrap()["tombstones"] =
        json!([{"fingerprint": PEER_FP, "removal_id": "1:permanent:1"}]);
    fixture.area.apply_devices(fixture.devices.snapshot());
    fixture.area.step(Instant::now()).await;
    assert!(!fixture.record().revocations_pushed);
    fixture.server.down.store(false, Ordering::SeqCst);
    // The retry backoff starts at one second.
    fixture
        .area
        .step(Instant::now() + Duration::from_secs(2))
        .await;
    assert!(fixture.record().revocations_pushed);
}

#[tokio::test]
async fn the_hub_machines_own_leaf_uses_its_machine_id_and_the_local_identity() {
    let fixture = Fixture::new();
    let local: &dyn LocalHub = fixture.area.as_ref();
    assert!(local.identity().await.is_err());
    fixture.become_hub().await;
    let identity = local.identity().await.unwrap();
    assert_eq!(identity.leaf_port, 7422);
    assert_eq!(identity.spki_sha256, fixture.record().spki_sha256);
    let nkey = leaf_nkey();
    let jwt = local.issue(&nkey).await.unwrap();
    let (publish, _) = leaf_permissions(OWN_BOX).unwrap();
    assert_eq!(
        decode_claims(&jwt).unwrap()["nats"]["pub"]["allow"],
        json!(publish)
    );
    assert_eq!(issued(&fixture.area)[HUB_FP], (OWN_BOX.to_string(), nkey));
}

// Paused time: the bucket refills at 20 a second, so a real clock could hand back a
// token between the calls and hide the refusal.
#[tokio::test(start_paused = true)]
async fn tls_signing_takes_only_certificate_verify_content_and_stops_at_the_burst() {
    let fixture = Fixture::new();
    fixture.become_hub().await;
    let tls = KeyPair::from_public_key(&encode_public(
        NkeyRole::User,
        &fixture.vault.raw_public(&key(RootCredential::BusHubTls)),
    ))
    .unwrap();
    let message = certificate_verify(32);
    let signed = fixture
        .area
        .tls_sign(&URL_SAFE_NO_PAD.encode(&message), "ed25519")
        .await
        .unwrap();
    let signature = URL_SAFE_NO_PAD
        .decode(signed["signature_b64"].as_str().unwrap())
        .unwrap();
    tls.verify(&message, &signature).unwrap();

    let refusal = |result: Result<Value, super::area::Refusal>| result.unwrap_err().code;
    assert_eq!(
        refusal(
            fixture
                .area
                .tls_sign(&URL_SAFE_NO_PAD.encode(b"anything else"), "ed25519")
                .await
        ),
        tls_code::INVALID_MESSAGE
    );
    assert_eq!(
        refusal(
            fixture
                .area
                .tls_sign(&URL_SAFE_NO_PAD.encode(certificate_verify(48)), "rsa")
                .await
        ),
        tls_code::BAD_REQUEST
    );
    // Three of the burst of 40 are spent; 37 more pass and the next is refused.
    let encoded = URL_SAFE_NO_PAD.encode(certificate_verify(48));
    for _ in 0..37 {
        fixture.area.tls_sign(&encoded, "ed25519").await.unwrap();
    }
    let signs = fixture.vault.calls.lock().unwrap().len();
    assert_eq!(
        refusal(fixture.area.tls_sign(&encoded, "ed25519").await),
        tls_code::RATE_LIMITED
    );
    assert_eq!(
        fixture.vault.calls.lock().unwrap().len(),
        signs,
        "a refused handshake never reaches the vault"
    );
}

#[tokio::test]
async fn a_rotated_tls_key_reissues_the_certificate_and_keeps_the_accounts() {
    let timing = HubTiming {
        key_poll: Duration::ZERO,
        ..HubTiming::default()
    };
    let fixture = Fixture::with(timing, None);
    fixture.become_hub().await;
    let before = fixture.record();
    fixture
        .vault
        .mint(&key(RootCredential::BusHubTls), KeyPair::new_user());
    // A signature under the new key is refused until the certificate names it.
    assert_eq!(
        fixture
            .area
            .tls_sign(&URL_SAFE_NO_PAD.encode(certificate_verify(32)), "ed25519")
            .await
            .unwrap_err()
            .code,
        tls_code::KEY_ROTATED
    );
    fixture.area.step(Instant::now()).await;
    let after = fixture.record();
    assert_ne!(after.spki_sha256, before.spki_sha256);
    assert_ne!(after.cert_der, before.cert_der);
    assert_eq!(after.hub_account, before.hub_account);
    assert_eq!(after.system_account, before.system_account);
    assert_eq!(fixture.area.current().listener, Some(after.cert_der));
}

fn caller(principal: Option<Principal>, harness: &str) -> Caller {
    Caller {
        principal,
        harness: harness.to_string(),
    }
}

fn reserved(module_id: &str) -> Option<Principal> {
    Some(Principal::Reserved {
        module_id: module_id.to_string(),
    })
}

fn outcome_code(outcome: Option<HandlerOutcome>) -> String {
    match outcome {
        Some(HandlerOutcome::Error { code, .. }) => code,
        Some(HandlerOutcome::Response(body)) => {
            format!("response {}", String::from_utf8_lossy(&body))
        }
        _ => "no answer".to_string(),
    }
}

#[test]
fn only_callosum_with_a_fed_computer_harness_names_a_forwarded_computer() {
    let fed = format!("fed-computer:{PEER_FP}");
    assert_eq!(
        forwarded_computer(Some(&caller(reserved("callosum"), &fed))).as_deref(),
        Some(PEER_FP)
    );
    for refused in [
        caller(reserved("nats-hub"), &fed),
        caller(Some(Principal::Direct), &fed),
        caller(None, &fed),
        caller(reserved("callosum"), "ck-bus"),
        caller(reserved("callosum"), "fed-computer:ABC"),
        caller(
            reserved("callosum"),
            &format!("fed-computer:{}", PEER_FP.to_uppercase()),
        ),
    ] {
        assert_eq!(forwarded_computer(Some(&refused)), None, "{refused:?}");
    }
}

#[tokio::test]
async fn each_op_answers_only_its_admitted_caller() {
    let fixture = Fixture::new();
    fixture.become_hub().await;
    let request = |method: &str, params: Value| json!({ "method": method, "params": params });
    let tls = request(
        "ckbus.hub_tls_sign",
        json!({ "message_b64": URL_SAFE_NO_PAD.encode(certificate_verify(32)), "scheme": "ed25519" }),
    );
    let identity = request("ckbus.hub_identity_read", json!({}));
    let issue = request("ckbus.hub_leaf_issue", json!({ "leaf_nkey": leaf_nkey() }));
    let hub = caller(reserved("nats-hub"), "ck-nats-server");
    let callosum = caller(reserved("callosum"), "callosum");
    let peer = caller(reserved("callosum"), &format!("fed-computer:{PEER_FP}"));

    assert!(outcome_code(query(&fixture.area, Some(&hub), &tls).await).starts_with("response"));
    assert_eq!(
        outcome_code(query(&fixture.area, Some(&callosum), &tls).await),
        "not admitted"
    );
    assert!(
        outcome_code(query(&fixture.area, Some(&callosum), &identity).await)
            .starts_with("response")
    );
    assert_eq!(
        outcome_code(query(&fixture.area, Some(&hub), &identity).await),
        "not admitted"
    );
    let issued = outcome_code(query(&fixture.area, Some(&peer), &issue).await);
    let body: Value = serde_json::from_str(issued.strip_prefix("response ").unwrap()).unwrap();
    assert!(body["result"]["jwt"].as_str().unwrap().contains('.'));
    assert_eq!(body.as_object().unwrap().len(), 1);
    for refused in [&callosum, &hub] {
        assert_eq!(
            outcome_code(query(&fixture.area, Some(refused), &issue).await),
            "not admitted"
        );
    }
    assert_eq!(
        outcome_code(query(&fixture.area, None, &request("ckbus.other", json!({}))).await),
        "no answer"
    );
}

#[test]
fn the_share_and_the_watch_are_declared() {
    use subc_protocol::manifest::{Concurrency, ProviderRole};
    let manifest = ModuleManifest::builder("ckbus", "0.0.0")
        .provides(vec![ProviderRole::ManagementSurface {
            operations: vec![],
            config_schema: json!({}),
            observability: vec![],
            identity_scope: vec![],
            concurrency: Concurrency::ModuleManaged,
        }])
        .build();
    let manifest = advertise(manifest);
    assert!(manifest.is_watch_operation("ckbus.hub_server_watch"));
    for op in [
        "ckbus.hub_tls_sign",
        "ckbus.hub_identity_read",
        "ckbus.hub_leaf_issue",
    ] {
        assert!(!manifest.is_watch_operation(op));
        assert!(manifest.provides.iter().any(|role| matches!(
            role,
            ProviderRole::ManagementSurface { operations, .. }
                if operations.iter().any(|declared| declared.name == op)
        )));
    }
    assert_eq!(
        serde_json::to_string(&manifest.role_declarations["peer-sharing/v1"]).unwrap(),
        r#"{"shares":[{"id":"hub","label":"Sign up for this computer's hub","operations":["ckbus.hub_leaf_issue"],"peer_classes":["computer"]}]}"#
    );
    assert_eq!(hub_share()["operations"], json!(["ckbus.hub_leaf_issue"]));
}
