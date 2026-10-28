use super::*;
use cortexkit_test_support::ScratchDir;
use std::{fs, path::Path};

struct KernelCgroup {
    root: PathBuf,
    placement: subc_cgroup::Placement,
    modules: PathBuf,
}

impl KernelCgroup {
    fn new(test_name: &str) -> Option<Self> {
        let root = PathBuf::from(format!(
            "/sys/fs/cgroup/subc-reap-test-{test_name}-{}-{}",
            std::process::id(),
            unix_ms_now()
        ));
        if let Err(error) = fs::create_dir(&root) {
            assert!(
                std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
                "required cgroup test cannot execute: {error}"
            );
            eprintln!("SKIP {test_name}: no writable delegated cgroup: {error}");
            return None;
        }
        let placement = subc_cgroup::prepare_at(&root).unwrap().unwrap();
        // Each spawn gets its own uniquely named cgroup under `subc-modules`, so
        // the fixture watches that directory instead of guessing the name.
        let modules = root.join("subc-modules");
        let fixture = Self {
            root,
            placement,
            modules,
        };
        // The test root is itself a non-root cgroup, so it has the same
        // interface files a module cgroup gets.
        if !fixture.root.join("cgroup.kill").exists()
            || !fixture.root.join("cgroup.freeze").exists()
        {
            assert!(
                std::env::var_os("SUBC_REQUIRE_CGROUP_TEST").is_none(),
                "required cgroup.kill/cgroup.freeze interface unavailable"
            );
            eprintln!("SKIP {test_name}: cgroup.kill/cgroup.freeze unavailable");
            return None;
        }
        Some(fixture)
    }

    /// The module cgroups that exist right now.
    fn module_cgroups(&self) -> Vec<PathBuf> {
        fs::read_dir(&self.modules)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                    .map(|entry| entry.path())
                    .collect()
            })
            .unwrap_or_default()
    }
}

impl Drop for KernelCgroup {
    fn drop(&mut self) {
        for module in self.module_cgroups() {
            let _ = fs::write(module.join("cgroup.kill"), "1");
            let deadline = std::time::Instant::now() + Duration::from_secs(1);
            while cgroup_is_populated(&module) && std::time::Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
            }
            let _ = fs::remove_dir(&module);
        }
        let _ = fs::remove_dir(&self.modules);
        let _ = fs::remove_dir(&self.root);
    }
}

fn spawn_module(
    placement: subc_cgroup::Placement,
    dir: &Path,
    args: Vec<String>,
) -> SupervisedChild {
    let supervisor = Supervisor::new_for_test(
        Arc::new(Registry::default()),
        RestartPolicy::new(3, Duration::ZERO),
    )
    .with_cgroup_placement(Some(placement));
    let spec = ModuleSpec {
        module_id: "reaped-module".to_string(),
        program: PathBuf::from("/bin/sh"),
        args,
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.to_string(), dir.display().to_string()))
            .collect(),
        reserved: false,
        reserved_prefixes: Vec::new(),
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
    };
    spawn_and_mark_running(
        &spec,
        &supervisor.runtime_config(),
        &Arc::new(Mutex::new(SupervisorSnapshot::starting())),
    )
    .expect("spawn module that exits without waiting for its child")
}

async fn reap_leftover(test_name: &str, ignore_term: bool) {
    let Some(kernel) = KernelCgroup::new(test_name) else {
        return;
    };
    let dir = ScratchDir::new(test_name);
    let marker = dir.join("terminated");
    let pid_file = dir.join("leftover.pid");
    let script = dir.join("leftover.sh");
    let body = if ignore_term {
        "trap '' TERM\necho $$ > \"$2\"\nexec sleep 600\n"
    } else {
        "trap 'printf terminated > \"$1\"; exit 0' TERM\necho $$ > \"$2\"\nwhile :; do :; done\n"
    };
    fs::write(&script, body).unwrap();
    let mut child = spawn_module(
        kernel.placement.clone(),
        &dir,
        vec![
            "-c".into(),
            "/bin/sh \"$1\" \"$2\" \"$3\" & while [ ! -s \"$3\" ]; do sleep 0.01; done".into(),
            "fixture".into(),
            script.display().to_string(),
            marker.display().to_string(),
            pid_file.display().to_string(),
        ],
    );
    // The spawn creates its cgroup before exec; without this, a cleanup that
    // never placed the module would pass the removal check below.
    assert_eq!(
        kernel.module_cgroups().len(),
        1,
        "the spawn must create exactly one module cgroup"
    );
    let started = Instant::now();
    timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("leftover cleanup must finish")
        .expect("reap module");
    let elapsed = started.elapsed();
    let pid: i32 = fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let running = fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, rest)| !rest.starts_with('Z'))
        })
        .unwrap_or(false);
    assert!(!running, "leftover {pid} must not outlive cleanup");
    assert_eq!(
        kernel.module_cgroups(),
        Vec::<PathBuf>::new(),
        "module cgroup must be removed"
    );
    if ignore_term {
        assert!(
            elapsed >= Duration::from_millis(1900),
            "SIGTERM grace was skipped: {elapsed:?}"
        );
        assert!(
            elapsed < Duration::from_secs(5),
            "SIGKILL escalation was late: {elapsed:?}"
        );
    } else {
        assert!(
            marker.exists(),
            "leftover must receive SIGTERM before cgroup.kill"
        );
        assert!(
            elapsed < Duration::from_secs(1),
            "a cooperative child should not use the full grace: {elapsed:?}"
        );
    }
    eprintln!("EXECUTED {test_name}: leftover {pid} stopped in {elapsed:?}");
}

#[tokio::test]
async fn linux_reaped_cgroup_sigterm_then_exit() {
    reap_leftover("linux_reaped_cgroup_sigterm_then_exit", false).await;
}

#[tokio::test]
async fn linux_reaped_cgroup_ignored_sigterm_escalates_after_two_seconds() {
    reap_leftover("linux_reaped_cgroup_ignored_sigterm", true).await;
}

#[tokio::test]
async fn linux_reaped_cgroup_empty_has_no_grace_wait() {
    let Some(kernel) = KernelCgroup::new("linux_reaped_cgroup_empty") else {
        return;
    };
    let dir = ScratchDir::new("linux-reaped-cgroup-empty");
    let mut child = spawn_module(
        kernel.placement.clone(),
        &dir,
        vec!["-c".into(), "exit 0".into()],
    );
    assert_eq!(
        kernel.module_cgroups().len(),
        1,
        "the spawn must create exactly one module cgroup"
    );
    let started = Instant::now();
    child.wait().await.unwrap();
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        kernel.module_cgroups(),
        Vec::<PathBuf>::new(),
        "empty module cgroup must be removed"
    );
    eprintln!("EXECUTED linux_reaped_cgroup_empty: no grace wait");
}

// Ordinary files let the policy tests run without cgroup delegation. The child
// remains owned and unreaped until signalling ends, so its PID cannot be reused
// even though these stand-ins do not implement the kernel's freezer.
async fn scratch_leftover(test_name: &str, ignore_term: bool, freezer: &str) -> (bool, Duration) {
    let dir = ScratchDir::new(test_name);
    fs::write(dir.join("cgroup.procs"), "").unwrap();
    let placement = subc_cgroup::prepare_at(&dir).unwrap().unwrap();
    let module = placement.module_path("reaped-module").unwrap();
    fs::write(module.join("cgroup.kill"), "0").unwrap();
    let frozen = if freezer == "confirmed" { 1 } else { 0 };
    fs::write(
        module.join("cgroup.events"),
        format!("populated 1\nfrozen {frozen}\n"),
    )
    .unwrap();
    match freezer {
        "unavailable" => {}
        "io-error" => fs::create_dir(module.join("cgroup.freeze")).unwrap(),
        _ => fs::write(module.join("cgroup.freeze"), "0").unwrap(),
    }
    let marker = dir.join("terminated");
    let ready = dir.join("ready");
    let body = if ignore_term {
        "trap '' TERM; echo ready > \"$2\"; exec sleep 600"
    } else {
        "trap 'printf terminated > \"$1\"; exit 0' TERM; echo ready > \"$2\"; while :; do :; done"
    };
    let mut child = Command::new("/bin/sh")
        .args(["-c", body, "fixture"])
        .arg(&marker)
        .arg(&ready)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    timeout(Duration::from_secs(5), async {
        while !ready.exists() {
            sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    fs::write(module.join("cgroup.procs"), format!("{pid}\n")).unwrap();
    let kernel = async {
        loop {
            tokio::select! {
                status = child.wait() => { status.unwrap(); break; }
                _ = sleep(Duration::from_millis(1)) => {
                    if fs::read(module.join("cgroup.kill")).unwrap() == b"1" {
                        let pid = rustix::process::Pid::from_raw(pid as i32).unwrap();
                        rustix::process::kill_process(pid, rustix::process::Signal::KILL).unwrap();
                        child.wait().await.unwrap();
                        break;
                    }
                }
            }
        }
        fs::write(module.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
        // Kernel interface files disappear with rmdir; ordinary files must be
        // removed here so the same production removal call can succeed.
        for entry in fs::read_dir(&module).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                fs::remove_dir(path).unwrap();
            } else {
                fs::remove_file(path).unwrap();
            }
        }
    };
    let started = Instant::now();
    timeout(Duration::from_secs(5), async {
        tokio::join!(cleanup_reaped_cgroup(&placement, "reaped-module"), kernel);
    })
    .await
    .expect("scratch cleanup must finish");
    assert!(!module.exists(), "scratch module directory must be removed");
    (marker.exists(), started.elapsed())
}

#[tokio::test]
async fn scratch_reaped_cgroup_sigterm_then_exit() {
    let (marker, elapsed) = scratch_leftover("scratch-reap-term", false, "confirmed").await;
    assert!(marker, "leftover must receive SIGTERM before cgroup.kill");
    assert!(elapsed < Duration::from_secs(1));
}

#[tokio::test]
async fn scratch_reaped_cgroup_ignored_sigterm_escalates_after_two_seconds() {
    let (_, elapsed) = scratch_leftover("scratch-reap-ignore", true, "confirmed").await;
    assert!(
        elapsed >= Duration::from_millis(1900),
        "grace was skipped: {elapsed:?}"
    );
    assert!(elapsed < Duration::from_secs(5));
}

#[tokio::test]
async fn scratch_reaped_cgroup_freeze_failures_never_send_sigterm() {
    for freezer in ["unavailable", "timeout", "io-error"] {
        let (marker, elapsed) =
            scratch_leftover(&format!("scratch-reap-{freezer}"), false, freezer).await;
        assert!(!marker, "unfrozen PIDs must not be signalled: {freezer}");
        assert!(
            elapsed < Duration::from_secs(1),
            "fallback must skip grace: {elapsed:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn scratch_reaped_cgroup_empty_skips_freeze_and_kill() {
    let dir = ScratchDir::new("scratch-reap-empty");
    fs::write(dir.join("cgroup.procs"), "").unwrap();
    let placement = subc_cgroup::prepare_at(&dir).unwrap().unwrap();
    let module = placement.module_path("reaped-module").unwrap();
    fs::write(module.join("cgroup.events"), "populated 0\nfrozen 0\n").unwrap();
    fs::write(module.join("cgroup.freeze"), "unchanged").unwrap();
    fs::write(module.join("cgroup.kill"), "unchanged").unwrap();
    let started = Instant::now();
    cleanup_reaped_cgroup(&placement, "reaped-module").await;
    assert_eq!(started.elapsed(), Duration::ZERO);
    assert_eq!(
        fs::read(module.join("cgroup.freeze")).unwrap(),
        b"unchanged"
    );
    assert_eq!(fs::read(module.join("cgroup.kill")).unwrap(), b"unchanged");
}
