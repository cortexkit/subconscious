#![forbid(unsafe_code)]

//! When a module ends its daemon connection itself (here: a `ModuleStopHandle`
//! stop), the daemon must read the module's GOODBYE followed by a clean EOF,
//! never a connection reset.
//!
//! Closing a TCP socket that still holds unread received bytes makes the OS send
//! a reset (RST) instead of an orderly FIN; Linux and Windows do this. A
//! peer that receives the reset can fail its next read and lose data it had not
//! read yet, including the GOODBYE. Each fake daemon below therefore sends one
//! frame, a PING, in the same write as HELLO_ACK, which the module reads past
//! only if it drains its socket before closing. The module must read the
//! HELLO_ACK before it can stop, and the PING arrives in the same small write,
//! so the PING is already queued when the module stops rather than racing it.

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use cortexkit_test_support::ScratchDir;
use subc_client_rs::{
    async_trait, HandlerOutcome, ModuleHandler, ModuleStopHandle, RequestCtx, ServeOptions,
};
use subc_protocol::{
    manifest::ModuleManifest, Flags, Frame, FrameType, ModuleHelloAckBody, Priority,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_server, generate_daemon_id, generate_key, read_frame, write_atomic, write_frame,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::AsyncWriteExt,
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};

/// Stops the module from inside `on_hello_ack`, so the stop is requested before
/// the module reads anything after HELLO_ACK. With `hold_handshake` the hook
/// then never returns, which ends the connection from the handshake's stop
/// path instead of the serve loop's.
struct StopOnHelloAck {
    stop: ModuleStopHandle,
    hold_handshake: bool,
}

#[async_trait]
impl ModuleHandler for StopOnHelloAck {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }

    async fn on_hello_ack(&self, _ack: &ModuleHelloAckBody) {
        self.stop.stop();
        if self.hold_handshake {
            std::future::pending::<()>().await;
        }
    }
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

struct FakeDaemon {
    _dir: ScratchDir,
    path: PathBuf,
    listener: TcpListener,
    connection: ConnectionInfo,
}

async fn fake_daemon() -> FakeDaemon {
    let dir = ScratchDir::new("subc-client-orderly-close");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connection = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "orderly-close-test".into(),
    };
    let path = dir.join("subc-conn.json");
    write_atomic(&path, &connection).unwrap();
    FakeDaemon {
        _dir: dir,
        path,
        listener,
        connection,
    }
}

fn start_module(
    daemon: &FakeDaemon,
    budget: Duration,
    hold_handshake: bool,
) -> JoinHandle<Result<(), subc_client_rs::SubcModuleError>> {
    let stop = ModuleStopHandle::new();
    let options = ServeOptions::default()
        .with_stop_handle(stop.clone())
        .with_writer_drain_timeout(budget);
    let handler = StopOnHelloAck {
        stop,
        hold_handshake,
    };
    let path = daemon.path.clone();
    tokio::spawn(async move {
        let manifest = ModuleManifest::builder("orderly-close", "1.0.0").build();
        options.serve_with(&path, manifest, handler).await
    })
}

/// Accept and authenticate the module, read its HELLO, then send HELLO_ACK and
/// a PING in one write so the PING is already queued when the module stops.
async fn accept_and_ack(daemon: &FakeDaemon) -> TcpStream {
    let (mut stream, _) = timeout(Duration::from_secs(5), daemon.listener.accept())
        .await
        .expect("module must connect")
        .unwrap();
    authenticate_server(
        &mut stream,
        &daemon.connection.key,
        &daemon.connection.daemon_id,
        &daemon.connection.daemon_ver,
        Duration::from_secs(2),
    )
    .await
    .unwrap();
    let hello = timeout(Duration::from_secs(5), read_frame(&mut stream))
        .await
        .expect("module must send HELLO")
        .unwrap()
        .expect("module must send HELLO");
    assert_eq!(hello.header.ty, FrameType::Hello);
    let ack = ModuleHelloAckBody {
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: Vec::new(),
        subc_capabilities: Vec::new(),
        storage: None,
        machine_id: None,
    };
    let mut bytes = Vec::new();
    let ack = Frame::build(
        FrameType::HelloAck,
        control_flags(),
        0,
        0,
        hello.header.corr,
        serde_json::to_vec(&ack).unwrap(),
    )
    .unwrap();
    write_frame(&mut bytes, &ack).await.unwrap();
    let ping = Frame::build(FrameType::Ping, control_flags(), 0, 0, 7, Vec::new()).unwrap();
    write_frame(&mut bytes, &ping).await.unwrap();
    stream.write_all(&bytes).await.unwrap();
    stream.flush().await.unwrap();
    stream
}

/// Read the module's GOODBYE and then its FIN, the way a daemon does, and fail
/// on a reset. `eof_within` is far shorter than the module's drain budget, so a
/// module that never shuts down its sending side fails here rather than having
/// its close forced by the drain deadline.
async fn expect_goodbye_then_clean_eof(stream: &mut TcpStream, eof_within: Duration) {
    // Give the module time to finish its side of the close first, so a reset,
    // if it sends one, has already arrived before the daemon reads.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let goodbye = timeout(Duration::from_secs(5), read_frame(stream))
        .await
        .expect("module must send GOODBYE");
    let goodbye = match goodbye {
        Ok(Some(frame)) => frame,
        other => panic!("expected the module's GOODBYE, got {other:?}"),
    };
    assert_eq!(
        (goodbye.header.ty, goodbye.header.channel),
        (FrameType::Goodbye, 0),
        "the module's last frame must be a channel-0 GOODBYE"
    );
    let end = timeout(eof_within, read_frame(stream))
        .await
        .expect("module must shut down its sending side right after GOODBYE");
    assert!(
        matches!(end, Ok(None)),
        "expected a clean EOF after GOODBYE, got {end:?}"
    );
    let pending = stream.take_error().unwrap();
    assert!(
        pending.is_none(),
        "the module reset the connection after its FIN: {pending:?}"
    );
    // A daemon may still answer with its own GOODBYE. The module is still
    // reading until the daemon closes, so the write lands rather than failing
    // on a socket the module already reset.
    let reply = Frame::build(FrameType::Goodbye, control_flags(), 0, 0, 0, Vec::new()).unwrap();
    write_frame(stream, &reply)
        .await
        .expect("the module must still be reading when the daemon answers GOODBYE");
    stream.flush().await.unwrap();
}

async fn stop_closes_with_fin_not_reset(hold_handshake: bool) {
    // A long budget, so only an orderly close (not the drain deadline) can
    // finish the exchange within the test's much shorter waits.
    let budget = Duration::from_secs(10);
    let daemon = fake_daemon().await;
    let serving = start_module(&daemon, budget, hold_handshake);
    let mut stream = accept_and_ack(&daemon).await;
    expect_goodbye_then_clean_eof(&mut stream, Duration::from_secs(1)).await;
    drop(stream);
    timeout(Duration::from_secs(3), serving)
        .await
        .expect("serve must return as soon as the daemon closes, not at the drain deadline")
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn stop_after_hello_ack_sends_goodbye_then_fin_not_reset() {
    stop_closes_with_fin_not_reset(false).await;
}

#[tokio::test]
async fn stop_during_handshake_sends_goodbye_then_fin_not_reset() {
    stop_closes_with_fin_not_reset(true).await;
}

/// A daemon that reads the module's GOODBYE and FIN and then closes its side at
/// once must end serve promptly: the drain stops at the daemon's close, not at
/// the drain deadline. The budget is ten times the allowed wait, so a drain
/// that ignores the close fails here by name.
async fn stop_ends_promptly_once_the_daemon_closes(hold_handshake: bool) {
    let budget = Duration::from_secs(10);
    let prompt = Duration::from_secs(1);
    let daemon = fake_daemon().await;
    let serving = start_module(&daemon, budget, hold_handshake);
    let mut stream = accept_and_ack(&daemon).await;
    let goodbye = timeout(Duration::from_secs(5), read_frame(&mut stream))
        .await
        .expect("module must send GOODBYE")
        .unwrap()
        .expect("module must send GOODBYE before EOF");
    assert_eq!(
        (goodbye.header.ty, goodbye.header.channel),
        (FrameType::Goodbye, 0)
    );
    let end = timeout(Duration::from_secs(5), read_frame(&mut stream))
        .await
        .expect("module must shut down its sending side after GOODBYE");
    assert!(matches!(end, Ok(None)), "expected EOF, got {end:?}");
    let closed = Instant::now();
    drop(stream);
    // Wait past the budget, so a drain that ignores the close reaches the
    // timing assertion below rather than this timeout.
    let result = timeout(budget + Duration::from_secs(5), serving)
        .await
        .expect("serve must return once its drain budget ends")
        .unwrap();
    let after_close = closed.elapsed();
    result.unwrap();
    assert!(
        after_close < prompt,
        "serve returned {after_close:?} after the daemon closed; it must stop draining at the close (within {prompt:?}), not wait for its {budget:?} drain budget"
    );
}

#[tokio::test]
async fn stop_after_hello_ack_ends_promptly_once_the_daemon_closes() {
    stop_ends_promptly_once_the_daemon_closes(false).await;
}

#[tokio::test]
async fn stop_during_handshake_ends_promptly_once_the_daemon_closes() {
    stop_ends_promptly_once_the_daemon_closes(true).await;
}

#[tokio::test]
async fn drain_waits_for_a_daemon_that_never_closes_only_until_the_budget() {
    let budget = Duration::from_secs(1);
    let daemon = fake_daemon().await;
    let started = Instant::now();
    let serving = start_module(&daemon, budget, false);
    let mut stream = accept_and_ack(&daemon).await;
    expect_goodbye_then_clean_eof(&mut stream, Duration::from_millis(500)).await;
    // Keep `stream` open: this daemon never closes its side.
    timeout(budget + Duration::from_secs(3), serving)
        .await
        .expect("a daemon that never closes must not hold serve past the drain budget")
        .unwrap()
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed >= budget,
        "serve returned after {elapsed:?}, before its {budget:?} drain budget, so it did not wait for the daemon to close"
    );
    drop(stream);
}
