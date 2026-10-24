//! The private-daemon kit supplies explicit connection/configuration paths and
//! proves the answering pid before any write or rescan.

use std::{path::PathBuf, sync::Arc, time::Duration};

use ck_ui::live::{self, LiveOptions, NoService};
use ck_ui::{App, Request, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;

#[allow(dead_code)]
#[path = "../../subc-core/tests/common/kit.rs"]
mod kit;

fn binary(name: &str) -> PathBuf {
    let path = std::env::current_exe()
        .unwrap()
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    assert!(path.is_file(), "build the private-daemon kit binaries first: cargo build -p subc-core --bin ck --bin ck-subc ({})", path.display());
    cortexkit_test_support::ckdev_binary(path)
}
fn ckdev_subc() -> PathBuf {
    binary("ck-subc")
}
fn ckdev_ck() -> PathBuf {
    binary("ck")
}

fn send(
    app: &mut App,
    snap: &ck_ui::Snapshot,
    code: KeyCode,
    out: &tokio::sync::mpsc::UnboundedSender<Request>,
) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap);
    for request in app.requests.drain(..) {
        out.send(request).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn enter_writes_and_backs_up_the_nondefault_file_reported_by_the_private_daemon() {
    let daemon = kit::Kit::boot(
        "settings-save-path",
        &[(
            "broca",
            json!({
                "program": "/example/ck-broca", "enabled": false, "env": {}
            }),
        )],
    );
    daemon.assert_answering();
    let config = daemon.root().join("daemon/config/cortexkit/subc.jsonc");
    let original = std::fs::read_to_string(&config).unwrap();
    let live::Live {
        mut snapshots,
        requests,
        ..
    } = live::start(LiveOptions::new(
        Some(daemon.connection()),
        Arc::new(NoService),
    ));
    let mut app = App::new(Theme::test());
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snap = snapshots.borrow().clone();
            app.on_snapshot(&snap, ck_ui::time::Timestamp::system_now());
            if snap.settings.editable() && snap.settings.text.is_some() {
                break;
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let snap = snapshots.borrow().clone();
    assert_eq!(
        PathBuf::from(snap.settings.config_path.as_ref().unwrap()),
        config
    );
    for key in [KeyCode::Char('7'), KeyCode::Tab, KeyCode::Tab] {
        send(&mut app, &snap, key, &requests);
    }
    app.settings.row = app
        .settings
        .rows(false)
        .iter()
        .position(|row| row.label == "Program")
        .unwrap()
        + 1;
    for key in [
        KeyCode::Char('e'),
        KeyCode::Char('2'),
        KeyCode::Enter,
        KeyCode::Char('w'),
    ] {
        send(&mut app, &snap, key, &requests);
    }
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snap = snapshots.borrow().clone();
            app.on_snapshot(&snap, ck_ui::time::Timestamp::system_now());
            if app.settings.save.preview().is_some() {
                break;
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    daemon.assert_answering();
    let candidate = app.settings.candidate.clone();
    let snap = snapshots.borrow().clone();
    send(&mut app, &snap, KeyCode::Enter, &requests);
    assert_eq!(std::fs::read_to_string(&config).unwrap(), candidate);
    assert!(!app.settings.dirty());
    let backups: Vec<_> = std::fs::read_dir(config.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("subc.jsonc.bak-")
        })
        .collect();
    assert_eq!(backups.len(), 1);
    assert_eq!(std::fs::read_to_string(&backups[0]).unwrap(), original);
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let snap = snapshots.borrow().clone();
            app.on_snapshot(&snap, ck_ui::time::Timestamp::system_now());
            if !app.settings.save.applying {
                break;
            }
            snapshots.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(!app.settings.save.open, "{:?}", app.settings.save.notice);
    assert_eq!(app.tab, ck_ui::app::Tab::Modules);
}
