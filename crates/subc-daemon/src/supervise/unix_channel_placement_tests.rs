use super::*;
use cortexkit_test_support::ScratchDir;
use std::io::Read;

fn spec(scratch: &ScratchDir) -> ModuleSpec {
    ModuleSpec {
        module_id: "channel-placement-probe".into(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            "exec /usr/bin/python3 -c \"$1\"".into(),
            "channel-probe".into(),
            r#"
import os, fcntl, socket, json, time
assert os.environ['SUBC_REGISTRATION_FD'].startswith('4:')
assert fcntl.fcntl(3, fcntl.F_GETFD) == 0
assert fcntl.fcntl(4, fcntl.F_GETFD) == 0
nonce = os.read(3, 512).decode()
assert len(nonce) == 64
channel = socket.socket(fileno=4)
channel.sendall(b'channel-at-four')
channel.close()
with open(os.environ['PROBE_RESULT']+'.tmp', 'w') as result:
    json.dump(dict(nonce=nonce, environment=dict(os.environ)), result)
os.rename(os.environ['PROBE_RESULT']+'.tmp', os.environ['PROBE_RESULT'])
time.sleep(60)
"#
            .into(),
        ],
        env: [
            "HOME",
            "XDG_CONFIG_HOME",
            "XDG_DATA_HOME",
            "XDG_RUNTIME_DIR",
        ]
        .into_iter()
        .map(|key| (key.into(), scratch.display().to_string()))
        .chain([(
            "PROBE_RESULT".into(),
            scratch.join("result.json").display().to_string(),
        )])
        .collect(),
        reserved: true,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    }
}

fn roster() -> ChildRoster {
    let roster = ChildRoster::default();
    #[cfg(target_os = "macos")]
    {
        let path = test_privacy_trampoline();
        roster.set_privacy_trampoline(probe_privacy_trampoline(&path).map(|()| path));
    }
    roster
}

async fn probe() {
    let scratch = ScratchDir::new("unix-channel-placement");
    let spec = spec(&scratch);
    let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
    let roster = roster();
    let mut child = spawn_child_in_slot(
        &spec,
        None,
        None,
        &ring,
        None,
        &roster,
        None,
        #[cfg(target_os = "linux")]
        None,
        SpawnRole::Plain,
        false,
    )
    .unwrap();
    #[cfg(target_os = "macos")]
    child.confirm_privacy_exec().await;
    let mut peer = child.registration_channel.take().expect("daemon endpoint");
    peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut bytes = Vec::new();
    peer.read_to_end(&mut bytes)
        .expect("daemon must close its copy of the child's endpoint after spawn");
    assert_eq!(bytes, b"channel-at-four");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !scratch.join("result.json").exists() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(scratch.join("result.json")).unwrap()).unwrap();
    assert_eq!(value["nonce"].as_str().unwrap().len(), 64);
    assert_eq!(
        value["environment"]["SUBC_DAEMON_PID"],
        std::process::id().to_string()
    );
    assert!(value["environment"].get("SUBC_LAUNCH_NONCE").is_none());
    if let Some(marker) = std::env::var_os("SUBC_CLOSED_STDIO_RESULT") {
        std::fs::write(marker, b"channel delivered with closed stdio").unwrap();
    }
    let _ = child.start_admission_kill();
    child.wait().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_channel_slots_survive_exec_and_parent_drops_child_endpoint() {
    probe().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unix_channel_survives_closed_daemon_stdio() {
    if std::env::var_os("SUBC_CLOSED_STDIO_RESULT").is_some() {
        probe().await;
        return;
    }
    let scratch = ScratchDir::new("closed-daemon-stdio");
    let marker = scratch.join("verified");
    let output = std::process::Command::new("/usr/bin/python3")
        .args([
            "-c",
            "import os,sys; os.closerange(0,3); os.execv(sys.argv[1],sys.argv[1:])",
        ])
        .arg(std::env::current_exe().unwrap())
        .args([
            "supervise::unix_channel_placement_tests::unix_channel_survives_closed_daemon_stdio",
            "--exact",
            "--nocapture",
        ])
        .env("SUBC_CLOSED_STDIO_RESULT", &marker)
        .output()
        .unwrap();
    assert!(output.status.success(), "closed-stdio subprocess failed");
    assert_eq!(
        std::fs::read(marker).unwrap(),
        b"channel delivered with closed stdio"
    );
}

#[test]
fn unix_channel_nonce_policy_preserves_ungated_and_none() {
    let scratch = ScratchDir::new("channel-nonce-policy");
    let mut spec = spec(&scratch);
    let handle = SupervisorHandle::new();
    for role in [SpawnRole::Plain, SpawnRole::SwapCandidate] {
        let mut command = Command::new(&spec.program);
        let handoff = apply_wire_spawn_args_for_admission(
            &mut command,
            &spec,
            None,
            Some(&handle),
            role,
            None,
        )
        .unwrap();
        assert!(handoff.is_some());
        if role == SpawnRole::Plain {
            assert!(handle.spawn_nonce(&spec.module_id).is_some());
        }
    }
    spec.protocol = ModuleProtocol::None;
    let mut command = Command::new(&spec.program);
    assert!(apply_wire_spawn_args_for_admission(
        &mut command,
        &spec,
        None,
        Some(&handle),
        SpawnRole::Plain,
        None
    )
    .unwrap()
    .is_none());
    for key in [
        subc_os::LAUNCH_NONCE_FD_ENV,
        SUBC_LAUNCH_NONCE_ENV,
        subc_os::registration_channel::REGISTRATION_FD_ENV,
        subc_os::registration_channel::DAEMON_PID_ENV,
    ] {
        assert!(command
            .as_std()
            .get_envs()
            .any(|(k, v)| k == key && v.is_none()));
    }
}

pub(crate) fn assert_channel_only_admission(
    spec: &ModuleSpec,
    admission: &crate::config_gate::runtime::SpawnAdmission,
) {
    assert!(admission.approved_base().is_some());
    let mut spec = spec.clone();
    spec.protocol = ModuleProtocol::Subc;
    spec.reserved = true;
    let handle = SupervisorHandle::new();
    for role in [SpawnRole::Plain, SpawnRole::SwapCandidate] {
        let incumbent = handle.spawn_nonce(&spec.module_id);
        let mut command = Command::new(&spec.program);
        let handoff = apply_wire_spawn_args_for_admission(
            &mut command,
            &spec,
            None,
            Some(&handle),
            role,
            Some(admission),
        )
        .unwrap();
        assert!(handoff.is_none(), "gated children receive no nonce pipe");
        assert!(command
            .as_std()
            .get_envs()
            .any(|(key, value)| key == subc_os::LAUNCH_NONCE_FD_ENV && value.is_none()));
        assert!(command
            .as_std()
            .get_envs()
            .any(|(key, value)| { key == SUBC_LAUNCH_NONCE_ENV && value.is_none() }));
        match role {
            SpawnRole::Plain => {
                assert!(handle.spawn_nonce(&spec.module_id).is_none());
                assert!(handle.reserved_launch_nonce_for(&spec.module_id).is_none());
            }
            SpawnRole::SwapCandidate => {
                assert_eq!(handle.spawn_nonce(&spec.module_id), incumbent);
                let swaps = handle.swaps.lock().unwrap();
                let swap = swaps.get(&spec.module_id).expect("candidate nonce record");
                assert!(swap.candidate_nonce.is_empty());
                assert_eq!(swap.incumbent_nonce, incumbent);
            }
        }
    }
}
