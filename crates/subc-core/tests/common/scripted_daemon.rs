use cortexkit_test_support::ScratchDir;
use std::{future::pending, process, time::Duration};

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
    _temp_dir: ScratchDir,
    task: JoinHandle<()>,
}

pub struct ScriptedControlStep {
    pub expected: ClientControlRequest,
    pub action: ScriptedControlAction,
}

pub enum ScriptedControlAction {
    Reply(Box<ClientControlResponse>),
    Error(subc_protocol::ErrorBody),
    NeverReply,
}

impl ScriptedDaemon {
    pub async fn start(name: &str, steps: Vec<ScriptedControlStep>) -> Self {
        let temp_dir = ScratchDir::new(name);
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
                    ScriptedControlAction::Error(error) => {
                        let response = Frame::build_with_version(
                            request_frame.header.ver,
                            FrameType::Error,
                            request_frame.header.flags,
                            0,
                            0,
                            request_frame.header.corr,
                            serde_json::to_vec(&error).expect("scripted error serializes"),
                        )
                        .expect("scripted error frame builds");
                        write_frame(&mut stream, &response)
                            .await
                            .expect("scripted error writes");
                        stream.flush().await.expect("scripted error flushes");
                    }
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

/// Answers every `server.describe` on every connection with a fixed JSON
/// body, sent as it is. The typed response can only carry fields
/// `subc_control` knows, so a test that needs a describe reply with a field
/// the daemon doesn't send yet uses this. Any other request closes the
/// connection.
pub struct ScriptedDescribeDaemon {
    pub connection_file_path: std::path::PathBuf,
    _temp_dir: ScratchDir,
    task: JoinHandle<()>,
}

impl ScriptedDescribeDaemon {
    /// `daemon_ver` is written to the connection file, which is where `ck
    /// daemon` reads the version it prints.
    pub async fn start(name: &str, daemon_ver: &str, describe: serde_json::Value) -> Self {
        let temp_dir = ScratchDir::new(name);
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
            daemon_ver: daemon_ver.to_string(),
        };
        write_atomic(&connection_file_path, &connection).expect("scripted connection file");
        let body = serde_json::to_vec(&describe).expect("describe serializes");

        let task = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let connection = connection.clone();
                let body = body.clone();
                tokio::spawn(async move {
                    if authenticate_server(
                        &mut stream,
                        &connection.key,
                        &connection.daemon_id,
                        &connection.daemon_ver,
                        Duration::from_secs(2),
                    )
                    .await
                    .is_err()
                    {
                        return;
                    }
                    while let Ok(Some(request)) = read_frame(&mut stream).await {
                        let describe =
                            serde_json::from_slice::<ClientControlRequest>(&request.body)
                                .is_ok_and(|request| {
                                    matches!(request, ClientControlRequest::ServerDescribe {})
                                });
                        if request.header.channel != 0 || !describe {
                            return;
                        }
                        let response = Frame::build_with_version(
                            request.header.ver,
                            FrameType::Response,
                            request.header.flags,
                            0,
                            0,
                            request.header.corr,
                            body.clone(),
                        )
                        .expect("scripted response frame builds");
                        if write_frame(&mut stream, &response).await.is_err()
                            || stream.flush().await.is_err()
                        {
                            return;
                        }
                    }
                });
            }
        });

        Self {
            connection_file_path,
            _temp_dir: temp_dir,
            task,
        }
    }
}

impl Drop for ScriptedDescribeDaemon {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Sends one control request to the daemon named by `connection_file` and
/// returns its reply body as JSON, every field kept, the way a client that
/// reads the raw reply sees it.
pub async fn raw_control(
    connection_file: &std::path::Path,
    request: &ClientControlRequest,
) -> serde_json::Value {
    let connection =
        subc_transport::read_for_client(connection_file).expect("readable connection file");
    let endpoint = connection.endpoints.first().expect("an endpoint");
    let mut stream = tokio::net::TcpStream::connect((endpoint.host.as_str(), endpoint.port))
        .await
        .expect("daemon accepts");
    subc_transport::authenticate_client(&mut stream, &connection, Duration::from_secs(2))
        .await
        .expect("client authenticates");
    let frame = Frame::build(
        FrameType::Request,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        1,
        serde_json::to_vec(request).expect("request serializes"),
    )
    .expect("request frame builds");
    write_frame(&mut stream, &frame)
        .await
        .expect("request writes");
    stream.flush().await.expect("request flushes");
    let reply = read_frame(&mut stream)
        .await
        .expect("reply frame is valid")
        .expect("daemon replies");
    assert_eq!(
        reply.header.ty,
        FrameType::Response,
        "control request refused: {}",
        String::from_utf8_lossy(&reply.body)
    );
    serde_json::from_slice(&reply.body).expect("reply body is JSON")
}
