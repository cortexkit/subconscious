use std::{future::pending, process, time::Duration};
use subc_test_support::TestTempDir;

use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_daemon::{read_frame, write_frame, Frame};
use subc_protocol::{FrameType, PROTOCOL_VERSION};
use subc_transport::{
    authenticate_server, generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint,
    SCHEMA_VERSION,
};
use tokio::{io::AsyncWriteExt, net::TcpListener, task::JoinHandle};

/// Authenticated wire stub for deadline/no-reply tests that a correct real
/// daemon cannot serve deterministically.
pub struct ScriptedDaemon {
    pub connection_file_path: std::path::PathBuf,
    _temp_dir: TestTempDir,
    task: JoinHandle<()>,
}

pub struct ScriptedControlStep {
    pub expected: ClientControlRequest,
    pub action: ScriptedControlAction,
}

pub enum ScriptedControlAction {
    Reply(Box<ClientControlResponse>),
    NeverReply,
}

impl ScriptedDaemon {
    pub async fn start(name: &str, steps: Vec<ScriptedControlStep>) -> Self {
        let temp_dir = TestTempDir::new(name);
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("scripted daemon binds");
        let port = listener
            .local_addr()
            .expect("scripted daemon address")
            .port();
        let connection_file_path = temp_dir.join("subc-conn.json");
        let connection = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: Some(PROTOCOL_VERSION),
            endpoints: vec![Endpoint {
                host: std::net::Ipv4Addr::LOCALHOST.to_string(),
                port,
            }],
            key: generate_key().expect("scripted daemon key"),
            daemon_id: generate_daemon_id().expect("scripted daemon id"),
            pid: process::id(),
            daemon_ver: "scripted-test-daemon".to_string(),
        };
        write_atomic(&connection_file_path, &connection).expect("scripted connection file");

        let task = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("ck connects");
            authenticate_server(
                &mut stream,
                &connection.key,
                &connection.daemon_id,
                &connection.daemon_ver,
                Duration::from_secs(2),
            )
            .await
            .expect("ck authenticates");

            for step in steps {
                let request_frame = read_frame(&mut stream)
                    .await
                    .expect("scripted request frame is valid")
                    .expect("ck keeps scripted connection open");
                assert_eq!(request_frame.header.channel, 0);
                assert_eq!(request_frame.header.ty, FrameType::Request);
                let request: ClientControlRequest = serde_json::from_slice(&request_frame.body)
                    .expect("scripted request body decodes");
                assert_eq!(request, step.expected);

                match step.action {
                    ScriptedControlAction::Reply(response) => {
                        let response = Frame::build_with_version(
                            request_frame.header.ver,
                            FrameType::Response,
                            request_frame.header.flags,
                            0,
                            0,
                            request_frame.header.corr,
                            serde_json::to_vec(&response).expect("scripted response serializes"),
                        )
                        .expect("scripted response frame builds");
                        write_frame(&mut stream, &response)
                            .await
                            .expect("scripted response writes");
                        stream.flush().await.expect("scripted response flushes");
                    }
                    ScriptedControlAction::NeverReply => pending::<()>().await,
                }
            }
        });

        Self {
            connection_file_path,
            _temp_dir: temp_dir,
            task,
        }
    }
}

impl Drop for ScriptedDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}
