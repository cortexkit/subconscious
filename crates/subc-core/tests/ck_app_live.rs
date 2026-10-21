//! The ck app's live layer against a real daemon: a private daemon with two
//! test modules, read through `ck_ui::live` exactly as `ck` opens the app.
//!
//! It checks that the snapshot lists the modules with their pids and CPU
//! readings, that a restart sent through the app's key path shows up as a
//! spawn event and a new pid, and that stopping the daemon flips the app to
//! its daemon-down screen.

#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::Arc;
use std::time::Duration;

use ck_ui::app::{App, Request, Tab};
use ck_ui::live::{self, LiveOptions, NoService};
use ck_ui::model::{Daemon, ProcessEventKind, Snapshot};
use ck_ui::Theme;
use cortexkit_test_support::{dev_command, ScratchDir};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use tokio::sync::watch;

mod common;

const MODULES: [&str; 2] = ["app-alpha", "app-beta"];

struct PrivateDaemon {
    root: ScratchDir,
    child: Child,
}

impl Drop for PrivateDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        for id in MODULES {
            if let Some(pid) = fs::read_to_string(self.root.join(format!("{id}.pid")))
                .ok()
                .and_then(|p| p.trim().parse::<i32>().ok())
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

/// Starts the shipped daemon with every per-user location inside a scratch
/// directory. The locations are set on the daemon's command only, so this
/// test process and any other test keep their own.
fn boot() -> PrivateDaemon {
    let root = ScratchDir::new("ck-app-live");
    for dir in ["config/cortexkit", "runtime", "data/cortexkit/run"] {
        fs::create_dir_all(root.join(dir)).unwrap();
    }
    let mut modules = serde_json::Map::new();
    for id in MODULES {
        modules.insert(
            id.to_string(),
            json!({
                "program": env!("CARGO_BIN_EXE_fake-aft-stub"),
                "env": {
                    "FAKE_AFT_MODULE_ID": id,
                    "FAKE_AFT_PID_PATH": root.join(format!("{id}.pid")),
                }
            }),
        );
    }
    fs::write(
        root.join("config/cortexkit/subc.jsonc"),
        serde_json::to_vec(&json!({ "version": 1, "modules": modules })).unwrap(),
    )
    .unwrap();
    let child = dev_command(common::ckdev_subc())
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("SUBC_PORT", "0")
        .env("SUBC_CGROUP_PLACEMENT", "disabled")
        .env_remove("CK_LOG")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    PrivateDaemon { root, child }
}

impl PrivateDaemon {
    fn connection(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }
}

/// Waits until the snapshot satisfies `ok`, failing with `what` and the last
/// snapshot's modules after `secs`.
async fn wait_for(
    rx: &mut watch::Receiver<Snapshot>,
    secs: u64,
    what: &str,
    ok: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    let found = tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            {
                let snap = rx.borrow_and_update();
                if ok(&snap) {
                    return snap.clone();
                }
            }
            rx.changed().await.expect("the live layer keeps running");
        }
    })
    .await;
    match found {
        Ok(snap) => snap,
        Err(_) => {
            let snap = rx.borrow().clone();
            panic!(
                "{what} within {secs} s; daemon {:?}; modules {:?}; result {:?}",
                snap.daemon.data,
                snap.modules
                    .data
                    .iter()
                    .map(|m| (&m.id, m.status, m.pid, m.cpu_percent, &m.action))
                    .collect::<Vec<_>>(),
                snap.last_result
            )
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_live_layer_follows_a_real_daemon_through_a_restart_and_its_stop() {
    let mut daemon = boot();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while !daemon.connection().exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the daemon never wrote its connection file"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    let live = live::start(LiveOptions::new(
        Some(daemon.connection()),
        Arc::new(NoService),
    ));
    let mut rx = live.snapshots.clone();

    // Both modules listed, each with a pid and a CPU reading, which takes
    // two roster readings of the same process.
    let snap = wait_for(&mut rx, 60, "both modules with pids and CPU", |s| {
        matches!(s.daemon.data, Daemon::Running(_))
            && MODULES.iter().all(|id| {
                s.modules
                    .data
                    .iter()
                    .any(|m| m.id == *id && m.pid.is_some() && m.cpu_percent.is_some())
            })
    })
    .await;
    let alpha = snap
        .modules
        .data
        .iter()
        .position(|m| m.id == MODULES[0])
        .unwrap();
    let old_pid = snap.modules.data[alpha].pid.unwrap();
    assert!(snap.modules.data[alpha].memory_mb.is_some());

    // Restart through the app's own key path: select the module, press r,
    // and answer the confirmation if its health is in doubt.
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    app.mod_sel = alpha;
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &snap);
    if app.confirm.is_some() {
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &snap);
    }
    let sent: Vec<Request> = std::mem::take(&mut app.requests);
    assert_eq!(
        sent,
        [Request::Act {
            kind: ck_ui::model::ActionKind::Restart,
            module: MODULES[0].to_string(),
        }]
    );
    for request in sent {
        live.requests.send(request).unwrap();
    }

    let snap = wait_for(&mut rx, 60, "a new pid and a start event", |s| {
        let new_pid = s
            .modules
            .data
            .iter()
            .find(|m| m.id == MODULES[0])
            .and_then(|m| m.pid)
            .is_some_and(|p| p != old_pid);
        let started = s.process_events.data.iter().any(|e| {
            e.module == MODULES[0]
                && e.kind == ProcessEventKind::Started
                && !e.what.contains(&old_pid.to_string())
        });
        new_pid && started
    })
    .await;
    let new_pid = snap
        .modules
        .data
        .iter()
        .find(|m| m.id == MODULES[0])
        .and_then(|m| m.pid)
        .unwrap();
    // The result reports what happened, with the new pid.
    let snap = wait_for(&mut rx, 60, "the restart's result", |s| {
        s.last_result.is_some()
    })
    .await;
    let result = snap.last_result.unwrap();
    assert!(result.ok, "{result:?}");
    assert!(
        result.text.contains(&format!("pid {new_pid}")),
        "{result:?}"
    );

    // Stopping the daemon flips the app to the down screen.
    daemon.child.kill().unwrap();
    daemon.child.wait().unwrap();
    wait_for(&mut rx, 30, "the daemon-down screen", |s| {
        matches!(s.daemon.data, Daemon::Down(_))
    })
    .await;
}
