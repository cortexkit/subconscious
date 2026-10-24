//! Renders each screen at the sizes the design was drawn at and compares it
//! cell for cell with the frames in `tests/golden/`: glyphs with `.txt`, and
//! style roles with `.styles`. The frames are the specification; there is no
//! switch to rewrite them from the app's output.

use ck_ui::app::{App, Overlay, Request, Tab};
use ck_ui::live::settings::{PreviewOutcome, PreviewReply};
use ck_ui::screens::logs::LogState;
use ck_ui::time::{Timestamp, UtcOffset};
use ck_ui::{Snapshot, Theme};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

const GOLDEN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden");

/// The design frames' state: broca selected in Modules, "br" typed in the
/// palette, the first attention row selected, clock tick 0.
fn app() -> App {
    let mut app = App::new(Theme::test());
    app.mod_sel = 3;
    app.palette_query = "br".to_string();
    app
}

fn render_at(app: &App, snap: &Snapshot, w: u16, h: u16, now: Timestamp) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal
        .draw(|f| app.render(f.buffer_mut(), snap, now))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn glyph_rows(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    (0..a.height)
        .map(|y| (0..a.width).map(|x| buf[(x, y)].symbol()).collect())
        .collect()
}

/// Style runs in the golden format: `y x len fg bg mods` for every run of
/// cells sharing a style other than plain.
fn style_runs(buf: &Buffer) -> Vec<String> {
    let a = buf.area;
    let mut runs = Vec::new();
    for y in 0..a.height {
        let role = |x: u16| {
            let c = &buf[(x, y)];
            Theme::test_roles(c.fg, c.bg, c.modifier)
        };
        let mut x = 0;
        while x < a.width {
            let r = role(x);
            let mut n = 1;
            while x + n < a.width && role(x + n) == r {
                n += 1;
            }
            if r != ("-".into(), "-".into(), String::new()) {
                let mods = if r.2.is_empty() { "-" } else { &r.2 };
                runs.push(format!("{y} {x} {n} {} {} {mods}", r.0, r.1));
            }
            x += n;
        }
    }
    runs
}

fn check(name: &str, app: &App, snap: &Snapshot) {
    check_at(name, app, snap, Snapshot::fixture_now());
}

fn check_at(name: &str, app: &App, snap: &Snapshot, now: Timestamp) {
    let (w, h) = {
        let size = name.rsplit('-').next().unwrap();
        let (w, h) = size.split_once('x').unwrap();
        (w.parse().unwrap(), h.parse().unwrap())
    };
    let buf = render_at(app, snap, w, h, now);
    let want_txt = std::fs::read_to_string(format!("{GOLDEN}/{name}.txt")).unwrap();
    let want_styles = std::fs::read_to_string(format!("{GOLDEN}/{name}.styles")).unwrap();
    let want_rows: Vec<&str> = want_txt.lines().collect();
    let got_rows = glyph_rows(&buf);
    let want_runs: Vec<&str> = want_styles.lines().filter(|l| !l.is_empty()).collect();
    let got_runs = style_runs(&buf);

    let mut report = String::new();
    let mut shown = 0;
    for y in 0..want_rows.len().max(got_rows.len()) {
        let want = want_rows.get(y).copied().unwrap_or("<missing>");
        let got = got_rows.get(y).map_or("<missing>", String::as_str);
        if want != got && shown < 6 {
            report += &format!("row {y}\n  expected |{want}|\n  actual   |{got}|\n");
            shown += 1;
        }
    }
    let missing: Vec<&&str> = want_runs
        .iter()
        .filter(|r| !got_runs.iter().any(|g| g == **r))
        .collect();
    let extra: Vec<&String> = got_runs
        .iter()
        .filter(|g| !want_runs.contains(&g.as_str()))
        .collect();
    if !missing.is_empty() || !extra.is_empty() {
        report += "style runs expected but not drawn:\n";
        for r in missing.iter().take(20) {
            report += &format!("  - {r}\n");
        }
        report += "style runs drawn but not expected:\n";
        for r in extra.iter().take(20) {
            report += &format!("  + {r}\n");
        }
    }
    assert!(
        report.is_empty(),
        "{name} differs from the golden frame:\n{report}"
    );
}

#[test]
fn home_80x24() {
    check("home-80x24", &app(), &Snapshot::fixture());
}

#[test]
fn home_120x36() {
    check("home-120x36", &app(), &Snapshot::fixture());
}

#[test]
fn home_160x48() {
    check("home-160x48", &app(), &Snapshot::fixture());
}

#[test]
fn modules_120x36() {
    let mut app = app();
    app.tab = Tab::Modules;
    check("modules-120x36", &app, &Snapshot::fixture());
}

/// What the design's Save overlay says saving does: broca needs a restart
/// to use its new environment, with 3 runs in flight and a 30 s drain. The
/// live layer gets this from the daemon's `supervisor.rescan_checked`
/// preview.
fn broca_restart_preview() -> PreviewOutcome {
    PreviewOutcome::Ready(
        serde_json::from_value(serde_json::json!({
            "added": [], "removed": [], "changed_pending_reload": ["broca"],
            "unchanged": 25, "preview": true,
            "module_actions": [{
                "module_id": "broca", "action": "restart", "changed": ["environment"],
                "drain_timeout_ms": 30000,
                "busy_gauges": [{"gauge": "runs_in_flight", "value": 3}]
            }],
            "expect": {
                "config_sha256": "0".repeat(64),
                "staged_generation": {"incarnation": "design", "counter": 1}
            }
        }))
        .unwrap(),
    )
}

/// Answers the newest preview the app asked for, the way the live layer
/// records a reply in a later snapshot.
fn answer_preview(app: &mut App, snap: &mut Snapshot, outcome: PreviewOutcome) {
    app.on_snapshot(snap, Snapshot::fixture_now());
    let ticket = app
        .requests
        .iter()
        .rev()
        .find_map(|r| match r {
            Request::RescanPreview { ticket, .. } => Some(*ticket),
            _ => None,
        })
        .expect("an edit asks for a preview");
    let seq = snap.previews.len() as u64 + 1;
    snap.previews.push((seq, PreviewReply { ticket, outcome }));
    app.on_snapshot(snap, Snapshot::fixture_now());
}

/// Settings as the design frames draw it: broca selected, `subc.jsonc` read
/// from `text` at `config_path`, and the frame's one edit, a new `CK_LOG`
/// variable, typed through the editor so it is on its undo history and asks
/// for a preview.
fn settings_session(text: String, config_path: &str) -> (App, Snapshot) {
    let mut app = app();
    app.tab = Tab::Settings;
    let mut snap = Snapshot::fixture();
    snap.host.data.home = Some("/Users/alex".into());
    snap.settings = ck_ui::live::settings::SettingsReading {
        config_path: Some(config_path.into()),
        capable: true,
        text: Some(text),
        error: None,
    };
    app.on_snapshot(&snap, Snapshot::fixture_now());
    app.settings.selected = Some("broca".into());
    let mut staged = app.settings.candidate.clone();
    subc_jsonc::set_member(
        &mut staged,
        &["modules", "broca", "env", "CK_LOG"],
        &serde_json::json!("info,broca.requests=debug"),
    )
    .unwrap();
    app.settings.focus = 2;
    app.settings.row = 6;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    app.settings.key(key(KeyCode::Char('e')), 24);
    for ch in "CK_LOG".chars() {
        app.settings.key(key(KeyCode::Char(ch)), 24);
    }
    app.settings.key(key(KeyCode::Enter), 24);
    for ch in "info,broca.requests=debug".chars() {
        app.settings.key(key(KeyCode::Char(ch)), 24);
    }
    app.settings.key(key(KeyCode::Enter), 24);
    assert_eq!(
        app.settings.document(),
        serde_json::from_str::<serde_json::Value>(&subc_jsonc::jsonc_to_json(&staged).unwrap())
            .unwrap()
    );
    app.settings.focus = 1;
    (app, snap)
}

/// The broca entry's comment, as the Settings frame lists it.
const BROCA_COMMENT: &str = "// BROCA_STATE_ROOT is the stable data folder. Its ledgers are not\n// disposable -- anyone writing a cleanup for it cannot tell the\n// difference.\n";

/// The banner names the restart from the daemon's preview of the edit.
#[test]
fn settings_120x36() {
    let snap = Snapshot::fixture();
    let mut modules = serde_json::Map::new();
    for module in &snap.modules.data {
        modules.insert(module.id.clone(), serde_json::json!({"program": "unused"}));
    }
    modules.insert(
        "broca".into(),
        serde_json::json!({
            "program": "/Users/alex/.local/share/cortexkit/bin/ck-broca",
            "env": {"BROCA_STATE_ROOT": "/Users/alex/.local/share/cortexkit/broca"},
            "reserved": "yes · only a daemon-launched broca may register",
            "restart": "drains up to 30 s · backoff 100 ms to 30 s",
            "health": "checked every 30 s"
        }),
    );
    let mut text = serde_json::to_string_pretty(&serde_json::json!({"modules": modules})).unwrap();
    let at = text.find("\"BROCA_STATE_ROOT\"").unwrap();
    text.insert_str(at, BROCA_COMMENT);
    let path = if cfg!(windows) {
        "C:/design/subc.jsonc"
    } else {
        "/design/subc.jsonc"
    };
    let (mut app, mut snap) = settings_session(text, path);
    answer_preview(&mut app, &mut snap, broca_restart_preview());
    check("settings-120x36", &app, &snap);
}

/// The Save overlay over the Settings frame. The frame's diff sits at lines
/// 306-310 of `~/.config/cortexkit/subc.jsonc`, with `"env"` and broca's
/// comment unindented, so the file is built to put them there; the comment
/// lines above `"modules"` only fill the lines the frame doesn't show.
///
/// Not run on Windows: the frame shows a config path under a Unix home, and
/// on Windows only a path with a drive is absolute, which ck requires before
/// it edits a file.
#[cfg(not(windows))]
#[test]
fn diff_120x36() {
    let snap = Snapshot::fixture();
    let mut head = vec!["{".to_string(), "  \"modules\": {".to_string()];
    let mut ids: Vec<&str> = snap.modules.data.iter().map(|m| m.id.as_str()).collect();
    ids.sort();
    for id in ids.into_iter().filter(|id| *id != "broca") {
        head.push(format!("    \"{id}\": {{\"program\": \"unused\"}},"));
    }
    head.extend(
        [
            "    \"broca\": {",
            "      \"program\": \"/Users/alex/.local/share/cortexkit/bin/ck-broca\",",
            "      \"reserved\": \"yes · only a daemon-launched broca may register\",",
            "      \"restart\": \"drains up to 30 s · backoff 100 ms to 30 s\",",
        ]
        .map(String::from),
    );
    // Broca's comment starts on line 304, so its last line is line 306.
    let filler = 303 - head.len();
    let mut lines = vec![head[0].clone()];
    lines.extend((0..filler).map(|i| format!("  // line {} of the example's padding", i + 2)));
    lines.extend(head[1..].iter().cloned());
    lines.extend(BROCA_COMMENT.lines().map(String::from));
    lines.extend(
        [
            "\"env\": {",
            "  \"BROCA_STATE_ROOT\": \"/Users/alex/.local/share/cortexkit/broca\"",
            "},",
            "      \"health\": \"checked every 30 s\"",
            "    }",
            "  }",
            "}",
        ]
        .map(String::from),
    );
    assert_eq!(lines[305], "// difference.");
    let text = lines.join("\n") + "\n";
    let (mut app, mut snap) = settings_session(text, "/Users/alex/.config/cortexkit/subc.jsonc");
    answer_preview(&mut app, &mut snap, broca_restart_preview());
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::NONE), &snap);
    assert!(app.settings.save.open);
    check("diff-120x36", &app, &snap);
}

#[test]
fn devices_120x36() {
    let mut app = app();
    app.tab = Tab::Devices;
    app.devices.sel = Some(1);
    check("devices-120x36", &app, &Snapshot::fixture());
}

#[test]
fn pair_120x36() {
    let mut app = app();
    app.tab = Tab::Devices;
    app.devices.sel = Some(1);
    app.overlay = Some(Overlay::Pairing);
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("golden/fixture.json")).unwrap();
    let group = &fixture["pairing"]["groups"][0];
    let now = Snapshot::fixture_now();
    app.pairing.window = Some(
        ck_ui::pairing::PairingState::new(
            fixture["pairing"]["payload"].as_str().unwrap(),
            group[0].as_str().unwrap().into(),
            group[1].as_u64(),
            now.add_millis(-1_000),
            now.add_millis(599_000),
        )
        .unwrap(),
    );
    check("pair-120x36", &app, &Snapshot::fixture());
}

#[test]
fn modules_160x48() {
    let mut app = app();
    app.tab = Tab::Modules;
    check("modules-160x48", &app, &Snapshot::fixture());
}

#[test]
fn palette_120x36() {
    let mut app = app();
    app.overlay = Some(Overlay::Palette);
    check("palette-120x36", &app, &Snapshot::fixture());
}

#[test]
fn help_120x36() {
    let mut app = app();
    app.overlay = Some(Overlay::Help);
    check("help-120x36", &app, &Snapshot::fixture());
}

#[test]
fn down_120x36() {
    check("down-120x36", &app(), &Snapshot::fixture_daemon_down());
}

#[test]
fn logs_120x36() {
    let mut app = app();
    app.tab = Tab::Modules;
    app.log = Some(LogState {
        query: "429".to_string(),
        ..LogState::default()
    });
    check("logs-120x36", &app, &Snapshot::fixture_logs());
}

/// The Activity frame's machine is two hours east of UTC: its pane reads
/// "18:20:36 local · UTC+2" while its clock and list read the fixture's
/// times. The fixture holds those times as UTC, so here the host gets the
/// frame's offset, and the entries and the moment drawn move two hours
/// earlier, which leaves every wall-clock reading the frame shows as it is.
fn east_of_utc(mut snap: Snapshot) -> (Snapshot, Timestamp) {
    const OFFSET_SECS: i32 = 2 * 3600;
    let shift = -i64::from(OFFSET_SECS) * 1000;
    snap.host.data.utc_offset = UtcOffset(OFFSET_SECS);
    for row in &mut snap.activity_log.data.rows {
        row.at = row.at.add_millis(shift);
    }
    (snap, Snapshot::fixture_now().add_millis(shift))
}

#[test]
fn activity_120x36() {
    let mut app = app();
    app.tab = Tab::Activity;
    let (snap, now) = east_of_utc(Snapshot::fixture());
    check_at("activity-120x36", &app, &snap, now);
}

/// The Usage frame's heading reads "refreshed 4s ago": the mock draws the
/// reading four seconds old at clock tick 0, a time `fixture.json` doesn't
/// carry, so it is set here. Claude, the first provider, is selected.
#[test]
fn usage_120x36() {
    let mut app = app();
    app.tab = Tab::Usage;
    let mut snap = Snapshot::fixture();
    snap.usage.received_at = Snapshot::fixture_now().add_millis(-4_000);
    check("usage-120x36", &app, &snap);
}

/// Settings → Updates as the Upgrade frame draws it, before an upgrade
/// runs: the fixture's plan rows, checked twelve minutes before the
/// fixture's moment.
#[test]
fn upgrade_120x36() {
    let mut app = app();
    app.tab = Tab::Settings;
    app.settings.section = 2;
    check("upgrade-120x36", &app, &Snapshot::fixture());
}

/// The Agents frame: nothing selected, so no row is highlighted.
#[test]
fn agents_120x36() {
    let mut app = app();
    app.tab = Tab::Agents;
    check("agents-120x36", &app, &Snapshot::fixture());
}
