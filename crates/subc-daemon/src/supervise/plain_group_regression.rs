use super::*;
use cortexkit_test_support::ScratchDir;

#[tokio::test(start_paused = true)]
async fn plain_group_exit_wakes_without_a_timer_and_keeps_the_leader_unreaped() {
    use std::{
        future::{poll_fn, Future},
        os::unix::process::ExitStatusExt,
        task::Poll,
    };

    let child = Command::new("/bin/sh")
        .args(["-c", "read line; exit 23"])
        .stdin(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut child = containment::ProcessChild::plain(child, pid);
    child.plain_group = plain_group::PlainGroup::from_spawn(pid).map(Arc::new);
    let tick = Instant::now();
    let (parked, mut did_park) = tokio::sync::oneshot::channel();
    let waiter = tokio::spawn(async move {
        let status = {
            let wait = child.wait();
            tokio::pin!(wait);
            poll_fn(|cx| {
                assert!(wait.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            parked.send(()).unwrap();
            wait.await.unwrap()
        };
        (child, status)
    });
    // Let another task observe exit. This task stays runnable so paused time
    // cannot advance and accidentally hide a dependency on a polling timer.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while did_park.try_recv().is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the exit wait never parked"
        );
        tokio::task::yield_now().await;
    }
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    while !waiter.is_finished() {
        assert!(
            std::time::Instant::now() < deadline,
            "the exit wait did not wake without advancing Tokio time"
        );
        tokio::task::yield_now().await;
    }
    let (mut child, status) = waiter.await.unwrap();
    assert_eq!(Instant::now(), tick);
    assert_eq!(status.signal(), Some(15));
    assert_eq!(subc_os::child_exit::observe(pid).unwrap(), Some(status));
    child.reap().await.unwrap();
    assert!(subc_os::child_exit::observe(pid).is_err());
}

#[tokio::test]
async fn ordinary_unix_exit_terminates_helpers_before_leader_is_reaped() {
    let dir = ScratchDir::new("ordinary-unix-group");
    let spec = ModuleSpec {
        script: None,
        carry_containment: false,
        module_id: "group-exit".into(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            r#"
                /bin/sh -c 'trap "/usr/bin/touch \"$1/term\"; exit 0" TERM
                    /usr/bin/touch "$1/ready"
                    /bin/sleep 4' helper "$1" &
                while [ ! -e "$1/ready" ]; do /bin/sleep .01; done
                exit 33
            "#
            .into(),
            "leader".into(),
            dir.display().to_string(),
        ],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
    };
    let mut child = spawn_child(
        &spec,
        None,
        None,
        &Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default()))),
        None,
        &ChildRoster::default(),
        None,
        None,
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(child.wait().await.unwrap().code(), Some(33));
    child.finish_exit(false).await.unwrap();
    assert!(
        dir.join("term").exists(),
        "ordinary exit never sent TERM to the helper's group"
    );
}

#[tokio::test]
async fn ordinary_unix_restart_stop_and_shutdown_send_term_to_the_group() {
    let dir = ScratchDir::new("ordinary-unix-stop");
    let mut spec = ModuleSpec {
        script: None,
        carry_containment: false,
        module_id: "group-stop".into(),
        program: PathBuf::from("/bin/sh"),
        args: vec![
            "-c".into(),
            r#"
            /bin/sh -c 'trap "echo term >> \"$1/terms\"; exit 0" TERM
                echo ready >> "$1/ready"
                /bin/sleep 15' helper "$1" &
            wait
        "#
            .into(),
            "leader".into(),
            dir.display().to_string(),
        ],
        env: vec![],
        reserved: false,
        reserved_prefixes: vec![],
        protocol: ModuleProtocol::None,
        overlap: Default::default(),
    };
    let supervisor =
        Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
    let run = supervisor.runtime_config();
    let start = |spec: ModuleSpec| {
        let snapshot = Arc::new(Mutex::new(SupervisorSnapshot::starting()));
        let runtime = run.clone();
        let child = spawn_and_mark_running(&spec, &runtime, &snapshot).unwrap();
        supervisor.supervised_module(spec, runtime, snapshot, Some(child))
    };
    let wait_for = |path: PathBuf, expected| async move {
        timeout(Duration::from_secs(5), async {
            while std::fs::read_to_string(&path).map_or(0, |text| text.lines().count()) < expected {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
    };
    let module = start(spec.clone());
    wait_for(dir.join("ready"), 1).await;
    module.restart(None).await.unwrap();
    wait_for(dir.join("terms"), 1).await;
    wait_for(dir.join("ready"), 2).await;
    module.stop().await.unwrap();
    wait_for(dir.join("terms"), 2).await;

    spec.module_id = "group-shutdown".into();
    let _shutdown_module = start(spec);
    wait_for(dir.join("ready"), 3).await;
    supervisor
        .end_children_for_daemon_shutdown(false, std::future::pending())
        .await;
    assert_eq!(
        std::fs::read_to_string(dir.join("terms"))
            .unwrap()
            .lines()
            .count(),
        3
    );
}
