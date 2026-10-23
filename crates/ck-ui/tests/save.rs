//! The Save overlay: the exact diff, scrolled by keyboard; preview revisions
//! and which replies count; what saving does, as the daemon's preview says;
//! and when `⏎` and `s` may ask for a save.

use ck_ui::app::{App, Request, Tab};
use ck_ui::live::settings::{
    preview_outcome, PreviewOutcome, PreviewReply, PreviewTicket, SettingsReading, CAPABILITY,
};
use ck_ui::model::Serves;
use ck_ui::screens::save::{diff, DiffKind, SaveMode, CHECKING};
use ck_ui::{Snapshot, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};
use serde_json::{json, Value};

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap);
}

fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Vec<String> {
    let mut buffer = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buffer, snap, Snapshot::fixture_now());
    (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

fn config_path() -> String {
    if cfg!(windows) {
        "C:/example/subc.jsonc".into()
    } else {
        "/example/subc.jsonc".into()
    }
}

/// Settings over a config with `env_count` variables on broca, with the
/// save keys served as `save` says, focused on broca's values.
fn session(env_count: usize, save: bool) -> (App, Snapshot) {
    let env: serde_json::Map<String, Value> = (0..env_count)
        .map(|i| (format!("ENV_{i:02}"), json!(format!("value-{i}"))))
        .collect();
    let config = json!({"modules": {
        "broca": {"program": "/example/ck-broca", "env": env},
        "engram": {"program": "/example/ck-engram"}
    }});
    started(
        SettingsReading {
            config_path: Some(config_path()),
            capable: true,
            text: Some(serde_json::to_string_pretty(&config).unwrap()),
            error: None,
        },
        save,
    )
}

/// Settings over `reading`, focused on the first entry's values.
fn started(reading: SettingsReading, save: bool) -> (App, Snapshot) {
    let mut snap = Snapshot::fixture();
    snap.serves = Serves {
        save,
        ..Serves::LIVE
    };
    snap.settings = reading;
    let mut app = App::new(Theme::test());
    app.on_snapshot(&snap, Snapshot::fixture_now());
    press(&mut app, &snap, KeyCode::Char('7'));
    press(&mut app, &snap, KeyCode::Tab);
    press(&mut app, &snap, KeyCode::Tab);
    (app, snap)
}

/// Toggles broca's enabled flag: one edit, one new revision.
fn toggle(app: &mut App, snap: &Snapshot) {
    app.settings.row = 0;
    press(app, snap, KeyCode::Enter);
}

/// The preview requests sent so far, oldest first.
fn previews(app: &App) -> Vec<(PreviewTicket, String)> {
    app.requests
        .iter()
        .filter_map(|r| match r {
            Request::RescanPreview { ticket, candidate } => Some((*ticket, candidate.clone())),
            _ => None,
        })
        .collect()
}

fn reply(snap: &mut Snapshot, ticket: PreviewTicket, outcome: PreviewOutcome) {
    let seq = snap.previews.last().map_or(1, |(seq, _)| seq + 1);
    snap.previews.push((seq, PreviewReply { ticket, outcome }));
}

/// A ready preview with these module actions and diagnostics.
fn ready(actions: Value, earlier: &[&str], sections: &[&str], warnings: &[&str]) -> PreviewOutcome {
    PreviewOutcome::Ready(
        serde_json::from_value(json!({
            "added": [], "removed": [], "changed_pending_reload": [], "unchanged": 1,
            "preview": true,
            "module_actions": actions,
            "earlier_pending": earlier,
            "restart_required": sections,
            "capability_warnings": warnings,
            "expect": {
                "config_sha256": "ab".repeat(32),
                "staged_generation": {"incarnation": "example", "counter": 2}
            }
        }))
        .unwrap(),
    )
}

fn restart(id: &str, gauges: Value) -> Value {
    json!({"module_id": id, "action": "restart", "changed": ["environment"],
           "drain_timeout_ms": 30000, "busy_gauges": gauges})
}

/// Rows inside the Save box at 120 × 36 (box columns 12-107, rows 6-27).
fn overlay(app: &App, snap: &Snapshot) -> Vec<String> {
    render(app, snap, 120, 36)[7..27]
        .iter()
        .map(|r| r.chars().skip(13).take(94).collect::<String>())
        .collect()
}

#[test]
fn w_opens_the_exact_diff_and_the_keyboard_scrolls_all_of_it() {
    let (mut app, snap) = session(40, true);
    // Change every variable, so the diff is much taller than the box.
    for i in 0..40 {
        app.settings.row = 0;
        let at = app
            .settings
            .rows(false)
            .iter()
            .position(|r| r.env.as_deref() == Some(&format!("ENV_{i:02}")))
            .unwrap();
        app.settings.row = at + 1;
        press(&mut app, &snap, KeyCode::Char('e'));
        press(&mut app, &snap, KeyCode::Char('!'));
        press(&mut app, &snap, KeyCode::Enter);
    }
    let lines = diff(&app.settings.original, &app.settings.candidate);
    assert_eq!(
        lines.iter().filter(|l| l.kind == DiffKind::Removed).count(),
        40
    );
    press(&mut app, &snap, KeyCode::Char('w'));
    assert!(app.settings.save.open);
    let first = overlay(&app, &snap).join("\n");
    assert!(first.contains("· 40 changes · comments kept"), "{first}");
    assert!(first.contains("↓ "), "a hidden tail is counted: {first}");

    // Every line of the diff shows, with its number, as the keys move down.
    let mut seen = std::collections::BTreeSet::new();
    for step in 0..200 {
        for row in overlay(&app, &snap) {
            for (i, line) in lines.iter().enumerate() {
                let sign = match line.kind {
                    DiffKind::Removed => '-',
                    DiffKind::Added => '+',
                    _ => ' ',
                };
                let drawn = format!("{:>4}  {sign} {}", line.number, line.text);
                if row.trim_end().ends_with(drawn.trim_end()) {
                    seen.insert(i);
                }
            }
        }
        press(
            &mut app,
            &snap,
            if step % 2 == 0 {
                KeyCode::Down
            } else {
                KeyCode::PageDown
            },
        );
    }
    let missing: Vec<_> = (0..lines.len()).filter(|i| !seen.contains(i)).collect();
    assert!(missing.is_empty(), "never shown: {missing:?}");
    let bottom = overlay(&app, &snap).join("\n");
    assert!(!bottom.contains("↓ "), "{bottom}");
    for _ in 0..100 {
        press(&mut app, &snap, KeyCode::PageUp);
    }
    assert_eq!(app.settings.save.scroll, 0);
    press(&mut app, &snap, KeyCode::Up);
    assert_eq!(app.settings.save.scroll, 0);

    // esc closes the overlay and keeps the edits.
    press(&mut app, &snap, KeyCode::Esc);
    assert!(!app.settings.save.open);
    assert_eq!(app.settings.changes(), 40);
}

#[test]
fn the_diff_keeps_comments_numbers_lines_and_shows_a_little_context() {
    let old = "{\n  // keep me\n  \"a\": 1,\n  \"b\": 2,\n  \"c\": 3,\n  \"d\": 4,\n  \"e\": 5,\n  \"f\": 6,\n  \"g\": 7,\n  \"h\": 8\n}\n";
    let new = old
        .replace("\"b\": 2", "\"b\": 20")
        .replace("\"h\": 8", "\"h\": 80");
    let lines: Vec<_> = diff(old, &new)
        .into_iter()
        .map(|l| (l.kind, l.number, l.text))
        .collect();
    assert_eq!(
        lines,
        vec![
            (DiffKind::Same, 2, "  // keep me".into()),
            (DiffKind::Same, 3, "  \"a\": 1,".into()),
            (DiffKind::Removed, 4, "  \"b\": 2,".into()),
            (DiffKind::Added, 4, "  \"b\": 20,".into()),
            (DiffKind::Same, 5, "  \"c\": 3,".into()),
            (DiffKind::Gap, 0, "…".into()),
            (DiffKind::Same, 8, "  \"f\": 6,".into()),
            (DiffKind::Same, 9, "  \"g\": 7,".into()),
            (DiffKind::Removed, 10, "  \"h\": 8".into()),
            (DiffKind::Added, 10, "  \"h\": 80".into()),
            (DiffKind::Same, 11, "}".into()),
        ]
    );
}

/// Previews answered out of order, with the newest one refused: the stale
/// ready reply never counts, the refusal shows verbatim, and neither `⏎`
/// nor `s` asks for a save or sends anything but previews.
#[test]
fn out_of_order_previews_with_the_current_one_refused_write_nothing() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/scratch")
        .join(format!("save-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("subc.jsonc");
    let text = "{\"modules\": {\"broca\": {\"program\": \"/example/ck-broca\"}}}";
    std::fs::write(&path, text).unwrap();
    let reading = SettingsReading::read(
        Some(path.to_string_lossy().into_owned()),
        &[CAPABILITY.to_string()],
    );
    assert_eq!(reading.text.as_deref(), Some(text));
    let (mut app, mut snap) = started(reading, true);

    toggle(&mut app, &snap);
    toggle(&mut app, &snap);
    toggle(&mut app, &snap);
    let sent = previews(&app);
    assert_eq!(sent.len(), 3, "each edit is a new revision, previewed");
    let revisions: Vec<u64> = sent.iter().map(|(t, _)| t.revision).collect();
    assert!(revisions.windows(2).all(|w| w[0] < w[1]), "{revisions:?}");
    let (current, candidate) = sent[2].clone();
    assert_eq!(candidate, app.settings.candidate);

    press(&mut app, &snap, KeyCode::Char('w'));
    assert!(overlay(&app, &snap).join("\n").contains(CHECKING));

    // The newest request is refused first; older revisions' ready replies
    // arrive after it.
    let message =
        "supervisor rescan rejected daemon config: modules.broca: unknown field `enabeld`";
    reply(&mut snap, current, PreviewOutcome::Refused(message.into()));
    reply(&mut snap, sent[1].0, ready(json!([]), &[], &[], &[]));
    reply(&mut snap, sent[0].0, ready(json!([]), &[], &[], &[]));
    app.on_snapshot(&snap, Snapshot::fixture_now());
    let shown = overlay(&app, &snap).join("\n");
    assert!(shown.contains("unknown field `enabeld`"), "{shown}");
    assert!(!shown.contains("Save and"), "{shown}");
    assert!(!shown.contains("Save only"), "{shown}");
    assert!(shown.contains("esc  Cancel"), "{shown}");

    let before = app.requests.len();
    press(&mut app, &snap, KeyCode::Enter);
    press(&mut app, &snap, KeyCode::Char('s'));
    assert_eq!(app.settings.save.order, None);
    assert_eq!(
        app.requests.len(),
        before,
        "no rescan or anything else sent"
    );
    assert!(app.settings.save.open);
    assert_eq!(app.settings.candidate, candidate, "the edits stay");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_failed_preview_shows_its_code_and_blocks_saving() {
    let (mut app, mut snap) = session(0, true);
    toggle(&mut app, &snap);
    let (ticket, _) = previews(&app)[0].clone();
    press(&mut app, &snap, KeyCode::Char('w'));
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(app.settings.save.order, None, "nothing while checking");
    reply(&mut snap, ticket, PreviewOutcome::Failed("no_reply".into()));
    app.on_snapshot(&snap, Snapshot::fixture_now());
    let shown = overlay(&app, &snap).join("\n");
    assert!(
        shown.contains("Couldn't check what saving does: no_reply"),
        "{shown}"
    );
    assert!(!shown.contains("Save only"));
    press(&mut app, &snap, KeyCode::Enter);
    press(&mut app, &snap, KeyCode::Char('s'));
    assert_eq!(app.settings.save.order, None);
}

#[test]
fn a_ready_preview_allows_saving_exactly_the_previewed_text() {
    let (mut app, mut snap) = session(0, true);
    toggle(&mut app, &snap);
    let (ticket, candidate) = previews(&app)[0].clone();
    press(&mut app, &snap, KeyCode::Char('w'));
    reply(
        &mut snap,
        ticket,
        ready(json!([restart("broca", json!([]))]), &[], &[], &[]),
    );
    app.on_snapshot(&snap, Snapshot::fixture_now());
    press(&mut app, &snap, KeyCode::Char('s'));
    let order = app.settings.save.order.clone().expect("s asks for a save");
    assert_eq!(order.mode, SaveMode::Only);
    assert_eq!(order.candidate, candidate);
    assert_eq!(order.revision, ticket.revision);
    assert_eq!(order.restarts, vec!["broca"]);
    assert_eq!(order.expect.staged_generation.counter, 2);
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(
        app.settings.save.order.as_ref().unwrap().mode,
        SaveMode::Apply
    );
}

#[test]
fn after_saving_follows_each_actions_wording_and_the_buttons_count_restarts() {
    let cases: Vec<(PreviewOutcome, Vec<&str>, Vec<&str>, &str)> = vec![
        (
            ready(
                json!([restart(
                    "broca",
                    json!([{"gauge": "runs", "value": 2}, {"gauge": "queued", "value": 1}])
                )]),
                &[],
                &[],
                &[],
            ),
            vec![
                "↻ broca  needs a restart to use the new environment. It has 3 runs in flight;",
                "a restart waits up to 30 s for them to finish.",
            ],
            vec!["for them to finish. It has"],
            "⏎  Save and restart broca",
        ),
        (
            ready(json!([restart("broca", json!([]))]), &[], &[], &[]),
            vec!["↻ broca  needs a restart to use the new environment."],
            vec!["in flight", "waits up to"],
            "⏎  Save and restart broca",
        ),
        (
            ready(
                json!([
                    {"module_id": "broca", "action": "restart", "changed": ["environment", "arguments"]},
                    {"module_id": "engram", "action": "restart", "changed": ["program"],
                     "drain_timeout_ms": 1500, "busy_gauges": [{"gauge": "runs", "value": 1}]},
                    {"module_id": "aft", "action": "reload", "changed": ["health"]},
                    {"module_id": "motor", "action": "start"},
                    {"module_id": "plexus", "action": "stop", "drain_timeout_ms": 5000,
                     "busy_gauges": [{"gauge": "runs"}]}
                ]),
                &["wernicke", "insula"],
                &["storage"],
                &["memory.read is required by engram and no enabled module provides it"],
            ),
            vec![
                "↻ broca  needs a restart to use the new settings.",
                "↻ engram  needs a restart to use the new program. It has 1 run in flight;",
                "a restart waits up to 1.5 s for them to finish.",
                "✓ aft  applies the change without a restart.",
                "◉ motor  starts.",
                "○ plexus  stops. Its runs in flight are unknown;",
                "stopping waits up to 5 s for them to finish.",
                "Also waiting for a restart from an earlier change: wernicke, insula",
                "storage changed; it applies only when the daemon restarts",
                "memory.read is required by engram and no enabled module provides it",
            ],
            vec![],
            "⏎  Save and restart 2 modules",
        ),
        (
            ready(json!([]), &[], &["storage"], &["a warning, verbatim"]),
            vec![
                "storage changed; it applies only when the daemon restarts",
                "a warning, verbatim",
            ],
            vec!["Saving changes no running module."],
            "⏎  Save and apply",
        ),
        (
            ready(json!([]), &[], &[], &[]),
            vec!["Saving changes no running module."],
            vec![],
            "⏎  Save and apply",
        ),
    ];
    for (outcome, present, absent, button) in cases {
        let (mut app, mut snap) = session(0, true);
        toggle(&mut app, &snap);
        let (ticket, _) = previews(&app)[0].clone();
        press(&mut app, &snap, KeyCode::Char('w'));
        reply(&mut snap, ticket, outcome);
        app.on_snapshot(&snap, Snapshot::fixture_now());
        // A taller window, so the longest list shows whole.
        let rows = render(&app, &snap, 120, 48);
        let shown = rows
            .iter()
            .map(|r| r.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect::<Vec<_>>()
            .join("\n");
        for line in present {
            let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(shown.contains(&line), "missing {line:?} in\n{shown}");
        }
        for text in absent {
            assert!(!shown.contains(text), "unexpected {text:?} in\n{shown}");
        }
        let button = button.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(shown.contains(&button), "missing {button:?} in\n{shown}");
        assert!(shown.contains("s Save only"), "{shown}");
        assert!(shown.contains("esc Cancel"), "{shown}");
    }
}

#[test]
fn the_banner_names_the_restarts_from_the_preview() {
    let (mut app, mut snap) = session(0, false);
    toggle(&mut app, &snap);
    let banner = |app: &App, snap: &Snapshot| {
        render(app, snap, 120, 36)
            .into_iter()
            .find(|r| r.contains("unsaved change"))
            .unwrap()
    };
    let before = banner(&app, &snap);
    assert!(!before.contains("needs a restart"), "{before}");
    assert!(before.contains("w  review"), "{before}");
    assert!(!before.contains("review and save"), "{before}");
    let (ticket, _) = previews(&app)[0].clone();
    reply(
        &mut snap,
        ticket,
        ready(
            json!([restart("broca", json!([])), restart("engram", json!([]))]),
            &[],
            &[],
            &[],
        ),
    );
    app.on_snapshot(&snap, Snapshot::fixture_now());
    let after = banner(&app, &snap);
    assert!(
        after.contains("1 unsaved change · needs a restart of broca, engram"),
        "{after}"
    );
}

/// A reconnect drops every request still waiting and previews the revision
/// on screen again; a reply to a request from before it never counts.
#[test]
fn a_reconnect_drops_outstanding_previews_and_previews_again() {
    let (mut app, mut snap) = session(0, true);
    snap.daemon_connection = 1;
    app.on_snapshot(&snap, Snapshot::fixture_now());
    assert!(
        previews(&app).is_empty(),
        "nothing to preview without edits"
    );
    toggle(&mut app, &snap);
    let (before, candidate) = previews(&app)[0].clone();
    press(&mut app, &snap, KeyCode::Char('w'));

    snap.daemon_connection = 2;
    app.on_snapshot(&snap, Snapshot::fixture_now());
    let sent = previews(&app);
    assert_eq!(sent.len(), 2);
    let (again, again_text) = sent[1].clone();
    assert_eq!(again.revision, before.revision, "the same revision");
    assert_ne!(again, before);
    assert_eq!(again_text, candidate);

    reply(&mut snap, before, ready(json!([]), &[], &[], &[]));
    app.on_snapshot(&snap, Snapshot::fixture_now());
    assert!(overlay(&app, &snap).join("\n").contains(CHECKING));
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(app.settings.save.order, None);

    reply(&mut snap, again, ready(json!([]), &[], &[], &[]));
    app.on_snapshot(&snap, Snapshot::fixture_now());
    press(&mut app, &snap, KeyCode::Enter);
    assert!(app.settings.save.order.is_some());
    assert_eq!(app.settings.changes(), 1, "edits stay across a reconnect");
}

/// Undo is a revision of its own; undoing back to the file's text leaves
/// nothing to preview or save.
#[test]
fn undo_makes_a_new_revision_and_w_needs_a_change() {
    let (mut app, snap) = session(0, true);
    press(&mut app, &snap, KeyCode::Char('w'));
    assert!(!app.settings.save.open, "nothing to save");
    toggle(&mut app, &snap);
    toggle(&mut app, &snap);
    assert_eq!(previews(&app).len(), 2);
    press(&mut app, &snap, KeyCode::Char('u'));
    let sent = previews(&app);
    assert_eq!(sent.len(), 3, "undo previews its revision");
    assert!(sent[2].0.revision > sent[1].0.revision);
    assert_eq!(sent[2].1, sent[0].1, "the text after the first edit");
    press(&mut app, &snap, KeyCode::Char('u'));
    assert_eq!(
        previews(&app).len(),
        3,
        "back to the file's text: no preview"
    );
    assert!(app.settings.save.pending.is_none());
    press(&mut app, &snap, KeyCode::Char('w'));
    assert!(!app.settings.save.open);
}

/// With saving not served, the overlay reviews the change and offers only
/// `esc`; `⏎` and `s` ask for nothing and send nothing.
#[test]
fn the_live_app_without_save_shows_no_save_button_and_its_keys_do_nothing() {
    let (mut app, mut snap) = session(0, false);
    // The live build doesn't write files yet.
    assert!(!std::hint::black_box(Serves::LIVE).save);
    toggle(&mut app, &snap);
    let footer = render(&app, &snap, 120, 36).last().unwrap().clone();
    assert!(footer.contains("w  review"), "{footer}");
    assert!(!footer.contains("w  save"), "{footer}");
    let (ticket, _) = previews(&app)[0].clone();
    press(&mut app, &snap, KeyCode::Char('w'));
    reply(
        &mut snap,
        ticket,
        ready(json!([restart("broca", json!([]))]), &[], &[], &[]),
    );
    app.on_snapshot(&snap, Snapshot::fixture_now());
    let shown = overlay(&app, &snap).join("\n");
    assert!(shown.contains("needs a restart"), "{shown}");
    assert!(shown.contains("esc  Cancel"), "{shown}");
    assert!(!shown.contains("⏎"), "{shown}");
    assert!(!shown.contains("Save only"), "{shown}");
    let before = app.requests.clone();
    press(&mut app, &snap, KeyCode::Enter);
    press(&mut app, &snap, KeyCode::Char('s'));
    assert_eq!(app.settings.save.order, None);
    assert_eq!(app.requests, before);
    assert!(app.settings.save.open);
    assert_eq!(app.tab, Tab::Settings);
}

#[test]
fn preview_replies_map_to_ready_refused_or_failed() {
    use subc_client_rs::CallError;
    use subc_protocol::ErrorBody;
    let PreviewOutcome::Ready(result) = ready(json!([]), &[], &[], &[]) else {
        unreachable!()
    };
    let result = *result;
    assert!(matches!(
        preview_outcome(Ok(result.clone())),
        PreviewOutcome::Ready(_)
    ));
    let mut not_preview = result.clone();
    not_preview.preview = false;
    assert_eq!(
        preview_outcome(Ok(not_preview)),
        PreviewOutcome::Failed("preview_not_honoured".into())
    );
    let mut no_expect = result;
    no_expect.expect = None;
    assert_eq!(
        preview_outcome(Ok(no_expect)),
        PreviewOutcome::Failed("preview_without_expect".into())
    );
    let refusal = |code: &str| {
        CallError::Module(ErrorBody {
            code: code.into(),
            message: "the daemon's own words".into(),
            detail: None,
        })
    };
    assert_eq!(
        preview_outcome(Err(refusal("invalid_daemon_config"))),
        PreviewOutcome::Refused("the daemon's own words".into())
    );
    assert_eq!(
        preview_outcome(Err(refusal("candidate_too_large"))),
        PreviewOutcome::Failed("candidate_too_large".into())
    );
}

#[test]
fn the_overlay_draws_at_every_size() {
    let (mut app, mut snap) = session(30, true);
    for i in 0..30 {
        let at = app
            .settings
            .rows(false)
            .iter()
            .position(|r| r.env.as_deref() == Some(&format!("ENV_{i:02}")))
            .unwrap();
        app.settings.row = at + 1;
        press(&mut app, &snap, KeyCode::Char('d'));
    }
    let ticket = previews(&app).last().unwrap().0;
    press(&mut app, &snap, KeyCode::Char('w'));
    let many: Vec<Value> = (0..30)
        .map(|i| {
            restart(
                &format!("module-{i}"),
                json!([{"gauge": "runs", "value": i}]),
            )
        })
        .collect();
    reply(&mut snap, ticket, ready(Value::Array(many), &[], &[], &[]));
    app.on_snapshot(&snap, Snapshot::fixture_now());
    for (w, h) in [(80, 24), (100, 30), (140, 40), (160, 48), (200, 60)] {
        app.size = (w, h);
        let rows = render(&app, &snap, w, h).join("\n");
        assert!(
            rows.contains("↓ "),
            "{w}x{h}: hidden diff lines are counted"
        );
        assert!(rows.contains(" more"), "{w}x{h}");
        assert!(
            rows.contains("Save and restart 30 modules"),
            "{w}x{h}\n{rows}"
        );
        press(&mut app, &snap, KeyCode::PageDown);
    }
}
