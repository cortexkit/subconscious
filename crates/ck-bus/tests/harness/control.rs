use std::{
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};

use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_protocol::{ErrorBody, Flags, Frame, FrameType, Priority};
use subc_transport::{authenticate_client, connection_file, read_frame, write_frame};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::sleep};

/// How long to wait for the daemon's reply to one control request.
///
/// It must exceed the daemon's own health-probe deadline (5 s,
/// `DEFAULT_HEALTH_PROBE_TIMEOUT` in subc-daemon's control.rs). A health read
/// against a module that is restarting waits out that deadline before the
/// daemon answers, so a client deadline of exactly 5 s raced the answer and
/// failed under load (the daemon logged the dispatch at 5001 ms). The daemon
/// always answers within its deadline plus dispatch time, so the margin only
/// decides how long a truly dead daemon takes to fail the test.
const CONTROL_RESPONSE_TIMEOUT: Duration = Duration::from_secs(15);

static NEXT_CORRELATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug)]
pub enum ControlReply {
    Response(ClientControlResponse),
    Error(ErrorBody),
}

pub async fn wait_for_connection(path: &Path, deadline: Instant) {
    loop {
        if connection_file::read(path).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "observable connection file never became readable: {}",
            path.display()
        );
        sleep(Duration::from_millis(10)).await;
    }
}

pub async fn rpc(path: &Path, request: ClientControlRequest) -> ControlReply {
    let connection = connection_file::read(path).expect("observable connection file must decode");
    let endpoint = connection
        .endpoints
        .first()
        .expect("observable endpoint list must be non-empty");
    let mut stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .expect("observable daemon endpoint must accept a client");
    authenticate_client(&mut stream, &connection, Duration::from_secs(2))
        .await
        .expect("observable client authentication must succeed");
    let correlation = NEXT_CORRELATION.fetch_add(1, Ordering::Relaxed);
    let body = serde_json::to_vec(&request).expect("control request must encode");
    let frame = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        correlation,
        body,
    )
    .expect("control request frame must build");
    write_frame(&mut stream, &frame)
        .await
        .expect("observable control request must write");
    stream
        .flush()
        .await
        .expect("observable control request must flush");
    let response = tokio::time::timeout(CONTROL_RESPONSE_TIMEOUT, read_frame(&mut stream))
        .await
        .expect("observable control response timed out")
        .expect("observable control response must decode")
        .expect("observable daemon closed before its control response");
    assert_eq!(
        response.header.corr, correlation,
        "observable response correlation must match request"
    );
    match response.header.ty {
        FrameType::Response => ControlReply::Response(
            serde_json::from_slice(&response.body)
                .expect("observable control response body must decode"),
        ),
        FrameType::Error => ControlReply::Error(
            serde_json::from_slice(&response.body)
                .expect("observable control error body must decode"),
        ),
        other => panic!("observable control response must be RESPONSE or ERROR, got {other:?}"),
    }
}

pub async fn response(path: &Path, request: ClientControlRequest) -> ClientControlResponse {
    match rpc(path, request).await {
        ControlReply::Response(response) => response,
        ControlReply::Error(error) => panic!(
            "observable control request was refused: code={} message={}",
            error.code, error.message
        ),
    }
}
