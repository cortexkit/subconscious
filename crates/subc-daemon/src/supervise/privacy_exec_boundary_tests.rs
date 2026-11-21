#[cfg(target_os = "macos")]
use super::*;
#[cfg(target_os = "macos")]
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
};

/// Unit-test-only pause at the actual early image read, not at a later
/// status read. Production supervisors never inspect this environment key.
#[cfg(target_os = "macos")]
pub(super) fn before_image_sample(spec: &ModuleSpec, pid: u32) {
    if let Some((_, path)) = spec
        .env
        .iter()
        .find(|(key, _)| key == "SUBC_TEST_PRIVACY_SAMPLE_BARRIER")
    {
        let mut barrier = TcpStream::connect(path).unwrap();
        barrier
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        barrier.write_all(&pid.to_ne_bytes()).unwrap();
        let mut release = [0];
        barrier.read_exact(&mut release).unwrap();
        assert_eq!(&release, b"X");
    }
}

#[cfg(target_os = "macos")]
fn spec(program: &str, args: &[&str]) -> ModuleSpec {
    ModuleSpec {
        script: None,
        module_id: "privacy-boundary".into(),
        program: program.into(),
        args: args.iter().map(|arg| (*arg).into()).collect(),
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: ModuleOverlap::Exclusive,
        carry_containment: false,
    }
}

#[cfg(target_os = "macos")]
async fn accept(listener: TcpListener) -> TcpStream {
    // Socket readiness, not elapsed time, establishes both pause points.
    let listener = tokio::net::TcpListener::from_std({
        listener.set_nonblocking(true).unwrap();
        listener
    })
    .unwrap();
    let (stream, _) = tokio::time::timeout(Duration::from_secs(30), listener.accept())
        .await
        .unwrap()
        .unwrap();
    let stream = stream.into_std().unwrap();
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
}

#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
async fn macos_roster_withholds_a_nonnull_trampoline_image_until_exec_confirmation() {
    #[cfg(target_os = "macos")]
    {
        let root = cortexkit_test_support::ScratchDir::new("privacy-roster-barrier");
        // Loopback sockets also work when the replay adapter's TMPDIR is
        // longer than Darwin's Unix-domain socket path limit.
        let exec_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let sample_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let record = root.join("live-children.json");
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .with_live_children_record(&record);
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let mut spec = spec("/bin/sleep", &["30"]);
        spec.env = vec![
            (
                "SUBC_TEST_PRIVACY_EXEC_BARRIER".into(),
                exec_listener.local_addr().unwrap().to_string(),
            ),
            (
                "SUBC_TEST_PRIVACY_SAMPLE_BARRIER".into(),
                sample_listener.local_addr().unwrap().to_string(),
            ),
        ];
        let spawn = tokio::task::spawn_blocking(move || {
            spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap()
        });
        let mut sample = accept(sample_listener).await;
        let mut pid = [0; 4];
        sample.read_exact(&mut pid).unwrap();
        let pid = u32::from_ne_bytes(pid);
        let mut exec = accept(exec_listener).await;
        let mut ready = [0];
        exec.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"R");
        // The early read is guaranteed to see a real, non-null trampoline
        // image: the fixture has reached its barrier and cannot exec yet.
        let trampoline = subc_os::file_identity(&test_privacy_trampoline()).unwrap();
        assert_eq!(
            observe_spawned_image(pid).unwrap().executable,
            Some(trampoline)
        );
        sample.write_all(b"X").unwrap();
        let mut child = spawn.await.unwrap();
        let early = crate::live_children::read_record(&record).unwrap();
        assert_eq!(early.len(), 1);
        assert_eq!(early[0].pid, pid);
        assert_eq!(
            early[0].executable, None,
            "unconfirmed trampoline image entered the roster"
        );
        assert!(child.report_ready.get().is_none());
        // The barrier's duration is unrelated to the production five-second
        // exec budget. Start the test's confirmation budget upon release.
        child.privacy_exec.as_mut().unwrap().deadline =
            tokio::time::Instant::now() + Duration::from_secs(30);
        exec.write_all(b"X").unwrap();
        child.confirm_privacy_exec().await;
        assert_eq!(child.spawn_failure, None);
        assert!(child.report_ready.get().is_some());
        let confirmed = crate::live_children::read_record(&record).unwrap();
        let module = subc_os::file_identity(std::path::Path::new("/bin/sleep")).unwrap();
        assert_ne!(module, trampoline);
        assert_eq!(confirmed[0].executable, Some(module.into()));
        child.start_kill().unwrap();
        child.wait().await.unwrap();
        child.release_roster();
    }
}

#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
async fn macos_unstamped_reexec_records_the_observed_native_image() {
    #[cfg(target_os = "macos")]
    {
        let root = cortexkit_test_support::ScratchDir::new("privacy-reexec");
        let record = root.join("live-children.json");
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                .with_live_children_record(&record);
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let fixture = crate::config_gate::program::native_fixture_for_test();
        let mut spec = spec(
            "/bin/sh",
            &["-c", "exec \"$1\" 600", "reexec", fixture.to_str().unwrap()],
        );
        spec.env
            .push(("SUBC_TEST_PRIVACY_IMAGE_DELAY_MS".into(), "5".into()));
        let mut child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
        let native_image = subc_os::file_identity(&fixture).unwrap();
        // Native image installation, not shell startup speed, establishes
        // that this launch has crossed the re-exec boundary before sampling.
        let reexec = tokio::time::timeout(Duration::from_secs(5), async {
            while observe_spawned_image(child.pid).and_then(|image| image.executable)
                != Some(native_image)
            {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await;
        child.confirm_privacy_exec().await;
        let failure = child.spawn_failure.clone();
        let ready = child.report_ready.get().is_some();
        let confirmed = crate::live_children::read_record(&record).unwrap();
        child.start_kill().unwrap();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        child.release_roster();

        reexec.expect("the shell must re-exec the native fixture");
        assert_eq!(
            failure, None,
            "an unstamped program may re-exec before observation"
        );
        assert!(ready);
        assert_eq!(confirmed.len(), 1);
        assert_eq!(confirmed[0].executable, Some(native_image.into()));
    }
}

#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
async fn macos_unstamped_ack_without_exec_still_refuses_the_trampoline() {
    #[cfg(target_os = "macos")]
    {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let supervisor =
            Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let fixture = crate::config_gate::program::native_fixture_for_test();
        let mut spec = spec(fixture.to_str().unwrap(), &["600"]);
        spec.env.push((
            "SUBC_TEST_PRIVACY_EXEC_BARRIER".into(),
            listener.local_addr().unwrap().to_string(),
        ));
        let mut child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
        let mut exec = accept(listener).await;
        let mut ready = [0];
        exec.read_exact(&mut ready).unwrap();
        assert_eq!(&ready, b"R");
        assert_eq!(
            observe_spawned_image(child.pid).unwrap().executable,
            subc_os::file_identity(&test_privacy_trampoline())
        );
        // Simulate a broken trampoline that acknowledges exec but keeps
        // running its own image, held at the fixture's pre-exec barrier.
        let (reader, writer) = std::io::pipe().unwrap();
        drop(writer);
        child.privacy_exec.as_mut().unwrap().reader =
            tokio::io::unix::AsyncFd::new(reader).unwrap();
        child.confirm_privacy_exec().await;
        let failure = child.spawn_failure.clone();
        let ready = child.report_ready.get().is_some();
        child.start_kill().unwrap();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        child.release_roster();
        assert_eq!(
            failure.as_deref(),
            Some("privacy identity trampoline did not exec the module")
        );
        assert!(!ready);
    }
}

#[tokio::test]
#[cfg_attr(not(target_os = "macos"), ignore = "requires macOS privacy trampoline")]
async fn macos_resumed_121_is_a_module_exit_not_a_trampoline_refusal() {
    #[cfg(target_os = "macos")]
    {
        let registry = Arc::new(Registry::default());
        let policy = RestartPolicy::new(0, Duration::ZERO);
        let fixture = crate::config_gate::program::native_fixture_for_test();
        let spec = spec(fixture.to_str().unwrap(), &["exit121"]);
        let scratch = cortexkit_test_support::ScratchDir::new("resumed-exit");
        let supervisor = Supervisor::new_for_test(Arc::clone(&registry), policy)
            .with_config_runtime(suspended_spawn_tests::gated_owner(&scratch, &spec, true));
        let runtime = supervisor.runtime_config();
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        // A module cannot exit before suspended admission. After resume,
        // retain the real exit status even when it resembles a refusal.
        let mut child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
        child.confirm_privacy_exec().await;
        assert_eq!(child.spawn_failure, None);
        assert!(child.report_ready.get().is_some());
        let pid = child.pid;
        tokio::task::spawn_blocking(move || {
            subc_os::fork_exec_test::wait_for_child_exit_without_reaping(pid)
        })
        .await
        .unwrap()
        .unwrap();
        let status = child.wait().await.unwrap();
        assert_eq!(status.code(), Some(121));
        assert!(child.privacy_exec.is_none());
        let report = classify_reaped_child_exit(&snapshot, &child, &status);
        on_child_exit(
            &spec,
            policy,
            &registry,
            &snapshot,
            &runtime.terminal_ring,
            &runtime.spawn_events,
            &runtime.child_roster,
            report,
            true,
        )
        .await;
        let state = lock_snapshot(&snapshot).unwrap();
        assert_eq!(state.state, ModuleState::Failed);
        assert_eq!(state.last_exit.as_ref().unwrap().code, Some(121));
        assert_eq!(state.reported_pid(), None);
        drop(state);
        let history = runtime.terminal_ring.lock().unwrap().snapshot();
        assert_eq!(history.entries.len(), 1);
        let terminal = &history.entries[0];
        assert_eq!(terminal.exit_code, Some(121));
        assert_eq!(terminal.exit_kind, subc_control::TerminalExitKind::Crash);
        assert_eq!(terminal.disposition, TerminalDisposition::Failed);
        assert_eq!(
            terminal.disposition_detail.as_deref(),
            Some(policy.budget_exhausted_detail().as_str()),
            "module exit 121 was classified as a trampoline refusal: {terminal:?}"
        );
        assert_eq!(child.spawn_failure, None);
        child.release_roster();
    }
}
