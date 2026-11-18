use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Condvar, Mutex,
};
use tokio::net::UnixStream;

#[derive(Default)]
struct Probe {
    released: Mutex<bool>,
    wake: Condvar,
    entered: tokio::sync::Notify,
    calls: AtomicUsize,
}

impl Probe {
    fn block(&self) {
        let guard = self.released.lock().unwrap();
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.entered.notify_one();
        // The native call cannot be cancelled. The test owns a release
        // switch so even the regression can tear down its worker.
        drop(self.wake.wait_while(guard, |released| !*released).unwrap());
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.wake.notify_all();
    }
}

struct Release(Arc<Probe>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_peer_evidence_does_not_stall_local_accept() {
    use subc_protocol::{Flags, FrameType, Priority};
    use subc_transport::{
        authenticate_client, generate_daemon_id, generate_key, ConnectionInfo, SCHEMA_VERSION,
    };

    let path = std::env::temp_dir().join(format!("subc-lane-{}.sock", std::process::id()));
    let unix = std::os::unix::net::UnixListener::bind(&path).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = tcp.local_addr().unwrap();
    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![],
        key: generate_key().unwrap().to_vec(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "test".into(),
    };
    let budget = Duration::from_secs(2);
    let auth = ServerAuth::new(info.key.clone(), info.daemon_id, &info.daemon_ver);
    let probe = Arc::new(Probe::default());
    let _release = Release(probe.clone());
    let lane = Arc::new(
        crate::peer_identity_lane::PeerIdentityLane::start().with_probe({
            let probe = probe.clone();
            move || probe.block()
        }),
    );
    let mut handler = crate::ControlHandler::default();
    handler.operator_app_policy.test_lane = Some(lane.clone());
    let mut router = Router::with_control_handler(Arc::new(handler));
    router.register_backend(7, crate::EchoBackend).unwrap();
    let router = Arc::new(router);
    let tcp_task = tokio::spawn(serve_listener(tcp, router.clone(), auth.clone()));
    let unix_task = tokio::spawn(serve_unix_local(unix, path.clone(), router, auth));
    let mut existing = tokio::net::TcpStream::connect(addr).await.unwrap();
    authenticate_client(&mut existing, &info, budget)
        .await
        .unwrap();
    let mut ready = UnixStream::connect(&path).await.unwrap();
    authenticate_client(&mut ready, &info, budget)
        .await
        .unwrap();

    assert_eq!(probe.calls.load(Ordering::Relaxed), 0);
    let mut operator = UnixStream::connect(&path).await.unwrap();
    let operator_info = info.clone();
    let operator_task = tokio::spawn(async move {
        authenticate_client(&mut operator, &operator_info, budget)
            .await
            .unwrap();
        let frame = crate::Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            0,
            0,
            1,
            br#"{"op":"connection.describe"}"#.to_vec(),
        )
        .unwrap();
        let started = tokio::time::Instant::now();
        crate::write_frame(&mut operator, &frame).await.unwrap();
        let response = crate::read_frame(&mut operator).await.unwrap().unwrap();
        let elapsed = started.elapsed();
        crate::write_frame(&mut operator, &frame).await.unwrap();
        let cached = crate::read_frame(&mut operator).await.unwrap().unwrap();
        (response, cached, elapsed)
    });
    tokio::time::timeout(budget, probe.entered.notified())
        .await
        .expect("operator evidence probe started");
    let fresh_unix = async {
        let mut client = UnixStream::connect(&path).await.unwrap();
        authenticate_client(&mut client, &info, budget).await
    };
    let fresh_tcp = async {
        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        authenticate_client(&mut client, &info, budget).await
    };
    let route = async {
        let frame = crate::Frame::build(
            FrameType::Request,
            Flags::new(true, Priority::Interactive, false),
            7,
            0,
            2,
            b"through the already connected route".to_vec(),
        )
        .unwrap();
        crate::write_frame(&mut existing, &frame).await.unwrap();
        crate::read_frame(&mut existing).await.unwrap().unwrap()
    };
    let timer = async {
        let mut ticks = tokio::time::interval(Duration::from_millis(10));
        for _ in 0..3 {
            ticks.tick().await;
        }
    };
    let (local, tcp, response, ticked) = tokio::join!(
        tokio::time::timeout(budget, fresh_unix),
        tokio::time::timeout(budget, fresh_tcp),
        tokio::time::timeout(budget, route),
        tokio::time::timeout(budget, timer),
    );
    assert!(
        matches!(local, Ok(Ok(_))),
        "new Unix client could not authenticate: {local:?}"
    );
    assert!(
        matches!(tcp, Ok(Ok(_))),
        "new TCP client could not authenticate: {tcp:?}"
    );
    assert_eq!(
        response.unwrap().body,
        b"through the already connected route"
    );
    ticked.unwrap();
    let (response, cached, elapsed) = tokio::time::timeout(budget, operator_task)
        .await
        .unwrap()
        .unwrap();
    assert!(
        elapsed < Duration::from_millis(500),
        "operator evidence took {elapsed:?}"
    );
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.body, cached.body);
    let body: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
    assert_eq!(body["local"], true);
    assert!(body["operator_app"].is_null());
    assert_eq!(probe.calls.load(Ordering::Relaxed), 1);
    assert_eq!(lane.stuck_slots(), 1);
    unix_task.abort();
    tcp_task.abort();
}
