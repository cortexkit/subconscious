//! The leaf area against fake callosum, hub, vault and leaf report. Time is passed to
//! `step` explicitly, so every deadline is checked without sleeping.

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::Duration,
};

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use nkeys::KeyPair;
use serde_json::{json, Value};
use subc_client_rs::{HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::{
    manifest::ModuleManifest,
    session::{HealthReport, HealthStatus},
    Principal,
};
use tokio::{sync::mpsc, time::Instant};

use super::{
    area::{follow_hub, LeafArea, LeafDeps, Timing},
    handler::{admitted, advertise, LeafHandler},
    hub::{Candidate, HubView},
    seams::{
        AlwaysQuiet, Callosum, FederationAccounts, LeafLinks, LocalHub, LocalHubIdentity,
        NoLocalHub, OutboundQuiet, PeerIssuer,
    },
    sign_code,
};
use crate::{
    bootstrap::plane::LeafLink, credentials::roots::RootCredential,
    federation::test_support::KeyVault,
};

const HUB: &str = "11111111111111111111111111111111";
const PIN: &str = "abababababababababababababababababababababababababababababababab";
const ACCOUNT: &str = "AFEDERATIONACCOUNTFORTESTS";
/// What nats-server sends: 11 random bytes in unpadded base64url.
const NONCE: &str = "abcdefghijklmn0";
const WALL_START: i64 = 100_000;
const JWT_LIFE: i64 = 86_400;
const HALF_LIFE: u64 = JWT_LIFE as u64 / 2;

fn leaf_id() -> String {
    RootCredential::BusLeaf.credential_id().unwrap()
}

/// An unsigned JWT with the claims the area reads; the hub's signature is not its
/// concern.
fn jwt_at(subject: &str, now: i64, publish: &[String]) -> String {
    static ISSUED: AtomicUsize = AtomicUsize::new(0);
    let jti = ISSUED.fetch_add(1, Ordering::SeqCst);
    format!(
        "{}.{}.sig",
        URL_SAFE_NO_PAD.encode(br#"{"typ":"JWT","alg":"ed25519-nkey"}"#),
        URL_SAFE_NO_PAD.encode(
            json!({"jti": jti, "sub": subject, "iat": now, "exp": now + JWT_LIFE,
                "nats": {"pub": {"allow": publish}}})
            .to_string()
        )
    )
}

#[derive(Default)]
struct FakeCallosum {
    watch_up: AtomicBool,
    reads: AtomicUsize,
    bridges: Mutex<Vec<String>>,
}

#[async_trait]
impl Callosum for FakeCallosum {
    async fn hub_watch(&self) -> Result<mpsc::Receiver<Vec<u8>>, String> {
        if !self.watch_up.load(Ordering::SeqCst) {
            return Err("callosum is not running".to_string());
        }
        let (notices, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            notices.send(br#"{"hub":null}"#.to_vec()).await.ok();
            // Held open for the rest of the test.
            std::future::pending::<()>().await;
            drop(notices);
        });
        Ok(receiver)
    }

    async fn hub_read(&self) -> Result<Vec<u8>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Ok(br#"{"hub":null}"#.to_vec())
    }

    async fn leaf_bridge_open(&self, fingerprint: &str) -> Result<u16, String> {
        self.bridges.lock().unwrap().push(fingerprint.to_string());
        Ok(49152)
    }
}

/// Signs JWTs on the fixture's wall clock, after any configured failures.
struct FakeIssuer {
    calls: Mutex<Vec<(String, String)>>,
    failures: Mutex<usize>,
    clock: Arc<AtomicI64>,
    publish: Mutex<Vec<String>>,
}

#[async_trait]
impl PeerIssuer for FakeIssuer {
    async fn hub_leaf_issue(&self, hub: &str, leaf_nkey: &str) -> Result<String, String> {
        self.calls
            .lock()
            .unwrap()
            .push((hub.to_string(), leaf_nkey.to_string()));
        let mut failures = self.failures.lock().unwrap();
        if *failures > 0 {
            *failures -= 1;
            return Err("the hub is unreachable".to_string());
        }
        Ok(jwt_at(
            leaf_nkey,
            self.clock.load(Ordering::SeqCst),
            &self.publish.lock().unwrap(),
        ))
    }
}

struct FakeLocalHub {
    issued: AtomicUsize,
    pin: Mutex<String>,
}

#[async_trait]
impl LocalHub for FakeLocalHub {
    async fn identity(&self) -> Result<LocalHubIdentity, String> {
        Ok(LocalHubIdentity {
            spki_sha256: self.pin.lock().unwrap().clone(),
            leaf_port: 7422,
        })
    }

    async fn issue(&self, leaf_nkey: &str) -> Result<String, String> {
        self.issued.fetch_add(1, Ordering::SeqCst);
        Ok(jwt_at(leaf_nkey, WALL_START, &[]))
    }
}

#[derive(Default)]
struct FakeLinks {
    up: AtomicBool,
    /// The peer address an up link reports; `None` means a direct link to 10.0.0.1:7422.
    peer: Mutex<Option<(String, u16)>>,
    kicks: Mutex<Vec<u64>>,
}

#[async_trait]
impl LeafLinks for FakeLinks {
    async fn leafs(&self, account: &str) -> Result<Vec<LeafLink>, String> {
        assert_eq!(account, ACCOUNT);
        Ok(if self.up.load(Ordering::SeqCst) {
            let (ip, port) = self
                .peer
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| ("10.0.0.1".to_string(), 7422));
            vec![LeafLink {
                server_id: "NSERVER".to_string(),
                id: 7,
                ip,
                port,
            }]
        } else {
            Vec::new()
        })
    }

    async fn kick(&self, link: &LeafLink) -> Result<(), String> {
        self.kicks.lock().unwrap().push(link.id);
        Ok(())
    }
}

struct FixedAccount;

#[async_trait]
impl FederationAccounts for FixedAccount {
    async fn ensure(&self) -> Result<String, String> {
        Ok(ACCOUNT.to_string())
    }
}

struct NeverQuiet;

impl OutboundQuiet for NeverQuiet {
    fn quiet_for(&self, _window: Duration) -> bool {
        false
    }
}

struct Fixture {
    area: Arc<LeafArea>,
    vault: Arc<KeyVault>,
    issuer: Arc<FakeIssuer>,
    callosum: Arc<FakeCallosum>,
    links: Arc<FakeLinks>,
    t0: Instant,
    clock: Arc<AtomicI64>,
}

impl Fixture {
    fn new() -> Self {
        Self::with(Arc::new(NoLocalHub), Arc::new(AlwaysQuiet))
    }

    fn with(local_hub: Arc<dyn LocalHub>, quiet: Arc<dyn OutboundQuiet>) -> Self {
        let vault = Arc::new(KeyVault::default());
        vault.mint(&leaf_id(), KeyPair::new_user());
        let clock = Arc::new(AtomicI64::new(WALL_START));
        let issuer = Arc::new(FakeIssuer {
            publish: Mutex::new(Vec::new()),
            clock: clock.clone(),
            calls: Mutex::default(),
            failures: Mutex::default(),
        });
        let callosum = Arc::new(FakeCallosum::default());
        let links = Arc::new(FakeLinks::default());
        let deps = LeafDeps {
            callosum: callosum.clone(),
            issuer: issuer.clone(),
            local_hub,
            vault: vault.clone(),
            links: links.clone(),
            quiet,
            accounts: Arc::new(FixedAccount),
        };
        let timing = Timing {
            // The report is read on every step, so each step sees the fake's answer.
            link_poll: Duration::ZERO,
            ..Timing::default()
        };
        let area_clock = clock.clone();
        let area = LeafArea::with_clock(deps, timing, move || area_clock.load(Ordering::SeqCst));
        Self {
            area,
            vault,
            issuer,
            callosum,
            links,
            t0: Instant::now(),
            clock,
        }
    }

    async fn at(&self, seconds: u64) {
        self.clock
            .store(WALL_START + seconds as i64, Ordering::SeqCst);
        self.area.step(self.t0 + Duration::from_secs(seconds)).await;
    }

    fn leaf_nkey(&self) -> String {
        crate::credentials::nkey::encode_public(
            crate::credentials::nkey::NkeyRole::User,
            &self.vault.raw_public(&leaf_id()),
        )
    }

    fn remotes(&self) -> Vec<Value> {
        self.area
            .current()
            .remotes
            .iter()
            .map(|remote| remote.to_json())
            .collect()
    }

    fn metric(&self, name: &str) -> Value {
        self.area.health_metrics()[name].clone()
    }
}

#[tokio::test]
async fn wall_clock_jump_refuses_leaf_signature_and_renews_without_monotonic_advance() {
    let fixture = Fixture::new();
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    fixture.at(0).await;
    let first = fixture.area.sign(&name(&fixture), NONCE).await.unwrap();

    fixture
        .clock
        .store(WALL_START + JWT_LIFE - 60, Ordering::SeqCst);
    let refused = fixture.area.sign(&name(&fixture), NONCE).await.unwrap_err();
    assert_eq!(refused.code, sign_code::NO_CREDENTIAL);
    fixture
        .clock
        .store(WALL_START + JWT_LIFE + 1, Ordering::SeqCst);
    fixture.area.step(fixture.t0).await;
    let renewed = fixture.area.sign(&name(&fixture), NONCE).await.unwrap();
    assert_ne!(first["jwt"], renewed["jwt"]);
    assert_eq!(fixture.issuer.calls.lock().unwrap().len(), 2);
}

fn peer(candidates: &[(&str, u16)]) -> HubView {
    HubView::Peer {
        fingerprint: HUB.to_string(),
        spki_sha256: PIN.to_string(),
        candidates: candidates
            .iter()
            .map(|(host, port)| Candidate {
                host: host.to_string(),
                port: *port,
            })
            .collect(),
    }
}

#[tokio::test]
async fn no_hub_means_no_remote_and_neutral_health() {
    let fixture = Fixture::new();
    fixture.at(0).await;
    assert!(fixture.remotes().is_empty());
    assert_eq!(fixture.metric("fed.hub_kind"), "none");
    assert_eq!(fixture.metric("fed.remote_state"), "none");
    assert_eq!(fixture.metric("fed.jwt_expires_at_ms"), Value::Null);
    assert!(fixture.issuer.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_peer_hub_gets_one_pinned_remote_with_the_hubs_jwt() {
    let fixture = Fixture::new();
    fixture
        .area
        .apply_hub(peer(&[("192.168.1.46", 7422), ("10.0.0.9", 7422)]));
    fixture.at(0).await;

    assert_eq!(
        *fixture.issuer.calls.lock().unwrap(),
        vec![(HUB.to_string(), fixture.leaf_nkey())],
        "the JWT is asked of the hub computer, for the vault key's public half"
    );
    let remotes = fixture.remotes();
    assert_eq!(remotes.len(), 1);
    let remote = &remotes[0];
    assert_eq!(remote["account"], ACCOUNT);
    assert_eq!(
        remote["urls"],
        json!(["tls://192.168.1.46:7422", "tls://10.0.0.9:7422"])
    );
    assert_eq!(remote["pin"], json!({"kind": "spki", "sha256_hex": PIN}));
    assert_eq!(remote["deny_imports"], json!(["fedlocal.>", "fedout.>"]));
    assert_eq!(remote["deny_exports"], json!(["fedlocal.>", "fedout.>"]));
    assert_eq!(fixture.metric("fed.hub_kind"), "peer");
    assert_eq!(fixture.metric("fed.remote_state"), "connecting");
    assert!(fixture.metric("fed.jwt_expires_at_ms").as_u64().is_some());

    fixture.links.up.store(true, Ordering::SeqCst);
    let generation = fixture.area.current().generation;
    fixture.at(1).await;
    assert_eq!(fixture.metric("fed.remote_state"), "connected");
    assert_eq!(
        fixture.area.current().generation,
        generation,
        "an unchanged remote is not sent again"
    );
}

#[tokio::test]
async fn generations_strictly_increase_and_start_at_wall_clock() {
    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let fixture = Fixture::new();
    let first = fixture.area.current().generation;
    assert!(first >= before);
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    fixture.at(0).await;
    let second = fixture.area.current().generation;
    fixture.area.apply_hub(HubView::None);
    fixture.at(1).await;
    let third = fixture.area.current().generation;
    assert!(first < second && second < third, "{first} {second} {third}");
    assert!(
        fixture.remotes().is_empty(),
        "a removed hub drops its remote"
    );
}

#[tokio::test]
async fn refused_and_hosted_hubs_get_no_remote() {
    let fixture = Fixture::new();
    fixture.area.apply_hub(HubView::Refused {
        reason: "callosum named 2 hubs; exactly one is allowed".to_string(),
    });
    fixture.at(0).await;
    assert!(fixture.remotes().is_empty());
    assert_eq!(fixture.metric("fed.hub_kind"), "refused");
    fixture.area.apply_hub(HubView::Hosted);
    fixture.at(1).await;
    assert!(fixture.remotes().is_empty());
    assert_eq!(fixture.metric("fed.remote_state"), "hosted_unsupported");
    assert!(fixture.issuer.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_self_hub_waits_for_the_local_hub_seam() {
    let fixture = Fixture::new();
    fixture.area.apply_hub(HubView::SelfHub {
        fingerprint: HUB.to_string(),
    });
    fixture.at(0).await;
    assert!(fixture.remotes().is_empty());
    assert_eq!(fixture.metric("fed.remote_state"), "self_hub_unavailable");

    let local = Arc::new(FakeLocalHub {
        issued: AtomicUsize::new(0),
        pin: Mutex::new("cd".repeat(32)),
    });
    let fixture = Fixture::with(local.clone(), Arc::new(AlwaysQuiet));
    fixture.area.apply_hub(HubView::SelfHub {
        fingerprint: HUB.to_string(),
    });
    fixture.at(0).await;
    let remotes = fixture.remotes();
    assert_eq!(remotes[0]["urls"], json!(["tls://127.0.0.1:7422"]));
    assert_eq!(remotes[0]["pin"]["sha256_hex"], "cd".repeat(32));
    assert_eq!(local.issued.load(Ordering::SeqCst), 1);
    *local.pin.lock().unwrap() = "ef".repeat(32);
    fixture.at(Timing::default().key_poll.as_secs()).await;
    assert_eq!(fixture.remotes()[0]["pin"]["sha256_hex"], "ef".repeat(32));
    assert_eq!(local.issued.load(Ordering::SeqCst), 2);
    assert!(
        fixture.issuer.calls.lock().unwrap().is_empty(),
        "a self hub is never asked through callosum"
    );
}

#[tokio::test]
async fn a_new_peer_pin_reissues_the_leaf_grant_and_publishes_its_inbox_route() {
    let fixture = Fixture::new();
    let route = fixture.area.subscribe_inbox();
    let old = "HUB_INBOX_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_abababababababab";
    let new = "HUB_INBOX_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa_cdcdcdcdcdcdcdcd";
    let grant = |streams: &[&str]| {
        *fixture.issuer.publish.lock().unwrap() = streams
            .iter()
            .map(|name| format!("$JS.hub.API.STREAM.PURGE.{name}"))
            .collect();
    };
    grant(&[old]);
    let peer = peer(&[("10.0.0.1", 7422)]);
    fixture.area.apply_hub(peer.clone());
    fixture.at(0).await;
    assert_eq!(route.borrow().as_ref().unwrap().purge_streams, [old]);

    grant(&[old, new]);
    let HubView::Peer {
        fingerprint,
        candidates,
        ..
    } = peer
    else {
        unreachable!()
    };
    fixture.area.apply_hub(HubView::Peer {
        fingerprint,
        candidates,
        spki_sha256: "cd".repeat(32),
    });
    fixture.at(1).await;
    assert_eq!(fixture.issuer.calls.lock().unwrap().len(), 2);
    let next = route.borrow();
    let next = next.as_ref().unwrap();
    assert_eq!(next.hub.as_str(), "cdcdcdcdcdcdcdcd");
    assert_eq!(next.purge_streams, [old, new]);
}

#[tokio::test]
async fn a_failed_issue_backs_off_from_one_second_doubling() {
    let fixture = Fixture::new();
    *fixture.issuer.failures.lock().unwrap() = 3;
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    let calls = || fixture.issuer.calls.lock().unwrap().len();
    let mut seen = Vec::new();
    for second in 0..8 {
        fixture.at(second).await;
        seen.push(calls());
    }
    // Fails at 0, 1 and 3 (waits of 1 s and 2 s), succeeds at 7 (a 4 s wait).
    assert_eq!(seen, [1, 2, 2, 3, 3, 3, 3, 4]);
    assert_eq!(fixture.remotes().len(), 1);
    assert_eq!(fixture.metric("fed.remote_state"), "connecting");
}

#[tokio::test]
async fn renewal_at_half_life_kicks_the_link_once_at_a_quiet_moment() {
    let fixture = Fixture::new();
    fixture.links.up.store(true, Ordering::SeqCst);
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    fixture.at(0).await;
    let first_jwt = fixture.area.sign(&name(&fixture), NONCE).await.unwrap()["jwt"].clone();
    fixture.at(HALF_LIFE - 1).await;
    assert_eq!(fixture.issuer.calls.lock().unwrap().len(), 1);
    assert!(fixture.links.kicks.lock().unwrap().is_empty());

    // Renewal at the midpoint bounces the live connection once.
    fixture.at(HALF_LIFE).await;
    assert_eq!(fixture.issuer.calls.lock().unwrap().len(), 2);
    assert_eq!(*fixture.links.kicks.lock().unwrap(), vec![7]);
    let second_jwt = fixture.area.sign(&name(&fixture), NONCE).await.unwrap()["jwt"].clone();
    assert_ne!(first_jwt, second_jwt, "the reconnect presents the new JWT");
    fixture.at(HALF_LIFE + 1).await;
    assert_eq!(
        *fixture.links.kicks.lock().unwrap(),
        vec![7],
        "one kick only"
    );
}

#[tokio::test]
async fn a_busy_link_is_kicked_no_later_than_an_hour_after_renewal() {
    let fixture = Fixture::with(Arc::new(NoLocalHub), Arc::new(NeverQuiet));
    fixture.links.up.store(true, Ordering::SeqCst);
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    fixture.at(0).await;
    fixture.at(HALF_LIFE).await;
    assert!(fixture.links.kicks.lock().unwrap().is_empty());
    fixture.at(HALF_LIFE + 3599).await;
    assert!(fixture.links.kicks.lock().unwrap().is_empty());
    fixture.at(HALF_LIFE + 3600).await;
    assert_eq!(*fixture.links.kicks.lock().unwrap(), vec![7]);
}

#[tokio::test]
async fn a_replaced_leaf_key_gets_a_new_jwt_at_once() {
    let fixture = Fixture::new();
    fixture.links.up.store(true, Ordering::SeqCst);
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    fixture.at(0).await;
    let old = fixture.leaf_nkey();
    fixture.vault.mint(&leaf_id(), KeyPair::new_user());
    let new = fixture.leaf_nkey();

    // Before the next poll a signature already shows the key moved.
    let refused = fixture.area.sign(&name(&fixture), NONCE).await.unwrap_err();
    assert_eq!(refused.code, sign_code::KEY_ROTATED);
    fixture.at(1).await;
    let calls = fixture.issuer.calls.lock().unwrap().clone();
    assert_eq!(
        calls.iter().map(|(_, key)| key.clone()).collect::<Vec<_>>(),
        vec![old, new]
    );
    assert_eq!(*fixture.links.kicks.lock().unwrap(), vec![7]);
    fixture.area.sign(&name(&fixture), NONCE).await.unwrap();
}

#[tokio::test]
async fn the_bridge_opens_after_one_failed_round_and_comes_last() {
    let fixture = Fixture::new();
    fixture
        .area
        .apply_hub(peer(&[("10.0.0.1", 7422), ("10.0.0.2", 7422)]));
    fixture.at(0).await;
    let name_before = name(&fixture);
    // Two candidates make a 20 s round.
    fixture.at(19).await;
    assert!(fixture.callosum.bridges.lock().unwrap().is_empty());
    fixture.at(20).await;
    assert_eq!(*fixture.callosum.bridges.lock().unwrap(), vec![HUB]);
    fixture.at(21).await;
    let remotes = fixture.remotes();
    assert_eq!(
        remotes[0]["urls"],
        json!([
            "tls://10.0.0.1:7422",
            "tls://10.0.0.2:7422",
            "tls://127.0.0.1:49152"
        ])
    );
    assert_eq!(remotes[0]["name"], name_before, "a new URL keeps the name");
    *fixture.links.peer.lock().unwrap() = Some(("127.0.0.1".to_string(), 49152));
    fixture.links.up.store(true, Ordering::SeqCst);
    fixture.at(22).await;
    assert_eq!(fixture.metric("fed.remote_state"), "connected_via_bridge");
}

#[tokio::test]
async fn a_direct_link_reports_connected_while_a_bridge_is_open() {
    let fixture = Fixture::new();
    fixture
        .area
        .apply_hub(peer(&[("10.0.0.1", 7422), ("10.0.0.2", 7422)]));
    fixture.at(0).await;
    fixture.at(20).await;
    assert_eq!(*fixture.callosum.bridges.lock().unwrap(), vec![HUB]);
    // The engine reached a direct candidate after the bridge opened.
    fixture.links.up.store(true, Ordering::SeqCst);
    fixture.at(22).await;
    assert_eq!(fixture.metric("fed.remote_state"), "connected");
}

#[tokio::test]
async fn a_connected_link_never_opens_a_bridge_and_no_candidates_opens_one_at_once() {
    let fixture = Fixture::new();
    fixture.links.up.store(true, Ordering::SeqCst);
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    for second in [0, 30, 60] {
        fixture.at(second).await;
    }
    assert!(fixture.callosum.bridges.lock().unwrap().is_empty());

    let fixture = Fixture::new();
    fixture.area.apply_hub(peer(&[]));
    fixture.at(0).await;
    assert_eq!(*fixture.callosum.bridges.lock().unwrap(), vec![HUB]);
    fixture.at(1).await;
    assert_eq!(
        fixture.remotes()[0]["urls"],
        json!(["tls://127.0.0.1:49152"])
    );
}

fn name(fixture: &Fixture) -> String {
    fixture.area.current().remotes[0].name.clone()
}

#[tokio::test]
async fn leaf_sign_signs_only_a_nats_nonce_for_the_current_remote() {
    let fixture = Fixture::new();
    fixture.area.apply_hub(peer(&[("10.0.0.1", 7422)]));
    fixture.at(0).await;
    let remote = name(&fixture);

    let answer = fixture.area.sign(&remote, NONCE).await.unwrap();
    let signature = URL_SAFE_NO_PAD
        .decode(answer["signature_b64"].as_str().unwrap())
        .unwrap();
    KeyPair::from_public_key(&fixture.leaf_nkey())
        .unwrap()
        .verify(NONCE.as_bytes(), &signature)
        .expect("the signature is the leaf key's, over the nonce text");
    assert_eq!(
        crate::bootstrap::account_jwt::decode_claims(answer["jwt"].as_str().unwrap()).unwrap()
            ["sub"],
        fixture.leaf_nkey()
    );

    for bad in [
        "abcdefghijklmn",
        "abcdefghijklmn01",
        "abcdefghijklm+0",
        "abcdefghijklm=0",
    ] {
        assert_eq!(
            fixture.area.sign(&remote, bad).await.unwrap_err().code,
            sign_code::INVALID_NONCE,
            "{bad}"
        );
    }
    assert_eq!(
        fixture
            .area
            .sign("hub-0000000000000000", NONCE)
            .await
            .unwrap_err()
            .code,
        sign_code::UNKNOWN_REMOTE
    );
    let signs_before = fixture.vault.calls.lock().unwrap().len();
    fixture.vault.down.store(true, Ordering::SeqCst);
    let refused = fixture.area.sign(&remote, NONCE).await.unwrap_err();
    assert_eq!(refused.code, sign_code::VAULT_UNAVAILABLE);
    assert_eq!(refused.code, "vault unavailable");
    assert_eq!(
        fixture.vault.calls.lock().unwrap().len(),
        signs_before + 1,
        "only the well-formed request reached the vault"
    );
}

#[tokio::test(start_paused = true)]
async fn hub_read_polls_every_minute_only_while_the_watch_is_down() {
    let callosum = Arc::new(FakeCallosum::default());
    let (views, mut received) = mpsc::channel(16);
    tokio::spawn(follow_hub(callosum.clone(), views, Duration::from_secs(60)));
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(callosum.reads.load(Ordering::SeqCst), 1, "read at once");
    tokio::time::sleep(Duration::from_secs(59)).await;
    assert_eq!(callosum.reads.load(Ordering::SeqCst), 1);
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(callosum.reads.load(Ordering::SeqCst), 2);

    callosum.watch_up.store(true, Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(300)).await;
    let reads = callosum.reads.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(600)).await;
    assert_eq!(
        callosum.reads.load(Ordering::SeqCst),
        reads,
        "no reads while the watch is up"
    );
    let mut notices = VecDeque::new();
    while let Ok(view) = received.try_recv() {
        notices.push_back(view);
    }
    assert!(notices.iter().all(|view| *view == HubView::None));
    assert!(notices.len() >= 3);
}

#[test]
fn only_the_local_servers_wrapper_is_admitted() {
    let reserved = |module_id: &str| Principal::Reserved {
        module_id: module_id.to_string(),
    };
    assert!(admitted(Some(&reserved("nats-server"))));
    assert!(!admitted(Some(&reserved("nats-hub"))));
    assert!(!admitted(Some(&reserved("callosum"))));
    assert!(!admitted(Some(&Principal::Direct)));
    assert!(!admitted(None));
}

#[test]
fn the_watch_is_advertised_as_a_watch() {
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
    assert!(manifest.is_watch_operation("ckbus.leaf_watch"));
    assert!(!manifest.is_watch_operation("ckbus.leaf_sign"));
    assert!(manifest.provides.iter().any(|role| matches!(
        role,
        subc_protocol::manifest::ProviderRole::ManagementSurface { operations, .. }
            if operations.iter().any(|op| op.name == "ckbus.leaf_sign")
    )));
}

struct Inner;

#[async_trait]
impl ModuleHandler for Inner {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        unreachable!()
    }

    async fn health(&self) -> HealthReport {
        HealthReport {
            status: HealthStatus::Failing,
            detail: Some("Message bus is down.".to_string()),
            metrics: Some(json!({"class": "Unavailable", "cause": "broker-unavailable"})),
        }
    }
}

#[tokio::test]
async fn health_gains_fed_entries_and_keeps_everything_else() {
    let fixture = Fixture::new();
    let handler = LeafHandler::new(Inner, fixture.area.clone());
    let report = handler.health().await;
    assert_eq!(report.status, HealthStatus::Failing);
    assert_eq!(report.detail.as_deref(), Some("Message bus is down."));
    assert_eq!(
        report.metrics.unwrap(),
        json!({
            "class": "Unavailable",
            "cause": "broker-unavailable",
            "fed.hub_kind": "none",
            "fed.remote_state": "none",
            "fed.jwt_expires_at_ms": null,
        })
    );
}
