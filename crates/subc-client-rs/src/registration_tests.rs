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

async fn serve_peer(
    path: &Path,
    module_id: &str,
) -> (
    ModuleHandle,
    tokio::task::JoinHandle<Result<(), SubcModuleError>>,
) {
    let mut manifest = ModuleManifest::builder(module_id, "1.0.0")
        .registration_channel(true)
        .build();
    // Only the launched module can report how its launch nonce was delivered.
    // A second module served by the same process leaves that source field unset,
    // even when its HELLO uses the owner's environment-delivered nonce.
    manifest.provenance = Some(build_provenance(None, None, None).unwrap());
    let (handle, serving) = serve_with_handle(path, manifest, Hook).await.unwrap();
    assert!(!handle.shared.registration.active.load(Ordering::Acquire));
    (handle, tokio::spawn(serving))
}

struct RawClaimMinter(tokio::sync::Mutex<Option<Box<dyn ModuleStream>>>);

#[async_trait]
impl ClaimMinter for RawClaimMinter {
    async fn mint(&self, request: ClaimMintRequest) -> Result<ClaimMintReply, ClaimError> {
        let mut stream = self.0.lock().await;
        let stream = stream.as_mut().ok_or(ClaimError::Unavailable)?;
        send(stream, FrameType::Request, HELLO_CORR + 1, request).await;
        let reply = receive(stream).await;
        if reply.header.channel != 0 || reply.header.corr != HELLO_CORR + 1 {
            return Err(ClaimError::Protocol);
        }
        if reply.header.ty == FrameType::Error {
            let error: ErrorBody =
                serde_json::from_slice(&reply.body).map_err(|_| ClaimError::Protocol)?;
            return Err(ClaimError::Refused { code: error.code });
        }
        if reply.header.ty != FrameType::Response {
            return Err(ClaimError::Protocol);
        }
        serde_json::from_slice(&reply.body).map_err(|_| ClaimError::Protocol)
    }
}

async fn custom_loop(scenario: &str, path: &Path) {
    let unavailable = Arc::new(RawClaimMinter(tokio::sync::Mutex::new(None)));
    assert!(matches!(
        crate::registration::install_claim_minter(unavailable.clone()),
        Err(RegistrationError::NotAdopted)
    ));
    let channel = crate::registration::adopt_channel().unwrap();
    if scenario == "custom_none" {
        assert!(channel.is_none());
        assert!(matches!(
            crate::registration::adopt_channel(),
            Err(RegistrationError::Channel(
                os::RegistrationChannelError::AlreadyTaken
            ))
        ));
        assert!(matches!(
            crate::registration::install_claim_minter(unavailable),
            Err(RegistrationError::NotAdopted)
        ));
        assert!(!supervised());
    } else {
        let mut channel = channel.expect("daemon supplied a registration channel");
        assert!(matches!(
            crate::registration::adopt_channel(),
            Err(RegistrationError::Channel(
                os::RegistrationChannelError::AlreadyTaken
            ))
        ));
        let info = connection_file::read_for_client(path).unwrap();
        authenticate_client(&mut channel, &info, BUDGET)
            .await
            .unwrap();
        send(
            &mut channel,
            FrameType::Hello,
            HELLO_CORR,
            ModuleHelloBody {
                manifest: ModuleManifest::builder("channel-worker", "1.0.0")
                    .registration_channel(true)
                    .build(),
                protocol_ver: PROTOCOL_VERSION,
                control_ops: None,
                launch_nonce: None,
            },
        )
        .await;
        let ack = receive(&mut channel).await;
        assert_eq!(ack.header.ty, FrameType::HelloAck);
        let _: ModuleHelloAckBody = serde_json::from_slice(&ack.body).unwrap();
        let minter = Arc::new(RawClaimMinter(tokio::sync::Mutex::new(Some(channel))));
        crate::registration::install_claim_minter(minter.clone()).unwrap();
        assert!(matches!(
            crate::registration::install_claim_minter(minter),
            Err(RegistrationError::AlreadyInstalled)
        ));
    }
    let client = SubcConsumer::connect(path, options())
        .await
        .expect("custom-loop consumer must claim-bind when a channel was adopted");
    if scenario != "custom_none" {
        let error = client
            .open_route(
                RouteTarget::ToolProvider {
                    module_id: "provider".into(),
                },
                BindIdentity::new("test", "test"),
                CallOptions {
                    consumer_identity: Some(ConsumerIdentity {
                        module_id: "someone-else".into(),
                        launch_nonce: "not-a-binding".into(),
                    }),
                    ..CallOptions::default()
                },
            )
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("conflicts with connection binding"));
    }
    query(&client).await;
    println!("CASE_DONE");
    std::process::exit(0);
}

async fn child_case(scenario: &str, test: &str, path: &Path) {
    let role = env::var(ROLE).unwrap();
    if matches!(scenario, "custom" | "custom_none") {
        custom_loop(scenario, path).await;
        unreachable!();
    }
    if role == "ordinary" || role == "helper" {
        assert!(!supervised());
        let client = SubcConsumer::connect(path, options()).await.unwrap();
        query(&client).await;
        std::process::exit(0);
    }
    if scenario == "legacy_peer" {
        let (_, serving) = serve_peer(path, "fed:peer:legacy").await;
        assert!(!supervised());
        serving.abort();
        println!("CASE_DONE");
        std::process::exit(0);
    }
    let mut manifest = ModuleManifest::builder("channel-worker", "1.0.0")
        .registration_channel(true)
        .build();
    if scenario == "provenance" {
        manifest.provenance = Some(build_provenance(None, None, None).unwrap());
    }
    if scenario == "lazy" {
        assert!(
            take_stream().unwrap().is_none(),
            "transport lookup adopted before serving"
        );
        assert!(!bound(), "consumer identity lookup adopted before serving");
    }
    let mut peers = Vec::new();
    let early_peer = if scenario == "peer_first" {
        let path = path.to_owned();
        let mut peer = tokio::spawn(async move { serve_peer(&path, "fed:peer:before").await });
        assert!(
            timeout(Duration::from_millis(30), &mut peer).await.is_err(),
            "re-export did not wait for the owner registration"
        );
        assert!(
            OWNER.get().is_none(),
            "re-export became the registration owner"
        );
        Some(peer)
    } else {
        None
    };
    let stop = ModuleStopHandle::default();
    let (handle, serving) = ServeOptions::default()
        .with_stop_handle(stop.clone())
        .serve_with_handle(path, manifest.clone(), Hook)
        .await
        .unwrap()
        .unwrap();
    assert!(supervised());
    if scenario == "eof" {
        serving.await.unwrap();
        panic!("adopted channel EOF returned instead of ending the process");
    }
    if scenario == "close" || scenario == "provenance" {
        stop.stop();
        serving.await.unwrap();
        println!("LOCAL_CLOSE_RETURNED");
        std::process::exit(0);
    }
    let serving = tokio::spawn(serving);
    match scenario {
        "multi" => {
            peers.push(serve_peer(path, "fed:peer:one").await);
            peers.push(serve_peer(path, "fed:peer:two").await);
            peers[0].0.catalog_update(vec![]).await.unwrap();
        }
        "peer_first" => peers.push(early_peer.unwrap().await.unwrap()),
        "prefix_consumer" => {
            let peer = serve_peer(path, "fed:peer:consumer").await;
            for local in [false, true] {
                let result = if local {
                    peer.0.consumer_local(path, options()).await
                } else {
                    peer.0.consumer(path, options()).await
                };
                let error = result.err().expect("prefix child opened a consumer");
                assert!(error
                    .to_string()
                    .contains("prefix children have no consumer connections in this SDK yet"));
            }
            assert!(matches!(
                peer.0.helper_claim_handoff().await,
                Err(ClaimError::Unavailable)
            ));
            // Even an over-advertising daemon cannot make a prefix child mint
            // claims or send an owner-only operation on its registration.
            assert!(matches!(
                peer.0.scope_sync(1, vec![]).await,
                Err(ScopeCallError::NotSupported { .. })
            ));
            peers.push(peer);
        }
        "prefix_fallback" => peers.push(serve_peer(path, "unprefixed-extra").await),
        "prefix_error" => {
            let error = serve_with_handle(
                path,
                ModuleManifest::builder("fed:peer:refused", "1.0.0").build(),
                Hook,
            )
            .await
            .err()
            .expect("mint refusal registered an extra module");
            assert!(error.to_string().contains("fed:peer:refused"));
            assert!(error
                .to_string()
                .contains(error_codes::CLAIM_MINT_NOT_ALLOWED));
        }
        "duplicate" => {
            let error = match serve_with_handle(path, manifest, Hook).await {
                Err(error) => error,
                Ok(_) => panic!("duplicate launched module registered"),
            };
            assert_eq!(
                error.to_string(),
                "registration channel: launched module \"channel-worker\" is already serving on the registration channel"
            );
        }
        "second_take" | "lazy" => {
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
    for (_, serving) in peers {
        serving.abort();
    }
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

async fn register_peer(
    listener: &TcpListener,
    info: &ConnectionInfo,
    module_id: &str,
    claim: Option<Claim>,
    nonce: Option<&str>,
) -> tokio::net::TcpStream {
    let (mut stream, _) = timeout(BUDGET, listener.accept()).await.unwrap().unwrap();
    auth(&mut stream, info).await;
    if let Some(claim) = claim {
        let presentation = receive(&mut stream).await;
        assert_eq!(presentation.header.ty, FrameType::Request);
        let presented: ClaimPresentRequest = serde_json::from_slice(&presentation.body).unwrap();
        assert_eq!(presented.claim, claim);
        send(
            &mut stream,
            FrameType::Response,
            presentation.header.corr,
            ClaimPresentReply::new(),
        )
        .await;
    }
    let frame = receive(&mut stream).await;
    assert_eq!(
        frame.header.ty,
        FrameType::Hello,
        "expected exactly one HELLO after any claim presentation"
    );
    let hello: ModuleHelloBody = serde_json::from_slice(&frame.body).unwrap();
    assert_eq!(hello.manifest.module_id, module_id);
    assert_eq!(hello.launch_nonce.as_deref(), nonce);
    assert_eq!(
        hello.manifest.provenance.unwrap().launch_nonce_source,
        None,
        "re-export claimed the launch's nonce source"
    );
    send(
        &mut stream,
        FrameType::HelloAck,
        HELLO_CORR,
        ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: vec![
                "catalog.update".into(),
                "scope.sync".into(),
                "claim.mint".into(),
            ],
            subc_capabilities: vec![],
            storage: None,
            machine_id: None,
        },
    )
    .await;
    stream
}

async fn mint_prefix(
    registration: &mut Box<dyn ModuleStream>,
    module_id: &str,
    refusal: Option<&str>,
) -> Option<Claim> {
    let mint = receive(registration).await;
    assert_eq!(mint.header.ty, FrameType::Request);
    let request: ClaimMintRequest = serde_json::from_slice(&mint.body).unwrap();
    assert_eq!(
        request.purpose,
        ClaimPurpose::Prefix {
            module_id: module_id.into()
        }
    );
    if let Some(code) = refusal {
        send(
            registration,
            FrameType::Error,
            mint.header.corr,
            ErrorBody::new(code, "refused"),
        )
        .await;
        None
    } else {
        let claim = Claim::new(format!("prefix-{module_id}"));
        send(
            registration,
            FrameType::Response,
            mint.header.corr,
            ClaimMintReply::new(claim.clone(), 0),
        )
        .await;
        Some(claim)
    }
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
    if scenario == "custom_none" {
        command
            .env_remove(os::REGISTRATION_FD_ENV)
            .env_remove(os::REGISTRATION_PIPE_ENV);
        let mut child = command.spawn().unwrap();
        let (mut stream, _) = timeout(BUDGET, listener.accept()).await.unwrap().unwrap();
        auth(&mut stream, &info).await;
        let request = receive(&mut stream).await;
        assert_eq!(request.header.ty, FrameType::Request);
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(
            body["op"], "activity.query",
            "legacy consumer minted a claim"
        );
        send(&mut stream, FrameType::Response, request.header.corr,
            json!({"op":"activity.query","entries":[],"cursor":null,"head":{"daemon_incarnation":"test","seq":0}})).await;
        let output = assert_child(&mut child).await;
        assert!(
            output.contains("CASE_DONE"),
            "child exited before assertions: {output}"
        );
        return;
    }
    if matches!(
        scenario,
        "provenance" | "multi" | "peer_first" | "duplicate" | "legacy_peer"
    ) {
        command.env("SUBC_LAUNCH_NONCE", "legacy-diagnostic-nonce");
    }
    if scenario == "legacy_peer" {
        command
            .env_remove(os::REGISTRATION_FD_ENV)
            .env_remove(os::REGISTRATION_PIPE_ENV);
        let mut child = command.spawn().unwrap();
        let _peer = register_peer(
            &listener,
            &info,
            "fed:peer:legacy",
            None,
            Some("legacy-diagnostic-nonce"),
        )
        .await;
        let output = assert_child(&mut child).await;
        assert!(
            output.contains("CASE_DONE"),
            "child exited before assertions: {output}"
        );
        return;
    }
    #[cfg(unix)]
    let (mut registration, mut child) = inherited_channel(&mut command);
    #[cfg(windows)]
    let (mut registration, mut child) = inherited_channel(&mut command, scenario).await;
    let mut peers = Vec::new();
    if scenario == "peer_first" {
        assert!(
            timeout(Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "extra serve connected before the owner registration was live"
        );
    }
    auth(&mut registration, &info).await;
    let hello = receive(&mut registration).await;
    assert_eq!(hello.header.ty, FrameType::Hello);
    let hello: ModuleHelloBody = serde_json::from_slice(&hello.body).unwrap();
    assert!(hello.launch_nonce.is_none());
    assert_eq!(hello.manifest.module_id, "channel-worker");
    assert_eq!(hello.manifest.registration_channel, Some(true));
    if scenario == "provenance" {
        assert_eq!(
            hello.manifest.provenance.unwrap().launch_nonce_source,
            Some(LaunchNonceSource::Env)
        );
    }
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
    if matches!(
        scenario,
        "multi"
            | "peer_first"
            | "duplicate"
            | "prefix_fallback"
            | "prefix_error"
            | "prefix_consumer"
    ) {
        let ids: &[&str] = match scenario {
            "multi" => &["fed:peer:one", "fed:peer:two"],
            "peer_first" => &["fed:peer:before"],
            "prefix_fallback" => &["unprefixed-extra"],
            "prefix_error" => &["fed:peer:refused"],
            "prefix_consumer" => &["fed:peer:consumer"],
            _ => &[],
        };
        for id in ids {
            let refusal = match scenario {
                "prefix_fallback" => Some(error_codes::CLAIM_PREFIX_REFUSED),
                "prefix_error" => Some(error_codes::CLAIM_MINT_NOT_ALLOWED),
                _ => None,
            };
            let claim = mint_prefix(&mut registration, id, refusal).await;
            if scenario != "prefix_error" {
                peers.push(register_peer(&listener, &info, id, claim, None).await);
            }
        }
        if scenario == "multi" {
            let update = receive(&mut peers[0]).await;
            assert_eq!(
                update.header.ty,
                FrameType::Request,
                "child sent a second HELLO"
            );
            let body: Value = serde_json::from_slice(&update.body).unwrap();
            assert_eq!(body["op"], "catalog.update");
            send(
                &mut peers[0],
                FrameType::Response,
                update.header.corr,
                json!({"op":"catalog.update"}),
            )
            .await;
        }
        let output = assert_child(&mut child).await;
        assert!(
            output.contains("CASE_DONE"),
            "child exited before assertions: {output}"
        );
        assert!(
            next_request(&mut registration).await.is_none(),
            "unexpected owner-channel operation after prefix registration"
        );
        for peer in &mut peers {
            assert!(
                next_request(peer).await.is_none(),
                "extra serve sent another HELLO or an unrequested registration operation"
            );
        }
        assert!(
            timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err(),
            "unexpected TCP registration"
        );
        return;
    }
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
    if scenario == "close" || scenario == "provenance" {
        while read_frame(&mut registration).await.unwrap().is_some() {}
        let output = assert_child(&mut child).await;
        assert!(
            output.contains("LOCAL_CLOSE_RETURNED"),
            "local stop exited the process: {output}"
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
            "second_take" | "lazy" => 0,
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
        "direct" | "second_take" | "lazy" => 0,
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
contract_test!(custom_loop_adopts_and_claim_binds_consumer, "custom");
contract_test!(
    custom_loop_without_channel_uses_legacy_connection,
    "custom_none"
);
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
contract_test!(
    supervised_process_registers_own_id_and_two_reexports,
    "multi"
);
contract_test!(
    reexport_before_own_serve_preserves_inherited_channel,
    "peer_first"
);
contract_test!(duplicate_launched_module_serve_names_the_id, "duplicate");
contract_test!(
    legacy_reexport_sends_owner_nonce_without_claiming_its_provenance,
    "legacy_peer"
);
contract_test!(
    unprefixed_extra_serve_falls_back_only_on_prefix_refusal,
    "prefix_fallback"
);
contract_test!(prefix_mint_failure_names_module_and_code, "prefix_error");
contract_test!(
    prefix_child_refuses_consumers_and_owner_operations,
    "prefix_consumer"
);
contract_test!(consumer_lookup_does_not_adopt_before_serving, "lazy");
contract_test!(local_stop_returns_without_exiting, "close");
contract_test!(
    channel_hello_reports_legacy_nonce_source_without_sending_nonce,
    "provenance"
);
