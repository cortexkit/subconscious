use std::future::Future;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::Duration;

use async_nats::{jetstream, Auth, AuthError, ClientError, ConnectOptions, Event, ServerError};
use cortexkit_bus_nats::{ConnectConfig, NatsConnection, NatsConnectionEvent, NatsConnectionState};
use cortexkit_bus_testkit::{
    skip_or_fail,
    trust::{box_root_id, unix_now, TrustChain},
    BusOptions, BusServer, PreloadedAccount,
};
use cortexkit_bus_trait::{
    BusError, Register, RegisterEntry, RegisterUpdate, RegisterWatch, Revision, WatchEvent,
};
use nkeys::KeyPair;
use time::{OffsetDateTime, PrimitiveDateTime};
use tokio::sync::mpsc;
use tokio::time::timeout;

const DEADLINE: Duration = Duration::from_secs(12);
const RECONNECT_LIMIT: usize = 3;
const BUCKET: &str = "jwt_expiry";
const KEY: &str = "watched";

async fn within<T>(stage: &str, operation: impl Future<Output = T>) -> T {
    timeout(DEADLINE, operation)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {stage}"))
}

async fn jwt_expiry(renew: bool) {
    let trust = TrustChain::generate();
    let account = KeyPair::new_account().public_key();
    let root = box_root_id();
    let server = match within(
        "NATS startup",
        BusServer::start(BusOptions {
            trust: Some(trust.clone()),
            preload: vec![PreloadedAccount {
                public_key: account.clone(),
                signing_keys: vec![trust.box_root_public()],
            }],
            ..Default::default()
        }),
    )
    .await
    {
        Ok(server) => server,
        Err(error) => {
            skip_or_fail(error);
            return;
        }
    };
    eprintln!(
        "JWT expiry (renew={renew}): {} at {}",
        server.version, server.url
    );

    // A separate user keeps the writer alive without changing the watched connection.
    let writer = Arc::new(KeyPair::new_user());
    let jwt = trust.user_jwt(&root, &account, &writer, unix_now());
    let writer = within(
        "writer connection",
        ConnectOptions::with_jwt(jwt, move |nonce| {
            let writer = writer.clone();
            async move { writer.sign(&nonce).map_err(AuthError::new) }
        })
        .connect(&server.url),
    )
    .await
    .unwrap();
    let store = within(
        "KV bucket creation",
        jetstream::new(writer).create_key_value(jetstream::kv::Config {
            bucket: BUCKET.into(),
            ..Default::default()
        }),
    )
    .await
    .unwrap();

    let user = KeyPair::new_user();
    let watcher_key = user.public_key();
    let config = ConnectConfig::new(&watcher_key).unwrap();
    let expires_at = unix_now() + 5;
    eprintln!("watcher JWT exp={expires_at}");
    let jwt = trust.expiring_user_jwt(&root, &account, &user, expires_at);
    let first_auth = AtomicBool::new(true);
    let connection_attempts = AtomicUsize::new(0);
    let options = ConnectOptions::with_auth_callback(move |nonce| {
        // The initial handshake always uses the short-lived JWT. Only the
        // renewing source supplies a later JWT when NATS sends another nonce.
        let jwt = if renew && !first_auth.swap(false, Ordering::Relaxed) {
            trust.expiring_user_jwt(&root, &account, &user, unix_now() + 60)
        } else {
            jwt.clone()
        };
        let signature = user.sign(&nonce);
        async move {
            let mut auth = Auth::new();
            auth.jwt = Some(jwt);
            auth.signature = Some(signature.map_err(AuthError::new)?);
            Ok(auth)
        }
    })
    .max_reconnects(RECONNECT_LIMIT)
    .reconnect_delay_callback(move |_| {
        // NATS 2.15.0 validates JWTs at whole-second granularity (now > exp),
        // but a connection accepted at now == exp gets a zero expiry timer.
        // Give only the first retry a chance to exercise that boundary. Count
        // across successful handshakes, since async-nats resets its counter.
        if !renew && connection_attempts.fetch_add(1, Ordering::Relaxed) == 2 {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(50)
        }
    });
    let (send, mut events) = mpsc::unbounded_channel();
    let connection = within(
        "watcher connection",
        NatsConnection::connect_observed(&server.url, options, config, None, move |event| {
            eprintln!("watcher (renew={renew}): {event:?}");
            if let NatsConnectionEvent::Event(event) = event {
                let _ = send.send(event.clone());
            }
        }),
    )
    .await
    .unwrap();
    let mut state = connection.connection_state();
    let register = within("register bind", connection.register(BUCKET))
        .await
        .unwrap();
    let mut watch = within("watch open", register.watch(KEY)).await.unwrap();
    assert!(
        matches!(within("watch snapshot", watch.next()).await.unwrap(),
        WatchEvent::Snapshot { entries, .. } if entries.is_empty())
    );

    let before = within("pre-expiry put", store.put(KEY, "before".into()))
        .await
        .unwrap();
    assert_eq!(
        within("pre-expiry delivery", watch.next()).await.unwrap(),
        put_event(before, b"before")
    );
    assert_eq!(*state.borrow(), NatsConnectionState::Connected);

    if let ReconnectOutcome::Exhausted { boundary_connected } =
        check_pending_watch(renew, &store, &mut watch, &mut state, &mut events).await
    {
        check_broker_rejections(&server, &watcher_key, expires_at, boundary_connected);
    }
}

fn put_event(revision: u64, value: &[u8]) -> WatchEvent {
    WatchEvent::Update(RegisterUpdate::Put(RegisterEntry {
        key: KEY.into(),
        value: value.to_vec(),
        revision: Revision(revision),
    }))
}

async fn next_event(events: &mut mpsc::UnboundedReceiver<Event>) -> Event {
    events.recv().await.expect("watcher event source closed")
}

enum ReconnectOutcome {
    Renewed(u64),
    Exhausted { boundary_connected: bool },
}

fn check_reconnect_failure(event: Event) {
    // A refused socket can close before async-nats receives the NATS error.
    // Check the broker log separately to distinguish expiry from network failure.
    assert!(
        matches!(&event, Event::ClientError(ClientError::Other(message))
            if matches!(message.as_str(), "authorization violation" | "IO error")),
        "unexpected reconnect event: {event:?}"
    );
}

async fn check_pending_watch(
    renew: bool,
    store: &jetstream::kv::Store,
    watch: &mut cortexkit_bus_nats::NatsRegisterWatch,
    state: &mut tokio::sync::watch::Receiver<NatsConnectionState>,
    events: &mut mpsc::UnboundedReceiver<Event>,
) -> ReconnectOutcome {
    let pending = watch.next();
    tokio::pin!(pending);
    assert!(
        futures_util::poll!(&mut pending).is_pending(),
        "watch must be waiting before expiry"
    );

    let watch_state = state.clone();
    let observe_expiry = async {
        let mut event = next_event(events).await;
        if event == Event::Connected {
            event = next_event(events).await;
        }
        let expired = Event::ServerError(ServerError::Other("User Authentication Expired".into()));
        assert_eq!(event, expired);
        assert_eq!(next_event(events).await, Event::Disconnected);

        if renew {
            assert_eq!(next_event(events).await, Event::Connected);
            state
                .wait_for(|state| *state == NatsConnectionState::Connected)
                .await
                .unwrap();
            ReconnectOutcome::Renewed(
                store
                    .put(KEY, "after".into())
                    .await
                    .expect("post-expiry put"),
            )
        } else {
            let first = next_event(events).await;
            let boundary_connected = first == Event::Connected;
            if boundary_connected {
                let mut event = next_event(events).await;
                if event == expired {
                    event = next_event(events).await;
                }
                assert_eq!(event, Event::Disconnected);
            } else {
                check_reconnect_failure(first);
            }
            for _ in usize::from(!boundary_connected)..RECONNECT_LIMIT {
                check_reconnect_failure(next_event(events).await);
            }
            assert_eq!(
                next_event(events).await,
                Event::ClientError(ClientError::MaxReconnects)
            );
            state
                .wait_for(|state| *state == NatsConnectionState::Closed)
                .await
                .unwrap();
            ReconnectOutcome::Exhausted { boundary_connected }
        }
    };
    let (reconnect, (outcome, state_at_return)) =
        within("expiry, reconnect outcome, and pending watch", async {
            tokio::join!(observe_expiry, async {
                let outcome = pending.await;
                let observed = *watch_state.borrow();
                eprintln!(
                    "pending watch (renew={renew}) returned {outcome:?} in state {observed:?}"
                );
                (outcome, observed)
            })
        })
        .await;

    match reconnect {
        ReconnectOutcome::Renewed(revision) => {
            assert_eq!(outcome.unwrap(), put_event(revision, b"after"));
            assert_eq!(state_at_return, NatsConnectionState::Connected);
        }
        ReconnectOutcome::Exhausted { .. } => {
            assert!(
                matches!(outcome, Err(BusError::Unavailable { .. })),
                "expected Unavailable, got {outcome:?}"
            );
            assert_eq!(state_at_return, NatsConnectionState::Closed);
        }
    }
    reconnect
}

fn check_broker_rejections(
    server: &BusServer,
    watcher_key: &str,
    expires_at: i64,
    boundary_connected: bool,
) {
    let log = std::fs::read_to_string(&server.log).expect("NATS broker log");
    let mut accepted = log
        .lines()
        .filter(|line| line.contains("Authenticated JWT:") && line.contains(watcher_key));
    let initial = accepted.next().expect("broker logged initial watcher auth");
    let boundary = accepted.next();
    assert!(broker_log_time(initial).unix_timestamp() < expires_at);
    assert!(
        accepted.next().is_none(),
        "more than one boundary connection:\n{log}"
    );

    if let Some(boundary) = boundary {
        let authenticated = broker_log_time(boundary);
        assert_eq!(
            authenticated.unix_timestamp(),
            expires_at,
            "wrong second:\n{log}"
        );
        let cid = broker_log_cid(boundary);
        let closed = log
            .lines()
            .find(|line| {
                line.contains("Client connection closed: Authentication Expired")
                    && broker_log_cid(line) == cid
            })
            .expect("broker closed the boundary connection for expiry");
        let elapsed = (broker_log_time(closed) - authenticated).abs();
        assert!(
            elapsed <= Duration::from_millis(10),
            "boundary connection did not close promptly:\n{log}"
        );
        eprintln!("boundary JWT connection ({elapsed}):\n{boundary}\n{closed}");
    } else {
        assert!(
            !boundary_connected,
            "client reconnected without broker auth:\n{log}"
        );
    }

    // If the broker closed the boundary connection before async-nats finished
    // the handshake, it used one retry slot even though the JWT validated.
    let expected = RECONNECT_LIMIT - usize::from(boundary.is_some() && !boundary_connected);
    let rejected: Vec<_> = log
        .lines()
        .filter(|line| {
            line.contains("User JWT no longer valid:") && line.contains("claim is expired")
        })
        .collect();
    assert_eq!(rejected.len(), expected, "NATS broker log:\n{log}");
    for line in rejected {
        assert!(
            broker_log_time(line).unix_timestamp() > expires_at,
            "premature rejection: {line}"
        );
        eprintln!("broker JWT rejection: {line}");
    }
}

fn broker_log_time(line: &str) -> OffsetDateTime {
    let mut parts = line.splitn(4, ' ').skip(1);
    let date = parts.next().expect("broker log date");
    let time = parts.next().expect("broker log time");
    let format = time::format_description::parse_borrowed::<2>(
        "[year]/[month]/[day] [hour]:[minute]:[second].[subsecond]",
    )
    .unwrap();
    PrimitiveDateTime::parse(&format!("{date} {time}"), &format)
        .unwrap()
        .assume_utc()
}

fn broker_log_cid(line: &str) -> &str {
    line.split_whitespace()
        .find(|word| word.starts_with("cid:"))
        .expect("broker log connection id")
}

#[tokio::test]
async fn jwt_expiry_without_renewal_closes_pending_watch() {
    jwt_expiry(false).await;
}

#[tokio::test]
async fn jwt_expiry_with_renewal_preserves_pending_watch() {
    jwt_expiry(true).await;
}
