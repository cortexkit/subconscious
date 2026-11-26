use super::*;

thread_local! {
    static LAST_PID: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

pub(super) fn after_create(child: &Child, _spec: &ModuleSpec) {
    LAST_PID.set(child.id());
    #[cfg(windows)]
    if let Some((_, address)) = _spec
        .env
        .iter()
        .find(|(key, _)| key == "SUBC_TEST_AFTER_CREATE")
    {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream
            .write_all(&child.id().unwrap().to_ne_bytes())
            .unwrap();
        let mut release = [0];
        stream.read_exact(&mut release).unwrap();
    }
}

#[cfg(windows)]
pub(super) fn before_roster_admit(spec: &ModuleSpec) {
    if spec
        .env
        .iter()
        .any(|(key, _)| key == "SUBC_TEST_NONCE_SOURCE_PATH")
    {
        // The child is still suspended. Hold it longer than the pipe's whole
        // delivery window, just as a slow durable record write could.
        std::thread::sleep(2 * REGISTRY_RELEASE_TIMEOUT);
    }
}

#[cfg(windows)]
pub(super) fn assignment_failpoint(spec: &ModuleSpec) -> io::Result<()> {
    if spec
        .env
        .iter()
        .any(|(key, _)| key == "SUBC_TEST_ASSIGNMENT_FAILURE")
    {
        Err(io::Error::other("injected module job assignment failure"))
    } else {
        Ok(())
    }
}

fn spec() -> ModuleSpec {
    ModuleSpec {
        script: None,
        module_id: "spawn-record-worker".into(),
        #[cfg(unix)]
        program: "/bin/sleep".into(),
        #[cfg(windows)]
        program: std::env::current_exe().unwrap(),
        #[cfg(unix)]
        args: vec!["60".into()],
        #[cfg(windows)]
        args: vec![
            "--exact".into(),
            "supervise::spawn_record_tests::windows_store_touch_fixture".into(),
            "--nocapture".into(),
        ],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
        carry_containment: false,
    }
}

#[tokio::test]
async fn record_sync_failure_fails_spawn_and_reaps_child() {
    let home = cortexkit_test_support::ScratchDir::new("record-sync-failure");
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_live_children_record(home.join("live-children.json"));
    crate::live_children::fail_next_file_sync();
    let result = supervisor.spawn(spec());
    let failed = result.is_err();
    if let Ok(module) = result {
        module.stop().await.unwrap();
    }
    assert!(failed, "record failure must fail spawn");
    let pid = LAST_PID.get().expect("the real OS child was created");
    assert!(
        subc_os::Process::open(pid).unwrap().is_none(),
        "child {pid} survived rejected admission or was not reaped"
    );
}

#[cfg(target_os = "macos")]
#[test]
#[ignore = "manual on-device latency measurement"]
fn durable_record_write_latency_50() {
    let home = cortexkit_test_support::ScratchDir::new("record-latency");
    let path = home.join("live-children.json");
    let children = vec![crate::live_children::LiveChild {
        module_id: "latency-worker".into(),
        pid: std::process::id(),
        protocol: ModuleProtocol::None,
        start_time: Some(1),
        executable: None,
        cgroup_name: None,
        container: None,
    }];
    let mut samples = Vec::new();
    for _ in 0..50 {
        let start = std::time::Instant::now();
        crate::live_children::write_record(&path, &children).unwrap();
        samples.push(start.elapsed());
    }
    samples.sort();
    eprintln!(
        "durable_record_write_latency_50: n=50 median={:?} p95={:?}",
        (samples[24] + samples[25]) / 2,
        samples[47]
    );
}

#[cfg(windows)]
#[test]
fn windows_store_touch_fixture() {
    if let Some(path) = std::env::var_os("SUBC_TEST_STORE_TOUCH") {
        std::fs::write(path, b"module ran").unwrap();
    }
}

#[cfg(windows)]
#[test]
fn windows_nonce_source_fixture() {
    let Some(path) = std::env::var_os("SUBC_TEST_NONCE_SOURCE_PATH") else {
        return;
    };
    let path = PathBuf::from(path);
    let source = subc_os::launch_nonce()
        .map(|nonce| nonce.map(|nonce| nonce.source()))
        .map_err(|error| error.to_string());
    // Publish only the source, never the secret. Rename after the write so
    // the parent cannot observe a partly written result.
    let pending = path.with_extension("pending");
    std::fs::write(&pending, format!("{source:?}")).unwrap();
    std::fs::rename(pending, path).unwrap();
    std::thread::park();
}

#[cfg(windows)]
#[tokio::test]
async fn windows_nonce_window_starts_after_suspended_admission() {
    let home = cortexkit_test_support::ScratchDir::new("nonce-suspended-admission");
    let result_path = home.join("nonce-source");
    let mut spec = spec();
    spec.protocol = ModuleProtocol::Subc;
    spec.args[1] = "supervise::spawn_record_tests::windows_nonce_source_fixture".into();
    spec.env.push((
        "SUBC_TEST_NONCE_SOURCE_PATH".into(),
        result_path.display().to_string(),
    ));
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
            .with_live_children_record(home.join("live-children.json"));
    let module = supervisor.spawn(spec).unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match std::fs::read_to_string(&result_path) {
                Ok(source) => break source,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Err(error) => panic!("cannot read fixture result: {error}"),
            }
        }
    })
    .await;
    module.stop().await.unwrap();
    assert_eq!(
        result.expect("child did not report its nonce source"),
        "Ok(Some(Pipe))"
    );
}

#[cfg(windows)]
#[tokio::test]
async fn windows_daemon_fixture() {
    let Ok(address) = std::env::var("SUBC_TEST_DAEMON_BARRIER") else {
        return;
    };
    let mut spec = spec();
    spec.env.push(("SUBC_TEST_AFTER_CREATE".into(), address));
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let _module = supervisor.spawn(spec).unwrap();
}

#[cfg(windows)]
#[test]
fn windows_daemon_death_before_module_job_assignment_leaves_no_child() {
    use std::io::Read;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    // An outer job models a scheduler/service host. The daemon's root and
    // root job must nest without requiring breakaway from this job.
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "supervise::spawn_record_tests::windows_daemon_fixture",
            "--nocapture",
        ])
        .env(
            "SUBC_TEST_DAEMON_BARRIER",
            listener.local_addr().unwrap().to_string(),
        );
    let mut daemon = subc_jobobject::spawn_contained(&mut command).unwrap();
    listener.set_nonblocking(true).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut stream = loop {
        if let Ok((stream, _)) = listener.accept() {
            break stream;
        }
        assert!(
            daemon.child.try_wait().unwrap().is_none(),
            "daemon failed before rendezvous"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "daemon did not reach CreateProcess failpoint"
        );
        std::thread::yield_now();
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    let mut pid = [0; 4];
    stream.read_exact(&mut pid).unwrap();
    let pid = u32::from_ne_bytes(pid);
    assert!(subc_jobobject::process_exists(pid));
    // Terminate only the daemon, not its outer job: otherwise the host job
    // would also kill a child whose daemon never joined its root job.
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    let exited = subc_jobobject::wait_for_process_exit(pid, Duration::from_secs(10));
    if !exited {
        daemon.job.terminate().unwrap();
    }
    assert!(exited, "suspended child {pid} outlived hard-killed daemon");
}

#[cfg(windows)]
#[tokio::test]
async fn windows_containment_failure_never_resumes() {
    let home = cortexkit_test_support::ScratchDir::new("assignment-refusal");
    let sentinel = home.join("store-touched");
    let mut spec = spec();
    spec.env
        .push(("SUBC_TEST_ASSIGNMENT_FAILURE".into(), "1".into()));
    spec.env.push((
        "SUBC_TEST_STORE_TOUCH".into(),
        sentinel.display().to_string(),
    ));
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let result = supervisor.spawn(spec);
    // A version that resumes despite assignment failure must have time to
    // execute its sentinel before dropping the returned module kills it.
    if let Ok(module) = result {
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while !sentinel.exists() && module.state().unwrap() == ModuleState::Running {
                tokio::task::yield_now().await;
            }
        })
        .await;
        let _ = module.stop().await;
        panic!(
            "assignment failure resumed a module; store touched={}",
            sentinel.exists()
        );
    }
    assert!(
        !sentinel.exists(),
        "refused module executed store-touch sentinel"
    );
    assert!(!subc_jobobject::process_exists(LAST_PID.get().unwrap()));
}

#[cfg(windows)]
#[tokio::test]
async fn windows_nested_daemon_fixture() {
    let Some(sentinel) = std::env::var_os("SUBC_TEST_NESTED_STORE") else {
        return;
    };
    let mut spec = spec();
    spec.env.push((
        "SUBC_TEST_STORE_TOUCH".into(),
        sentinel.to_str().unwrap().into(),
    ));
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let module = supervisor
        .spawn(spec)
        .expect("per-module job must nest below daemon and host jobs");
    tokio::time::timeout(Duration::from_secs(10), async {
        while !PathBuf::from(&sentinel).exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    module.stop().await.unwrap();
}

#[cfg(windows)]
#[test]
fn windows_module_job_nests_under_host_job() {
    let home = cortexkit_test_support::ScratchDir::new("nested-jobs");
    let sentinel = home.join("nested-store-touched");
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "supervise::spawn_record_tests::windows_nested_daemon_fixture",
            "--nocapture",
        ])
        .env("SUBC_TEST_NESTED_STORE", &sentinel);
    let mut daemon = subc_jobobject::spawn_contained(&mut command).unwrap();
    assert!(daemon.child.wait().unwrap().success());
    assert!(sentinel.exists(), "nested protocol:none module never ran");
}
