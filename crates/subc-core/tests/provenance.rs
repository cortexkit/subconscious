use std::time::{SystemTime, UNIX_EPOCH};
use std::{
    fs,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Duration,
};

#[cfg(target_os = "windows")]
use subc_control::RunningImageUnavailableReason;
use subc_control::{
    ClientControlRequest, ClientControlResponse, ModuleDeclaredProvenance, RunningImageAgreement,
    RunningImageEvidence,
};
use subc_core::{
    read_frame, write_frame, Frame, ModuleSpec, RestartPolicy, Supervisor, SupervisorHandle,
    SupervisorProcessLiveness,
};
use subc_protocol::manifest::ManifestProvenance;
use subc_protocol::{Flags, FrameType, Priority};
use tokio::{
    io::AsyncWriteExt,
    time::{sleep, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

const READ_TIMEOUT: Duration = Duration::from_secs(10);
static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_provenance_reports_declared_and_observed_module_facts() {
    let before_start_ms = unix_ms();
    let process_liveness = Arc::new(SupervisorProcessLiveness::new());
    let supervisor_handle = SupervisorHandle::new();
    let daemon = start_test_daemon_with_process_liveness_and_supervisor(
        "provenance-reported",
        process_liveness.clone(),
        supervisor_handle.clone(),
    )
    .await;
    let after_start_ms = unix_ms();
    let supervisor = Supervisor::new(Arc::clone(&daemon.registry), RestartPolicy::default())
        .with_process_liveness(process_liveness)
        .with_handle(supervisor_handle)
        .with_drain_timeout(Duration::from_millis(25))
        .with_connection_file_path(daemon.connection_file_path.clone());
    let module = supervisor
        .spawn(stub_spec(
            "provenance-reported",
            vec![
                (
                    "FAKE_AFT_BUILD_COMMIT",
                    "ffffffffffffffffffffffffffffffffffffffff",
                ),
                (
                    "FAKE_AFT_BUILD_LOCK_DIGEST",
                    "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                ),
                ("FAKE_AFT_WIRE_CRATE_VERSION", "0.0.0-forged"),
                ("FAKE_AFT_STORE_SCHEMA_VERSION", "9999-forged-schema"),
            ],
        ))
        .unwrap();
    wait_for_registration(&daemon, "provenance-reported").await;

    let response = provenance_request(&daemon, 1, Some("provenance-reported")).await;
    let ClientControlResponse::SupervisorProvenance {
        daemon: observed_daemon,
        modules,
    } = response
    else {
        panic!("supervisor.provenance must return a provenance response");
    };
    assert_eq!(
        observed_daemon.daemon_observed.pid,
        Some(std::process::id())
    );
    assert!(matches!(
        observed_daemon.daemon_observed.running_image,
        RunningImageAgreement::Match { .. }
            | RunningImageAgreement::Unavailable {
                reason: subc_control::RunningImageUnavailableReason::UnsupportedPlatform
            }
    ));
    assert!(
        observed_daemon.daemon_observed.started_at_ms >= Some(before_start_ms)
            && observed_daemon.daemon_observed.started_at_ms <= Some(after_start_ms)
    );
    assert_eq!(
        observed_daemon.daemon_build.build_git_sha.as_deref(),
        Some("test-daemon-build")
    );
    assert_eq!(
        observed_daemon.daemon_build.build_lock_digest.as_deref(),
        Some("test-daemon-lock")
    );
    assert_eq!(modules.len(), 1);
    let observed = &modules[0];
    let ModuleDeclaredProvenance::Reported { build } = &observed.module_declared else {
        panic!("the provenance block must remain a module declaration");
    };
    assert_eq!(
        build.build_git_sha.as_deref(),
        Some("ffffffffffffffffffffffffffffffffffffffff")
    );
    assert_eq!(
        build.build_lock_digest.as_deref(),
        Some("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
    );
    assert_eq!(build.wire_crate_version.as_deref(), Some("0.0.0-forged"));
    assert_eq!(
        build.store_schema_version.as_deref(),
        Some("9999-forged-schema")
    );
    let status = module.status().unwrap();
    assert_eq!(observed.daemon_observed.pid, status.pid);
    assert_eq!(
        observed.daemon_observed.spawned_from,
        Some(PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")))
    );
    assert!(observed.daemon_observed.spawned_at_ms.unwrap_or_default() > 0);
    assert_running_image_matches(&observed.daemon_observed.running_image);
    let rendered = serde_json::to_string(&observed_daemon).unwrap();
    // Destructured rather than field-accessed so that adding a field to
    // ManifestProvenance fails to compile here instead of silently escaping the
    // leakage sweep below.
    let ManifestProvenance {
        build_git_sha,
        build_lock_digest,
        wire_crate_version,
        store_schema_version,
    } = build;
    for declared in [
        build_git_sha.as_deref(),
        build_lock_digest.as_deref(),
        wire_crate_version.as_deref(),
        store_schema_version.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        assert!(
            !rendered.contains(declared),
            "daemon facts must not contain declared provenance value {declared:?}"
        );
    }
    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_provenance_marks_absent_manifest_block_unverifiable() {
    let process_liveness = Arc::new(SupervisorProcessLiveness::new());
    let supervisor_handle = SupervisorHandle::new();
    let daemon = start_test_daemon_with_process_liveness_and_supervisor(
        "provenance-unverifiable",
        process_liveness.clone(),
        supervisor_handle.clone(),
    )
    .await;
    let supervisor = Supervisor::new(Arc::clone(&daemon.registry), RestartPolicy::default())
        .with_process_liveness(process_liveness)
        .with_handle(supervisor_handle)
        .with_drain_timeout(Duration::from_millis(25))
        .with_connection_file_path(daemon.connection_file_path.clone());
    let module = supervisor
        .spawn(stub_spec("provenance-unverifiable", Vec::new()))
        .unwrap();
    wait_for_registration(&daemon, "provenance-unverifiable").await;

    let response = provenance_request(&daemon, 2, Some("provenance-unverifiable")).await;
    let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
        panic!("supervisor.provenance must return a provenance response");
    };
    assert!(matches!(
        modules[0].module_declared,
        ModuleDeclaredProvenance::Unverifiable
    ));
    module.stop().await.unwrap();
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_provenance_detects_replaced_executable_image() {
    let process_liveness = Arc::new(SupervisorProcessLiveness::new());
    let supervisor_handle = SupervisorHandle::new();
    let daemon = start_test_daemon_with_process_liveness_and_supervisor(
        "provenance-replacement",
        process_liveness.clone(),
        supervisor_handle.clone(),
    )
    .await;
    let supervisor = Supervisor::new(Arc::clone(&daemon.registry), RestartPolicy::default())
        .with_process_liveness(process_liveness)
        .with_handle(supervisor_handle)
        .with_drain_timeout(Duration::from_millis(25))
        .with_connection_file_path(daemon.connection_file_path.clone());
    let temp_dir = unique_temp_dir("provenance-replacement");
    fs::create_dir_all(&temp_dir).unwrap();
    let copied_stub = temp_dir.join("fake-aft-stub");
    fs::copy(env!("CARGO_BIN_EXE_fake-aft-stub"), &copied_stub).unwrap();
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: "provenance-replacement".to_string(),
            program: copied_stub.clone(),
            args: Vec::new(),
            env: vec![(
                "FAKE_AFT_MODULE_ID".to_string(),
                "provenance-replacement".to_string(),
            )],
            reserved: false,
            reserved_prefixes: Vec::new(),
        })
        .unwrap();
    wait_for_registration(&daemon, "provenance-replacement").await;
    let replacement = temp_dir.join("replacement");
    fs::copy(env!("CARGO_BIN_EXE_ck"), &replacement).unwrap();
    fs::rename(&replacement, &copied_stub).unwrap();

    let response = provenance_request(&daemon, 3, Some("provenance-replacement")).await;
    let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
        panic!("supervisor.provenance must return a provenance response");
    };
    assert!(matches!(
        modules[0].daemon_observed.running_image,
        RunningImageAgreement::Mismatch { .. }
    ));
    module.stop().await.unwrap();
    fs::remove_dir_all(temp_dir).unwrap();
}

fn stub_spec(module_id: &str, env: Vec<(&str, &str)>) -> ModuleSpec {
    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env: std::iter::once(("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()))
            .chain(
                env.into_iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            )
            .collect(),
        reserved: false,
        reserved_prefixes: Vec::new(),
    }
}

async fn wait_for_registration(daemon: &TestDaemon, module_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if daemon.registry.get_module(module_id).unwrap().is_some() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} did not register"
        );
        sleep(Duration::from_millis(10)).await;
    }
}

async fn provenance_request(
    daemon: &TestDaemon,
    corr: u64,
    module_id: Option<&str>,
) -> ClientControlResponse {
    let mut client = connect_authed_client(&daemon.connection_file_path)
        .await
        .unwrap();
    let body = serde_json::to_vec(&ClientControlRequest::SupervisorProvenance {
        module_id: module_id.map(str::to_string),
    })
    .unwrap();
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap();
    write_frame(&mut client, &request).await.unwrap();
    client.flush().await.unwrap();
    let frame = timeout(READ_TIMEOUT, read_frame(&mut client))
        .await
        .unwrap()
        .unwrap()
        .expect("server closed before provenance response");
    assert_eq!(frame.header.ty, FrameType::Response);
    serde_json::from_slice(&frame.body).unwrap()
}

fn assert_running_image_matches(result: &RunningImageAgreement) {
    #[cfg(target_os = "linux")]
    assert!(matches!(
        result,
        RunningImageAgreement::Match {
            evidence: RunningImageEvidence::LinuxProcSha256 { .. }
        }
    ));
    #[cfg(target_os = "macos")]
    assert!(matches!(
        result,
        RunningImageAgreement::Match {
            evidence: RunningImageEvidence::MacosSpawnInode { .. }
        }
    ));
    #[cfg(target_os = "windows")]
    assert!(matches!(
        result,
        RunningImageAgreement::Unavailable {
            reason: RunningImageUnavailableReason::UnsupportedPlatform
        }
    ));
}

fn unique_temp_dir(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "subc-{label}-{}-{}",
        std::process::id(),
        TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}
