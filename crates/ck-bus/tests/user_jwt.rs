//! Ladder row "User JWT and seed absence" (slice 3 of `docs/specs/ck-bus-module.md`). served-by: harness-signer.
//!
//! ck-bus builds a participant user JWT and has the harness signer's root sign it over
//! the daemon, exactly as it would have claustrum sign it. A real `nats-server`, whose
//! operator and account JWTs the harness wrote from the same fixture keys, accepts it,
//! with the connect nonce signed by ck-bus's in-memory custody (the path
//! `ckbus.nonce_sign` answers through). Controls: a flipped signature byte, a JWT signed
//! by a root the server does not trust, and a nonce signed with another seed are each
//! refused by the server as an authorization error.
//!
//! Seed absence is checked on enumerated surfaces only: the supervised ck-bus's
//! environment and argv, the participant's (this test process's) environment and argv,
//! the store root, the capture logs and the row report. A seed the harness plants in the
//! store is found by the same scan.

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

use std::{
    collections::BTreeSet,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_nats::{ConnectErrorKind, ConnectOptions};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use credentials::{
    custody::KeyCustody,
    issue::{sign_user_jwt, SignedUserJwt, UserJwtRequest},
    roots::KeyIdLedger,
    vault::ClaustrumRoute,
};
use harness::{
    report::{Row, RowReport, ServedBy},
    signer::{
        nats::{nats_server_bin, unix_now, NatsFixture},
        run::{ClaustrumSide, SignerRun},
        seeds, HarnessSigner, SIGNER_OPERATIONS,
    },
};
use nkeys::KeyPair;

const ROOT_ID: &str = "signing:harness-box-account:1";
const UNTRUSTED_ROOT_ID: &str = "signing:harness-untrusted-account:1";
const ACCOUNT: &str = "box_harnessjwt";

fn signer_vocabulary() -> BTreeSet<String> {
    SIGNER_OPERATIONS
        .iter()
        .map(|op| (*op).to_string())
        .collect()
}

/// Connects with `jwt`, answering the server's nonce through `sign`.
async fn connect(
    url: &str,
    jwt: &str,
    sign: impl Fn(&[u8]) -> Vec<u8> + Send + Sync + 'static,
    events: Arc<Mutex<Vec<String>>>,
) -> Result<async_nats::Client, async_nats::ConnectError> {
    let sign = Arc::new(sign);
    ConnectOptions::with_jwt(jwt.to_string(), move |nonce| {
        let sign = sign.clone();
        async move { Ok(sign(&nonce)) }
    })
    .event_callback(move |event| {
        let events = events.clone();
        async move {
            events.lock().unwrap().push(event.to_string());
        }
    })
    .connect(url)
    .await
}

fn custody_signer(custody: &Arc<KeyCustody>, user: &str) -> impl Fn(&[u8]) -> Vec<u8> {
    let custody = custody.clone();
    let user = user.to_string();
    move |nonce| {
        custody
            .sign_nonce(&user, nonce)
            .expect("ck-bus holds this user's key")
    }
}

fn assert_refused_as_authorization(
    result: Result<async_nats::Client, async_nats::ConnectError>,
    what: &str,
) {
    match result {
        Ok(_) => panic!("{what}: the server accepted a connection it must refuse"),
        Err(error) => assert_eq!(
            error.kind(),
            ConnectErrorKind::AuthorizationViolation,
            "{what}: refused, but not as an authorization error: {error}"
        ),
    }
}

async fn issue(
    route: &ClaustrumRoute,
    root_id: &str,
    user: &str,
    issuer_account: &str,
    grant: &grants::Grant,
) -> SignedUserJwt {
    sign_user_jwt(
        route,
        &KeyIdLedger::new(),
        &UserJwtRequest {
            root_credential_id: root_id,
            user_public: user,
            issuer_account: Some(issuer_account),
            name: "user-jwt-row",
            issued_at: unix_now() - 60,
            grant,
        },
    )
    .await
    .expect("ck-bus issues the user JWT through the harness signer")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn user_jwt_built_by_ck_bus_is_accepted_by_a_real_server_and_no_seed_leaks() {
    let _gate = harness::acceptance_gate().await;
    harness::install_tracing();
    let (nats_bin, nats_version) = match nats_server_bin() {
        Ok(found) => found,
        Err((gate, observation)) => {
            RowReport::skipped(Row::UserJwt, gate, observation)
                .served_by(ServedBy::HarnessSigner)
                .emit(&signer_vocabulary());
            return;
        }
    };

    let signer = HarnessSigner::generated(&[ROOT_ID, UNTRUSTED_ROOT_ID]);
    let binary = Path::new(env!("CARGO_BIN_EXE_ck-bus"));
    let run = SignerRun::start(
        SignerRun::tree(),
        binary,
        ClaustrumSide::Signer(signer.clone()),
    )
    .await;
    let nats = NatsFixture::start(
        &nats_bin,
        nats_version,
        &run.root.join("nats"),
        &signer.root(ROOT_ID).account_public(),
    )
    .await;

    let route = ClaustrumRoute::new(run.connection_file.clone(), None);
    let custody = Arc::new(KeyCustody::new());
    let user = custody.generate_user();
    let account = grants::derive_account(ACCOUNT).unwrap();
    let grant = grants::participant_grant(&account, &user, &[]).unwrap();
    let issued = issue(&route, ROOT_ID, &user, &nats.account_public, &grant).await;
    assert_eq!(issued.issuer, signer.root(ROOT_ID).account_public());

    // Accepted: the harness first confirms the material is what its own signer signed
    // under a key it wrote into the server config.
    nats.assert_harness_signed(&issued.jwt);
    let events = Arc::new(Mutex::new(Vec::new()));
    let client = connect(
        &nats.url,
        &issued.jwt,
        custody_signer(&custody, &user),
        events.clone(),
    )
    .await
    .unwrap_or_else(|error| {
        panic!(
            "the server refused ck-bus's JWT: {error}\n{}",
            nats.log_text()
        )
    });
    // The grant came from the JWT: a subscribe outside it is a permissions violation,
    // and one inside it is not.
    let _allowed = client
        .subscribe(format!("_INBOX.{user}.probe"))
        .await
        .unwrap();
    let _denied = client
        .subscribe(format!("ck.{ACCOUNT}.room.>"))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !nats.log_text().contains("Subscription Violation") {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the server never enforced the JWT's grant: {}",
            nats.log_text()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let log = nats.log_text();
    assert!(
        log.contains(&format!("ck.{ACCOUNT}.room.>"))
            && !log.contains(&format!("_INBOX.{user}.probe\"")),
        "only the subscribe outside the grant is a violation: {log}"
    );
    drop(client);

    // Control: one flipped signature byte.
    let mut parts: Vec<String> = issued.jwt.split('.').map(str::to_string).collect();
    let mut signature = URL_SAFE_NO_PAD.decode(&parts[2]).unwrap();
    signature[0] ^= 0x01;
    parts[2] = URL_SAFE_NO_PAD.encode(signature);
    let flipped = parts.join(".");
    assert_refused_as_authorization(
        connect(
            &nats.url,
            &flipped,
            custody_signer(&custody, &user),
            events.clone(),
        )
        .await,
        "flipped signature byte",
    );

    // Control: a JWT signed by a root the server does not trust.
    let untrusted = issue(
        &route,
        UNTRUSTED_ROOT_ID,
        &user,
        &nats.account_public,
        &grant,
    )
    .await;
    assert_refused_as_authorization(
        connect(
            &nats.url,
            &untrusted.jwt,
            custody_signer(&custody, &user),
            events.clone(),
        )
        .await,
        "JWT signed by an untrusted root",
    );

    // Control: the right JWT, the nonce signed with a different seed.
    let other = KeyPair::new_user();
    assert_refused_as_authorization(
        connect(
            &nats.url,
            &issued.jwt,
            move |nonce| other.sign(nonce).unwrap(),
            events.clone(),
        )
        .await,
        "nonce signed with another seed",
    );

    // Seed absence on the enumerated surfaces.
    let ckbus_pid = run.supervised_pid("ckbus").await;
    let participant_environment = std::env::vars()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n");
    let participant_argv = std::env::args().collect::<Vec<_>>().join(" ");
    let report = RowReport::passed(Row::UserJwt)
        .served_by(ServedBy::HarnessSigner)
        .reached("credential.sign")
        .reached("credential.public_key");
    let report_text = format!("{report:?} {}", report.served_by_label());
    for (surface, text) in [
        ("ck-bus environment", seeds::process_environment(ckbus_pid)),
        ("ck-bus argv", seeds::process_argv(ckbus_pid)),
        ("participant environment", participant_environment),
        ("participant argv", participant_argv),
        ("row report", report_text),
    ] {
        assert_eq!(
            seeds::find_seeds(&text),
            Vec::<String>::new(),
            "a seed on {surface}"
        );
    }
    nats.stop().await;
    let root = run.shutdown().await;
    for dir in ["data", "run/logs"] {
        assert_eq!(
            seeds::scan_tree(&root.join(dir)),
            Vec::new(),
            "a seed under {dir}"
        );
    }

    // Control: a seed the harness plants in the store is found by the same scan.
    let planted = KeyPair::new_user().seed().unwrap();
    let store = root.join("data/cortexkit/ckbus");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::write(
        store.join("planted.json"),
        format!("{{\"k\":\"{planted}\"}}"),
    )
    .unwrap();
    let found = seeds::scan_tree(&root.join("data"));
    // The pattern covers the first 56 of a seed's 58 characters, so a hit is a prefix.
    assert!(
        found.iter().any(
            |(path, seed)| path.ends_with("planted.json") && planted.starts_with(seed.as_str())
        ),
        "the planted seed must be found: {found:?}"
    );

    report.emit(&signer_vocabulary());
}
