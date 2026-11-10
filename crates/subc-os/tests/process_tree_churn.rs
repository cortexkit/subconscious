#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

struct OwnedParent(std::process::Child);

impl Drop for OwnedParent {
    fn drop(&mut self) {
        // The fixture exits on its own bounded timer, including after an
        // assertion failure in the reader. Reap only the process we spawned.
        let _ = self.0.wait();
    }
}

#[test]
fn process_tree_child_churn_fixture() {
    if std::env::var_os("SUBC_PROCESS_TREE_CHURN_FIXTURE").is_none() {
        return;
    }
    let until = Instant::now() + Duration::from_secs(4);
    let workers: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(move || {
                let mut count = 0;
                while Instant::now() < until {
                    let mut child = Command::new("/usr/bin/true").spawn().unwrap();
                    assert!(child.wait().unwrap().success());
                    count += 1;
                }
                count
            })
        })
        .collect();
    assert!(
        workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .sum::<u64>()
            > 0
    );
}

#[test]
fn process_tree_survives_owned_short_lived_children() {
    let mut parent = OwnedParent(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "process_tree_child_churn_fixture"])
            .env("SUBC_PROCESS_TREE_CHURN_FIXTURE", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let pid = parent.0.id();
    let start = subc_os::start_time(pid).unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    let mut observations = 0;
    while Instant::now() < until {
        let table = subc_os::ProcessTable::read().unwrap();
        assert!(
            table.tree(pid, start, false).is_some(),
            "owned children exiting must not void the live parent"
        );
        observations += 1;
    }
    assert!(parent.0.wait().unwrap().success());
    assert!(observations > 0);
}
