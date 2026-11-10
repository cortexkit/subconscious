use super::*;
use consumer::{CallOptions, RetryBackoff};
use serde_json::{json, Value};
use std::process::{Command, Stdio};
use subc_transport::{
    authenticate_server,
    connection_file::{ConnectionInfo, Endpoint, SCHEMA_VERSION},
};
use tokio::{io::AsyncWriteExt, net::TcpListener};

const BUDGET: Duration = Duration::from_secs(8);
const ROLE: &str = "SUBC_SDK_CHANNEL_TEST_ROLE";

struct Hook;
#[async_trait]
impl ModuleHandler for Hook {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
    async fn on_connection_end(&self, end: ConnectionEnd) {
        println!("HOOK_{}", end.as_str());
    }
}

fn command(test: &str, role: &str, path: &Path) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", test, "--nocapture"])
        .env(ROLE, role)
        .env("SUBC_SDK_CHANNEL_TEST_PATH", path)
        .env("SUBC_MODULE_ID", "channel-worker")
        .env_remove("SUBC_LAUNCH_NONCE")
        .env_remove("SUBC_LAUNCH_NONCE_FD")
        .env_remove("SUBC_LAUNCH_NONCE_PIPE")
        .env_remove(os::HELPER_CLAIM_FD_ENV)
        .env_remove(os::HELPER_CLAIM_PIPE_ENV)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn options() -> ConsumerOptions {
    ConsumerOptions {
        reconnect_backoff: RetryBackoff {
            base: Duration::from_millis(1),
            max_attempts: 2,
            ..RetryBackoff::default()
        },
        ..ConsumerOptions::default()
    }
}

async fn query(client: &SubcConsumer) {
    let request = serde_json::from_value(json!({"op":"activity.query","limit":1})).unwrap();
    client.query_activity(request).await.unwrap();
}

async fn child_case(scenario: &str, test: &str, path: &Path) {
    let role = env::var(ROLE).unwrap();
    if role == "ordinary" || role == "helper" {
        assert!(!supervised());
        let client = SubcConsumer::connect(path, options()).await.unwrap();
        query(&client).await;
        std::process::exit(0);
    }
    let manifest = ModuleManifest::builder("channel-worker", "1.0.0")
        .registration_channel(true)
        .build();
    let (handle, serving) = serve_with_handle(path, manifest.clone(), Hook)
        .await
        .unwrap();
    assert!(supervised());
    if scenario == "eof" {
        serving.await.unwrap();
        panic!("adopted channel EOF returned instead of ending the process");
    }
    let serving = tokio::spawn(serving);
    match scenario {
        "second_take" => {
            assert!(matches!(
                serve_with_handle(path, manifest, Hook).await,
                Err(SubcModuleError::RegistrationChannel(_))
            ));
        }
        "direct" | "helper" => {
            let mut command = command(
                test,
                if scenario == "helper" {
                    "helper"
                } else {
                    "ordinary"
                },
                path,
            );
            let handoff = if scenario == "helper" {
                Some(handle.helper_claim_handoff().await.unwrap())
            } else {
                None
            };
            #[cfg(unix)]
            if let Some(handoff) = handoff {
                handoff.install_last(&mut command);
            }
            #[cfg(windows)]
            if let Some(handoff) = &handoff {
                handoff.configure(&mut command);
            }
            let mut child = command.spawn().unwrap();
            #[cfg(windows)]
            if let Some(handoff) = handoff {
                let pid = child.id();
                tokio::task::spawn_blocking(move || handoff.serve(pid))
                    .await
                    .unwrap()
                    .unwrap();
            }
            assert_child(&mut child).await;
        }
        "mint_refused" | "present_refused" => {
            assert!(
                SubcConsumer::connect(path, options()).await.is_err(),
                "refused claim became direct"
            );
        }
        "pool" => {
            let one = handle.consumer(path, options()).await.unwrap();
            let two = handle.consumer(path, options()).await.unwrap();
            assert!(
                Arc::ptr_eq(&one, &two),
                "equal binding/options did not reuse consumer"
            );
            let weak = Arc::downgrade(&one);
            let mut different = options();
            different.call_timeout += Duration::from_millis(1);
            let other = handle.consumer(path, different).await.unwrap();
            assert!(
                !Arc::ptr_eq(&one, &other),
                "different options shared a connection"
            );
            drop(other);
            drop(one);
            drop(two);
            assert!(weak.upgrade().is_none(), "pool kept the consumer alive");
            let fresh = handle.consumer(path, options()).await.unwrap();
            query(&fresh).await;
        }
        "reconnect" => {
            let client = SubcConsumer::connect(path, options()).await.unwrap();
            query(&client).await;
            timeout(BUDGET, async {
                while client.current_epoch() == 1 {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            query(&client).await;
        }
        "identity" => {
            let client = SubcConsumer::connect(path, options()).await.unwrap();
            let opts = CallOptions {
                consumer_identity: Some(ConsumerIdentity {
                    module_id: "someone-else".into(),
                    launch_nonce: "not-a-binding".into(),
                }),
                ..CallOptions::default()
            };
            let error = client
                .open_route(
                    RouteTarget::ToolProvider {
                        module_id: "provider".into(),
                    },
                    BindIdentity::new("test", "test"),
                    opts,
                )
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("conflicts with connection binding"));
            query(&client).await;
        }
        "rate" => {
            let client = SubcConsumer::connect(path, options()).await.unwrap();
            query(&client).await;
        }
        _ => panic!("unknown child scenario"),
    }
    // Only the EOF case asks the SDK to end the child. Other cases exit before
    // the test daemon closes registration, so an unexpected EOF cannot mask a failure.
    serving.abort();
    println!("CASE_DONE");
    std::process::exit(0);
}

async fn assert_child(child: &mut std::process::Child) -> String {
    let deadline = Instant::now() + BUDGET;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut stdout = String::new();
            let mut stderr = String::new();
            use std::io::Read;
            child
                .stdout
                .take()
                .unwrap()
                .read_to_string(&mut stdout)
                .unwrap();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut stderr)
                .unwrap();
            assert!(
                status.success(),
                "child failed: {status}\n{stdout}\n{stderr}"
            );
            return stdout;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("child did not terminate");
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn receive<S: AsyncRead + Unpin>(stream: &mut S) -> Frame {
    timeout(BUDGET, read_frame(stream))
        .await
        .unwrap()
        .unwrap()
        .expect("frame before EOF")
}

async fn next_request<S: AsyncRead + Unpin>(stream: &mut S) -> Option<Frame> {
    match read_frame(stream).await {
        Ok(frame) => frame,
        // The subprocess deliberately uses process::exit after its assertions.
        // Windows may reset its TCP consumers instead of delivering a FIN.
        Err(FrameIoError::Io(error)) if is_disconnect(&error) => None,
        Err(error) => panic!("invalid consumer frame: {error}"),
    }
}

async fn send<S: AsyncWrite + Unpin>(
    stream: &mut S,
    ty: FrameType,
    corr: u64,
    body: impl serde::Serialize,
) {
    let frame = Frame::build(
        ty,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&body).unwrap(),
    )
    .unwrap();
    write_frame(stream, &frame).await.unwrap();
    stream.flush().await.unwrap();
}

async fn auth<S: AsyncRead + AsyncWrite + Unpin>(stream: &mut S, info: &ConnectionInfo) {
    authenticate_server(stream, &info.key, &info.daemon_id, &info.daemon_ver, BUDGET)
        .await
        .unwrap();
}

#[cfg(unix)]
fn inherited_channel(command: &mut Command) -> (Box<dyn ModuleStream>, std::process::Child) {
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};
    let (server, client) = std::os::unix::net::UnixStream::pair().unwrap();
    let fd: std::os::fd::OwnedFd = client.try_clone().unwrap().into();
    let file = std::fs::File::from(fd);
    let inode = file.metadata().unwrap().ino();
    socket2::SockRef::from(&client).set_cloexec(false).unwrap();
    command.env(
        os::REGISTRATION_FD_ENV,
        format!("{}:{inode}", client.as_raw_fd()),
    );
    // Keep the inheritable end alive through spawn, then close the parent's copy.
    let child = command.spawn().unwrap();
    drop(client);
    server.set_nonblocking(true).unwrap();
    (
        Box::new(tokio::net::UnixStream::from_std(server).unwrap()),
        child,
    )
}

#[cfg(windows)]
async fn inherited_channel(
    command: &mut Command,
    scenario: &str,
) -> (Box<dyn ModuleStream>, std::process::Child) {
    let name = format!(
        r"\\.\pipe\subc-sdk-registration-{}-{scenario}",
        std::process::id()
    );
    let server = tokio::net::windows::named_pipe::ServerOptions::new()
        .first_pipe_instance(true)
        .create(&name)
        .unwrap();
    command.env(os::REGISTRATION_PIPE_ENV, name);
    let child = command.spawn().unwrap();
    timeout(BUDGET, server.connect()).await.unwrap().unwrap();
    (Box::new(server), child)
}

async fn run(scenario: &'static str, test: &str) {
    if env::var_os(ROLE).is_some() {
        let path = PathBuf::from(env::var_os("SUBC_SDK_CHANNEL_TEST_PATH").unwrap());
        child_case(scenario, test, &path).await;
        unreachable!();
    }
    let directory = cortexkit_test_support::ScratchDir::new("registration");
    let path = directory.path().join("connection.json");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: vec![0x42; 32],
        daemon_id: [0x21; 16],
        pid: std::process::id(),
        daemon_ver: "0.39.0".into(),
    };
    connection_file::write_atomic(&path, &info).unwrap();
    let mut command = command(test, "module", &path);
    command.env(os::DAEMON_PID_ENV, std::process::id().to_string());
    #[cfg(unix)]
    let (mut registration, mut child) = inherited_channel(&mut command);
    #[cfg(windows)]
    let (mut registration, mut child) = inherited_channel(&mut command, scenario).await;
    auth(&mut registration, &info).await;
    let hello = receive(&mut registration).await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    let hello: ModuleHelloBody = serde_json::from_slice(&hello.body).unwrap();
    assert!(hello.launch_nonce.is_none());
    assert_eq!(hello.manifest.registration_channel, Some(true));
    send(
        &mut registration,
        FrameType::HelloAck,
        HELLO_CORR,
        ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec!["claim.mint".into()],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        },
    )
    .await;
    if scenario == "eof" {
        registration.shutdown().await.unwrap();
        drop(registration);
        let output = assert_child(&mut child).await;
        assert!(
            output.contains("HOOK_eof"),
            "process exited without its serve hook: {output}"
        );

        return;
    }

    let (minted, mut claims) = mpsc::channel(16);
    let broker = tokio::spawn(async move {
        let mut purposes = Vec::new();
        let mut limited_at = None;
        while let Some(frame) = read_frame(&mut registration).await.unwrap() {
            let request: ClaimMintRequest = serde_json::from_slice(&frame.body).unwrap();
            assert_eq!(frame.header.channel, 0);
            if scenario == "rate" && limited_at.is_none() {
                limited_at = Some(Instant::now());
                send(
                    &mut registration,
                    FrameType::Error,
                    frame.header.corr,
                    ErrorBody::new(error_codes::CLAIM_RATE_LIMITED, "wait")
                        .with_detail(json!({"retry_after_ms":80})),
                )
                .await;
                continue;
            }
            if let Some(at) = limited_at {
                assert!(
                    Instant::now().duration_since(at) >= Duration::from_millis(80),
                    "SDK ignored retry_after_ms"
                );
            }
            purposes.push(request.purpose);
            if scenario == "mint_refused" {
                send(
                    &mut registration,
                    FrameType::Error,
                    frame.header.corr,
                    ErrorBody::new(error_codes::CLAIM_MINT_NOT_ALLOWED, "no authority"),
                )
                .await;
            } else {
                let claim = Claim::new(format!("{:022}", purposes.len()));
                minted.send(claim.clone()).await.unwrap();
                // An expired-looking wall clock must not become the SDK's
                // validity decision. The daemon's monotonic clock owns expiry.
                send(
                    &mut registration,
                    FrameType::Response,
                    frame.header.corr,
                    ClaimMintReply::new(claim, 0),
                )
                .await;
            }
        }
        purposes
    });
    let tcp = tokio::spawn(async move {
        let expected = match scenario {
            "second_take" => 0,
            "pool" => 3,
            "reconnect" => 2,
            _ => 1,
        };
        let mut tasks = Vec::new();
        for index in 0..expected {
            let (mut stream, _) = timeout(BUDGET, listener.accept()).await.unwrap().unwrap();
            auth(&mut stream, &info).await;
            if scenario == "mint_refused" {
                assert!(
                    timeout(BUDGET, read_frame(&mut stream))
                        .await
                        .unwrap()
                        .unwrap()
                        .is_none(),
                    "mint refusal sent an unbound request"
                );
                continue;
            }
            if scenario != "direct" {
                let presentation = receive(&mut stream).await;
                assert_eq!(presentation.header.channel, 0);
                let presented: ClaimPresentRequest =
                    serde_json::from_slice(&presentation.body).unwrap();
                assert!(
                    presented.claim == claims.recv().await.unwrap(),
                    "connection did not use its fresh claim"
                );
                if scenario == "present_refused" {
                    send(
                        &mut stream,
                        FrameType::Error,
                        presentation.header.corr,
                        ErrorBody::new(error_codes::CLAIM_INVALID, "refused"),
                    )
                    .await;
                    assert!(
                        read_frame(&mut stream).await.unwrap().is_none(),
                        "presentation refusal continued as direct"
                    );
                    continue;
                }
                send(
                    &mut stream,
                    FrameType::Response,
                    presentation.header.corr,
                    ClaimPresentReply::new(),
                )
                .await;
            }
            tasks.push(tokio::spawn(async move {
                while let Some(request) = next_request(&mut stream).await {
                    if request.header.ty == FrameType::Goodbye { break; }
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    assert_eq!(body["op"], "activity.query");
                    assert!(body.get("consumer_identity").is_none(), "connection binding leaked a nonce identity");
                    send(&mut stream, FrameType::Response, request.header.corr,
                        json!({"op":"activity.query","entries":[],"cursor":null,"head":{"daemon_incarnation":"test","seq":0}})).await;
                    if scenario == "reconnect" && index == 0 {
                        stream.shutdown().await.unwrap();
                        break;
                    }
                }
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
    });
    let output = assert_child(&mut child).await;
    assert!(
        output.contains("CASE_DONE"),
        "child exited before assertions: {output}"
    );
    tcp.await.unwrap();
    let purposes = broker.await.unwrap();
    let expected_mints = match scenario {
        "direct" | "second_take" => 0,
        "pool" => 3,
        "reconnect" => 2,
        _ => 1,
    };
    assert_eq!(purposes.len(), expected_mints);
    for purpose in purposes {
        assert_eq!(
            purpose,
            if scenario == "helper" {
                ClaimPurpose::Helper
            } else {
                ClaimPurpose::SelfConnection
            }
        );
    }
}

macro_rules! contract_test {
    ($name:ident, $scenario:literal) => {
        #[tokio::test]
        async fn $name() {
            run(
                $scenario,
                concat!("registration::registration_contract::", stringify!($name)),
            )
            .await;
        }
    };
}
contract_test!(adopted_eof_exits_after_hook, "eof");
contract_test!(inherited_variable_without_descriptor_is_direct, "direct");
contract_test!(mint_refusal_never_falls_back_to_direct, "mint_refused");
contract_test!(
    presentation_refusal_never_falls_back_to_direct,
    "present_refused"
);
contract_test!(weak_pool_reuses_only_equal_binding_options, "pool");
contract_test!(reconnect_presents_a_fresh_self_claim, "reconnect");
contract_test!(rate_limit_observes_retry_after_ms, "rate");
contract_test!(bound_connection_refuses_identity_override, "identity");
contract_test!(only_explicit_helper_handoff_delegates_claim, "helper");
contract_test!(registration_is_never_reconnected, "second_take");
