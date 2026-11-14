//! Windows carried-job tests check each child's parent and kernel creation
//! time before retaining the process handle used for forced cleanup.

use super::*;
use cortexkit_test_support::ScratchDir;

struct OwnedProcess {
    process: subc_os::Process,
    birth: u64,
}
impl OwnedProcess {
    fn capture(pid: u32, parent: u32) -> Self {
        assert_eq!(subc_os::parent_pid(pid), Some(parent));
        let process = subc_os::Process::open(pid).unwrap().unwrap();
        let birth = process.observe().unwrap().start_time;
        assert_eq!(subc_os::parent_pid(pid), Some(parent));
        Self { process, birth }
    }
    fn alive(&self) -> bool {
        !matches!(self.process.wait_for_exit(Duration::ZERO), Ok(true))
            && self
                .process
                .observe()
                .is_some_and(|now| now.start_time == self.birth)
    }
    fn kill(&self) {
        if self.alive() {
            assert!(self
                .process
                .force_stop(self.birth, Duration::from_secs(5))
                .unwrap());
        }
    }
    async fn gone(&self) {
        timeout(Duration::from_secs(5), async {
            while self.alive() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the job must stop its owned holder");
    }
}
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.process.force_stop(self.birth, Duration::from_secs(5));
        }
    }
}

struct MonitorGuard(Option<SupervisedModule>);
impl Drop for MonitorGuard {
    fn drop(&mut self) {
        if let Some(module) = &self.0 {
            if let Some(monitor) = module.inner.monitor.lock().unwrap().take() {
                monitor.abort();
            }
        }
    }
}

struct Fixture {
    module: MonitorGuard,
    supervisor: Supervisor,
    _dir: ScratchDir,
    spec: ModuleSpec,
    first: OwnedProcess,
    holder: OwnedProcess,
    holder_pid: u32,
    holder_file: PathBuf,
    record: PathBuf,
}
impl Fixture {
    async fn new(label: &str, budget: u32, backoff: Duration) -> Self {
        let dir = ScratchDir::new(label);
        let holder_file = dir.join("holder.pid");
        let record = dir.join("live-children.json");
        let stub = cortexkit_test_support::cargo_profile_bin("fake-aft-stub");
        assert!(
            stub.is_file(),
            "build prerequisite: cargo build -p subc-core --bin fake-aft-stub"
        );
        let mut spec = ModuleSpec {
            carry_containment: true,
            module_id: label.into(),
            program: stub,
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_NEVER_CONNECT".into(), "1".into()),
                (
                    "FAKE_AFT_GRANDCHILD_PID_FILE".into(),
                    holder_file.display().to_string(),
                ),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        };
        spec.env.extend(
            ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                .into_iter()
                .map(|key| (key.into(), dir.display().to_string())),
        );
        let policy = RestartPolicy::new(budget, backoff);
        let supervisor = Supervisor::new_for_test(Arc::new(Registry::default()), policy)
            .with_drain_timeout(Duration::from_millis(20))
            .with_forwarding(Arc::new(ForwardingTable::default()))
            .with_live_children_record(&record);
        let module = MonitorGuard(Some(
            supervisor
                .supervise_configured_requested(
                    spec.clone(),
                    true,
                    HealthConfig::default(),
                    None,
                    policy,
                    Duration::from_secs(1),
                    None,
                )
                .unwrap(),
        ));
        let first = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = module.0.as_ref().unwrap().status().unwrap().pid {
                    break OwnedProcess::capture(pid, std::process::id());
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the assigned first leader must start");
        let leader_pid = module.0.as_ref().unwrap().status().unwrap().pid.unwrap();
        let (holder, holder_pid) = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = std::fs::read_to_string(&holder_file)
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                {
                    break (OwnedProcess::capture(pid, leader_pid), pid);
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the owned leader must publish its own holder");
        // Only the first generation creates the holder. Updating the next
        // generation's environment avoids adding a second holder or trusting
        // a stale PID file inside the child fixture.
        spec.env
            .retain(|(key, _)| key != "FAKE_AFT_GRANDCHILD_PID_FILE");
        module
            .0
            .as_ref()
            .unwrap()
            .update_configuration(spec.clone(), HealthConfig::default(), None)
            .await
            .unwrap();
        Self {
            module,
            supervisor,
            _dir: dir,
            spec,
            first,
            holder,
            holder_pid,
            holder_file,
            record,
        }
    }
    fn module(&self) -> &SupervisedModule {
        self.module.0.as_ref().unwrap()
    }
    async fn status(&self, predicate: impl Fn(&ModuleStatus) -> bool) -> ModuleStatus {
        timeout(Duration::from_secs(5), async {
            loop {
                let status = self.module().status().unwrap();
                if predicate(&status) {
                    break status;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the carried job must reach the expected lifecycle state")
    }
    fn same_holder(&self) {
        assert!(
            self.holder.alive(),
            "the original holder must survive the leader replacement"
        );
        assert_eq!(
            std::fs::read_to_string(&self.holder_file)
                .unwrap()
                .trim()
                .parse::<u32>()
                .unwrap(),
            self.holder_pid
        );
        assert!(
            crate::live_children::read_record(&self.record)
                .unwrap()
                .iter()
                .any(|entry| entry.container.is_some()),
            "the container roster entry must outlive every leader"
        );
    }
    async fn stop(&self) {
        self.module().stop().await.unwrap();
        self.holder.gone().await;
        assert!(crate::live_children::read_record(&self.record)
            .unwrap()
            .is_empty());
    }
}

#[tokio::test]
async fn carried_windows_operator_restart_keeps_holder_across_two_generations() {
    let fixture = Fixture::new("carried-job-operator", 3, Duration::from_millis(5)).await;
    let mut generation = fixture.module().status().unwrap().spawn_generation;
    for _ in 0..2 {
        fixture.module().restart(None).await.unwrap();
        generation = fixture
            .status(|status| {
                status.state == ModuleState::Running && status.spawn_generation > generation
            })
            .await
            .spawn_generation;
        fixture.same_holder();
    }
    fixture.stop().await;
}

#[tokio::test]
async fn carried_windows_crash_restart_keeps_holder() {
    let fixture = Fixture::new("carried-job-crash", 3, Duration::from_millis(5)).await;
    let generation = fixture.module().status().unwrap().spawn_generation;
    fixture.first.kill();
    fixture
        .status(|status| {
            status.state == ModuleState::Running && status.spawn_generation > generation
        })
        .await;
    fixture.same_holder();
    fixture.stop().await;
}

#[tokio::test]
async fn carried_windows_stop_during_backoff_terminates_holder() {
    let fixture = Fixture::new("carried-job-stop-backoff", 3, Duration::from_secs(60)).await;
    fixture.first.kill();
    fixture
        .status(|status| status.state == ModuleState::Restarting && status.pid.is_none())
        .await;
    fixture.same_holder();
    fixture.stop().await;
}

#[tokio::test]
async fn carried_windows_disable_during_backoff_terminates_holder() {
    let fixture = Fixture::new("carried-job-disable-backoff", 3, Duration::from_secs(60)).await;
    fixture.first.kill();
    fixture
        .status(|status| status.state == ModuleState::Restarting && status.pid.is_none())
        .await;
    fixture.same_holder();
    assert!(fixture.module().set_enabled(false).await.unwrap());
    fixture.holder.gone().await;
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
    fixture.module().stop().await.unwrap();
}

#[tokio::test]
async fn carried_windows_budget_exhaustion_terminates_holder() {
    let fixture = Fixture::new("carried-job-budget", 0, Duration::ZERO).await;
    fixture.first.kill();
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    fixture.module().stop().await.unwrap();
}

#[tokio::test]
async fn carried_windows_monitor_abort_terminates_holder() {
    let fixture = Fixture::new("carried-job-abort", 3, Duration::ZERO).await;
    let monitor = fixture
        .module()
        .inner
        .monitor
        .lock()
        .unwrap()
        .take()
        .unwrap();
    monitor.abort();
    assert!(monitor.await.unwrap_err().is_cancelled());
    fixture.holder.gone().await;
}

#[tokio::test]
async fn carried_windows_health_restart_keeps_holder() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let fixture = Fixture::new("carried-job-health", 3, Duration::from_millis(5)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let mut first = true;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut method = [0; 4];
            stream.read_exact(&mut method).await.unwrap();
            assert_eq!(&method, b"GET ");
            let response: &[u8] = if first {
                b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            } else {
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            first = false;
            let _ = stream.write_all(response).await;
        }
    });
    let generation = fixture.module().status().unwrap().spawn_generation;
    fixture
        .module()
        .update_configuration(
            fixture.spec.clone(),
            HealthConfig {
                http: Some(format!("http://{address}/health")),
                cadence: Duration::from_millis(50),
                deadline: Duration::from_millis(100),
                failure_threshold: 1,
                on_failing: HealthAction::Restart,
                ..HealthConfig::default()
            },
            None,
        )
        .await
        .unwrap();
    fixture
        .status(|status| {
            status.state == ModuleState::Running && status.spawn_generation > generation
        })
        .await;
    fixture.same_holder();
    fixture.stop().await;
    server.abort();
}

#[tokio::test]
async fn carried_windows_reload_keeps_holder() {
    let fixture = Fixture::new("carried-job-reload", 3, Duration::from_millis(5)).await;
    let mut replacement = fixture.spec.clone();
    replacement.protocol = ModuleProtocol::Subc;
    fixture
        .module()
        .update_configuration(replacement, HealthConfig::default(), None)
        .await
        .unwrap();
    let generation = fixture.module().status().unwrap().spawn_generation;
    let connection = crate::registry::ConnectionId::new(43);
    // Supply HELLO's Registry effect directly; the actual child, suspended
    // assignment, replacement and holder are real Windows processes/jobs.
    let (reloaded, ()) = tokio::join!(fixture.module().reload(), async {
        fixture
            .status(|status| status.pid.is_some() && status.spawn_generation > generation)
            .await;
        fixture
            .supervisor
            .registry
            .register_with_control_ops(
                subc_protocol::manifest::ModuleManifest::builder(&fixture.spec.module_id, "0.1.0")
                    .build(),
                1,
                connection,
                Vec::new(),
            )
            .unwrap();
    });
    reloaded.unwrap();
    fixture.same_holder();
    fixture
        .supervisor
        .registry
        .deregister_connection(connection)
        .unwrap();
    fixture.stop().await;
}

#[tokio::test]
async fn carried_windows_reload_registration_timeout_terminates_holder() {
    let fixture = Fixture::new("carried-job-reload-timeout", 3, Duration::from_millis(5)).await;
    let error = fixture
        .module()
        .reload()
        .await
        .expect_err("the replacement never registers");
    assert!(
        matches!(error, SuperviseError::ReloadFailed { ref reason, .. } if reason.contains("did not register")),
        "{error:?}"
    );
    fixture.holder.gone().await;
    fixture.module().stop().await.unwrap();
}

#[tokio::test]
async fn carried_windows_replacements_share_the_original_kernel_job() {
    fn independently_observed_members(leaders: &[u32], holder: u32, current: u32) -> u64 {
        let owned = leaders
            .iter()
            .copied()
            .chain([holder])
            .map(|pid| pid.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let output = std::process::Command::new("powershell.exe")
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command"])
            .arg(format!("Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -in @({owned}) -or $_.ParentProcessId -in @({owned}) }} | Select-Object ProcessId,ParentProcessId,Name | ConvertTo-Json -Compress"))
            .output().unwrap();
        assert!(output.status.success());
        let facts: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let facts = facts
            .as_array()
            .expect("both owned live fixture processes must be present");
        let mut stubs = std::collections::BTreeSet::new();
        for entry in facts {
            let name = entry["Name"].as_str().unwrap();
            if name.eq_ignore_ascii_case("fake-aft-stub.exe") {
                stubs.insert(u32::try_from(entry["ProcessId"].as_u64().unwrap()).unwrap());
            } else {
                assert!(
                    name.eq_ignore_ascii_case("conhost.exe"),
                    "unexpected fixture descendant: {entry:?}"
                );
            }
        }
        assert_eq!(
            stubs,
            [holder, current].into_iter().collect(),
            "only the original holder and current leader may be live"
        );
        // Each stub can create a Windows console host, which also belongs in
        // the job. Count those from an independent process-tree observation,
        // not from the job query whose membership we are checking.
        u64::try_from(facts.len()).unwrap()
    }
    let fixture = Fixture::new("carried-job-kernel-membership", 3, Duration::from_millis(5)).await;
    let crate::subtree_resources::Target::Job(original) = fixture.module().subtree_target() else {
        panic!("Windows resources must expose the job query target")
    };
    let job = original
        .upgrade()
        .expect("the actor must own the original job");
    // Assigned-process counts can retain exited entries while a process handle
    // remains open. Identity-safe cleanup retains leader handles deliberately,
    // so use the query that independently confirms each live member instead.
    let live_members = || {
        job.lock()
            .unwrap()
            .query()
            .unwrap()
            .memory(std::time::Instant::now() + Duration::from_secs(5))
            .unwrap()
            .process_count
    };
    let mut leaders = vec![fixture.first.process.pid()];
    let mut retained_leaders = Vec::new();
    let expected = independently_observed_members(&leaders, fixture.holder_pid, leaders[0]);
    assert_eq!(live_members(), expected);
    assert_eq!(
        Arc::strong_count(&job),
        2,
        "only the actor's container and this test retain strong job Arcs; children are weak owners"
    );
    let mut generation = fixture.module().status().unwrap().spawn_generation;
    for _ in 0..2 {
        fixture.module().restart(None).await.unwrap();
        let status = fixture
            .status(|status| {
                status.state == ModuleState::Running && status.spawn_generation > generation
            })
            .await;
        generation = status.spawn_generation;
        let pid = status.pid.unwrap();
        retained_leaders.push(OwnedProcess::capture(pid, std::process::id()));
        leaders.push(pid);
        fixture.same_holder();
        assert!(
            !fixture.first.alive(),
            "the old module leader must have exited"
        );
        let crate::subtree_resources::Target::Job(current) = fixture.module().subtree_target()
        else {
            panic!("replacement must expose its actual assigned job")
        };
        assert!(
            std::sync::Weak::ptr_eq(&original, &current),
            "replacement resource accounting must point to the original job"
        );
        let expected = independently_observed_members(&leaders, fixture.holder_pid, pid);
        assert_eq!(live_members(), expected, "the original kernel job must contain the replacement and the whole carried tree, not only old survivors");
        assert_eq!(Arc::strong_count(&job), 2);
    }
    fixture.stop().await;
    assert_eq!(
        live_members(),
        0,
        "terminal cleanup explicitly terminates the job even while this test retains a handle"
    );
    drop(retained_leaders);
}

#[tokio::test]
async fn carried_windows_assignment_failure_terminates_old_holder_without_resuming() {
    let fixture = Fixture::new(
        "carried-job-assignment-rejection",
        3,
        Duration::from_millis(5),
    )
    .await;
    let marker = fixture._dir.join("rejected-child-ran");
    let mut spec = fixture.spec.clone();
    spec.env.extend([
        ("SUBC_TEST_ASSIGNMENT_FAILURE".into(), "1".into()),
        ("FAKE_AFT_PID_PATH".into(), marker.display().to_string()),
    ]);
    fixture
        .module()
        .update_configuration(spec, HealthConfig::default(), None)
        .await
        .unwrap();
    fixture.module().restart(None).await.unwrap();
    fixture
        .status(|status| status.state == ModuleState::Failed)
        .await;
    fixture.holder.gone().await;
    assert!(
        !marker.exists(),
        "the suspended rejected generation must never execute its startup marker"
    );
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
    fixture.module().stop().await.unwrap();
}
