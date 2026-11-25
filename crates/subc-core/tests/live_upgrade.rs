#![forbid(unsafe_code)]
// The daemon only carries children across exec on Linux and macOS.
#![cfg(any(target_os = "linux", target_os = "macos"))]

mod common;

use std::{
    fs,
    io::{self, BufRead},
    path::{Path, PathBuf},
    process::{Output, Stdio},
    time::Duration,
};

use common::{connect_authed_client, kit::ShortRoot};
use serde_json::{json, Value};
use subc_client_rs::{CallOptions, SubcConsumer};
use subc_control::ClientControlRequest;
use subc_protocol::{BindIdentity, Flags, FrameType, Priority, RouteTarget};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UnixListener},
    process::{Child, Command},
    time::{sleep, timeout},
};

const WAIT: Duration = Duration::from_secs(24);
const CARRY: &str = "carried-stub";
const AUTO: &str = "auto-stub";
const MANUAL: &str = "manual-stub";

fn isolated(program: impl AsRef<Path>, root: &Path) -> Command {
    let mut command = Command::new(program.as_ref());
    command
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("HOME", root)
        .env("TMPDIR", root)
        .env(
            "SUBC_CONNECTION_FILE",
            root.join("runtime")
                .join(subc_transport::CONNECTION_FILE_NAME),
        )
        .kill_on_drop(true)
        .stdin(Stdio::null());
    command
}

struct Rig {
    root: ShortRoot,
    process: Child,
    connection: PathBuf,
    executable: PathBuf,
}

impl Rig {
    async fn start() -> Self {
        let root = ShortRoot::create_short_socket_root();
        let executable = root.join("ckdev-subc-upgrade-fixture");
        let connection = root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME);
        fs::create_dir_all(root.join("runtime")).unwrap();
        fs::create_dir_all(root.join("config/cortexkit")).unwrap();
        let module = |fixture: &str, enabled: bool| {
            json!({
                "program": env!("CARGO_BIN_EXE_channel-module-rs"),
                "enabled":enabled,"reserved":true,"registration_channel":true,
                "restart":{"max_restarts":0},"drain_timeout_ms":900,
                "env":{
                    "CHANNEL_MODULE_ROOT":root.path(),"CHANNEL_MODULE_UPGRADE_FIXTURE":fixture,
                    "XDG_DATA_HOME":root.join("data"),"XDG_RUNTIME_DIR":root.join("runtime"),
                    "XDG_CONFIG_HOME":root.join("config"),"HOME":root.path(),
                },
            })
        };
        fs::write(root.join("config/cortexkit/subc.jsonc"), json!({
            "version":1,"privileged_operator_prompt":"log",
            "modules": { CARRY:module("e2e-carry", true), AUTO:module("slow-stop", true), MANUAL:module("plain", false) },
        }).to_string()).unwrap();
        let approval = root.join("approvals");
        fs::write(&approval, "approve\n".repeat(12)).unwrap();
        let log = fs::File::create(root.join("launch.log")).unwrap();
        // A fixed port makes the rebind assertion cover an existing TCP client.
        let port = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let binary = common::ckdev_subc_under_test();
        assert!(isolated("cp", root.path())
            .args([&binary, &executable])
            .status()
            .await
            .unwrap()
            .success());
        let process = isolated(&executable, root.path())
            .env("SUBC_TEST_OPERATOR_SCRIPT", approval)
            .env("SUBC_TEST_RESUME_PAUSE_MODULE", CARRY)
            .env("SUBC_TEST_RESUME_PAUSE_LOCAL_LISTENER", "1")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env("SUBC_PORT", port.to_string())
            .env_remove("SUBC_TEST_UPGRADE_TEAM_ID")
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let rig = Self {
            root,
            process,
            connection,
            executable,
        };
        timeout(WAIT, async {
            loop {
                if let Ok(describe) = rig.describe().await {
                    if describe["counters"]["daemon_identity"]["pid"] == rig.process.id().unwrap() {
                        break;
                    }
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("daemon did not come up: {}", rig.logs()));
        rig
    }

    fn run_dir(&self) -> PathBuf {
        self.root.join("data/cortexkit/run")
    }

    fn ck(&self) -> Command {
        let mut command = isolated(common::ckdev_ck(), self.root.path());
        command
            .arg("--subc")
            .arg(&self.connection)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    async fn stage(&self) {
        let stage = self.root.join("staged");
        assert!(isolated("cp", self.root.path())
            .args([&self.executable, &stage])
            .status()
            .await
            .unwrap()
            .success());
        fs::rename(stage, &self.executable).unwrap();
    }

    async fn describe(&self) -> io::Result<Value> {
        control(
            &mut connect_authed_client(&self.connection).await?,
            &ClientControlRequest::ServerDescribe {},
        )
        .await
        .map(|(kind, body)| {
            assert_eq!(kind, FrameType::Response, "{body}");
            body
        })
    }

    fn events(&self, module: &str) -> Vec<Value> {
        fs::read_to_string(self.root.join(format!("{module}.events")))
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
    }

    async fn wait_for_event(&self, module: &str, event: &str, count: usize) -> Vec<Value> {
        timeout(WAIT, async {
            loop {
                let events: Vec<_> = self
                    .events(module)
                    .into_iter()
                    .filter(|entry| entry["event"] == event)
                    .collect();
                if events.len() >= count {
                    break events;
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "{module} did not record {event}: {:?}\n{}",
                self.events(module),
                self.logs()
            )
        })
    }

    async fn registered_pid(&self, module: &str, previous: Option<u32>) -> u32 {
        timeout(WAIT, async {
            loop {
                if let Some(pid) =
                    fs::read_to_string(self.root.join(format!("{module}.registered")))
                        .ok()
                        .and_then(|raw| raw.parse().ok())
                {
                    if previous != Some(pid) {
                        break pid;
                    }
                }
                sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{module} did not register: {}", self.logs()))
    }

    fn logs(&self) -> String {
        let mut logs = fs::read_to_string(self.root.join("launch.log")).unwrap_or_default();
        for entry in fs::read_dir(self.run_dir().join("logs"))
            .into_iter()
            .flatten()
            .flatten()
        {
            logs.push_str(&fs::read_to_string(entry.path()).unwrap_or_default());
        }
        logs
    }

    fn upgrade(&self) -> Child {
        self.ck()
            .args(["--json", "daemon", "upgrade", "--in-place"])
            .spawn()
            .unwrap()
    }
}

fn report(output: Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("ck emits exactly one JSON document")
}

fn host_cold_starts() -> usize {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return 0;
    };
    // Only read the host's daemon log, not any connection or state file. The test
    // uses a separate HOME and all three XDG paths in every spawned command.
    let run = home.join(".local/share/cortexkit/run");
    let segments = fs::read_dir(run.join("logs"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            (name.starts_with("subc.") && name.ends_with(".log")).then(|| entry.path())
        });
    segments
        .chain(std::iter::once(run.join("subc.log")))
        .filter_map(|path| fs::File::open(path).ok())
        .map(|file| {
            io::BufReader::new(file)
                .split(b'\n')
                .map_while(Result::ok)
                .filter(|line| {
                    line.windows(b"subc daemon starting".len())
                        .any(|part| part == b"subc daemon starting")
                })
                .count()
        })
        .sum()
}

async fn call(
    consumer: &SubcConsumer,
    payload: &[u8],
) -> Result<Vec<u8>, subc_client_rs::CallError> {
    consumer
        .call(
            RouteTarget::ToolProvider {
                module_id: CARRY.into(),
            },
            BindIdentity::new("upgrade-test", "session"),
            payload.to_vec(),
            CallOptions {
                timeout: WAIT,
                ..Default::default()
            },
        )
        .await
}

async fn control(
    stream: &mut TcpStream,
    request: &ClientControlRequest,
) -> io::Result<(FrameType, Value)> {
    let frame = subc_daemon::Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        1,
        serde_json::to_vec(request)?,
    )
    .unwrap();
    subc_daemon::write_frame(stream, &frame)
        .await
        .map_err(io::Error::other)?;
    let frame = subc_daemon::read_frame(stream)
        .await
        .map_err(io::Error::other)?
        .ok_or(io::ErrorKind::UnexpectedEof)?;
    assert_eq!(frame.header.corr, 1);
    Ok((frame.header.ty, serde_json::from_slice(&frame.body)?))
}

#[tokio::test]
async fn live_upgrade_carries_a_handler_and_restarts_both_kinds_of_module_twice() {
    let host_starts = host_cold_starts();
    let mut rig = Rig::start().await;
    assert_eq!(rig.logs().matches("subc daemon starting").count(), 1);
    assert_eq!(rig.logs().matches("subc daemon resuming").count(), 0);
    let daemon_pid = rig.process.id().unwrap();
    let carried_pid = rig.registered_pid(CARRY, None).await;
    let mut auto_pid = rig.registered_pid(AUTO, None).await;
    assert!(!rig.root.join(format!("{MANUAL}.registered")).exists());
    let started = rig
        .ck()
        .args(["--json", "module", "start", MANUAL])
        .output()
        .await
        .unwrap();
    assert_eq!(report(started, 0)["module_id"], MANUAL);
    let mut manual_pid = rig.registered_pid(MANUAL, None).await;
    rig.wait_for_event(CARRY, "connected", 1).await;

    let gate = UnixListener::bind(rig.run_dir().join("rp.sock")).unwrap();
    let local_gate = UnixListener::bind(
        rig.connection
            .with_file_name("resume-local-listener.test.sock"),
    )
    .unwrap();
    let original_consumer = SubcConsumer::connect(&rig.connection, Default::default())
        .await
        .unwrap();
    let waiting =
        tokio::spawn(async move { call(&original_consumer, b"original-generation payload").await });
    rig.wait_for_event(CARRY, "entered_barrier", 1).await;
    let mut known_tokens = vec![];
    let mut prior_attempt = Value::Null;
    let first_ack = &rig.wait_for_event(CARRY, "hello_ack", 1).await[0];
    known_tokens.push(first_ack["token_sha256"].as_str().unwrap().to_owned());

    // Replace the startup path before asking ck to hash it. On Linux the
    // currently executing inode no longer has that pathname after the rename.
    rig.stage().await;
    for generation in 1..=2 {
        let old = subc_transport::read_for_client(&rig.connection).unwrap();
        let mut old_tcp_client = connect_authed_client(&rig.connection).await.unwrap();
        let upgrading = rig.upgrade();
        let (mut local, _) = timeout(WAIT, local_gate.accept()).await.unwrap().unwrap();
        assert_eq!(local.read_u8().await.unwrap(), 1);
        let endpoint = &old.endpoints[0];
        let mut tcp = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .unwrap();
        let proof =
            subc_transport::authenticate_client(&mut tcp, &old, Duration::from_secs(1)).await;
        // Receiving a new ServerProof proves the TCP accept loop is running,
        // even though local readiness has kept the new keys unpublished.
        assert!(
            matches!(
                proof,
                Err(subc_transport::AuthError::InvalidServerProof
                    | subc_transport::AuthError::DaemonIdMismatch)
            ),
            "TCP handshake while local is paused: {proof:?}"
        );
        assert_eq!(
            rig.logs().matches("resume_accept_ms=").count(),
            generation - 1,
            "the TCP accept loop must not finish the local readiness measurement"
        );
        local.write_u8(1).await.unwrap();
        let (mut held, _) = timeout(WAIT, gate.accept())
            .await
            .unwrap_or_else(|_| panic!("resume did not start: {}", rig.logs()))
            .unwrap();
        assert_eq!(timeout(WAIT, held.read_u8()).await.unwrap().unwrap(), 1);
        let current = subc_transport::read_for_client(&rig.connection).unwrap();
        assert_eq!(current.pid, daemon_pid);
        assert_ne!(current.daemon_id, old.daemon_id);
        assert_eq!(current.endpoints[0].port, old.endpoints[0].port);
        // Drain any close notification and require EOF on the old connection.
        let mut closed_frames = Vec::new();
        timeout(WAIT, old_tcp_client.read_to_end(&mut closed_frames))
            .await
            .unwrap()
            .unwrap();
        assert!(connect_authed_client(&rig.connection).await.is_ok());

        let described = rig.describe().await.unwrap();
        let pending = &described["counters"]["last_upgrade"];
        assert_eq!(pending["outcome"], "pending");
        assert!(pending["modules"]
            .as_array()
            .unwrap()
            .contains(&json!({"id":CARRY,"disposition":"pending"})));
        let busy = timeout(WAIT, rig.upgrade().wait_with_output())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(report(busy, 3)["reason"], "upgrade_busy");
        let mut direct = connect_authed_client(&rig.connection).await.unwrap();
        let (kind, refusal) = control(
            &mut direct,
            &ClientControlRequest::DaemonUpgradeInPlace {
                executable_sha256: described["counters"]["daemon_identity"]["executable_sha256"]
                    .as_str()
                    .unwrap()
                    .into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(kind, FrameType::Error);
        assert_eq!(refusal["detail"]["reason"], "upgrade");
        held.write_u8(1).await.unwrap();

        let result = timeout(WAIT, upgrading.wait_with_output())
            .await
            .unwrap()
            .unwrap();
        let result = report(result, 0);
        assert_eq!(result["outcome"], "upgraded");
        assert_eq!(result["daemon_pid"], daemon_pid);
        assert_eq!(result["attempt_id"], pending["attempt_id"]);
        assert_ne!(result["attempt_id"], prior_attempt);
        prior_attempt = result["attempt_id"].clone();
        assert_eq!(result["carried"], json!([CARRY]));
        assert_eq!(result["restarted"], json!([AUTO, MANUAL]));
        assert_eq!(result["stopped"], json!([]));
        assert_eq!(result["blocked"], json!([]));
        assert_eq!(
            result["build"]["core_version"],
            described["counters"]["daemon_identity"]["core_version"]
        );
        assert_eq!(
            result["build"]["build_git_sha"],
            described["counters"]["daemon_identity"]["build_git_sha"]
        );
        auto_pid = rig.registered_pid(AUTO, Some(auto_pid)).await;
        manual_pid = rig.registered_pid(MANUAL, Some(manual_pid)).await;
        assert_eq!(rig.registered_pid(CARRY, None).await, carried_pid);
        let ack = &rig.wait_for_event(CARRY, "hello_ack", generation + 1).await[generation];
        assert_eq!(ack["pid"], carried_pid);
        let hash = ack["token_sha256"].as_str().unwrap();
        assert!(
            !known_tokens.iter().any(|token| token == hash),
            "each image must issue a new token"
        );
        known_tokens.push(hash.to_owned());
        rig.wait_for_event(CARRY, "reconnected", generation).await;

        if generation == 1 {
            // Release the old handler only after a fresh call has proven the
            // replacement session can still authenticate as the same module.
            let counters = rig.describe().await.unwrap()["counters"].clone();
            fs::write(rig.root.join("release-handler"), "release").unwrap();
            rig.wait_for_event(CARRY, "finished_barrier", 1).await;
            let fresh = SubcConsumer::connect(&rig.connection, Default::default())
                .await
                .unwrap();
            assert_eq!(
                call(&fresh, b"replacement-generation payload")
                    .await
                    .unwrap(),
                b"replacement-generation payload"
            );
            let after = &rig.describe().await.unwrap()["counters"];
            assert_eq!(
                after["module_frames_dropped_no_route"], counters["module_frames_dropped_no_route"],
                "an old terminal must not even be received and dropped on the resumed connection"
            );
        }
    }
    assert!(
        timeout(WAIT, waiting).await.unwrap().unwrap().is_err(),
        "the terminal response for the original-generation payload must not be delivered"
    );
    let events = rig.events(CARRY);
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "connection_end"),
        "retry must never end the module: {events:?}"
    );
    let self_binds: Vec<_> = events
        .iter()
        .filter(|event| event["event"] == "bind" && event["principal"]["kind"] == "reserved")
        .collect();
    assert_eq!(
        self_binds.len(),
        3,
        "one self-identified bind in each session: {events:?}"
    );
    for bind in self_binds {
        assert_eq!(
            bind["principal"],
            json!({"kind":"reserved","module_id":CARRY})
        );
    }
    let logs = rig.logs();
    for (text, count) in [
        ("subc daemon starting", 1),
        ("subc daemon resuming", 2),
        ("upgrade output crosses the exec", 1),
        ("upgrade stderr before exec", 1),
        ("upgrade original-generation handler completed", 1),
    ] {
        assert_eq!(
            logs.matches(text).count(),
            count,
            "unexpected count for {text}: {logs}"
        );
    }
    let expiry: Vec<_> = logs
        .lines()
        .filter(|line| {
            line.contains("drain budget expired before the module exited") && line.contains(AUTO)
        })
        .collect();
    assert_eq!(
        expiry.len(),
        2,
        "the autostarted module must exhaust its stop budget on each attempt: {logs}"
    );
    let accept: Vec<_> = logs
        .lines()
        .filter_map(|line| line.split_once("resume_accept_ms="))
        .map(|(_, metric)| {
            metric
                .split_whitespace()
                .next()
                .unwrap()
                .parse::<u64>()
                .unwrap()
        })
        .collect();
    assert_eq!(
        accept.len(),
        2,
        "missing local listener measurements: {logs}"
    );
    assert!(
        accept.iter().all(|ms| *ms < 5_000),
        "resume listener took {accept:?} ms"
    );

    rustix::process::kill_process(
        rustix::process::Pid::from_raw(daemon_pid as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    assert!(timeout(WAIT, rig.process.wait())
        .await
        .unwrap()
        .unwrap()
        .success());
    assert_eq!(
        host_cold_starts(),
        host_starts,
        "the test must not restart the host's live daemon"
    );
}

mod cli {
    use super::*;
    use common::scripted_daemon::{ScriptedControlAction, ScriptedControlStep, ScriptedDaemon};
    use sha2::{Digest, Sha256};
    use subc_protocol::ErrorBody;

    struct Stub {
        root: ShortRoot,
        identity: Value,
        ask: ClientControlRequest,
    }

    impl Stub {
        fn new() -> Self {
            let root = ShortRoot::create_short_socket_root();
            let candidate = root.join("candidate");
            let fixture = b"bytes ck must hash, not the previous image's digest";
            fs::write(&candidate, fixture).unwrap();
            let checksum = format!("{:x}", Sha256::digest(fixture));
            let identity = json!({
                "pid":std::process::id(), "executable":candidate,
                "executable_sha256":"0".repeat(64), "core_version":"1.0.0", "build_git_sha":null,
            });
            Self {
                root,
                identity,
                ask: ClientControlRequest::DaemonUpgradeInPlace {
                    executable_sha256: checksum,
                },
            }
        }

        fn describe(&self, attempt: Value) -> ScriptedControlStep {
            describe(json!({"daemon_identity":self.identity,"last_upgrade":attempt}))
        }

        async fn output(&self, daemon: &mut ScriptedDaemon, code: i32) -> Value {
            let ck = common::ckdev_ck();
            let output = timeout(
                WAIT,
                isolated(ck, self.root.path())
                    .arg("--subc")
                    .arg(&daemon.connection_file_path)
                    .args(["daemon", "upgrade", "--in-place", "--json"])
                    .output(),
            )
            .await
            .unwrap()
            .unwrap();
            let result = report(output, code);
            daemon.assert_complete().await;
            result
        }
    }

    fn describe(counters: Value) -> ScriptedControlStep {
        response(
            ClientControlRequest::ServerDescribe {},
            json!({
                "op":"server.describe", "protocol_ver":subc_protocol::PROTOCOL_VERSION, "subc_ops":[],
                "capabilities":[], "connected_clients":1, "counters":counters,
            }),
        )
    }

    fn response(expected: ClientControlRequest, body: Value) -> ScriptedControlStep {
        ScriptedControlStep {
            expected,
            action: ScriptedControlAction::Reply(Box::new(serde_json::from_value(body).unwrap())),
        }
    }

    #[tokio::test]
    async fn live_upgrade_without_identity_refuses_before_sending_the_op() {
        let stub = Stub::new();
        let mut daemon =
            ScriptedDaemon::start("upgrade-no-identity", vec![describe(json!({}))]).await;
        let result = stub.output(&mut daemon, 3).await;
        assert_eq!(result["outcome"], "refused");
        assert_eq!(result["reason"], "upgrade_in_place_unsupported");
        assert_eq!(result["daemon_pid"], std::process::id());
        assert_eq!(result["build"], Value::Null);
    }

    #[tokio::test]
    async fn live_upgrade_distinguishes_refusal_from_an_attempt_that_failed() {
        for (code, exit) in [
            ("unknown_control_op", 3),
            ("operator_declined", 3),
            ("upgrade_failed", 4),
        ] {
            let stub = Stub::new();
            let mut error = ErrorBody::new(code, "test daemon refused the request");
            if exit == 4 {
                error = error.with_detail(json!({"stage":"planned_stop","attempt_id":"failed-attempt","restarted":["worker"]}));
            }
            let mut daemon = ScriptedDaemon::start(
                "upgrade-refusal",
                vec![
                    stub.describe(Value::Null),
                    ScriptedControlStep {
                        expected: stub.ask.clone(),
                        action: ScriptedControlAction::Error(error),
                    },
                ],
            )
            .await;
            let result = stub.output(&mut daemon, exit).await;
            if exit == 4 {
                assert_eq!(result["outcome"], "failed");
                assert_eq!(result["reason"], "planned_stop");
                assert_eq!(result["attempt_id"], "failed-attempt");
                assert_eq!(result["restarted"], json!(["worker"]));
            } else {
                assert_eq!(result["outcome"], "refused");
                assert_eq!(result["reason"], code);
                assert!(result.get("attempt_id").is_none());
            }
        }
    }

    #[tokio::test]
    async fn live_upgrade_lost_receipt_ignores_the_prior_and_polls_without_resending() {
        let mut stub = Stub::new();
        // Give the prior attempt a timestamp after the call; only the id can
        // rule it out. A different id alone is also insufficient.
        let attempt = |id, started_at_ms| json!({"attempt_id":id,"started_at_ms":started_at_ms,"outcome":"upgraded","modules":[]});
        let prior = attempt("prior-attempt", u64::MAX);
        let first = vec![
            stub.describe(prior.clone()),
            ScriptedControlStep {
                expected: stub.ask.clone(),
                action: ScriptedControlAction::Close,
            },
        ];
        let invalid_id = vec![stub.describe(prior)];
        let invalid_time = vec![stub.describe(attempt("stale-attempt", 0))];
        let ClientControlRequest::DaemonUpgradeInPlace { executable_sha256 } = &stub.ask else {
            unreachable!()
        };
        stub.identity["executable_sha256"] = executable_sha256.clone().into();
        stub.identity["core_version"] = "2.0.0".into();
        let last = vec![
            stub.describe(attempt("new-attempt", u64::MAX)),
            response(
                ClientControlRequest::SupervisorList {},
                json!({"op":"supervisor.list","generation":1,"modules":[]}),
            ),
        ];
        let mut daemon = ScriptedDaemon::start_sessions(
            "upgrade-no-reply",
            vec![first, invalid_id, invalid_time, last],
        )
        .await;
        let result = stub.output(&mut daemon, 0).await;
        assert_eq!(result["outcome"], "upgraded");
        assert_eq!(result["attempt_id"], "new-attempt");
        assert_eq!(result["build"]["core_version"], "2.0.0");
    }
}
