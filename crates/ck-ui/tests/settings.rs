//! Keyboard-only Settings sessions and recorded describe compatibility cases.

use ck_ui::app::{App, Request, Tab};
use ck_ui::live::settings::{SettingsReading, CAPABILITY};
use ck_ui::model::Serves;
use ck_ui::screens::settings::{display_value, entry_comments, place, OLDER};
use ck_ui::{Snapshot, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};
use serde_json::{json, Value};

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap);
}
fn type_text(app: &mut App, snap: &Snapshot, text: &str) {
    for ch in text.chars() {
        press(app, snap, KeyCode::Char(ch));
    }
}
fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Vec<String> {
    let mut buffer = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buffer, snap, Snapshot::fixture_now());
    (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}
fn config_path(name: &str) -> String {
    let prefix = if cfg!(windows) {
        "C:/example"
    } else {
        "/example"
    };
    format!("{prefix}/{name}/subc.jsonc")
}
fn session(env_count: usize) -> (App, Snapshot) {
    let env: serde_json::Map<String, Value> = (0..env_count)
        .map(|i| {
            (
                format!("ENV_{i:02}"),
                json!(if i == 29 {
                    "x".repeat(500)
                } else {
                    format!("value-{i}")
                }),
            )
        })
        .collect();
    let config = json!({"log_level": "debug", "modules": {"broca": {
        "program": "/Users/alex/bin/ck-broca", "args": ["--example"], "env": env,
        "reserved": true, "restart": {"backoff_ms": 100}, "health": {"cadence_ms": 30000}
    }, "other": {"program": "other"}}});
    configured(config)
}
fn configured(config: Value) -> (App, Snapshot) {
    let mut snap = Snapshot::fixture();
    snap.serves = Serves::LIVE;
    snap.host.data.home = Some("/Users/alex".into());
    snap.settings = SettingsReading {
        config_path: Some(config_path("non-default")),
        capable: true,
        text: Some(config.to_string()),
        error: None,
    };
    let mut app = App::new(Theme::test());
    app.on_snapshot(&snap, Snapshot::fixture_now());
    press(&mut app, &snap, KeyCode::Char('7'));
    (app, snap)
}

#[test]
fn real_health_fields_are_aligned_readonly_rows_and_absent_restart_is_default() {
    let (mut app, snap) = configured(json!({"modules": {"aft": {
        "program": "/example/ck-aft",
        "health": {"deadline_ms": 30000, "failure_threshold": 8}
    }}}));
    let rows = render(&app, &snap, 120, 36);
    let deadline = rows.iter().find(|row| row.contains("deadline_ms")).unwrap();
    let threshold = rows
        .iter()
        .find(|row| row.contains("failure_threshold"))
        .unwrap();
    assert!(deadline.contains("Health"));
    assert!(deadline.contains("30000"));
    assert_eq!(deadline.find("30000"), threshold.rfind('8'));
    assert!(rows
        .iter()
        .any(|row| row.contains("Restart") && row.contains("default")));
    let pane = rows.join("\n");
    assert!(!pane.contains('{'), "{pane}");
    assert!(!pane.contains("unknown"), "{pane}");
    let before = app.settings.candidate.clone();
    app.settings.focus = 2;
    for (index, row) in app.settings.rows(false).iter().enumerate() {
        if row.label == "Restart" || row.detail_key.is_some() {
            assert!(row.target.is_none());
            app.settings.row = index + 1;
            press(&mut app, &snap, KeyCode::Char('e'));
            press(&mut app, &snap, KeyCode::Enter);
            press(&mut app, &snap, KeyCode::Char('d'));
            assert!(app.settings.field.is_none());
        }
    }
    assert_eq!(before, app.settings.candidate);
}

#[test]
fn absent_optional_fields_show_dim_defaults_without_changing_the_document() {
    let (mut app, snap) = configured(json!({"modules": {"aft": {"program": "/example/ck-aft"}}}));
    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 36));
    app.render(&mut buffer, &snap, Snapshot::fixture_now());
    let rows = render(&app, &snap, 120, 36);
    let dim = Theme::test().fg(ck_ui::theme::Fg::Dim);
    for label in ["Arguments", "Environment", "Reserved", "Restart", "Health"] {
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, row)| row.contains(label))
            .unwrap();
        let x = row[..row.find("default").unwrap()].chars().count();
        assert_eq!(buffer[(x as u16, y as u16)].fg, dim, "{label}");
    }
    assert!(rows[3].contains("enabled  ◉ on · default"));
    let x = rows[3][..rows[3].find("default").unwrap()].chars().count();
    assert_eq!(buffer[(x as u16, 3)].fg, dim);
    assert!(!rows.join("\n").contains("unknown"));
    assert!(app.settings.document()["modules"]["aft"]
        .as_object()
        .unwrap()
        .keys()
        .all(|key| key == "program"));
    to_values(&mut app, &snap);
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(app.settings.document()["modules"]["aft"]["enabled"], false);
    assert!(!render(&app, &snap, 120, 36)[3].contains("default"));
}

#[test]
fn explicit_empty_collections_false_flags_and_null_policies_keep_their_meaning() {
    let (app, snap) = configured(json!({"modules": {"aft": {
        "program": "/example/ck-aft", "enabled": false, "reserved": false,
        "args": [], "env": {}, "health": null, "restart": null
    }}}));
    let rows = render(&app, &snap, 120, 36);
    assert!(rows[3].contains("enabled  ○ off"));
    assert!(!rows[3].contains("default"));
    for label in ["Arguments", "Environment"] {
        assert!(rows
            .iter()
            .any(|row| row.contains(label) && row.contains("none")));
    }
    assert!(rows
        .iter()
        .any(|row| row.contains("Reserved") && row.contains("no")));
    for label in ["Health", "Restart"] {
        assert!(rows
            .iter()
            .any(|row| row.contains(label) && row.contains("default")));
    }
    assert!(!rows.join("\n").contains("unknown"));
}

#[test]
fn unreadable_optional_values_are_unknown_not_defaults() {
    let (app, snap) = configured(json!({"modules": {"aft": {
        "program": "/example/ck-aft", "enabled": "on", "reserved": null,
        "args": false, "env": []
    }}}));
    let rows = render(&app, &snap, 120, 36);
    assert!(rows[3].contains("enabled  unknown"));
    for label in ["Arguments", "Environment", "Reserved"] {
        assert!(rows
            .iter()
            .any(|row| row.contains(label) && row.contains("unknown")));
    }
}

#[test]
fn configuration_objects_and_arrays_use_dotted_keys_and_plain_values() {
    let (mut app, snap) = configured(json!({
        "logging": {"rotation": {"enabled": true, "limits": [10, 20]}},
        "modules": {"aft": {"program": "/example/ck-aft",
            "health": {"nested": {"limits": [1, 2], "probes": [{"ok": false}]}}
        }}
    }));
    let pane = render(&app, &snap, 150, 40).join("\n");
    for value in ["nested.limits", "1, 2", "nested.probes[0].ok", "no"] {
        assert!(pane.contains(value), "{pane}");
    }
    assert!(!pane.contains('{'));
    assert!(!pane.contains("unknown"));
    press(&mut app, &snap, KeyCode::Down);
    let daemon = render(&app, &snap, 120, 36).join("\n");
    for value in [
        "logging.rotation.enabled",
        "yes",
        "logging.rotation.limits",
        "10, 20",
    ] {
        assert!(daemon.contains(value), "{daemon}");
    }
    assert!(!daemon.contains('{'));
    assert!(!daemon.contains("modules"));
}
fn to_values(app: &mut App, snap: &Snapshot) {
    press(app, snap, KeyCode::Tab);
    press(app, snap, KeyCode::Tab);
}
fn select_row(
    app: &mut App,
    snap: &Snapshot,
    target: impl Fn(&ck_ui::screens::settings::Row) -> bool,
) {
    let at = app.settings.rows(false).iter().position(target).unwrap() + 1;
    for _ in 0..at {
        press(app, snap, KeyCode::Down);
    }
}

#[test]
fn modules_e_opens_the_selected_config_entry_and_sections_are_keyboard_reachable() {
    let (mut app, snap) = session(2);
    let live = render(&app, &snap, 120, 36).join("\n");
    assert!(!live.contains("AI model runner"));
    assert!(!live.contains("     new"));
    assert!(live.contains("+ add argument"));
    press(&mut app, &snap, KeyCode::Char('2'));
    app.mod_sel = snap
        .modules
        .data
        .iter()
        .position(|m| m.id == "broca")
        .unwrap();
    press(&mut app, &snap, KeyCode::Char('e'));
    assert_eq!(app.tab, Tab::Settings);
    assert_eq!(app.settings.selected.as_deref(), Some("broca"));
    assert_eq!(app.settings.entry().as_deref(), Some("broca"));
    press(&mut app, &snap, KeyCode::BackTab);
    press(&mut app, &snap, KeyCode::Down);
    press(&mut app, &snap, KeyCode::Down);
    assert_eq!(app.settings.section, 2);
    assert!(app.showing(&snap).updates);
    assert!(render(&app, &snap, 120, 36)
        .join("\n")
        .contains("Update checks"));
    press(&mut app, &snap, KeyCode::Down);
    let about = render(&app, &snap, 120, 36).join("\n");
    assert!(about.contains(&format!("ck {}", ck_ui::ck_version())));
    assert!(about.contains("Daemon 0."));
    assert!(about.contains(&config_path("non-default")));
    assert!(!about.contains("Credentials"));
    assert!(!about.contains("Permissions"));
    press(&mut app, &snap, KeyCode::Up);
    press(&mut app, &snap, KeyCode::Up);
    let daemon = render(&app, &snap, 120, 36).join("\n");
    assert!(daemon.contains("log_level  debug"));
    assert!(!daemon.contains("ENV_"));
    let original = app.settings.candidate.clone();
    press(&mut app, &snap, KeyCode::Char('e'));
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(original, app.settings.candidate);
}

#[test]
fn edits_are_in_memory_undoable_survive_tabs_and_guard_both_quit_keys() {
    for ctrl_c in [false, true] {
        let (mut app, snap) = session(2);
        to_values(&mut app, &snap);
        press(&mut app, &snap, KeyCode::Enter);
        assert_eq!(
            app.settings.document()["modules"]["broca"]["enabled"],
            false
        );
        assert_eq!(app.settings.changes(), 1);
        assert!(render(&app, &snap, 120, 36)
            .last()
            .unwrap()
            .contains("1 unsaved change"));
        press(&mut app, &snap, KeyCode::Char('1'));
        press(&mut app, &snap, KeyCode::Char('7'));
        assert_eq!(app.settings.changes(), 1);
        let quit = |app: &mut App| {
            if ctrl_c {
                app.handle_key(
                    KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
                    &snap,
                );
            } else {
                press(app, &snap, KeyCode::Char('q'));
            }
        };
        quit(&mut app);
        assert!(!app.quit);
        assert!(render(&app, &snap, 120, 36)
            .last()
            .unwrap()
            .contains("1 unsaved changes · q again quits · esc stays"));
        press(&mut app, &snap, KeyCode::Esc);
        assert!(!app.settings.quit_asked);
        quit(&mut app);
        assert!(!app.quit);
        quit(&mut app);
        assert!(app.quit);
        app.quit = false;
        press(&mut app, &snap, KeyCode::Char('u'));
        assert_eq!(app.settings.changes(), 0);
        assert_eq!(app.settings.candidate, app.settings.original);
        assert!(app
            .requests
            .iter()
            .all(|r| !matches!(r, Request::Act { .. })));
    }
}

#[test]
fn keyboard_edits_the_thirtieth_env_and_scrolls_a_five_hundred_character_field() {
    let (mut app, snap) = session(30);
    app.size = (80, 24);
    to_values(&mut app, &snap);
    select_row(&mut app, &snap, |r| r.env.as_deref() == Some("ENV_29"));
    let rows = render(&app, &snap, 80, 24);
    assert!(rows.iter().any(|r| r.contains("ENV_29")));
    press(&mut app, &snap, KeyCode::Char('e'));
    assert_eq!(app.settings.field.as_ref().unwrap().text.len(), 500);
    let tail = render(&app, &snap, 80, 24).join("\n");
    assert!(tail.contains(&"x".repeat(60)));
    press(&mut app, &snap, KeyCode::Home);
    type_text(&mut app, &snap, "quit1?:");
    assert!(!app.quit);
    assert_eq!(app.tab, Tab::Settings);
    assert!(render(&app, &snap, 80, 24).join("\n").contains("quit1?:"));
    press(&mut app, &snap, KeyCode::End);
    type_text(&mut app, &snap, "tail");
    assert!(render(&app, &snap, 80, 24).join("\n").contains("tail"));
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(
        app.settings.document()["modules"]["broca"]["env"]["ENV_29"],
        format!("quit1?:{}tail", "x".repeat(500))
    );
    assert_eq!(app.settings.changes(), 1);
    press(&mut app, &snap, KeyCode::Char('e'));
    type_text(&mut app, &snap, "cancelled");
    press(&mut app, &snap, KeyCode::Esc);
    assert_eq!(app.settings.changes(), 1);
    press(&mut app, &snap, KeyCode::PageUp);
    assert!(app.settings.row < 30);
    press(&mut app, &snap, KeyCode::PageDown);
    assert!(app.settings.row >= 30);
}

#[test]
fn arguments_and_environment_add_edit_remove_and_home_is_display_only() {
    let (mut app, snap) = session(0);
    to_values(&mut app, &snap);
    assert_eq!(
        display_value("/Users/alex/bin/ck-broca", Some("/Users/alex")),
        "~/bin/ck-broca"
    );
    assert_eq!(
        display_value("/Users/alexander/bin", Some("/Users/alex")),
        "/Users/alexander/bin"
    );
    assert_eq!(
        display_value(r"C:\Users\alex\bin", Some(r"C:\Users\alex\")),
        r"~\bin"
    );
    select_row(&mut app, &snap, |r| r.label == "Program");
    press(&mut app, &snap, KeyCode::Char('e'));
    assert_eq!(
        app.settings.field.as_ref().unwrap().text,
        "/Users/alex/bin/ck-broca"
    );
    type_text(&mut app, &snap, "-next");
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(
        app.settings.document()["modules"]["broca"]["program"],
        "/Users/alex/bin/ck-broca-next"
    );
    app.settings.row = 0;
    select_row(&mut app, &snap, |r| r.value == "+ add argument");
    press(&mut app, &snap, KeyCode::Char('e'));
    type_text(&mut app, &snap, "--new");
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(
        app.settings.document()["modules"]["broca"]["args"],
        json!(["--example", "--new"])
    );
    app.settings.row = 0;
    select_row(&mut app, &snap, |r| r.value == "--new");
    press(&mut app, &snap, KeyCode::Char('d'));
    assert_eq!(
        app.settings.document()["modules"]["broca"]["args"],
        json!(["--example"])
    );
    app.settings.row = 0;
    select_row(&mut app, &snap, |r| r.value == "+ add variable");
    press(&mut app, &snap, KeyCode::Char('e'));
    type_text(&mut app, &snap, "CK_EXAMPLE");
    press(&mut app, &snap, KeyCode::Tab);
    type_text(&mut app, &snap, "/Users/alex/data");
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(
        app.settings.document()["modules"]["broca"]["env"]["CK_EXAMPLE"],
        "/Users/alex/data"
    );
    app.settings.row = 0;
    select_row(&mut app, &snap, |r| r.env.as_deref() == Some("CK_EXAMPLE"));
    press(&mut app, &snap, KeyCode::Char('d'));
    assert!(app.settings.document()["modules"]["broca"]["env"]
        .as_object()
        .unwrap()
        .is_empty());
    assert_eq!(app.settings.reading.text, snap.settings.text);
}

#[test]
fn filtering_consumes_global_keys_and_readonly_rows_do_not_edit() {
    let (mut app, snap) = session(2);
    press(&mut app, &snap, KeyCode::Char('/'));
    type_text(&mut app, &snap, "other");
    assert_eq!(app.settings.entries(), vec!["other"]);
    press(&mut app, &snap, KeyCode::Esc);
    assert_eq!(app.settings.entries().len(), 2);
    app.settings.focus = 2;
    app.settings.row = 0;
    select_row(&mut app, &snap, |r| r.label == "Reserved");
    let before = app.settings.candidate.clone();
    press(&mut app, &snap, KeyCode::Char('e'));
    press(&mut app, &snap, KeyCode::Enter);
    press(&mut app, &snap, KeyCode::Char('d'));
    assert!(app.settings.field.is_none());
    assert_eq!(before, app.settings.candidate);
}

#[test]
fn a_reconnect_to_the_same_path_keeps_edits_but_a_new_path_drops_them_once() {
    let (mut app, mut snap) = session(2);
    to_values(&mut app, &snap);
    press(&mut app, &snap, KeyCode::Enter);
    snap.settings.text = Some("{\"modules\":{}}".into());
    app.on_snapshot(&snap, Snapshot::fixture_now());
    assert_eq!(app.settings.changes(), 1);
    snap.settings.config_path = Some(config_path("another"));
    app.on_snapshot(&snap, Snapshot::fixture_now());
    assert_eq!(app.settings.changes(), 0);
    assert!(app
        .settings
        .notice
        .as_ref()
        .unwrap()
        .contains("unsaved edits were dropped"));
    assert!(render(&app, &snap, 120, 36)
        .last()
        .unwrap()
        .contains("config path changed"));
    assert!(!app.on_snapshot(&snap, Snapshot::fixture_now()));
}

#[test]
fn every_entry_comment_is_in_source_order_and_strings_are_not_comments() {
    let text = r#"{"modules":{"earlier":{}, // outside the entry
    "broca": { // first
      "program": "https://example.test/*not a comment*/", /* second */
      "env": {"A": "//not a comment", "B": "{\"url\": \"https://example.test\"} /*not either*/"}, // third
      "args": [/* fourth */ "x"]
    }}}"#;
    assert_eq!(
        entry_comments(text, "broca"),
        vec!["// first", "/* second */", "// third", "/* fourth */"]
    );
}

#[test]
fn sizes_count_hidden_rows_keep_the_banner_and_expand_values_at_150_columns() {
    let (mut app, snap) = session(30);
    to_values(&mut app, &snap);
    press(&mut app, &snap, KeyCode::Enter);
    let total = app.settings.rows(false).len();
    for (w, h) in [(80, 24), (100, 30), (140, 40), (160, 48), (200, 60)] {
        let rows = render(&app, &snap, w, h);
        let p = place(i32::from(h), total, true);
        assert!(p.list >= 3);
        if total > p.list {
            assert!(
                rows[6 + p.list - 1].contains(&format!("+{} more", total - p.list + 1)),
                "{w}x{h}: {:?}",
                rows
            );
            assert_eq!(p.banner, Some(i32::from(h) - 3));
        }
        let banner = p.banner.unwrap() as usize;
        assert!(rows[banner].contains("1 unsaved change"));
        assert!(rows[banner + 1..usize::from(h) - 2]
            .iter()
            .all(|r| r.chars().skip(51).all(|ch| ch == ' ')));
        if w == 80 {
            for hint in ["e  edit", "⏎  toggle", "u  undo", "/  filter"] {
                assert!(rows.last().unwrap().contains(hint));
            }
            assert!(!rows.last().unwrap().ends_with("fil"));
        }
    }
    let (mut app, mut snap) = session(1);
    let mut doc = app.settings.document();
    doc["modules"]["broca"]["env"]["ENV_00"] =
        json!("a-value-that-fits-only-in-the-wide-environment-column");
    snap.settings.text = Some(doc.to_string());
    app = App::new(Theme::test());
    app.tab = Tab::Settings;
    app.on_snapshot(&snap, Snapshot::fixture_now());
    let narrow = render(&app, &snap, 120, 36).join("\n");
    assert!(!narrow.contains("a-value-that-fits-only-in-the-wide-environment-column"));
    assert!(narrow.contains('…'));
    for w in [150, 160, 200] {
        assert!(render(&app, &snap, w, 48)
            .join("\n")
            .contains("a-value-that-fits-only-in-the-wide-environment-column"));
    }
}

#[test]
fn recorded_describe_requires_both_absolute_path_and_preview_capability_and_never_sends_rescan() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/scratch")
        .join(format!("settings-{}", std::process::id()));
    std::fs::create_dir_all(&root).unwrap();
    let path = root.join("non-default.jsonc");
    let text = "{\"modules\": {\"reported-file-entry\": {\"program\":\"example\"}}}";
    std::fs::write(&path, text).unwrap();
    for (reported, caps, editable) in [
        (Value::Null, json!([]), false),
        (json!(path), json!([]), false),
        (Value::Null, json!([CAPABILITY]), false),
        (
            json!("relative-config/subc.jsonc"),
            json!([CAPABILITY]),
            false,
        ),
        (json!(path), json!([CAPABILITY]), true),
    ] {
        let recorded = json!({"op":"server.describe", "protocol_ver":1, "subc_ops": [], "capabilities":caps,
            "connected_clients":1, "counters":{}, "config_path":reported});
        let response: subc_control::ClientControlResponse =
            serde_json::from_value(recorded).unwrap();
        let subc_control::ClientControlResponse::ServerDescribe {
            config_path,
            capabilities,
            ..
        } = response
        else {
            panic!("wrong describe")
        };
        let reading = SettingsReading::read(
            config_path.map(|p| p.to_string_lossy().into_owned()),
            &capabilities,
        );
        assert_eq!(reading.editable(), editable);
        if !editable {
            assert!(reading.text.is_none());
        }
        let mut snap = Snapshot::fixture();
        snap.serves = Serves::LIVE;
        snap.settings = reading;
        let mut app = App::new(Theme::test());
        app.tab = Tab::Settings;
        app.on_snapshot(&snap, Snapshot::fixture_now());
        let frame = render(&app, &snap, 120, 36).join(" ");
        if editable {
            assert!(frame.contains("reported-file-entry"));
        } else {
            assert!(frame.contains("This daemon is older than ck"));
            assert_eq!(OLDER, "This daemon is older than ck, so ck won't edit its settings. Update it from Settings → Updates.");
        }
        to_values(&mut app, &snap);
        press(&mut app, &snap, KeyCode::Char('e'));
        press(&mut app, &snap, KeyCode::Char('w'));
        assert!(
            app.requests.is_empty(),
            "no control request is sent by the in-memory editor"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
    }
    std::fs::remove_dir_all(root).unwrap();
}
