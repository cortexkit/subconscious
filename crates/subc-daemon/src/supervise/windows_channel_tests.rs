use super::*;

#[tokio::test]
async fn windows_channel_tokio_server_accepts_and_exchanges_frame() {
    use subc_os::registration_channel::{DAEMON_PID_ENV, REGISTRATION_PIPE_ENV};
    const CHILD_MODE: &str = "SUBC_REGISTRATION_CONVERSION_TEST_CHILD";
    const TEST: &str = "supervise::windows_channel_tests::windows_channel_tokio_server_accepts_and_exchanges_frame";
    let frame = subc_protocol::Frame::build(
        subc_protocol::FrameType::Ping,
        subc_protocol::Flags::new(false, subc_protocol::Priority::Passive, false),
        0,
        0,
        42,
        b"registration frame".to_vec(),
    )
    .unwrap();
    if std::env::var_os(CHILD_MODE).is_some() {
        let mut stream = subc_os::take_registration_channel()
            .unwrap()
            .unwrap()
            .into_tokio_pipe_client()
            .unwrap();
        subc_transport::write_frame(&mut stream, &frame)
            .await
            .unwrap();
        let echoed = tokio::time::timeout(
            Duration::from_secs(5),
            subc_transport::read_frame(&mut stream),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();
        assert_eq!(echoed.header, frame.header);
        assert_eq!(echoed.body, frame.body);
        return;
    }
    let scratch = cortexkit_test_support::ScratchDir::new("registration-tokio-server");
    let pipe = subc_os::registration_channel::RegistrationPipe::new().unwrap();
    let mut command = tokio::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([TEST, "--exact", "--nocapture"])
        .env(CHILD_MODE, "1")
        .env(REGISTRATION_PIPE_ENV, pipe.name())
        .env(DAEMON_PID_ENV, std::process::id().to_string())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "HOME",
        "XDG_DATA_HOME",
        "XDG_RUNTIME_DIR",
        "XDG_CONFIG_HOME",
    ] {
        command.env(key, scratch.display().to_string());
    }
    let child = command.spawn().unwrap();
    let pid = child.id().unwrap();
    let mut server = tokio::task::spawn_blocking(move || {
        pipe.accept_tokio_pipe_server(pid, std::time::Instant::now() + Duration::from_secs(5))
    })
    .await
    .unwrap()
    .unwrap();
    let received = tokio::time::timeout(
        Duration::from_secs(5),
        subc_transport::read_frame(&mut server),
    )
    .await
    .unwrap()
    .unwrap()
    .unwrap();
    assert_eq!(received.header, frame.header);
    assert_eq!(received.body, frame.body);
    subc_transport::write_frame(&mut server, &received)
        .await
        .unwrap();
    let output = tokio::time::timeout(Duration::from_secs(5), child.wait_with_output())
        .await
        .unwrap()
        .unwrap();
    assert!(
        output.status.success(),
        "child pipe exchange failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
use crate::{
    caller_identity::CallerIdentity,
    config_gate::{
        capture::{ConfigCapturer, UnavailableProjectRegistry},
        runtime::{daemon_review, RuntimeOwner},
        ApprovalFacts, ApprovalReply, PlanKind, PresenceDecision,
    },
    config_store::{ConfigStore, StoreRoot},
};
use cortexkit_test_support::ScratchDir;
use std::ffi::OsStr;

fn env(command: &Command, key: &str) -> Option<std::ffi::OsString> {
    command
        .as_std()
        .get_envs()
        .find(|(name, _)| *name == key)
        .and_then(|(_, value)| value.map(OsStr::to_os_string))
}

#[tokio::test]
async fn windows_channel_gated_spawn_withholds_nonce() {
    let scratch = ScratchDir::new("windows-channel-admission");
    let path = std::fs::canonicalize(&*scratch).unwrap();
    std::fs::create_dir(path.join("store")).unwrap();
    let program = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32/cmd.exe");
    std::fs::write(
        path.join("subc.jsonc"),
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "modules": {"channel-probe": {"program": program, "args": ["/C", "exit 0"]}}
        }))
        .unwrap(),
    )
    .unwrap();
    let owner = Arc::new(RuntimeOwner::new(
        "windows-channel-admission".into(),
        ConfigStore::open_digest_only_for_tests(
            StoreRoot::from_service_definition(path.join("store")).unwrap(),
        ),
        ConfigCapturer {
            daemon_path: path.join("subc.jsonc"),
            config_home: path.clone(),
            projects: Arc::new(UnavailableProjectRegistry),
        },
    ));
    let facts = ApprovalFacts {
        connection: 7,
        owner_uid: true,
        desktop_app: true,
        attested_operator_app: true,
        os_prompt_available: true,
        phone: None,
        caller: CallerIdentity::default(),
    };
    let plan = owner
        .finish_plan(owner.prepare(PlanKind::Stamp).unwrap(), daemon_review)
        .unwrap();
    owner
        .desktop_view_delivered(ConnectionId::new(7), &plan)
        .unwrap();
    let challenge = owner.begin_approval(&plan.digest, &facts).unwrap();
    owner
        .finish_approval(
            challenge.attempt(),
            &facts,
            ApprovalReply::Presence(PresenceDecision::InteractiveApproval),
            1,
        )
        .unwrap();
    let spec = crate::daemon_config::load(path.join("subc.jsonc"))
        .unwrap()
        .unwrap()
        .modules[0]
        .module_spec();
    let admission = owner.prepare_spawn(&spec).unwrap();
    assert!(admission.approved_base().is_some());
    check_environment(&spec, Some(&admission));
    check_environment(&spec, None);
}

fn check_environment(
    spec: &ModuleSpec,
    admission: Option<&crate::config_gate::runtime::SpawnAdmission>,
) {
    for role in [SpawnRole::Plain, SpawnRole::SwapCandidate] {
        let handle = SupervisorHandle::new();
        let mut command = Command::new(&spec.program);
        let nonce = apply_wire_spawn_args_for_admission(
            &mut command,
            spec,
            None,
            Some(&handle),
            role,
            admission,
        )
        .unwrap();
        let pipe = prepare_windows_registration_pipe(&mut command, spec)
            .unwrap()
            .expect("wire children receive a reserved registration instance");
        assert_eq!(
            env(
                &command,
                subc_os::registration_channel::REGISTRATION_PIPE_ENV
            ),
            Some(pipe.name().into())
        );
        assert_eq!(
            env(&command, subc_os::registration_channel::DAEMON_PID_ENV),
            Some(std::process::id().to_string().into())
        );
        if admission.is_some() {
            assert!(nonce.is_none());
            for key in [
                subc_os::LAUNCH_NONCE_PIPE_ENV,
                SUBC_LAUNCH_NONCE_ENV,
                subc_os::LAUNCH_NONCE_PIPE_FALLBACK_ENV,
            ] {
                assert!(env(&command, key).is_none());
            }
            assert!(handle.spawn_nonce(&spec.module_id).is_none());
            continue;
        }
        let nonce = nonce.expect("ungated children retain legacy nonce registration");
        assert_eq!(
            env(&command, subc_os::LAUNCH_NONCE_PIPE_ENV),
            Some(nonce.name().into())
        );
        assert!(env(&command, SUBC_LAUNCH_NONCE_ENV).is_some());
        assert_eq!(
            env(&command, subc_os::LAUNCH_NONCE_PIPE_FALLBACK_ENV),
            Some("env".into())
        );
        assert_ne!(pipe.name(), nonce.name());
    }
}

#[test]
fn windows_channel_protocol_none_gets_no_registration_or_nonce() {
    let spec = ModuleSpec {
        module_id: "no-channel".into(),
        program: PathBuf::from("unused.exe"),
        args: vec![],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    };
    let mut command = Command::new(&spec.program);
    for key in [
        subc_os::registration_channel::REGISTRATION_PIPE_ENV,
        subc_os::registration_channel::DAEMON_PID_ENV,
        subc_os::LAUNCH_NONCE_PIPE_ENV,
        SUBC_LAUNCH_NONCE_ENV,
    ] {
        command.env(key, "configured-stale-value");
    }
    assert!(apply_wire_spawn_args(&mut command, &spec, None, None)
        .unwrap()
        .is_none());
    assert!(prepare_windows_registration_pipe(&mut command, &spec)
        .unwrap()
        .is_none());
    for key in [
        subc_os::registration_channel::REGISTRATION_PIPE_ENV,
        subc_os::registration_channel::DAEMON_PID_ENV,
        subc_os::LAUNCH_NONCE_PIPE_ENV,
        SUBC_LAUNCH_NONCE_ENV,
    ] {
        assert!(env(&command, key).is_none(), "{key}");
    }
}
