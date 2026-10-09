use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use std::{sync::Arc, time::Duration};
// The sha256 re-hash apparatus (throwaway binaries in per-test temp dirs) is
// LINUX-only — macOS proves identity by spawn inode and Windows serves the
// unavailable arm, neither writes files — so these imports gate with it or
// the other platforms clippy-fail them as unused under -D warnings.
#[cfg(target_os = "linux")]
use std::fs;

// Used by the linux AND macos match arms of assert_running_image_matches.
#[cfg(target_os = "linux")]
use cortexkit_test_support::ScratchDir;
#[cfg(not(target_os = "windows"))]
use subc_control::RunningImageEvidence;
#[cfg(target_os = "windows")]
use subc_control::RunningImageUnavailableReason;
use subc_control::{
    ClientControlRequest, ClientControlResponse, ModuleDeclaredProvenance, ModuleProtocol,
    RunningImageAgreement,
};
use subc_daemon::{
    read_frame, write_frame, Frame, ModuleSpec, RestartPolicy, Supervisor, SupervisorHandle,
    SupervisorProcessLiveness,
};
use subc_protocol::manifest::LaunchNonceSource;
use subc_protocol::{Flags, FrameType, Priority};
use tokio::{
    io::AsyncWriteExt,
    time::{sleep, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

const PROVENANCE_REPLY_TIMEOUT: Duration = Duration::from_secs(120);

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "requires the macOS exec acknowledgement"
)]
async fn macos_status_and_provenance_publish_only_a_confirmed_module_pid() {
    #[cfg(target_os = "macos")]
    {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            "provenance-exec-ack",
            process_liveness.clone(),
            handle.clone(),
        )
        .await;
        // The trampoline runs as `ckdev-subc-under-test`; its running image is
        // compared with this placed file below.
        let trampoline = common::ckdev_subc_under_test();
        let supervisor = Supervisor::new(Arc::clone(&daemon.registry), RestartPolicy::default())
            .with_privacy_trampoline(&trampoline)
            .with_process_liveness(process_liveness)
            .with_handle(handle)
            .with_drain_timeout(Duration::from_millis(25))
            .with_connection_file_path(daemon.connection_file_path.clone());
        let id = "provenance-exec-ack";
        let mut spec = stub_spec(id, vec![("SUBC_TEST_PRIVACY_EXEC_DELAY_MS", "2500")]);
        spec.env.extend(
            ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| {
                    (
                        key.to_string(),
                        daemon.temp_dir.join(key).display().to_string(),
                    )
                }),
        );
        let module = supervisor.spawn(spec).unwrap();

        let ClientControlResponse::SupervisorSpawnSnapshot { snapshot } = control_request(
            &daemon,
            40,
            ClientControlRequest::SupervisorSpawnSnapshot {},
        )
        .await
        else {
            panic!("expected physical spawn facts");
        };
        let physical = snapshot
            .live
            .iter()
            .find(|entry| entry.module_id == id)
            .unwrap();
        let image = subc_os::Process::open(physical.pid)
            .unwrap()
            .unwrap()
            .observe()
            .unwrap();
        assert_eq!(
            image.executable,
            subc_os::file_identity(&trampoline),
            "the test must observe the actual delayed trampoline"
        );
        let pending = module.status().unwrap();
        assert_eq!(
            pending.pid, None,
            "an unconfirmed trampoline pid must not be published as the module"
        );
        assert_eq!(pending.spawned_at_ms, Some(physical.spawned_at_ms));
        assert!(
            pending.process_alive,
            "internal ownership still tracks the spawned process"
        );

        let ClientControlResponse::SupervisorProvenance { modules, .. } =
            provenance_request(&daemon, 41, Some(id)).await
        else {
            panic!("expected module provenance");
        };
        let observed = &modules[0].daemon_observed;
        assert_eq!(observed.pid, None);
        assert_eq!(observed.spawned_at_ms, pending.spawned_at_ms);
        assert_eq!(
            observed.running_image,
            RunningImageAgreement::Unavailable {
                reason: subc_control::RunningImageUnavailableReason::NotRunning
            }
        );
        let ClientControlResponse::SupervisorList { modules, .. } =
            control_request(&daemon, 42, ClientControlRequest::SupervisorList {}).await
        else {
            panic!("expected supervisor list");
        };
        assert_eq!(
            modules
                .iter()
                .find(|entry| entry.module_id == id)
                .unwrap()
                .resources,
            Some(subc_control::ChildResourceUsage::Unavailable {
                reason: subc_control::ChildResourceUnavailableReason::NotRunning
            })
        );
        assert_eq!(
            module.status().unwrap().pid,
            None,
            "the assertions must finish while exec is still delayed"
        );

        let deadline = Instant::now() + Duration::from_secs(10);
        while module.status().unwrap().pid.is_none() {
            assert!(
                Instant::now() < deadline,
                "module image was never confirmed"
            );
            sleep(Duration::from_millis(10)).await;
        }
        let confirmed = module.status().unwrap();
        assert_eq!(confirmed.pid, Some(physical.pid));
        assert_eq!(confirmed.spawned_at_ms, pending.spawned_at_ms);
        let image = subc_os::Process::open(physical.pid)
            .unwrap()
            .unwrap()
            .observe()
            .unwrap();
        assert_eq!(
            image.executable,
            subc_os::file_identity(std::path::Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")))
        );
        let ClientControlResponse::SupervisorProvenance { modules, .. } =
            provenance_request(&daemon, 43, Some(id)).await
        else {
            panic!("expected confirmed module provenance");
        };
        assert_eq!(modules[0].daemon_observed.pid, confirmed.pid);
        assert_running_image_matches(&modules[0].daemon_observed.running_image);
        module.stop().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg_attr(
    not(target_os = "macos"),
    ignore = "requires the macOS exec acknowledgement"
)]
async fn macos_refused_trampoline_never_publishes_a_module_pid() {
    #[cfg(target_os = "macos")]
    {
        let liveness = Arc::new(SupervisorProcessLiveness::new());
        let handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            "provenance-refused",
            liveness.clone(),
            handle.clone(),
        )
        .await;
        let supervisor = Supervisor::new(
            Arc::clone(&daemon.registry),
            RestartPolicy::new(0, Duration::ZERO),
        )
        .with_privacy_trampoline(common::ckdev_subc_under_test())
        .with_process_liveness(liveness)
        .with_handle(handle)
        .with_connection_file_path(daemon.connection_file_path.clone());
        let id = "provenance-refused";
        let mut spec = stub_spec(
            id,
            vec![
                ("SUBC_TEST_PRIVACY_EXEC_DELAY_MS", "500"),
                ("SUBC_TEST_PRIVACY_MISSING_SYMBOL", "1"),
            ],
        );
        spec.env.extend(
            ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| {
                    (
                        key.to_string(),
                        daemon.temp_dir.join(key).display().to_string(),
                    )
                }),
        );
        let module = supervisor.spawn(spec).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let status = module.status().unwrap();
            assert_eq!(
                status.pid, None,
                "a refused launch must never publish a module pid"
            );
            if status.state == subc_daemon::ModuleState::Failed {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "trampoline refusal did not finish"
            );
            sleep(Duration::from_millis(10)).await;
        }
        let ClientControlResponse::SupervisorProvenance { modules, .. } =
            provenance_request(&daemon, 44, Some(id)).await
        else {
            panic!("expected refused-launch provenance");
        };
        assert_eq!(modules[0].daemon_observed.pid, None);
        assert_eq!(
            modules[0].daemon_observed.running_image,
            RunningImageAgreement::Unavailable {
                reason: subc_control::RunningImageUnavailableReason::NotRunning
            }
        );
        assert!(daemon.registry.get_module(id).unwrap().is_none());
        module.stop().await.unwrap();
    }
}

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
        .with_privacy_trampoline(common::ckdev_subc())
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
    // Every declared value, read from the serialized block rather than named
    // field by field, so a field added to ManifestProvenance joins the
    // leakage sweep below without anyone having to remember it. (The struct is
    // non_exhaustive, so a destructuring pattern can no longer force that.)
    let declared_values: Vec<String> = serde_json::to_value(build)
        .unwrap()
        .as_object()
        .unwrap()
        .iter()
        // The absence reason and the nonce source are fixed vocabulary, not
        // module-chosen values, so a daemon fact may legitimately contain them.
        .filter(|(key, _)| {
            !matches!(
                key.as_str(),
                "build_git_sha_absence_reason" | "launch_nonce_source"
            )
        })
        .filter_map(|(_, value)| value.as_str().map(str::to_string))
        .collect();
    assert_eq!(declared_values.len(), 4, "{declared_values:?}");
    for declared in &declared_values {
        assert!(
            !rendered.contains(declared),
            "daemon facts must not contain declared provenance value {declared:?}"
        );
    }
    module.stop().await.unwrap();
}

/// A reserved module the supervisor spawns reads its launch nonce from the
/// descriptor the spawn hands it: it is admitted, which a reserved module is
/// only with its exact spawn nonce in HELLO, and its declared provenance says
/// the nonce came from `fd` even though the environment copy is set too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn supervisor_provenance_reports_a_reserved_module_reading_its_nonce_from_the_descriptor() {
    let process_liveness = Arc::new(SupervisorProcessLiveness::new());
    let supervisor_handle = SupervisorHandle::new();
    let daemon = start_test_daemon_with_process_liveness_and_supervisor(
        "provenance-nonce-source",
        process_liveness.clone(),
        supervisor_handle.clone(),
    )
    .await;
    let supervisor = Supervisor::new(Arc::clone(&daemon.registry), RestartPolicy::default())
        .with_privacy_trampoline(common::ckdev_subc())
        .with_process_liveness(process_liveness)
        .with_handle(supervisor_handle)
        .with_drain_timeout(Duration::from_millis(25))
        .with_connection_file_path(daemon.connection_file_path.clone());
    let scratch = cortexkit_test_support::ScratchDir::new("provenance-nonce-source");
    let xdg = |name: &str| (name.to_string(), scratch.join(name).display().to_string());
    let mut spec = stub_spec(
        "provenance-nonce-source",
        vec![("FAKE_AFT_WIRE_CRATE_VERSION", "0.0.0-test")],
    );
    spec.reserved = true;
    spec.env.extend([
        xdg("XDG_DATA_HOME"),
        xdg("XDG_RUNTIME_DIR"),
        xdg("XDG_CONFIG_HOME"),
    ]);
    let module = supervisor.spawn(spec).unwrap();
    wait_for_registration(&daemon, "provenance-nonce-source").await;

    let response = provenance_request(&daemon, 4, Some("provenance-nonce-source")).await;
    let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
        panic!("supervisor.provenance must return a provenance response");
    };
    let ModuleDeclaredProvenance::Reported { build } = &modules[0].module_declared else {
        panic!("the stub declares provenance");
    };
    let expected = if cfg!(unix) {
        LaunchNonceSource::Fd
    } else {
        LaunchNonceSource::Env
    };
    assert_eq!(build.launch_nonce_source, Some(expected));
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
        .with_privacy_trampoline(common::ckdev_subc())
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
        .with_privacy_trampoline(common::ckdev_subc())
        .with_process_liveness(process_liveness)
        .with_handle(supervisor_handle)
        .with_drain_timeout(Duration::from_millis(25))
        .with_connection_file_path(daemon.connection_file_path.clone());
    let temp_dir = unique_temp_dir("provenance-replacement");
    let copied_stub = temp_dir.join("fake-aft-stub");
    common::copy_executable(
        std::path::Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")),
        &copied_stub,
    );
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
            protocol: ModuleProtocol::Subc,
            overlap: Default::default(),
        })
        .unwrap();
    wait_for_registration(&daemon, "provenance-replacement").await;
    let replacement = temp_dir.join("replacement");
    // This is an inert byte fixture for the provenance hash comparison; the
    // test never dispatches `ck`, so it intentionally uses the shipped binary.
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
        protocol: ModuleProtocol::Subc,
        overlap: Default::default(),
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
    control_request(
        daemon,
        corr,
        ClientControlRequest::SupervisorProvenance {
            module_id: module_id.map(str::to_string),
        },
    )
    .await
}

async fn control_request(
    daemon: &TestDaemon,
    corr: u64,
    request: ClientControlRequest,
) -> ClientControlResponse {
    let mut client = connect_authed_client(&daemon.connection_file_path)
        .await
        .unwrap();
    let body = serde_json::to_vec(&request).unwrap();
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
    // The first provenance evaluation sha256-hashes the module's running
    // image through /proc and the replacement from disk; the replacement here
    // is the debug `ck` binary (~18 MiB), and on a cold, contended CI disk
    // that legitimately exceeds a 10 s read window: the ubuntu leg timed out
    // here with the same shape the ck_cli caller
    // (`control_rpc_value_on_stream_within`) had already been sized for.
    // The window is sized to the slowest acceptable progress, not the median.
    let frame = timeout(PROVENANCE_REPLY_TIMEOUT, read_frame(&mut client))
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

#[cfg(target_os = "linux")]
fn unique_temp_dir(label: &str) -> ScratchDir {
    ScratchDir::new(label)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}
