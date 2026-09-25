use std::time::Duration;

use subc_daemon::{
    bootstrap::{run_with_config, BootstrapConfig},
    read_frame, write_frame, Frame,
};
use subc_protocol::{
    manifest::ModuleManifest, Flags, FrameType, ModuleHelloAckBody, ModuleHelloBody, Priority,
    PROTOCOL_VERSION,
};
use subc_test_support::TestTempDir;
use subc_transport::{authenticate_client, connection_file};
use tokio::{
    net::TcpStream,
    time::{sleep, timeout},
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn consumer_bootstraps_daemon_and_registers_module() {
    let temp = TestTempDir::new("daemon-consumer-registration");
    let connection_path = temp.join("subc-conn.json");
    let config = BootstrapConfig::new(&connection_path, 0)
        .with_terminal_journal_path(temp.join("terminals.jsonl"))
        .with_capture_logs_dir(temp.join("logs"));
    let daemon = tokio::spawn(run_with_config(config));

    let result = timeout(Duration::from_secs(10), async {
        let conn = loop {
            assert!(!daemon.is_finished(), "daemon exited before discovery");
            if let Ok(conn) = connection_file::read(&connection_path) {
                break conn;
            }
            sleep(Duration::from_millis(10)).await;
        };
        let endpoint = &conn.endpoints[0];
        let mut module = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .unwrap();
        authenticate_client(&mut module, &conn, Duration::from_secs(2))
            .await
            .unwrap();
        let hello = ModuleHelloBody {
            manifest: ModuleManifest::builder("standalone-consumer", "1.0.0").build(),
            protocol_ver: PROTOCOL_VERSION,
            control_ops: None,
            launch_nonce: None,
        };
        let request = Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Passive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap();
        write_frame(&mut module, &request).await.unwrap();
        let response = read_frame(&mut module)
            .await
            .unwrap()
            .expect("HELLO response");
        assert_eq!(response.header.ty, FrameType::HelloAck);
        let ack: ModuleHelloAckBody = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(ack.negotiated_ver, PROTOCOL_VERSION);
    })
    .await;

    daemon.abort();
    assert!(daemon.await.unwrap_err().is_cancelled());
    result.expect("daemon registration timed out");
}
