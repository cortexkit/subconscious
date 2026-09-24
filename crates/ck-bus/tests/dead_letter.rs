//! Ladder row "Dead-letter" (slice 9 of `docs/specs/ck-bus-module.md`), against a real
//! nats-server and the acceptance daemon. served-by: harness-signer. Both connections
//! the row names authenticate under JWTs ck-bus's own code built and the harness signer
//! signed: ck-bus's box user (built by the supervised ck-bus at bootstrap, or by ck-bus's
//! bootstrap code in this process for the in-process arm) consuming `c_ckbus_dead`, and
//! a claimant user built here with ck-bus's `sign_user_jwt` and the generated
//! participant grant.
//!
//! Arms:
//! - Claimant crash, a real kill. A claimant process driven to cap exhaustion publishes
//!   the dead-letter record and is SIGKILLed before `term()`: the claimant waits on its
//!   stdin between the two calls, so the kill lands exactly there. At the kill the
//!   record is stored and the item is not terminated. ck-bus records the message id
//!   once. The item comes back after its ack wait, a second claimant publishes the same
//!   record again and terms it, and ck-bus still holds one record for the id.
//! - One record per message id, across a ck-bus restart. A record republished with a
//!   different `Nats-Msg-Id` (what a republish after the stream's duplicate window
//!   looks like) is a duplicate of the first, naming its sequence; another message id is
//!   recorded (control). After a real SIGKILL of the supervised ck-bus, a further
//!   republish is still a duplicate of the first record, and nothing is recorded twice.
//! - ck-bus stopped between reading, recording and settling a record, in-process at the
//!   exact boundary (a real kill cannot be aimed between two statements; the arms above
//!   use real kills). A record read and not recorded is never settled: the next process
//!   records it. A record recorded and not settled is recorded again by the next
//!   process with the same sequence, never as a second record for its id.
//!
//! The claimant's cap is one below the effect durable's max-deliver of 5. With the two
//! equal, the server would never offer the killed claimant's last delivery again, and
//! the crash could not leave the item redeliverable as the foundation states.

#[allow(dead_code)]
#[path = "../src/bootstrap/mod.rs"]
mod bootstrap;
#[allow(dead_code)]
#[path = "../src/credentials/mod.rs"]
mod credentials;
#[allow(dead_code)]
#[path = "../src/dead_letter/mod.rs"]
mod dead_letter;
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
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use async_nats::jetstream;
use async_trait::async_trait;
use bootstrap::plane::{BoxPlane, Broker, NatsBroker};
use cortexkit_bus_naming::AccountNames;
use cortexkit_bus_nats::{ConnectConfig, NatsConnection};
use cortexkit_bus_trait::{
    ClaimOutcome, ContentDigest, DeadLetterRecord, Stream as _, WorkQueue as _,
    DEAD_LETTER_REASON_MAX_DELIVERIES,
};
use credentials::{
    issue::{sign_user_jwt, UserJwtRequest},
    vault::{VaultError, VaultSigning},
    wire::{self, VaultPublicKey, VaultSignature},
    Credentials,
};
use dead_letter::{consumer::Boundary, consumer::DeadLetter, event, Journal};
use harness::{
    bus::{self, BusServer, TrustChain, LOOPBACK},
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const BOOT_LIMIT: Duration = Duration::from_secs(60);
/// How long ck-bus may take to write a line for a stored record.
const RECORD_LIMIT: Duration = Duration::from_secs(20);
const AGENT: &str = "agent_dead_a";
const SESSION: &str = "sess_dead_1";
/// One below `issuance::EFFECT_MAX_DELIVER` (see the module comment).
const CLAIMANT_CAP: u32 = 4;
/// Names the claimant child's configuration; set only when this executable runs as it.
const CLAIMANT_ENV: &str = "CKBUS_DEAD_LETTER_CLAIMANT";
/// The claimant child's protocol lines start with this, apart from libtest's own output.
const CLAIMANT_PREFIX: &str = "CLAIMANT ";

fn vocabulary() -> BTreeSet<String> {
    SIGNER_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

fn passed() {
    RowReport::passed(Row::DeadLetter)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key")
        .emit(&vocabulary());
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

#[async_trait]
impl VaultSigning for InProcessSigner {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let body = wire::sign_request(credential_id, payload)
            .map_err(|error| VaultError::Malformed(format!("{error:?}")))?;
        match self.0.answer(&body) {
            HandlerOutcome::Response(bytes) => wire::parse_sign_reply(&bytes)
                .map_err(|error| VaultError::Malformed(format!("{error:?}"))),
            other => Err(VaultError::Malformed(format!("{other:?}"))),
        }
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        match self.0.answer(&wire::public_key_request(credential_id)) {
            HandlerOutcome::Response(bytes) => wire::parse_public_key_reply(&bytes)
                .map_err(|error| VaultError::Malformed(format!("{error:?}"))),
            other => Err(VaultError::Malformed(format!("{other:?}"))),
        }
    }
}

/// A supervised ck-bus that booted and set up `c_ckbus_dead`, beside a real server.
struct Run {
    trust: TrustChain,
    server: BusServer,
    run: SignerRun,
    ready: Value,
    credentials: Arc<Credentials>,
}

impl Run {
    fn names(&self) -> AccountNames {
        grants::derive_account(self.ready["acct"].as_str().unwrap()).unwrap()
    }

    fn account_public(&self) -> String {
        self.ready["account_public"].as_str().unwrap().to_string()
    }

    fn root(&self) -> PathBuf {
        self.run.root.path().to_path_buf()
    }

    /// A user JWT built by ck-bus's own code for `user_public`, signed by the box
    /// account root through the harness signer.
    async fn user_jwt(&self, user_public: &str, name: &str, grant: &grants::Grant) -> String {
        sign_user_jwt(
            self.credentials.vault.as_ref(),
            &self.credentials.key_ids,
            &UserJwtRequest {
                root_credential_id: &bus::box_root_id(),
                user_public,
                issuer_account: Some(&self.account_public()),
                name,
                issued_at: unix_now() - 60,
                grant,
            },
        )
        .await
        .expect("ck-bus builds and the harness signer signs the user JWT")
        .jwt
    }

    /// A bus-module user of ck-bus's own making, connected through ck-bus's broker code.
    async fn bus_plane(&self) -> Arc<dyn BoxPlane> {
        let user = self.credentials.custody.generate_user();
        let grant = grants::bus_module_grant(&self.names(), &user).unwrap();
        let jwt = self.user_jwt(&user, "dead-letter-row-bus", &grant).await;
        NatsBroker::new(self.server.url.clone(), self.credentials.clone())
            .connect_box(&jwt, &user)
            .await
            .unwrap_or_else(|error| panic!("the bus-module user connects: {error}"))
    }

    /// A claimant in this process: a participant bound to `AGENT`, connected through
    /// the trait layer.
    async fn claimant(&self) -> NatsConnection {
        let user = self.credentials.custody.generate_user();
        let grant = grants::participant_grant(&self.names(), &user, &[]).unwrap();
        let jwt = self
            .user_jwt(&user, "dead-letter-row-claimant", &grant)
            .await;
        let credentials = self.credentials.clone();
        let signer = user.clone();
        let options = async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
            let signed = credentials
                .custody
                .sign_nonce(&signer, &nonce)
                .map_err(|error| async_nats::AuthError::new(error.to_string()));
            async move { signed }
        });
        NatsConnection::connect(
            self.server.url.as_str(),
            options,
            ConnectConfig::new(user).unwrap(),
        )
        .await
        .unwrap_or_else(|error| panic!("the claimant connects: {error:?}"))
    }

    /// An unrestricted harness client in the box account, for reading stream state.
    async fn observer(&self) -> jetstream::Context {
        jetstream::new(bus::box_client(&self.trust, &self.server, &self.account_public()).await)
    }

    /// Creates the claimant's durables with ck-bus's issuance code, as issuance does.
    async fn ensure_claimant_durables(&self) {
        let plane = issuance::Plane {
            names: self.names(),
            account_public: self.account_public(),
            server_url: self.server.url.clone(),
            box_plane: self.bus_plane().await,
        };
        membership::bind(&plane.names, plane.box_plane.as_ref(), AGENT)
            .await
            .unwrap_or_else(|refusal| panic!("durables for {AGENT}: {refusal}"));
    }
}

async fn start() -> Option<Run> {
    let bin = match nats_server_bin() {
        Ok((bin, _)) => bin,
        Err((gate, observation)) => {
            RowReport::skipped(Row::DeadLetter, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&vocabulary());
            return None;
        }
    };
    let trust = TrustChain::generate();
    let root = SignerRun::tree();
    let server = BusServer::start(&bin, &root.join("nats"), &trust, LOOPBACK).await;
    let mut env = server.ckbus_env();
    env.push(("CKBUS_SENTINEL_PERIOD_MS".to_string(), "500".to_string()));
    let run = SignerRun::start_with(
        root,
        Path::new(env!("CARGO_BIN_EXE_ck-bus")),
        ClaustrumSide::Signer(trust.signer.clone()),
        RunOptions {
            ckbus_env: env,
            machine_id: Some(fresh_machine_id()),
        },
    )
    .await;
    let ready = bus::wait_event(run.root.path(), "ckbus.bootstrap.ready", 1, BOOT_LIMIT).await;
    bus::wait_event(run.root.path(), event::READY, 1, BOOT_LIMIT).await;
    let credentials = Arc::new(Credentials::new(Arc::new(InProcessSigner(
        trust.signer.clone(),
    ))));
    Some(Run {
        trust,
        server,
        run,
        ready,
        credentials,
    })
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

/// `c_ckbus_dead`'s ack floor: the highest stream sequence ck-bus has settled with
/// every lower one settled too.
async fn dead_ack_floor(observer: &jetstream::Context, names: &AccountNames) -> u64 {
    observer
        .get_consumer_from_stream::<jetstream::consumer::pull::Config, _, _>(
            AccountNames::consumer_name(dead_letter::CONSUMER_ID).unwrap(),
            names.streams().effect_dead.clone(),
        )
        .await
        .expect("c_ckbus_dead exists")
        .cached_info()
        .ack_floor
        .stream_sequence
}

/// Waits until ck-bus has settled every record up to `sequence`, so no line for them
/// is still to come.
async fn wait_settled(observer: &jetstream::Context, names: &AccountNames, sequence: u64) {
    let deadline = Instant::now() + RECORD_LIMIT;
    loop {
        let floor = dead_ack_floor(observer, names).await;
        if floor >= sequence {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "ck-bus never settled the dead-letter stream up to {sequence} (floor {floor})"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// ck-bus's `event` lines for `message_id`, across every process of the run.
fn lines_for(root: &Path, event: &str, message_id: &str) -> Vec<Value> {
    bus::events(root, event)
        .into_iter()
        .filter(|line| line["message_id"] == message_id)
        .collect()
}

async fn wait_lines(root: &Path, event: &str, message_id: &str, count: usize) -> Vec<Value> {
    let deadline = Instant::now() + RECORD_LIMIT;
    loop {
        let found = lines_for(root, event, message_id);
        if found.len() >= count {
            return found;
        }
        assert!(
            Instant::now() < deadline,
            "ck-bus wrote {} {event} line(s) for {message_id} within {RECORD_LIMIT:?}, \
             expected {count}; last dead-letter down line: {:?}",
            found.len(),
            bus::events(root, event::DOWN).last()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// A dead-letter record for `message_id` as a claimant publishes it, stored under the
/// stream message id `nats_msg_id`. Returns the stored sequence.
async fn publish_record(
    claimant: &NatsConnection,
    names: &AccountNames,
    message_id: &str,
    nats_msg_id: &str,
) -> u64 {
    let record = DeadLetterRecord {
        original_subject: names.effect_intent(AGENT, SESSION).unwrap(),
        message_id: message_id.to_string(),
        digest: ContentDigest::of_bytes(message_id.as_bytes()),
        delivery_count: 5,
        reason: DEAD_LETTER_REASON_MAX_DELIVERIES.to_string(),
    };
    claimant
        .stream(names.streams().effect_dead.clone())
        .publish(
            &names.effect_dead(),
            nats_msg_id,
            ContentDigest::of_bytes(b"dead-letter-record"),
            record.headers(),
        )
        .await
        .unwrap_or_else(|error| panic!("the claimant publishes the dead letter: {error:?}"))
        .stream_seq
}

// ---- The claimant child ----

/// Runs only when an arm starts this executable as its claimant process.
#[test]
fn claimant_child() {
    let Ok(config) = std::env::var(CLAIMANT_ENV) else {
        return;
    };
    let config: Value = serde_json::from_str(&config).expect("claimant configuration");
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("claimant runtime")
        .block_on(claimant_main(config));
}

fn say(line: &str) {
    use std::io::Write;
    let mut out = std::io::stdout();
    writeln!(out, "{CLAIMANT_PREFIX}{line}").unwrap();
    out.flush().unwrap();
}

async fn read_stdin_line() -> String {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).expect("stdin");
        line.trim().to_string()
    })
    .await
    .expect("stdin reader")
}

/// The claimant: its key is generated here and never leaves this process; the parent
/// builds its JWT from the public half. It naks every delivery under the cap, and on
/// exhaustion publishes the dead-letter record, then waits for the parent's word before
/// `term()`. That wait is where a crash arm kills it.
async fn claimant_main(config: Value) {
    let key = Arc::new(nkeys::KeyPair::new_user());
    let public = key.public_key();
    say(&format!("public {public}"));
    let jwt = read_stdin_line().await;
    let options = async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
        let signed = key.sign(&nonce).map_err(async_nats::AuthError::new);
        async move { signed }
    });
    let connection = NatsConnection::connect(
        config["url"].as_str().unwrap(),
        options,
        ConnectConfig::new(public).unwrap(),
    )
    .await
    .expect("the claimant connects");
    let queue = connection
        .work_queue(
            config["stream"].as_str().unwrap(),
            config["durable"].as_str().unwrap(),
            config["cap"].as_u64().unwrap() as u32,
        )
        .await
        .expect("the claimant binds its work queue");
    let deadline = Instant::now() + Duration::from_secs(90);
    let exhausted = loop {
        match queue.claim().await.expect("claim") {
            ClaimOutcome::Item(item) => {
                say(&format!("delivery {}", item.delivery_count));
                queue
                    .nak(item.token, Duration::ZERO)
                    .await
                    .expect("nak for redelivery");
            }
            ClaimOutcome::Empty => {
                assert!(Instant::now() < deadline, "the item never came back");
            }
            ClaimOutcome::MaxDeliveriesExceeded(exhausted) => break exhausted,
        }
    };
    let record = DeadLetterRecord::from_exhaustion(&exhausted);
    let ack = connection
        .stream(config["dead_stream"].as_str().unwrap())
        .publish(
            config["dead_subject"].as_str().unwrap(),
            &record.message_id,
            ContentDigest::of_bytes(b"dead-letter-record"),
            record.headers(),
        )
        .await
        .expect("the dead-letter record is stored before term");
    say(&format!(
        "published {} delivery {}",
        ack.stream_seq, exhausted.item.delivery_count
    ));
    if read_stdin_line().await == "term" {
        queue.term(exhausted.item.token).await.expect("term");
        // `term()` only queues the ack; the flush is what puts it on the wire before
        // the parent ends this process.
        connection.client().flush().await.expect("flush the term");
        say("termed");
    }
}

/// The parent's handle on one claimant process.
struct Claimant {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    lines: tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
}

impl Claimant {
    async fn spawn(run: &Run) -> Self {
        let names = run.names();
        let config = json!({
            "url": run.server.url,
            "stream": names.streams().effect,
            "durable": AccountNames::consumer_name(AGENT).unwrap(),
            "cap": CLAIMANT_CAP,
            "dead_stream": names.streams().effect_dead,
            "dead_subject": names.effect_dead(),
        });
        let mut child = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "claimant_child",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CLAIMANT_ENV, config.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .expect("the claimant process starts");
        let stdin = child.stdin.take().unwrap();
        let lines = BufReader::new(child.stdout.take().unwrap()).lines();
        let mut claimant = Self {
            child,
            stdin,
            lines,
        };
        let public = claimant.expect("public ", Duration::from_secs(20)).await;
        let grant = grants::participant_grant(&names, &public, &[]).unwrap();
        let jwt = run
            .user_jwt(&public, "dead-letter-row-claimant", &grant)
            .await;
        claimant.send(&jwt).await;
        claimant
    }

    async fn send(&mut self, line: &str) {
        self.stdin
            .write_all(format!("{line}\n").as_bytes())
            .await
            .expect("claimant stdin");
        self.stdin.flush().await.expect("claimant stdin");
    }

    /// The rest of the first protocol line starting with `what`.
    async fn expect(&mut self, what: &str, limit: Duration) -> String {
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            let line = tokio::time::timeout_at(deadline, self.lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("the claimant never said `{what}` within {limit:?}"))
                .expect("claimant stdout")
                .unwrap_or_else(|| panic!("the claimant exited before saying `{what}`"));
            // libtest prints `test claimant_child ... ` without a newline, so the first
            // protocol line does not start at the beginning of its line.
            if let Some(rest) = line
                .find(CLAIMANT_PREFIX)
                .and_then(|at| line[at + CLAIMANT_PREFIX.len()..].strip_prefix(what))
            {
                return rest.to_string();
            }
        }
    }

    /// The stored sequence from a `published <seq> delivery <n>` line.
    async fn published(&mut self, limit: Duration) -> (u64, u64) {
        let rest = self.expect("published ", limit).await;
        let words: Vec<&str> = rest.split_whitespace().collect();
        (words[0].parse().unwrap(), words[2].parse().unwrap())
    }

    /// SIGKILL, and wait until the process is gone.
    async fn kill(mut self) {
        self.child.kill().await.expect("the claimant is killed");
    }
}

// ---- The arms ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_claimant_killed_between_the_record_and_term_leaves_one_record() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start().await else {
        return;
    };
    let names = run.names();
    let root = run.root();
    let observer = run.observer().await;
    run.ensure_claimant_durables().await;
    let message_id = "effect-dl-crash";
    let mut headers = async_nats::HeaderMap::new();
    headers.insert("Nats-Msg-Id", message_id);
    headers.insert(
        "Ck-Content-Digest",
        ContentDigest::of_bytes(b"work item").to_string().as_str(),
    );
    observer
        .publish_with_headers(
            names.effect_intent(AGENT, SESSION).unwrap(),
            headers,
            "work item".into(),
        )
        .await
        .expect("publish the work item")
        .await
        .expect("the work item is stored");

    // The first claimant exhausts its cap, stores the record and is killed before term.
    let mut first = Claimant::spawn(&run).await;
    let (record_sequence, delivery) = first.published(Duration::from_secs(60)).await;
    assert_eq!(delivery, u64::from(CLAIMANT_CAP));
    first.kill().await;
    assert_eq!(
        stored(&observer, &names.streams().effect_dead).await,
        1,
        "at the kill the dead-letter record is stored"
    );
    assert_eq!(
        stored(&observer, &names.streams().effect).await,
        1,
        "at the kill the item is not terminated: the record was stored first"
    );
    let recorded = wait_lines(&root, event::RECORDED, message_id, 1).await;
    assert_eq!(recorded[0]["stream_sequence"], record_sequence);
    assert_eq!(
        recorded[0]["original_subject"],
        names.effect_intent(AGENT, SESSION).unwrap()
    );

    // The item comes back once its ack wait runs out. The second claimant publishes the
    // same record again (the stream keeps the one it has) and terms the item.
    let mut second = Claimant::spawn(&run).await;
    let (again, delivery) = second.published(Duration::from_secs(80)).await;
    assert_eq!(delivery, u64::from(CLAIMANT_CAP) + 1);
    assert_eq!(again, record_sequence, "the republish is the stored record");
    second.send("term").await;
    second.expect("termed", Duration::from_secs(20)).await;
    second.kill().await;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = stored(&observer, &names.streams().effect).await;
        if left == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the item is terminated: the work queue still holds {left}; consumer {:?}",
            observer
                .get_consumer_from_stream::<jetstream::consumer::pull::Config, _, _>(
                    AccountNames::consumer_name(AGENT).unwrap(),
                    names.streams().effect.clone(),
                )
                .await
                .map(|consumer| consumer.cached_info().clone())
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(stored(&observer, &names.streams().effect_dead).await, 1);
    wait_settled(&observer, &names, record_sequence).await;
    assert_eq!(lines_for(&root, event::RECORDED, message_id).len(), 1);
    assert!(lines_for(&root, event::DUPLICATE, message_id).is_empty());
    passed();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_republished_record_is_recorded_once_per_message_id_across_a_ckbus_restart() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start().await else {
        return;
    };
    let names = run.names();
    let root = run.root();
    let observer = run.observer().await;
    let claimant = run.claimant().await;
    let message_id = "effect-dl-dup";

    let first = publish_record(&claimant, &names, message_id, message_id).await;
    let recorded = wait_lines(&root, event::RECORDED, message_id, 1).await;
    assert_eq!(recorded[0]["stream_sequence"], first);

    // Control: another message id is its own record.
    let other = publish_record(&claimant, &names, "effect-dl-other", "effect-dl-other").await;
    let recorded_other = wait_lines(&root, event::RECORDED, "effect-dl-other", 1).await;
    assert_eq!(recorded_other[0]["stream_sequence"], other);

    // A republish past the stream's duplicate window is stored as a second message.
    let late = publish_record(&claimant, &names, message_id, "effect-dl-dup.late").await;
    assert!(late > first, "the republish is stored as its own message");
    let duplicate = wait_lines(&root, event::DUPLICATE, message_id, 1).await;
    assert_eq!(duplicate[0]["stream_sequence"], late);
    assert_eq!(duplicate[0]["first_sequence"], first);
    wait_settled(&observer, &names, late).await;
    assert_eq!(lines_for(&root, event::RECORDED, message_id).len(), 1);

    // A real kill of the supervised ck-bus; the supervisor starts a new one, which
    // rebuilds its ledger from the stream.
    let pid = run.run.supervised_pid("ckbus").await;
    let killed = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .expect("kill runs");
    assert!(killed.success());
    let ready = bus::wait_event(&root, event::READY, 2, BOOT_LIMIT).await;
    assert_eq!(ready["settled_floor"], late);
    assert_ne!(run.run.supervised_pid("ckbus").await, pid);

    let later = publish_record(&claimant, &names, message_id, "effect-dl-dup.later").await;
    let duplicates = wait_lines(&root, event::DUPLICATE, message_id, 2).await;
    assert_eq!(duplicates[1]["stream_sequence"], later);
    assert_eq!(duplicates[1]["first_sequence"], first);
    wait_settled(&observer, &names, later).await;
    assert_eq!(
        lines_for(&root, event::RECORDED, message_id).len(),
        1,
        "one record for the id across the restart"
    );
    assert_eq!(
        lines_for(&root, event::RECORDED, "effect-dl-other").len(),
        1
    );
    passed();
}

/// The lines one in-process consumer wrote.
#[derive(Default)]
struct Collected(Mutex<Vec<(String, Value)>>);

impl Journal for Collected {
    fn event(&self, event: &str, fields: Value) {
        self.0.lock().unwrap().push((event.to_string(), fields));
    }
}

impl Collected {
    fn lines(&self, event: &str, message_id: &str) -> Vec<Value> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, fields)| name == event && fields["message_id"] == message_id)
            .map(|(_, fields)| fields.clone())
            .collect()
    }
}

/// Disables the supervised ck-bus and waits until its process is gone, so the consumer
/// in this process is the only one reading `c_ckbus_dead`.
async fn retire_supervised_ckbus(run: &Run) {
    let reply = harness::control::rpc(
        &run.run.connection_file,
        subc_control::ClientControlRequest::SupervisorSetEnabled {
            module_id: "ckbus".to_string(),
            enabled: false,
        },
    )
    .await;
    if let harness::control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.set_enabled ckbus false refused: {} {}",
            error.code, error.message
        );
    }
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let response = harness::control::response(
            &run.run.connection_file,
            subc_control::ClientControlRequest::SupervisorList {},
        )
        .await;
        let subc_control::ClientControlResponse::SupervisorList { modules, .. } = response else {
            panic!("supervisor.list must return its matching response variant");
        };
        let entry = modules
            .iter()
            .find(|module| module.module_id == "ckbus")
            .expect("supervisor.list lists ckbus");
        if !entry.enabled && !entry.live && entry.state != "running" {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the supervised ck-bus did not stop: {entry:?}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One process's consumer over ck-bus's own connection code, stopping at `stop_at`.
async fn open(
    plane: &Arc<dyn BoxPlane>,
    names: &AccountNames,
    stop_at: Option<Boundary>,
) -> (DeadLetter, Arc<Collected>) {
    let journal = Arc::new(Collected::default());
    let link = plane.sentinel_link().expect("a real box connection");
    let mut consumer = DeadLetter::open(link.client, names, journal.clone())
        .await
        .unwrap_or_else(|down| panic!("c_ckbus_dead opens: {down}"));
    consumer.stop_at = stop_at;
    (consumer, journal)
}

/// Handles records until one pull comes back empty or the consumer stops.
async fn drain(consumer: &mut DeadLetter) -> Result<(), dead_letter::consumer::Down> {
    while consumer.next(Duration::from_secs(2)).await? {}
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_record_is_settled_only_after_it_is_recorded_across_a_stop_at_each_boundary() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let Some(run) = start().await else {
        return;
    };
    retire_supervised_ckbus(&run).await;
    let names = run.names();
    let observer = run.observer().await;
    let plane = run.bus_plane().await;
    let claimant = run.claimant().await;

    // Stopped after reading the record and before recording it: it is not settled, and
    // the next process records it.
    let first = publish_record(&claimant, &names, "effect-dl-read", "effect-dl-read").await;
    let (mut consumer, journal) = open(&plane, &names, Some(Boundary::Read)).await;
    assert!(drain(&mut consumer).await.is_err(), "the stop was reached");
    drop(consumer);
    assert!(journal.lines(event::RECORDED, "effect-dl-read").is_empty());
    let (mut consumer, journal) = open(&plane, &names, None).await;
    drain(&mut consumer).await.expect("the next process runs");
    drop(consumer);
    let recorded = journal.lines(event::RECORDED, "effect-dl-read");
    assert_eq!(
        recorded.len(),
        1,
        "a record read but not recorded was never settled, so the next process records it"
    );
    assert_eq!(recorded[0]["stream_sequence"], first);
    assert_eq!(dead_ack_floor(&observer, &names).await, first);

    // Stopped after recording and before settling: the next process records the same
    // record again, at the same sequence, and nothing else.
    let second = publish_record(
        &claimant,
        &names,
        "effect-dl-recorded",
        "effect-dl-recorded",
    )
    .await;
    let (mut consumer, journal) = open(&plane, &names, Some(Boundary::Recorded)).await;
    assert!(drain(&mut consumer).await.is_err(), "the stop was reached");
    drop(consumer);
    let before_stop = journal.lines(event::RECORDED, "effect-dl-recorded");
    assert_eq!(before_stop.len(), 1);
    assert_eq!(before_stop[0]["stream_sequence"], second);
    assert!(
        journal.lines(event::RECORDED, "effect-dl-read").is_empty(),
        "a record settled by an earlier process is not recorded again"
    );
    assert_eq!(dead_ack_floor(&observer, &names).await, first);
    let (mut consumer, journal) = open(&plane, &names, None).await;
    drain(&mut consumer).await.expect("the next process runs");
    let after_stop = journal.lines(event::RECORDED, "effect-dl-recorded");
    assert_eq!(after_stop.len(), 1);
    assert_eq!(
        after_stop[0]["stream_sequence"], second,
        "the same record, never a second record for the id"
    );
    assert!(journal
        .lines(event::DUPLICATE, "effect-dl-recorded")
        .is_empty());
    assert!(journal.lines(event::RECORDED, "effect-dl-read").is_empty());
    assert_eq!(dead_ack_floor(&observer, &names).await, second);
    passed();
}
