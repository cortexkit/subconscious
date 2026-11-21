use super::{ChildRoster, ModuleProtocol, ModuleSpec, SupervisorHandle};
use crate::stderr_tail::{StderrRing, StderrTailConfig};
use cortexkit_test_support::ScratchDir;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

async fn probe(role: super::SpawnRole) {
    let scratch = ScratchDir::new("launch-nonce-descriptor");
    let fd_copy = scratch.join("from-descriptor");
    let env_copy = scratch.join("environment");
    let script = format!(
            "env > '{env}.tmp' && mv '{env}.tmp' '{env}'; cat <&3 > '{fd}.tmp' && mv '{fd}.tmp' '{fd}'; sleep 30",
            fd = fd_copy.display(), env = env_copy.display(),
        );
    let xdg = |name: &str| (name.to_string(), scratch.join(name).display().to_string());
    let spec = ModuleSpec {
        script: None,
        module_id: "nonce-descriptor-probe".to_string(),
        program: PathBuf::from("/bin/sh"),
        args: vec!["-c".to_string(), script],
        env: vec![
            xdg("XDG_DATA_HOME"),
            xdg("XDG_RUNTIME_DIR"),
            xdg("XDG_CONFIG_HOME"),
            ("SUBC_LAUNCH_NONCE".to_string(), "must-not-leak".to_string()),
        ],
        reserved: true,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
        carry_containment: false,
    };
    let handle = SupervisorHandle::new();
    let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
    let roster = ChildRoster::default();
    #[cfg(target_os = "macos")]
    {
        let path = super::test_privacy_trampoline();
        roster.set_privacy_trampoline(super::probe_privacy_trampoline(&path).map(|()| path));
    }
    let child = super::spawn_child_in_slot(
        &spec,
        None,
        Some(&handle),
        &ring,
        None,
        &roster,
        None,
        #[cfg(target_os = "linux")]
        None,
        role,
        matches!(role, super::SpawnRole::SwapCandidate),
        super::DEFAULT_REGISTER_TIMEOUT,
    )
    .expect("spawn probe");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !(fd_copy.exists() && env_copy.exists()) {
        assert!(Instant::now() < deadline, "probe never wrote its copies");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let nonce = std::fs::read_to_string(fd_copy).unwrap();
    assert!(!nonce.is_empty());
    let environment = std::fs::read_to_string(env_copy).unwrap();
    assert!(environment
        .lines()
        .any(|line| line.starts_with("SUBC_LAUNCH_NONCE_FD=3:")));
    let copy = environment
        .lines()
        .find_map(|line| line.strip_prefix("SUBC_LAUNCH_NONCE="));
    assert_eq!(
        copy, None,
        "Unix children must never receive the environment nonce"
    );
    if matches!(role, super::SpawnRole::Plain) {
        assert_eq!(
            handle.spawn_nonce(&spec.module_id).as_deref(),
            Some(nonce.as_str())
        );
    }
    drop(child);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_spawned_module_receives_its_nonce_only_on_descriptor_3() {
    probe(super::SpawnRole::Plain).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn swap_candidate_receives_its_nonce_only_on_descriptor_3() {
    probe(super::SpawnRole::SwapCandidate).await;
}
