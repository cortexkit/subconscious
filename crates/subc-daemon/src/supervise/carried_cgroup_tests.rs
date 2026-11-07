//! Real delegated-cgroup coverage. Required CI mode must fail, not skip, when
//! its explicitly delegated root is missing or cannot place a child.

use super::*;

struct OwnedMember {
    process: subc_os::Process,
    birth: u64,
}
impl OwnedMember {
    fn capture(pid: u32, parent: u32) -> Self {
        assert_eq!(subc_os::parent_pid(pid), Some(parent));
        let process = subc_os::Process::open(pid).unwrap().unwrap();
        let birth = process.observe().unwrap().start_time;
        assert_eq!(subc_os::parent_pid(pid), Some(parent));
        Self { process, birth }
    }
    fn alive(&self) -> bool {
        self.process
            .observe()
            .is_some_and(|now| now.start_time == self.birth)
    }
    fn kill(&self) {
        if self.alive() {
            self.process.signal(subc_os::Signal::Kill).unwrap();
        }
    }
    async fn gone(&self) {
        timeout(Duration::from_secs(5), async {
            while self.alive() {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the owned holder must die at container-terminal cleanup");
    }
}
impl Drop for OwnedMember {
    fn drop(&mut self) {
        if self.alive() {
            let _ = self.process.signal(subc_os::Signal::Kill);
        }
    }
}

struct StartingModule(Option<SupervisedModule>);
impl Drop for StartingModule {
    fn drop(&mut self) {
        if let Some(module) = &self.0 {
            if let Some(monitor) = module.inner.monitor.lock().unwrap().take() {
                monitor.abort();
            }
        }
    }
}

struct Fixture {
    module: StartingModule,
    supervisor: Supervisor,
    kernel: KernelCgroup,
    dir: ScratchDir,
    spec: ModuleSpec,
    holder: OwnedMember,
    first_leader: OwnedMember,
    path: PathBuf,
    record: PathBuf,
}
impl Fixture {
    async fn new(label: &str, backoff: Duration) -> Option<Self> {
        let kernel = KernelCgroup::new(label)?;
        let dir = ScratchDir::new(label);
        let record = dir.join("live-children.json");
        let holder_file = dir.join("holder.pid");
        let target_file = dir.join("holder.target");
        let spec = ModuleSpec {
            carry_containment: true,
            module_id: label.into(),
            program: "/bin/sh".into(),
            args: vec!["-c".into(),
                "if [ ! -s \"$1\" ]; then while [ ! -s \"$2\" ]; do /bin/sleep 0.01; done; /usr/bin/setsid /bin/sh -c 'echo 0 > \"$2\"; echo $$ > \"$1\"; exec /bin/sleep 600' holder \"$1\" \"$(cat \"$2\")\" </dev/null >/dev/null 2>&1 & fi; exec /bin/sleep 600".into(),
                "carried-cgroup".into(), holder_file.display().to_string(), target_file.display().to_string()],
            env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"].into_iter()
                .map(|key| (key.into(), dir.display().to_string())).collect(),
            reserved: false,
            reserved_prefixes: vec![],
            protocol: ModuleProtocol::None,
            overlap: Default::default(),
        };
        let policy = RestartPolicy::new(4, backoff);
        let supervisor = Supervisor::new_for_test(Arc::new(Registry::default()), policy)
            .with_drain_timeout(Duration::from_millis(20))
            .with_forwarding(Arc::new(ForwardingTable::default()))
            .with_cgroup_placement(Some(kernel.placement.clone()))
            .with_live_children_record(&record);
        let starting = StartingModule(Some(
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
        let module = starting.0.as_ref().unwrap();
        let leader = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = module.status().unwrap().pid {
                    break pid;
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("placed leader must start");
        let first_leader = OwnedMember::capture(leader, std::process::id());
        let container = crate::live_children::read_record(&record)
            .unwrap()
            .into_iter()
            .find_map(|entry| entry.container)
            .expect("carried container must be durably recorded");
        assert_eq!(
            container.pgid, None,
            "a carried cgroup must never authorize a group-wide signal"
        );
        let path = container
            .cgroup_path
            .expect("record must contain its exact cgroup path");
        assert!(path.starts_with(&kernel.modules));
        let nested = path.join("holder");
        fs::create_dir(&nested).unwrap();
        fs::write(
            &target_file,
            nested.join("cgroup.procs").as_os_str().as_encoded_bytes(),
        )
        .unwrap();
        let holder = timeout(Duration::from_secs(5), async {
            loop {
                if let Some(pid) = fs::read_to_string(&holder_file)
                    .ok()
                    .and_then(|text| text.trim().parse().ok())
                {
                    break OwnedMember::capture(pid, leader);
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("leader must publish its owned holder after nested placement");
        let holder_pid: u32 = fs::read_to_string(holder_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(fs::read_to_string(nested.join("cgroup.procs"))
            .unwrap()
            .lines()
            .any(|pid| pid == holder_pid.to_string()));
        assert_ne!(
            subc_os::process_group::pgid(holder_pid).unwrap(),
            Some(leader),
            "the holder escaped the process group, so killpg would miss it"
        );
        Some(Self {
            module: starting,
            supervisor,
            kernel,
            dir,
            spec,
            holder,
            first_leader,
            path,
            record,
        })
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
        .expect("supervisor must reach the expected lifecycle state")
    }
    fn same_container(&self) {
        let entries = crate::live_children::read_record(&self.record).unwrap();
        let container = entries
            .into_iter()
            .find_map(|entry| entry.container)
            .unwrap();
        assert_eq!(container.cgroup_path.as_ref(), Some(&self.path));
        assert_eq!(container.pgid, None);
        assert!(
            self.holder.alive(),
            "the original nested holder must survive replacement"
        );
    }
    async fn stop(&self) {
        self.module().stop().await.unwrap();
        self.holder.gone().await;
        assert!(
            !self.path.exists(),
            "terminal cleanup must remove child cgroups before their root"
        );
        assert!(crate::live_children::read_record(&self.record)
            .unwrap()
            .is_empty());
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.supervisor.child_roster.shutdown_flag().set_for_test();
        if let Some(monitor) = self.module().inner.monitor.lock().unwrap().take() {
            monitor.abort();
        }
        // KernelCgroup owns the unique subtree created by this fixture and kills
        // only that subtree. The leader's fallback process handle was captured
        // after checking its parent is this test; the holder's handle was
        // captured after checking its parent is that leader. Both handles are
        // used for cleanup only while their recorded kernel start times match.
        let _ = (&self.kernel, &self.dir);
    }
}

#[tokio::test]
async fn linux_carried_cgroup_operator_restart_keeps_nested_holder() {
    let Some(fixture) = Fixture::new("carried-cgroup-operator", Duration::from_millis(5)).await
    else {
        return;
    };
    let mut generation = fixture.module().status().unwrap().spawn_generation;
    for _ in 0..2 {
        fixture.module().restart(None).await.unwrap();
        generation = fixture
            .status(|status| {
                status.state == ModuleState::Running && status.spawn_generation > generation
            })
            .await
            .spawn_generation;
        fixture.same_container();
    }
    fixture.stop().await;
    eprintln!("EXECUTED carried cgroup operator restart and terminal cleanup");
}

#[tokio::test]
async fn linux_carried_cgroup_crash_restart_keeps_nested_holder() {
    let Some(fixture) = Fixture::new("carried-cgroup-crash", Duration::from_millis(5)).await else {
        return;
    };
    let generation = fixture.module().status().unwrap().spawn_generation;
    fixture.first_leader.kill();
    fixture
        .status(|status| {
            status.state == ModuleState::Running && status.spawn_generation > generation
        })
        .await;
    fixture.same_container();
    fixture.stop().await;
    eprintln!("EXECUTED carried cgroup crash restart and terminal cleanup");
}

#[tokio::test]
async fn linux_carried_cgroup_reload_keeps_nested_holder() {
    let Some(fixture) = Fixture::new("carried-cgroup-reload", Duration::from_millis(5)).await
    else {
        return;
    };
    let generation = fixture.module().status().unwrap().spawn_generation;
    let mut replacement = fixture.spec.clone();
    replacement.protocol = ModuleProtocol::Subc;
    fixture
        .module()
        .update_configuration(replacement, HealthConfig::default(), None)
        .await
        .unwrap();
    let connection = crate::registry::ConnectionId::new(42);
    // Model the new generation's HELLO at the Registry boundary. Process
    // placement and lifetime remain real kernel-cgroup operations in this test.
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
    fixture.same_container();
    fixture
        .supervisor
        .registry
        .deregister_connection(connection)
        .unwrap();
    fixture.stop().await;
    eprintln!("EXECUTED carried cgroup reload and terminal cleanup");
}

#[tokio::test]
async fn linux_carried_cgroup_health_restart_keeps_nested_holder() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let Some(fixture) = Fixture::new("carried-cgroup-health", Duration::from_millis(5)).await
    else {
        return;
    };
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
    fixture.same_container();
    fixture.stop().await;
    server.abort();
    eprintln!("EXECUTED carried cgroup health restart and terminal cleanup");
}

#[tokio::test]
async fn linux_carried_cgroup_disable_during_backoff_kills_nested_holder() {
    let Some(fixture) =
        Fixture::new("carried-cgroup-disable-backoff", Duration::from_secs(60)).await
    else {
        return;
    };
    fixture.first_leader.kill();
    fixture
        .status(|status| status.state == ModuleState::Restarting && status.pid.is_none())
        .await;
    fixture.same_container();
    assert!(fixture.module().set_enabled(false).await.unwrap());
    fixture.holder.gone().await;
    assert!(!fixture.path.exists());
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
    fixture.module().stop().await.unwrap();
    eprintln!("EXECUTED carried cgroup terminal cleanup with no live leader");
}

#[tokio::test]
async fn linux_carried_cgroup_recovery_sweeps_nested_holder_without_leader() {
    let Some(fixture) = Fixture::new("carried-cgroup-boot", Duration::from_secs(60)).await else {
        return;
    };
    fixture.first_leader.kill();
    fixture
        .status(|status| status.state == ModuleState::Restarting && status.pid.is_none())
        .await;
    fixture.same_container();
    // Leave the actor in its long backoff while exercising the exact persisted
    // boot-sweep entry point with the lock and trusted placement supplied by
    // bootstrap. Cleanup cannot rely on the already-reaped leader's PID.
    let owner = crate::run_dir_lock::RunDirLock::acquire(&fixture.record).unwrap();
    let decisions = crate::live_children::sweep_orphans_with_placement(
        &owner,
        &crate::live_children::AdoptedPids::none(),
        crate::live_children::SweepBounds::default(),
        Some(&fixture.kernel.placement),
    )
    .await;
    assert!(decisions
        .iter()
        .any(|(entry, decision)| entry.container.is_some()
            && matches!(
                decision,
                crate::live_children::SweepDecision::Terminated
                    | crate::live_children::SweepDecision::Killed
            )));
    fixture.holder.gone().await;
    assert!(!fixture.path.exists());
    assert!(crate::live_children::read_record(&fixture.record)
        .unwrap()
        .is_empty());
    fixture.module().stop().await.unwrap();
    eprintln!("EXECUTED carried cgroup persisted-path recovery with no live leader");
}

#[tokio::test]
async fn linux_carried_cgroup_monitor_abort_kills_the_nested_holder_or_retains_its_identity() {
    let Some(fixture) = Fixture::new("carried-cgroup-abort", Duration::ZERO).await else {
        return;
    };
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
    if fixture.holder.alive() {
        assert!(
            fixture
                .supervisor
                .child_roster
                .previous_instance(&fixture.spec.module_id)
                .is_some(),
            "an accepted cgroup kill cannot erase the identity of a still-live holder"
        );
        assert!(crate::live_children::read_record(&fixture.record)
            .unwrap()
            .iter()
            .any(|entry| entry
                .container
                .as_ref()
                .and_then(|container| container.cgroup_path.as_ref())
                == Some(&fixture.path)));
    }
    // This owned fixture is sleeping, not stuck in an uninterruptible kernel
    // call. Its hard-kill backstop must actually reach the nested cgroup, not
    // merely retain the identity forever without ever requesting a kill.
    fixture.holder.gone().await;
    timeout(Duration::from_secs(5), async {
        while fixture
            .supervisor
            .child_roster
            .previous_instance(&fixture.spec.module_id)
            .is_some()
        {
            sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("verified empty aborted containers must release their retained record");
    eprintln!(
        "EXECUTED carried cgroup monitor abort with nested holder and retained-identity check"
    );
}
