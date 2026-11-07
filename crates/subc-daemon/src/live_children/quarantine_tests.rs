use super::*;
use cortexkit_test_support::ScratchDir;

#[cfg(unix)]
async fn confirmed_live(module: &crate::supervise::SupervisedModule) {
    // On macOS, wait for the module's normal exec confirmation before it is live.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !module.status().unwrap().live {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the owned module must complete its normal exec confirmation");
}

#[tokio::test]
async fn unusable_records_are_quarantined_without_resetting_the_evidence() {
    for (label, bytes) in [
        ("malformed", b"{not-json".as_slice()),
        (
            "unsupported",
            b"{\"version\":4294967295,\"children\":[]}".as_slice(),
        ),
        ("unreadable", b"directory evidence".as_slice()),
    ] {
        let dir = ScratchDir::new(label);
        let path = record_path(&dir);
        if label == "unreadable" {
            fs::create_dir(&path).unwrap();
            fs::write(path.join("evidence"), bytes).unwrap();
        } else {
            fs::write(&path, bytes).unwrap();
        }
        let owner = RunDirLock::acquire(&path).unwrap();
        let decisions = sweep_orphans_with(
            &owner,
            &AdoptedPids::none(),
            SweepBounds::default(),
            |_, _, _| async { panic!("unusable identities must never reach the signal sweep") },
        )
        .await;
        assert!(decisions.is_empty());
        assert!(
            !path.exists(),
            "quarantine must rename, never reset the record"
        );
        let saved: Vec<_> = fs::read_dir(&*dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".quarantined.")
            })
            .collect();
        assert_eq!(saved.len(), 1, "keep one durable quarantine marker");
        let evidence = if label == "unreadable" {
            fs::read(saved[0].join("evidence")).unwrap()
        } else {
            fs::read(&saved[0]).unwrap()
        };
        assert_eq!(
            evidence, bytes,
            "quarantine must preserve the original evidence"
        );
        assert!(quarantine_block(&path)
            .unwrap()
            .contains("offline clearance"));
        assert!(
            read_record(&path).unwrap().is_empty(),
            "only ENOENT itself is empty"
        );
    }
}

#[cfg(unix)]
async fn acquire_released_run_directory(path: &Path) -> RunDirLock {
    // Other test-harness threads can fork while the previous owner holds the
    // lock. Their inherited descriptors close on exec, not when this test
    // drops its owner. Successful acquisition, never a fixed delay, proves release.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match RunDirLock::acquire(path) {
                Ok(owner) => break owner,
                Err(crate::bootstrap::BootstrapError::RunDirBusy { .. }) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => panic!("cannot acquire the released run directory: {error}"),
            }
        }
    })
    .await
    .expect("the previous daemon's run-directory lock must actually be released")
}

#[cfg(unix)]
#[tokio::test]
async fn run_directory_reacquisition_waits_for_actual_release() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    let dir = ScratchDir::new("quarantine-lock-release");
    let path = record_path(&dir);
    let owner = RunDirLock::acquire(&path).unwrap();
    let mut reacquire = std::pin::pin!(acquire_released_run_directory(&path));
    assert!(
        matches!(
            reacquire
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ),
        "a genuinely held lock must keep reacquisition pending"
    );
    drop(owner);
    let acquired = reacquire.await;
    assert!(
        matches!(
            RunDirLock::acquire(&path),
            Err(crate::bootstrap::BootstrapError::RunDirBusy { .. })
        ),
        "the returned owner must hold the actual kernel lock"
    );
    drop(acquired);
}

#[cfg(unix)]
#[tokio::test]
async fn quarantine_blocks_only_carried_modules_across_repeated_daemon_startup() {
    use crate::supervise::{ModuleOverlap, ModuleSpec, RestartPolicy, Supervisor};
    use std::sync::Arc;

    let dir = ScratchDir::new("quarantine-survives-restart");
    let run = dir.join("run");
    fs::create_dir(&run).unwrap();
    let path = record_path(&run);
    fs::write(&path, b"lost recovery identity").unwrap();
    let spec = |carry_containment| ModuleSpec {
        module_id: if carry_containment {
            "carried"
        } else {
            "ordinary"
        }
        .into(),
        carry_containment,
        program: PathBuf::from("/bin/sleep"),
        args: vec!["60".into()],
        env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
            .into_iter()
            .map(|key| (key.into(), dir.display().to_string()))
            .collect(),
        protocol: ModuleProtocol::None,
        reserved: false,
        reserved_prefixes: Vec::new(),
        overlap: ModuleOverlap::Exclusive,
    };

    // Recreate the actual startup lock, sweep and supervisor for each boot.
    // Starting ordinary modules also replaces live-children.json between boots, so
    // only the quarantined evidence can preserve the unknown-identity block.
    for _ in 0..3 {
        let owner = acquire_released_run_directory(&path).await;
        assert!(
            sweep_orphans(&owner, &AdoptedPids::none(), SweepBounds::default())
                .await
                .is_empty()
        );
        let supervisor = Supervisor::new_for_test(
            Arc::new(crate::registry::Registry::default()),
            RestartPolicy::default(),
        )
        .with_live_children_record(path.clone());
        let waiting = supervisor.supervise_configured(spec(true), true).unwrap();
        let ordinary = supervisor.supervise_configured(spec(false), true).unwrap();
        confirmed_live(&ordinary).await;
        let status = waiting.status().unwrap();
        assert!(!status.process_alive && status.pid.is_none());
        assert_eq!(
            status.spawn_generation, 0,
            "unknown identity forbids the first exec"
        );
        assert_eq!(
            status.waiting_for_previous_pid, None,
            "do not invent a lost PID"
        );
        assert!(status
            .waiting_for_previous_detail
            .as_ref()
            .unwrap()
            .contains("quarantined recovery record"));
        assert!(status.health.detail.unwrap().contains("offline clearance"));
        assert!(
            ordinary.status().unwrap().live,
            "non-opted-in modules remain unaffected"
        );
        assert!(ordinary
            .status()
            .unwrap()
            .waiting_for_previous_detail
            .is_none());
        for refused in [
            waiting.set_enabled(true).await.map(|_| ()),
            waiting.restart(None).await,
            waiting.reload().await,
        ] {
            assert!(refused
                .unwrap_err()
                .to_string()
                .contains("offline clearance"));
        }
        assert!(supervisor.spawn(spec(true)).is_err());
        ordinary.stop().await.unwrap();
        waiting.stop().await.unwrap();
        assert!(read_record(&path).unwrap().is_empty());
    }

    // Offline clearance moves the evidence out of the run directory; there is
    // deliberately no online reset operation that can discard unknown holders.
    let marker = fs::read_dir(&run)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .contains(".quarantined.")
        })
        .unwrap();
    fs::rename(marker, dir.join("offline-evidence")).unwrap();
    let owner = acquire_released_run_directory(&path).await;
    assert!(
        sweep_orphans(&owner, &AdoptedPids::none(), SweepBounds::default())
            .await
            .is_empty()
    );
    let supervisor = Supervisor::new_for_test(
        Arc::new(crate::registry::Registry::default()),
        RestartPolicy::default(),
    )
    .with_live_children_record(path);
    let running = supervisor.supervise_configured(spec(true), true).unwrap();
    confirmed_live(&running).await;
    assert!(
        running.status().unwrap().live,
        "offline clearance permits a fresh container"
    );
    running.stop().await.unwrap();
}
