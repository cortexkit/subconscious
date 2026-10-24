//! Saving and applying through the same keyboard/request boundary as the live app.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use ck_ui::app::Tab;
use ck_ui::live::{
    apply,
    settings::{ApplyOutcome, ApplyReply, PreviewOutcome, PreviewReply, SettingsReading},
};
use ck_ui::model::Serves;
use ck_ui::{App, Request, Snapshot, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use futures_util::future::BoxFuture;
use serde_json::json;
use subc_control::{RescanExpect, SupervisorRescanResult};

struct Files(cortexkit_test_support::ScratchDir);
impl Files {
    fn new() -> Self {
        Self(cortexkit_test_support::ScratchDir::new("settings-save"))
    }
    fn path(&self) -> PathBuf {
        self.0.join("subc.jsonc")
    }
    fn backups(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.0.path())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.file_name().unwrap().to_string_lossy().contains(".bak-"))
            .collect()
    }
}

const TEXT: &str = "{\n  // retained\n  \"modules\": {\n    \"broca\": {\"program\": \"/Users/alex/bin/broca\", \"env\": {\"A\": \"old\", \"B\": \"old\"}},\n    \"engram\": {\"program\": \"/example/engram\"}\n  }\n}\n";

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap);
}

fn session(path: &Path) -> (App, Snapshot) {
    std::fs::write(path, TEXT).unwrap();
    let mut snap = Snapshot::fixture();
    snap.serves = Serves::LIVE;
    snap.host.data.home = Some("/Users/alex".into());
    snap.settings = SettingsReading {
        config_path: Some(path.to_string_lossy().into_owned()),
        capable: true,
        text: Some(TEXT.into()),
        error: None,
    };
    let mut app = App::new(Theme::test());
    app.on_snapshot(&snap, Snapshot::fixture_now());
    press(&mut app, &snap, KeyCode::Char('7'));
    press(&mut app, &snap, KeyCode::Tab);
    press(&mut app, &snap, KeyCode::Tab);
    app.settings.row = app
        .settings
        .rows(false)
        .iter()
        .position(|r| r.label == "Program")
        .unwrap()
        + 1;
    (app, snap)
}

fn edit(app: &mut App, snap: &Snapshot) {
    press(app, snap, KeyCode::Char('e'));
    // The editor starts on the stored full path, never on the abbreviated row.
    assert_eq!(
        app.settings.field.as_ref().unwrap().text,
        "/Users/alex/bin/broca"
    );
    press(app, snap, KeyCode::Char('2'));
    press(app, snap, KeyCode::Enter);
    press(app, snap, KeyCode::Char('w'));
}

fn result(preview: bool, ids: &[&str]) -> SupervisorRescanResult {
    serde_json::from_value(json!({
        "added": [], "removed": [], "changed_pending_reload": if preview { vec![] } else { ids.to_vec() }, "unchanged": 1,
        "preview": preview,
        "module_actions": ids.iter().map(|id| json!({"module_id": id, "action": "restart", "changed": ["program"], "drain_timeout_ms": 30000, "busy_gauges": []})).collect::<Vec<_>>(),
        "earlier_pending": ["engram"],
        "expect": {"config_sha256": "ab".repeat(32), "staged_generation": {"incarnation": "test", "counter": 1}}
    })).unwrap()
}

fn preview(app: &mut App, snap: &mut Snapshot, ids: &[&str]) {
    let ticket = app
        .requests
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::RescanPreview { ticket, .. } => Some(*ticket),
            _ => None,
        })
        .unwrap();
    let seq = snap.previews.len() as u64 + 1;
    snap.previews.push((
        seq,
        PreviewReply {
            ticket,
            outcome: PreviewOutcome::Ready(Box::new(result(true, ids))),
        },
    ));
    app.on_snapshot(snap, Snapshot::fixture_now());
}

fn applies(app: &App) -> usize {
    app.requests
        .iter()
        .filter(|r| matches!(r, Request::RescanApply { .. }))
        .count()
}

fn ack(app: &mut App, snap: &mut Snapshot, outcome: ApplyOutcome) {
    let (revision, restarts) = app
        .requests
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::RescanApply {
                revision, restarts, ..
            } => Some((*revision, restarts.clone())),
            _ => None,
        })
        .unwrap();
    snap.applies.push((
        snap.applies.len() as u64 + 1,
        ApplyReply {
            revision,
            restarts,
            outcome,
        },
    ));
    app.on_snapshot(snap, Snapshot::fixture_now());
}

#[test]
fn save_only_writes_full_paths_and_sets_a_new_baseline_without_a_rescan() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    let candidate = app.settings.candidate.clone();
    press(&mut app, &snap, KeyCode::Char('s'));
    assert_eq!(std::fs::read_to_string(files.path()).unwrap(), candidate);
    assert!(candidate.contains("/Users/alex/bin/broca2"));
    assert!(!candidate.contains('~'));
    assert!(!app.settings.dirty());
    assert_eq!(app.settings.original, candidate);
    assert!(!app.settings.save.open);
    assert_eq!(applies(&app), 0);
    assert_eq!(files.backups().len(), 1);
    assert_eq!(std::fs::read_to_string(&files.backups()[0]).unwrap(), TEXT);
}

#[test]
fn enter_writes_once_waits_for_ack_and_selects_the_first_restart() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    press(&mut app, &snap, KeyCode::Enter);
    assert!(!app.settings.dirty());
    assert!(app.settings.save.open);
    assert!(app.settings.save.applying);
    assert_eq!(app.tab, Tab::Settings);
    for key in [
        KeyCode::Enter,
        KeyCode::Enter,
        KeyCode::Char('s'),
        KeyCode::Esc,
    ] {
        press(&mut app, &snap, key);
    }
    assert_eq!(applies(&app), 1);
    assert_eq!(files.backups().len(), 1);
    ack(
        &mut app,
        &mut snap,
        ApplyOutcome::Ready(Box::new(result(false, &["broca"]))),
    );
    assert!(!app.settings.save.open);
    assert_eq!(app.tab, Tab::Modules);
    assert_eq!(snap.modules.data[app.mod_sel].id, "broca");
}

#[test]
fn write_failure_keeps_edits_and_sends_no_rescan() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    std::fs::remove_file(files.path()).unwrap();
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(applies(&app), 0);
    assert!(app.settings.dirty());
    assert!(app.settings.save.open);
    assert!(app
        .settings
        .save
        .notice
        .as_ref()
        .unwrap()
        .starts_with("Couldn't save:"));
}

#[test]
fn a_refused_apply_is_explicitly_retried_without_writing_again() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    press(&mut app, &snap, KeyCode::Enter);
    ack(
        &mut app,
        &mut snap,
        ApplyOutcome::Refused("operator_declined".into()),
    );
    assert_eq!(
        app.settings.save.notice.as_deref(),
        Some("Saved, not applied: operator_declined")
    );
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(applies(&app), 2);
    assert_eq!(files.backups().len(), 1);
    assert!(!app.settings.dirty());
}

#[test]
fn a_lost_ack_cannot_be_replayed_even_by_enter_or_a_reconnect() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    press(&mut app, &snap, KeyCode::Enter);
    snap.daemon_connection += 1;
    app.on_snapshot(&snap, Snapshot::fixture_now());
    ack(&mut app, &mut snap, ApplyOutcome::Unknown);
    assert_eq!(
        app.settings.save.notice.as_deref(),
        Some("Saved; whether it applied is unknown · check ck module list")
    );
    press(&mut app, &snap, KeyCode::Enter);
    press(&mut app, &snap, KeyCode::Char('s'));
    assert_eq!(applies(&app), 1);
    assert_eq!(files.backups().len(), 1);
}

#[test]
fn a_changed_apply_precondition_previews_the_file_on_disk_for_fresh_review() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    press(&mut app, &snap, KeyCode::Enter);
    let disk = TEXT.replace("/example/engram", "/example/new-engram");
    std::fs::write(files.path(), &disk).unwrap();
    ack(
        &mut app,
        &mut snap,
        ApplyOutcome::Refused("config_changed_since_preview".into()),
    );
    assert!(app
        .settings
        .save
        .notice
        .as_ref()
        .unwrap()
        .contains("nothing was applied · ⏎ checks again"));
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(applies(&app), 1);
    assert_eq!(app.settings.candidate, disk);
    assert!(app.settings.save.pending.is_some());
    let Request::RescanPreview { candidate, .. } = app.requests.last().unwrap() else {
        panic!("fresh preview required")
    };
    assert_eq!(candidate, &disk);
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(applies(&app), 1);
}

#[test]
fn changed_bytes_are_rebased_and_all_conflicts_need_keep_or_drop_then_a_new_preview() {
    for resolution in [KeyCode::Char('k'), KeyCode::Char('d')] {
        let files = Files::new();
        let (mut app, mut snap) = session(&files.path());
        edit(&mut app, &snap);
        preview(&mut app, &mut snap, &["broca"]);
        let disk = TEXT
            .replace("/Users/alex/bin/broca", "/example/theirs")
            .replace("// retained", "// another writer's comment");
        std::fs::write(files.path(), &disk).unwrap();
        press(&mut app, &snap, KeyCode::Enter);
        assert_eq!(applies(&app), 0);
        assert_eq!(files.backups().len(), 0);
        assert_eq!(app.settings.save.conflicts.len(), 1);
        assert!(app.settings.candidate.contains("another writer's comment"));
        // Even a ready preview cannot authorize an unresolved conflict.
        preview(&mut app, &mut snap, &["broca"]);
        press(&mut app, &snap, KeyCode::Enter);
        press(&mut app, &snap, KeyCode::Char('s'));
        assert_eq!(std::fs::read_to_string(files.path()).unwrap(), disk);
        assert_eq!(applies(&app), 0);
        let old_revision = app.settings.save.revision;
        press(&mut app, &snap, resolution);
        assert!(app.settings.save.conflicts.is_empty());
        assert!(app.settings.save.revision > old_revision);
        if resolution == KeyCode::Char('k') {
            assert!(app.settings.save.pending.is_some());
            press(&mut app, &snap, KeyCode::Enter);
            assert_eq!(applies(&app), 0);
            preview(&mut app, &mut snap, &["broca"]);
            press(&mut app, &snap, KeyCode::Char('s'));
            assert!(std::fs::read_to_string(files.path())
                .unwrap()
                .contains("/Users/alex/bin/broca2"));
        } else {
            assert_eq!(app.settings.candidate, disk);
            assert!(!app.settings.dirty());
            assert!(
                app.settings.save.pending.is_some(),
                "dropping the final edit still previews the resolved file"
            );
        }
    }
}

#[test]
fn unrelated_disk_changes_are_kept_and_require_a_second_review() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    let disk = TEXT.replace("/example/engram", "/example/other-engram");
    std::fs::write(files.path(), &disk).unwrap();
    press(&mut app, &snap, KeyCode::Enter);
    assert!(app.settings.save.conflicts.is_empty());
    assert_eq!(app.settings.original, disk);
    assert!(app.settings.candidate.contains("/example/other-engram"));
    assert!(app.settings.candidate.contains("/Users/alex/bin/broca2"));
    assert!(app.settings.save.pending.is_some());
    assert_eq!(applies(&app), 0);
    assert_eq!(std::fs::read_to_string(files.path()).unwrap(), disk);
}

#[test]
fn backups_are_private_unique_retained_and_the_replacement_keeps_the_mode() {
    let files = Files::new();
    std::fs::write(files.path(), TEXT).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(files.path(), std::fs::Permissions::from_mode(0o640)).unwrap();
    }
    #[cfg(unix)]
    let original_inode = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(files.path()).unwrap().ino()
    };
    let one =
        ck_ui::save_file::write_at(&files.path(), TEXT, "second", "20260102T030405Z").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        assert_ne!(
            std::fs::metadata(files.path()).unwrap().ino(),
            original_inode,
            "the file is replaced, not overwritten in place"
        );
    }
    let two =
        ck_ui::save_file::write_at(&files.path(), "second", "third", "20260102T030405Z").unwrap();
    let three =
        ck_ui::save_file::write_at(&files.path(), "third", "fourth", "20260102T030405Z").unwrap();
    assert_eq!(one.file_name().unwrap(), "subc.jsonc.bak-20260102T030405Z");
    assert_eq!(
        two.file_name().unwrap(),
        "subc.jsonc.bak-20260102T030405Z-2"
    );
    assert_eq!(
        three.file_name().unwrap(),
        "subc.jsonc.bak-20260102T030405Z-3"
    );
    assert_eq!(std::fs::read_to_string(one).unwrap(), TEXT);
    assert_eq!(std::fs::read_to_string(two).unwrap(), "second");
    assert_eq!(std::fs::read_to_string(three).unwrap(), "third");
    assert_eq!(
        std::fs::read_dir(files.0.path()).unwrap().count(),
        4,
        "no temp files remain"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        assert_eq!(
            std::fs::metadata(files.path())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
        for path in files.backups() {
            let metadata = std::fs::metadata(path).unwrap();
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
            assert_ne!(
                metadata.ino(),
                std::fs::metadata(files.path()).unwrap().ino(),
                "backup is not the replacement inode"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn saving_a_symlink_preserves_the_link_and_places_backup_beside_its_target() {
    let files = Files::new();
    let target_dir = files.0.join("target");
    std::fs::create_dir(&target_dir).unwrap();
    let target = target_dir.join("subc.jsonc");
    std::fs::write(&target, TEXT).unwrap();
    std::os::unix::fs::symlink(&target, files.path()).unwrap();
    ck_ui::save_file::write_at(&files.path(), TEXT, "new", "20260102T030405Z").unwrap();
    assert!(std::fs::symlink_metadata(files.path())
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read_link(files.path()).unwrap(), target);
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    assert_eq!(
        std::fs::read_to_string(target_dir.join("subc.jsonc.bak-20260102T030405Z")).unwrap(),
        TEXT
    );
    assert!(files.backups().is_empty());
}

struct Script {
    rescan: ApplyOutcome,
    failure: Option<String>,
    sent: Mutex<Vec<String>>,
    replies: Mutex<Vec<ApplyReply>>,
    lines: Mutex<Vec<String>>,
}
impl Script {
    fn new(ids: &[&str], failure: Option<&str>) -> Self {
        Self {
            rescan: ApplyOutcome::Ready(Box::new(result(false, ids))),
            failure: failure.map(str::to_string),
            sent: Mutex::new(Vec::new()),
            replies: Mutex::new(Vec::new()),
            lines: Mutex::new(Vec::new()),
        }
    }
}
impl apply::Service for Script {
    fn rescan(&self, _expect: RescanExpect, timeout: Duration) -> BoxFuture<'_, ApplyOutcome> {
        assert_eq!(timeout, Duration::from_secs(10));
        self.sent.lock().unwrap().push("rescan".into());
        Box::pin(async { self.rescan.clone() })
    }
    fn acknowledged(&self, reply: ApplyReply) {
        self.replies.lock().unwrap().push(reply);
    }
    fn restart(&self, module: String, _notes: String) -> BoxFuture<'_, apply::RestartResult> {
        Box::pin(async move {
            assert_eq!(
                self.replies.lock().unwrap().len(),
                1,
                "ack before any restart"
            );
            self.sent.lock().unwrap().push(format!("restart {module}"));
            tokio::task::yield_now().await;
            if self.failure.as_deref() == Some(&module) {
                apply::RestartResult {
                    healthy: false,
                    state: "degraded".into(),
                    detail: format!("{module} degraded"),
                }
            } else {
                apply::RestartResult {
                    healthy: true,
                    state: "healthy".into(),
                    detail: format!("{module} restarted · pid 123 · healthy after 0.1 s"),
                }
            }
        })
    }
    fn result(&self, _ok: bool, text: String) {
        self.lines.lock().unwrap().push(text);
    }
}

#[tokio::test]
async fn scripted_e_edit_w_enter_sends_one_rescan_and_restarts_only_y_not_pending_x_or_reply_extras(
) {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    preview(&mut app, &mut snap, &["broca"]);
    assert_eq!(
        app.settings.save.preview().unwrap().earlier_pending,
        ["engram"]
    );
    press(&mut app, &snap, KeyCode::Enter);
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(applies(&app), 1);
    let script = Script::new(&["broca", "extra"], None);
    for request in app.requests {
        if let Request::RescanApply {
            revision,
            expect,
            restarts,
        } = request
        {
            apply::run(&script, revision, expect, restarts, &[]).await;
        }
    }
    assert_eq!(*script.sent.lock().unwrap(), ["rescan", "restart broca"]);
    assert!(script
        .lines
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("Not restarted (not in the preview): extra"));
    assert!(script
        .lines
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .contains("pid 123 · healthy after 0.1 s"));
}

#[tokio::test]
async fn a_degraded_first_restart_stops_the_second_in_preview_order() {
    let script = Script::new(&["engram", "broca"], Some("engram"));
    let expect = result(true, &[]).expect.unwrap();
    apply::run(
        &script,
        1,
        expect,
        vec!["engram".into(), "broca".into()],
        &[],
    )
    .await;
    assert_eq!(*script.sent.lock().unwrap(), ["rescan", "restart engram"]);
    assert_eq!(
        script.lines.lock().unwrap().last().unwrap(),
        "✗ engram degraded · not restarted: broca · engram degraded"
    );
}

#[test]
fn the_fleet_timeout_sums_typed_budgets_and_fallback_with_a_control_minimum() {
    use ck_ui::live::settings::fleet_drain_response_timeout;
    use subc_control::{SupervisorEntry, SupervisorHealthStatus};
    let mut first =
        SupervisorEntry::new("first", "running", true, true, SupervisorHealthStatus::Ok);
    let mut second =
        SupervisorEntry::new("second", "running", true, true, SupervisorHealthStatus::Ok);
    first.drain_timeout_ms = Some(45_000);
    second.drain_timeout_ms = Some(1_000);
    assert_eq!(fleet_drain_response_timeout(&[]), Duration::from_secs(10));
    assert_eq!(
        fleet_drain_response_timeout(&[first.clone(), second.clone()]),
        Duration::from_secs(48)
    );
    second.drain_timeout_ms = None;
    assert_eq!(
        fleet_drain_response_timeout(&[first, second]),
        Duration::from_secs(77)
    );
}

#[tokio::test]
async fn refused_or_unknown_rescans_never_restart_or_replay() {
    for outcome in [
        ApplyOutcome::Refused("operator_declined".into()),
        ApplyOutcome::Unknown,
    ] {
        let mut script = Script::new(&["broca"], None);
        script.rescan = outcome;
        apply::run(
            &script,
            1,
            result(true, &[]).expect.unwrap(),
            vec!["broca".into()],
            &[],
        )
        .await;
        assert_eq!(*script.sent.lock().unwrap(), ["rescan"]);
        assert!(script.lines.lock().unwrap().is_empty());
    }
}

#[test]
fn rebasing_arguments_and_new_environment_members_keeps_other_values_and_comments() {
    let original = "{\"modules\":{\"broca\":{\"args\":[\"a\", /* keep */ \"b\"]}}}";
    let candidate = original.replace("\"a\"", "\"mine\"");
    let disk = original.replace("\"b\"", "\"theirs\"");
    let (text, conflicts) = ck_ui::save_file::rebase(original, &candidate, &disk).unwrap();
    assert!(conflicts.is_empty());
    assert_eq!(
        text,
        "{\"modules\":{\"broca\":{\"args\":[\"mine\", /* keep */ \"theirs\"]}}}"
    );
    let original = "{\"modules\":{\"broca\":{}}}";
    let candidate = "{\"modules\":{\"broca\":{\"env\":{\"A\":\"mine\"}}}}";
    let disk = "{\"modules\":{\"broca\":{\"env\":{\"B\":\"theirs\"}}}}";
    let (text, conflicts) = ck_ui::save_file::rebase(original, candidate, disk).unwrap();
    assert!(conflicts.is_empty());
    assert!(text.contains("\"B\":\"theirs\""));
    assert!(text.contains("\"mine\""));
}

#[test]
fn dropping_an_edit_inside_a_deleted_parent_restores_its_absence() {
    let original = "{\"modules\":{\"broca\":{\"program\":\"old\"}}}";
    let candidate = original.replace("old", "mine");
    let disk = "{\"modules\":{}}";
    let (mut text, conflicts) = ck_ui::save_file::rebase(original, &candidate, disk).unwrap();
    assert_eq!(conflicts.len(), 1);
    ck_ui::save_file::replace(&mut text, &conflicts[0].path, conflicts[0].disk.as_ref()).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        json!({"modules":{}})
    );
}

#[test]
fn resolving_one_of_two_conflicts_still_blocks_both_save_keys() {
    let files = Files::new();
    let (mut app, mut snap) = session(&files.path());
    edit(&mut app, &snap);
    press(&mut app, &snap, KeyCode::Esc);
    app.settings.row = app
        .settings
        .rows(false)
        .iter()
        .position(|row| {
            row.target == Some(ck_ui::screens::settings::Target::Environment("A".into()))
        })
        .unwrap()
        + 1;
    for key in [
        KeyCode::Char('e'),
        KeyCode::Char('2'),
        KeyCode::Enter,
        KeyCode::Char('w'),
    ] {
        press(&mut app, &snap, key);
    }
    preview(&mut app, &mut snap, &["broca"]);
    let disk = TEXT
        .replace("/Users/alex/bin/broca", "/example/theirs")
        .replace("\"A\": \"old\"", "\"A\": \"theirs\"");
    std::fs::write(files.path(), &disk).unwrap();
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(app.settings.save.conflicts.len(), 2);
    press(&mut app, &snap, KeyCode::Char('k'));
    preview(&mut app, &mut snap, &["broca"]);
    assert_eq!(app.settings.save.conflicts.len(), 1);
    for key in [KeyCode::Enter, KeyCode::Char('s')] {
        press(&mut app, &snap, key);
    }
    assert_eq!(applies(&app), 0);
    assert_eq!(std::fs::read_to_string(files.path()).unwrap(), disk);
    press(&mut app, &snap, KeyCode::Char('d'));
    assert!(app.settings.save.conflicts.is_empty());
    assert!(app.settings.save.pending.is_some());
    assert_eq!(std::fs::read_to_string(files.path()).unwrap(), disk);
}

#[test]
fn sequence_progress_names_the_active_module_and_keeps_rescan_diagnostics() {
    use ck_ui::model::{ActionKind, ActionPhase, ModuleAction};
    let action = ModuleAction {
        kind: ActionKind::Restart,
        phase: ActionPhase::Draining,
        started_at: Snapshot::fixture_now(),
    };
    let line = apply::progress_line(
        "engram",
        &action,
        "storage changed; it applies only when the daemon restarts",
    );
    assert!(line.starts_with("engram restart · draining"), "{line}");
    assert!(line.contains("storage changed; it applies only when the daemon restarts"));
}
