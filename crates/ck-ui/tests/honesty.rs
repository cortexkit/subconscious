//! The honesty rules as a user sees them, and the Modules actions: stale
//! panels show their age and then read as a last good reading, a failing
//! source shows its error instead of zeros, unknown values read "unknown",
//! and restart, stop and start ask only when they should and send what
//! `ck module restart|stop|start` send.

use ck_ui::app::{App, Request, Tab};
use ck_ui::model::{
    ActionKind, ActionPhase, ActionResult, Daemon, DaemonDown, ModuleAction, Panel, Status,
};
use ck_ui::theme::{Fg, Theme};
use ck_ui::Snapshot;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;

fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buf, snap, Snapshot::fixture_now());
    buf
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) -> bool {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap)
}

fn index(snap: &Snapshot, id: &str) -> usize {
    snap.modules.data.iter().position(|m| m.id == id).unwrap()
}

/// The fixture with its usage panel on a 30-second schedule, last read
/// `age` seconds before the fixture's moment.
fn usage_aged(age: f64) -> Snapshot {
    let mut s = Snapshot::fixture();
    s.usage.refresh_secs = Some(30.0);
    s.usage.received_at = Snapshot::fixture_now().add_secs(-age);
    s
}

// Home at 120×36: the Usage heading is on row 8, from column 45 to 82, and
// its first provider row is row 10.
const USAGE_HEADING: u16 = 8;
const USAGE_FIRST: u16 = 10;
const USAGE_X: u16 = 45;
const USAGE_END: u16 = 82;

#[test]
fn a_fresh_panel_shows_its_usual_meta() {
    let buf = render(&App::new(Theme::test()), &usage_aged(59.0), 120, 36);
    assert!(row(&buf, USAGE_HEADING).contains("this week"));
}

#[test]
fn past_twice_its_interval_a_panel_shows_its_age_in_amber() {
    let theme = Theme::test();
    let buf = render(&App::new(theme), &usage_aged(61.0), 120, 36);
    let heading = row(&buf, USAGE_HEADING);
    assert!(heading.contains("1m ago"), "{heading}");
    assert!(!heading.contains("this week"), "{heading}");
    assert!(!heading.contains("last good reading"), "{heading}");
    assert_eq!(buf[(USAGE_END - 1, USAGE_HEADING)].fg, theme.fg(Fg::Warn));
    // Not yet dimmed.
    assert!(!buf[(USAGE_X, USAGE_FIRST)].modifier.contains(Modifier::DIM));
}

#[test]
fn past_five_times_its_interval_a_panel_reads_as_a_last_good_reading() {
    let theme = Theme::test();
    let buf = render(&App::new(theme), &usage_aged(151.0), 120, 36);
    let heading = row(&buf, USAGE_HEADING);
    assert!(heading.contains("last good reading"), "{heading}");
    assert_eq!(buf[(USAGE_END - 1, USAGE_HEADING)].fg, theme.fg(Fg::Warn));
    for x in [USAGE_X, USAGE_X + 12, USAGE_END - 1] {
        assert!(
            buf[(x, USAGE_FIRST)].modifier.contains(Modifier::DIM),
            "the values at column {x} dim"
        );
    }
}

#[test]
fn a_failing_provider_shows_its_error_never_zero_percent() {
    let mut s = Snapshot::fixture();
    s.usage.data.providers[0].used = None;
    s.usage.data.providers[0].error = Some("upstream_failed".into());
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    let line = row(&buf, USAGE_FIRST);
    assert!(line.contains("upstream_failed"), "{line}");
    let usage_cells: String = line.chars().skip(USAGE_X as usize).take(37).collect();
    assert!(!usage_cells.contains('%'), "{usage_cells}");
    assert!(!usage_cells.contains('█'), "no bar for a failing provider");
}

#[test]
fn a_failing_source_with_no_reading_shows_its_error_and_unknown() {
    let mut s = Snapshot::fixture();
    s.usage = Panel {
        error: Some("insula unreachable · retrying".into()),
        ..Panel::waiting(s.usage.data.clone(), Some(30.0))
    };
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    assert!(row(&buf, USAGE_HEADING).contains("insula unreachable · retrying"));
    let body = row(&buf, USAGE_FIRST);
    assert!(body.contains("unknown"), "{body}");
    assert!(!body.contains('%'), "{body}");
}

#[test]
fn unknown_values_read_unknown() {
    let mut s = Snapshot::fixture();
    let broca = index(&s, "broca");
    let m = &mut s.modules.data[broca];
    m.cpu_percent = None;
    m.memory_mb = None;
    m.pid = None;
    m.started_at = None;
    m.health_checked_at = None;
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    app.mod_sel = broca;
    let buf = render(&app, &s, 120, 36);
    // In the table's numeric columns a missing value is a dim dash; the
    // detail pane spells it out.
    let table_row = row(&buf, 7 + broca as u16);
    assert_eq!(table_row.matches('–').count(), 3, "{table_row}");
    assert!(!table_row.contains("unknown"), "{table_row}");
    assert!(row(&buf, 4).contains("pid unknown · up unknown"));
    assert!((0..36).any(|y| row(&buf, y).contains("never checked")));

    let mut s = Snapshot::fixture();
    s.host.data.load = None;
    s.host.data.memory_total_gb = None;
    s.approvals.data.waiting = None;
    let buf = render(&App::new(Theme::test()), &s, 160, 48);
    assert!(row(&buf, 22).contains("Load         unknown"));
    assert!(row(&buf, 20).contains("Memory       unknown"));
    assert!(row(&buf, 19).contains("Approvals    unknown"));
}

#[test]
fn an_unknown_agent_state_is_not_drawn_as_idle() {
    let mut s = Snapshot::fixture();
    for a in &mut s.agents.data.list {
        a.state = "unknown".into();
    }
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    // The agents panel starts at column 85.
    let lines: Vec<String> = (26..32)
        .map(|y| row(&buf, y).chars().skip(85).collect())
        .collect();
    assert!(lines
        .iter()
        .all(|l| l.contains('◌') && l.contains("unknown")));
    assert!(lines.iter().all(|l| !l.contains("idle")));
}

// ---- actions --------------------------------------------------------------------

fn modules_app(snap: &Snapshot, id: &str) -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    app.mod_sel = index(snap, id);
    app
}

#[test]
fn r_on_an_ok_module_asks_first_naming_what_it_interrupts() {
    let mut s = Snapshot::fixture();
    let broca = index(&s, "broca");
    assert_eq!(s.modules.data[broca].status, Status::Ok);
    s.modules.data[broca].serving = Some(vec!["aft".into(), "alfonso".into(), "aft".into()]);
    s.modules.data[broca].drain_limit_secs = Some(30.0);
    let mut app = modules_app(&s, "broca");
    assert!(press(&mut app, &s, KeyCode::Char('r')));
    assert!(app.requests.is_empty(), "nothing is sent before the answer");
    let q = app.confirm.clone().expect("an ok module asks too");
    assert_eq!(q.kind, ActionKind::Restart);
    for part in [
        "3 open routes close and reopen (aft, alfonso)",
        "\u{201c}3 runs in flight\u{201d}",
        "up to 30s",
    ] {
        assert!(
            q.consequence.contains(part),
            "{part:?} in {:?}",
            q.consequence
        );
    }
    // Answering sends the restart; esc would have sent nothing.
    press(&mut app, &s, KeyCode::Enter);
    assert_eq!(
        app.requests,
        [Request::Act {
            kind: ActionKind::Restart,
            module: "broca".into()
        }]
    );
    // What isn't known is said to be unknown.
    s.modules.data[broca].serving = None;
    s.modules.data[broca].drain_limit_secs = None;
    let mut app = modules_app(&s, "broca");
    press(&mut app, &s, KeyCode::Char('r'));
    let q = app.confirm.unwrap();
    assert!(
        q.consequence.contains("open routes are unknown"),
        "{}",
        q.consequence
    );
    assert!(q.consequence.contains("is unknown."), "{}", q.consequence);
}

#[test]
fn restarting_a_degraded_module_asks_once_naming_the_consequence() {
    let s = Snapshot::fixture();
    let mut app = modules_app(&s, "engram");
    assert_eq!(s.modules.data[app.mod_sel].status, Status::Degraded);
    press(&mut app, &s, KeyCode::Char('r'));
    assert!(app.requests.is_empty());
    let q = app.confirm.clone().expect("a degraded module asks first");
    assert!(
        q.consequence.contains("engram is degraded"),
        "{}",
        q.consequence
    );
    let buf = render(&app, &s, 120, 36);
    assert!((0..36).any(|y| row(&buf, y).contains("Restart engram?")));
    // Esc cancels; asking again and answering sends the request.
    press(&mut app, &s, KeyCode::Esc);
    assert!(app.confirm.is_none() && app.requests.is_empty());
    press(&mut app, &s, KeyCode::Char('r'));
    press(&mut app, &s, KeyCode::Enter);
    assert_eq!(
        app.requests,
        [Request::Act {
            kind: ActionKind::Restart,
            module: "engram".into()
        }]
    );
}

#[test]
fn stopping_asks_and_names_who_it_serves_and_starting_does_not_ask() {
    let mut s = Snapshot::fixture();
    let broca = index(&s, "broca");
    s.modules.data[broca].serving = Some(vec!["aft".into(), "direct".into()]);
    let mut app = modules_app(&s, "broca");
    press(&mut app, &s, KeyCode::Char('s'));
    let q = app.confirm.clone().expect("stopping asks");
    assert_eq!(q.kind, ActionKind::Stop);
    assert!(
        q.consequence.contains("2 open routes close (aft, direct)"),
        "{}",
        q.consequence
    );
    press(&mut app, &s, KeyCode::Char('y'));
    assert_eq!(
        app.requests,
        [Request::Act {
            kind: ActionKind::Stop,
            module: "broca".into()
        }]
    );

    // A module whose routes the daemon couldn't list says so.
    s.modules.data[broca].serving = None;
    let mut app = modules_app(&s, "broca");
    press(&mut app, &s, KeyCode::Char('s'));
    assert!(app.confirm.unwrap().consequence.contains("unknown"));

    s.modules.data[broca].status = Status::Stopped;
    let mut app = modules_app(&s, "broca");
    press(&mut app, &s, KeyCode::Char('s'));
    assert!(app.confirm.is_none());
    assert_eq!(
        app.requests,
        [Request::Act {
            kind: ActionKind::Start,
            module: "broca".into()
        }]
    );
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 35).contains(" s  start"));
}

#[test]
fn edit_does_nothing_yet() {
    let s = Snapshot::fixture();
    let mut app = modules_app(&s, "broca");
    assert!(!press(&mut app, &s, KeyCode::Char('e')));
    assert!(app.requests.is_empty());
    assert_eq!(app.tab, Tab::Modules);
}

#[test]
fn an_action_in_flight_shows_its_phase_and_the_result_says_what_happened() {
    let mut s = Snapshot::fixture();
    let broca = index(&s, "broca");
    let now = Snapshot::fixture_now();
    s.modules.data[broca].action = Some(ModuleAction {
        kind: ActionKind::Restart,
        phase: ActionPhase::Starting { pid: 81022 },
        started_at: now.add_secs(-12.0),
    });
    let app = modules_app(&s, "broca");
    let buf = render(&app, &s, 120, 36);
    let table_row = row(&buf, 7 + broca as u16);
    assert!(table_row.contains("◐ broca"), "{table_row}");
    assert!(table_row.contains("starting"), "{table_row}");
    assert!(row(&buf, 5).contains("restart · starting · pid 81022 · 12s"));
    // A second r while it runs does nothing.
    let mut app = app;
    assert!(!press(&mut app, &s, KeyCode::Char('r')));

    s.modules.data[broca].action = None;
    s.last_result = Some(ActionResult {
        at: now.add_secs(-1.0),
        text: "broca restarted · pid 81022 · healthy after 2.1 s".into(),
        ok: true,
    });
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 33).contains("✓ broca restarted · pid 81022 · healthy after 2.1 s"));
}

#[test]
fn l_opens_the_log_view_and_esc_returns() {
    let s = Snapshot::fixture_logs();
    let mut app = modules_app(&s, "broca");
    press(&mut app, &s, KeyCode::Char('l'));
    assert!(app.log.is_some());
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 3).contains("← broca · log"));
    // Errors only: the one error line is all that's left.
    press(&mut app, &s, KeyCode::Tab);
    press(&mut app, &s, KeyCode::Tab);
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 5).contains("error  tool call aft.bash timed out"));
    assert!(row(&buf, 6).trim().is_empty());
    // Search highlights without hiding.
    press(&mut app, &s, KeyCode::Tab);
    press(&mut app, &s, KeyCode::Char('/'));
    for ch in "429".chars() {
        press(&mut app, &s, KeyCode::Char(ch));
    }
    press(&mut app, &s, KeyCode::Enter);
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 3).contains("/ 429 2 matches"));
    press(&mut app, &s, KeyCode::Esc); // clears the search
    press(&mut app, &s, KeyCode::Esc); // closes the view
    assert!(app.log.is_none());
    assert_eq!(app.tab, Tab::Modules);
}

#[test]
fn the_log_on_screen_is_the_selected_modules_while_modules_shows() {
    let s = Snapshot::fixture();
    let mut app = App::new(Theme::test());
    assert_eq!(app.log_focus(&s), None);
    app.tab = Tab::Modules;
    app.mod_sel = index(&s, "broca");
    assert_eq!(app.log_focus(&s).as_deref(), Some("broca"));
}

#[test]
fn the_down_screen_starts_the_service_shows_the_log_and_says_how_it_waits() {
    let mut s = Snapshot::fixture_daemon_down();
    if let Daemon::Down(d) = &mut s.daemon.data {
        d.note = Some("Checking for the daemon every 2 s.".into());
        d.log_tail = vec![ck_ui::model::LogLine {
            at: None,
            level: ck_ui::model::LogLevel::Info,
            message: "subc daemon stopping: SIGTERM".into(),
        }];
    }
    let mut app = App::new(Theme::test());
    let buf = render(&app, &s, 120, 36);
    assert!((0..36).any(|y| row(&buf, y).contains("Checking for the daemon every 2 s.")));
    press(&mut app, &s, KeyCode::Enter);
    assert_eq!(app.requests, [Request::StartDaemon]);
    press(&mut app, &s, KeyCode::Char('l'));
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 3).contains("← daemon · log"));
    assert!(row(&buf, 5).contains("subc daemon stopping: SIGTERM"));
    press(&mut app, &s, KeyCode::Esc);
    assert!(!app.down_log);

    // When nothing recorded when or how the daemon stopped, the screen
    // shows "unknown".
    s.daemon.data = Daemon::Down(DaemonDown {
        stopped_at: None,
        stopped_by: String::new(),
        last_line: "unknown".into(),
        service: "unknown".into(),
        note: None,
        log_tail: Vec::new(),
    });
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    assert!((0..36).any(|y| row(&buf, y).contains("Stopped     unknown")));
}

// ---- columns, overflow and formats ----------------------------------------------

#[test]
fn no_cell_writes_past_its_column() {
    let mut s = Snapshot::fixture();
    let broca = index(&s, "broca");
    let m = &mut s.modules.data[broca];
    // The longest health word with no CPU reading, then absurd values.
    m.status = Status::Degraded;
    m.cpu_percent = None;
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    let y = 7 + broca as u16;
    let buf = render(&app, &s, 120, 36);
    let line = row(&buf, y);
    let cols: Vec<String> = line.chars().map(String::from).collect();
    let span = |a: usize, b: usize| cols[a..b].concat();
    assert_eq!(span(24, 34), "degraded  ", "{line}");
    assert_eq!(span(34, 39), "    –", "{line}");

    let m = &mut s.modules.data[broca];
    m.cpu_percent = Some(123_456.0);
    m.memory_mb = Some(9.9e9);
    m.started_at = Some(Snapshot::fixture_now().add_secs(-9.9e9));
    let buf = render(&app, &s, 120, 36);
    let line = row(&buf, y);
    let cols: Vec<String> = line.chars().map(String::from).collect();
    let span = |a: usize, b: usize| cols[a..b].concat();
    // Health ends with a space before CPU; CPU, Memory and Up each keep the
    // space before the next column.
    assert_eq!(span(24, 34), "degraded  ", "{line}");
    assert_eq!(&cols[39], " ", "{line}");
    assert_eq!(&cols[49], " ", "{line}");
    assert_eq!(span(58, 61), "   ", "{line}");
}

#[test]
fn home_cpu_now_shows_a_dash_for_a_missing_reading() {
    let mut s = Snapshot::fixture();
    s.modules.data[0].cpu_percent = None;
    let buf = render(&App::new(Theme::test()), &s, 160, 48);
    // The first grid slot is column 2, row 10; its CPU cell ends at 29.
    let line = row(&buf, 10);
    let cell: String = line.chars().skip(22).take(7).collect();
    assert_eq!(cell, "      –", "{line}");
}

fn many_agents(n: usize) -> Snapshot {
    let mut s = Snapshot::fixture();
    let template = s.agents.data.list[0].clone();
    s.agents.data.list = (0..n)
        .map(|i| {
            let mut a = template.clone();
            a.name = format!("AG{i:02}");
            a
        })
        .collect();
    s
}

#[test]
fn agents_say_how_many_more_they_leave_out() {
    // 120×36: rows 26 to 32 hold six agents and the note.
    let buf = render(&App::new(Theme::test()), &many_agents(33), 120, 36);
    let agents: Vec<String> = (26..=32)
        .map(|y| row(&buf, y).chars().skip(85).collect())
        .collect();
    assert!(agents[5].contains("AG05"), "{agents:?}");
    assert!(agents[6].starts_with("+27 more"), "{agents:?}");
    // 160×48: sixteen rows, then the summary line below.
    let buf = render(&App::new(Theme::test()), &many_agents(33), 160, 48);
    let text: Vec<String> = (27..46).map(|y| row(&buf, y)).collect();
    assert!(text.iter().any(|l| l.contains("+18 more")), "{text:?}");
    assert!(text.iter().any(|l| l.contains("2 machines")), "{text:?}");
}

#[test]
fn the_devices_row_agrees_with_the_online_count() {
    let mut s = Snapshot::fixture();
    // An offline device listed first in the reply.
    s.devices.data.list.rotate_right(1);
    assert_eq!(s.devices.data.list[0].status, Status::Stopped);
    let buf = render(&App::new(Theme::test()), &s, 160, 48);
    assert!(row(&buf, 3).contains("2 devices online"));
    let devices = row(&buf, 18);
    let first_chip = devices.find("● Mac 2").unwrap();
    let offline = devices.find("○ test-sim").unwrap();
    assert!(first_chip < offline, "{devices}");
    // 120×36 has room for two chips: the online ones, then "+1".
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    assert!(row(&buf, 20).ends_with("● Mac 2  ● iPhone +1  "));
}

#[test]
fn a_long_provider_name_keeps_a_space_before_its_value() {
    let mut s = Snapshot::fixture();
    s.usage.data.providers[0].name = "Claude-console".into();
    s.usage.data.providers[0].used = None;
    s.usage.data.providers[0].error = Some("no usage reported".into());
    s.usage.data.providers[1].used = None;
    s.usage.data.providers[1].balance = Some("$196.63 of $200.00 left".into());
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    assert!(
        row(&buf, 10).contains("Claude-console no usage reported"),
        "{}",
        row(&buf, 10)
    );
    assert!(
        row(&buf, 12).contains("Codex          $196.63 of $200.00 le…"),
        "{}",
        row(&buf, 12)
    );
}

#[test]
fn this_macs_memory_disk_and_load_fit_their_rows() {
    let mut s = Snapshot::fixture();
    let h = &mut s.host.data;
    h.memory_used_gb = Some(82.613);
    h.memory_total_gb = Some(128.0);
    h.disk_used_gb = Some(3_921.7);
    h.disk_total_gb = Some(3_996.0);
    h.load = Some([14.6, 20.6, 17.6]);
    let buf = render(&App::new(Theme::test()), &s, 160, 48);
    assert!(
        row(&buf, 20).contains("82.6 of 128 GB"),
        "{}",
        row(&buf, 20)
    );
    assert!(
        row(&buf, 21).contains("3,922 of 3,996 GB"),
        "{}",
        row(&buf, 21)
    );
    let load = row(&buf, 22);
    assert!(load.contains("14.6 · 20.6 · 17.6"), "{load}");
    assert!(!load.contains("1, 5"), "the span label doesn't fit: {load}");
    for y in 20..=22 {
        assert!(
            row(&buf, y).ends_with("  "),
            "row {y} stays inside the panel: {}",
            row(&buf, y)
        );
    }
}

#[test]
fn without_settings_no_key_offers_it() {
    let mut s = Snapshot::fixture();
    s.serves.settings = false;
    let mut app = modules_app(&s, "broca");
    let buf = render(&app, &s, 120, 36);
    assert!(!row(&buf, 35).contains("settings"), "{}", row(&buf, 35));
    assert!(row(&buf, 35).contains("l  logs"));
    app.overlay = Some(ck_ui::app::Overlay::Help);
    let buf = render(&app, &s, 120, 36);
    assert!((0..36).all(|y| !row(&buf, y).contains("settings")));
}

#[test]
fn an_empty_day_says_so_under_starts_and_exits() {
    let mut s = Snapshot::fixture();
    s.modules.data.truncate(5);
    s.process_events.data.clear();
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    let buf = render(&app, &s, 120, 36);
    let at = (0..36)
        .find(|&y| row(&buf, y).contains("Starts and exits"))
        .expect("the heading is drawn");
    assert!(row(&buf, at + 2).contains("No starts or exits today"));
}

#[test]
fn the_live_app_advertises_only_what_it_serves() {
    use ck_ui::app::Overlay;
    use ck_ui::model::Serves;
    use ck_ui::screens::overlays::help_columns;
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    s.commands = ck_ui::live::palette_commands(&s.modules.data);

    // The keys page lists the live app's keys, each of them handled below.
    let listed: Vec<String> = help_columns(s.serves)
        .iter()
        .flat_map(|(_, rows)| rows.iter().map(|(k, _)| k.clone()))
        .collect();
    assert_eq!(
        listed,
        [
            "1 – 2",
            ":",
            "?",
            "esc",
            "t",
            "q",
            "↑↓ j k",
            "⏎",
            "g G",
            "r",
            "s",
            "l",
            "m"
        ]
    );
    let mut app = App::new(Theme::test());
    app.overlay = Some(Overlay::Help);
    let buf = render(&app, &s, 120, 36);
    let page: String = (0..36).map(|y| row(&buf, y) + "\n").collect();
    for gone in [
        "restart, no downtime",
        "settings",
        "space",
        "filter",
        "double-click",
        "ckni",
        "3 Activity",
    ] {
        assert!(
            !page.contains(gone),
            "the live keys page offers {gone:?}:\n{page}"
        );
    }

    // Each listed key does something.
    let served =
        |app: &mut App, code: KeyCode| app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), &s);
    let mut app = App::new(Theme::test());
    for code in [
        KeyCode::Char('2'),
        KeyCode::Char('1'),
        KeyCode::Char(':'),
        KeyCode::Esc,
        KeyCode::Char('?'),
        KeyCode::Esc,
        KeyCode::Char('t'),
    ] {
        assert!(served(&mut app, code), "{code:?}");
    }
    assert!(
        served(&mut app, KeyCode::Down) && served(&mut app, KeyCode::Up),
        "move on Home"
    );
    assert!(served(&mut app, KeyCode::Enter), "open from Home");
    assert_eq!(app.tab, Tab::Modules);
    for code in [
        KeyCode::Char('G'),
        KeyCode::Char('g'),
        KeyCode::Char('j'),
        KeyCode::Char('k'),
    ] {
        assert!(served(&mut app, code), "{code:?}");
    }
    let broca = index(&s, "broca");
    app.mod_sel = broca;
    assert!(served(&mut app, KeyCode::Char('r')));
    app.confirm = None;
    assert!(served(&mut app, KeyCode::Char('s')));
    app.confirm = None;
    assert!(served(&mut app, KeyCode::Char('m')));
    assert!(served(&mut app, KeyCode::Esc));
    assert!(served(&mut app, KeyCode::Char('l')));
    assert!(app.log.is_some());
    assert!(served(&mut App::new(Theme::test()), KeyCode::Char('q')));

    // The Modules view shows no filter or settings hint, and only the two
    // tabs that have screens.
    let mut app = modules_app(&s, "broca");
    let buf = render(&app, &s, 120, 36);
    assert!(!row(&buf, 3).contains("/ to filter"), "{}", row(&buf, 3));
    assert!(!row(&buf, 35).contains("settings"));
    assert!(!row(&buf, 0).contains("3 Activity"), "{}", row(&buf, 0));
    assert!(row(&buf, 0).contains("2 Modules"));

    // Every palette command runs: a restart request or the log view.
    for cmd in s.commands.clone() {
        app = App::new(Theme::test());
        app.overlay = Some(Overlay::Palette);
        app.palette_query = cmd.title.clone();
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &s);
        let did = !app.requests.is_empty()
            || app.confirm.is_some()
            || app.log.is_some()
            || app.metrics.is_some()
            || app.overlay.is_some()
            || app.quit
            || app.theme.mode() != Theme::test().mode()
            || app.tab == Tab::Modules
            || cmd.title == "Home";
        assert!(did, "{:?} did nothing", cmd.title);
    }
}

#[test]
fn a_long_device_name_is_cut_so_every_online_device_shows() {
    let mut s = Snapshot::fixture();
    let template = s.devices.data.list[0].clone();
    let device = |name: &str, status: Status| {
        let mut d = template.clone();
        d.name = name.into();
        d.short_name = name.into();
        d.status = status;
        d
    };
    s.devices.data.list = vec![
        device("lab-iphone-sim-pool-1", Status::Ok),
        device("Mac 2", Status::Ok),
        device("old-ipad", Status::Stopped),
        device("test-sim", Status::Stopped),
        device("spare-mac", Status::Stopped),
    ];
    assert_eq!("lab-iphone-sim-pool-1".len(), 21);
    s.devices.data.online = 2;
    let buf = render(&App::new(Theme::test()), &s, 160, 48);
    assert!(row(&buf, 3).contains("2 devices online"));
    let line = row(&buf, 18);
    let devices: String = line.chars().skip(112).collect();
    // Both online devices, the long name cut, then the three offline ones
    // as "+3".
    assert!(devices.contains("● lab-iphone-sim"), "{devices}");
    assert!(devices.contains("…  ● Mac 2 +3"), "{devices}");
    // Never cut below eight characters and the ellipsis.
    let (names, shown) =
        ck_ui::screens::home::device_chips(&["lab-iphone-sim-pool-1", "Mac 2"], 2, 12);
    assert!(names.iter().all(|n| n.chars().count() >= 5));
    assert!(names[0].chars().count() >= 9, "{names:?}");
    assert!(shown <= 2);
}

// ---- the pane, all metrics, palette, tab bar, usage names ---------------------------

#[test]
fn the_pane_keeps_its_chart_and_log_with_a_long_sentence_and_many_metrics() {
    use ck_ui::model::Metric;
    let mut s = Snapshot::fixture();
    let broca = index(&s, "broca");
    let m = &mut s.modules.data[broca];
    m.summary = "A module sentence long enough to wrap across several lines of the pane, saying in full what the module wants said about its health right now.".into();
    m.metrics = (0..30)
        .map(|i| Metric {
            label: format!("a_rather_long_metric_name_{i}"),
            value: format!("{i}"),
        })
        .collect();
    let app = modules_app(&s, "broca");
    let buf = render(&app, &s, 120, 36);
    let pane: Vec<String> = (3..33)
        .map(|y| row(&buf, y).chars().skip(77).collect())
        .collect();
    assert!(
        pane.iter().any(|l| l.starts_with("A module sentence long")),
        "{pane:#?}"
    );
    assert!(
        pane.iter().any(|l| l.contains("of the pane")),
        "the sentence wraps: {pane:#?}"
    );
    assert!(
        pane.iter().any(|l| l.starts_with("CPU and memory")),
        "{pane:#?}"
    );
    assert!(pane.iter().any(|l| l.starts_with("Log")), "{pane:#?}");
    assert!(
        pane.iter().any(|l| l.contains("more")),
        "the metrics left out are counted: {pane:#?}"
    );
    // Labels keep a gap before their values.
    let metric = pane
        .iter()
        .find(|l| l.starts_with("a_rather_long_metric_name_0"))
        .unwrap();
    assert!(
        metric.starts_with("a_rather_long_metric_name_0  "),
        "{metric:?}"
    );
}

#[test]
fn m_lists_every_metric_with_its_full_key() {
    use ck_ui::model::{Metric, Serves};
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    let broca = index(&s, "broca");
    s.modules.data[broca].all_metrics = vec![
        Metric {
            label: "build_server.permits_held".into(),
            value: "80".into(),
        },
        Metric {
            label: "control_deliveries".into(),
            value: "76,000".into(),
        },
    ];
    let mut app = modules_app(&s, "broca");
    assert!(row(&render(&app, &s, 120, 36), 35).contains("m  all metrics"));
    press(&mut app, &s, KeyCode::Char('m'));
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 3).contains("← broca · all metrics"));
    assert!(
        row(&buf, 5).contains("build_server.permits_held  80"),
        "{}",
        row(&buf, 5)
    );
    assert!(
        row(&buf, 6).contains("control_deliveries         76,000"),
        "{}",
        row(&buf, 6)
    );
    press(&mut app, &s, KeyCode::Esc);
    assert!(app.metrics.is_none());
}

#[test]
fn the_palette_ranks_prefix_then_word_start_then_scattered() {
    use ck_ui::model::PaletteCommand;
    use ck_ui::screens::overlays::matches;
    let cmd = |t: &str| PaletteCommand {
        title: t.into(),
        area: "Modules".into(),
    };
    let commands = [
        cmd("Restart magic-context"),
        cmd("Restart motor"),
        cmd("motor logs"),
        cmd("Some other thing"),
    ];
    let titles = |q: &str, p: Option<&str>| -> Vec<String> {
        matches(&commands, q, p)
            .iter()
            .map(|c| c.title.clone())
            .collect()
    };
    assert_eq!(
        titles("mot", None),
        [
            "motor logs",
            "Restart motor",
            "Restart magic-context",
            "Some other thing"
        ]
    );
    // With nothing typed, the selected module's commands come first.
    assert_eq!(titles("", Some("motor"))[0], "Restart motor");
}

#[test]
fn the_tab_bar_keeps_names_when_they_fit() {
    use ck_ui::model::Serves;
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    let buf = render(&App::new(Theme::test()), &s, 80, 24);
    assert!(
        row(&buf, 0).contains("1 Home  2 Modules"),
        "{}",
        row(&buf, 0)
    );
}

#[test]
fn the_usage_name_column_fits_the_longest_name_shown() {
    let mut s = Snapshot::fixture();
    s.usage.data.providers[0].name = "Claude Console".into();
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    assert!(
        row(&buf, 10).contains("Claude Console █"),
        "{}",
        row(&buf, 10)
    );
    // Never wider than 16 for the name.
    s.usage.data.providers[0].name = "A provider with a very long name".into();
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    assert!(
        row(&buf, 10).contains("A provider with… "),
        "{}",
        row(&buf, 10)
    );
}

#[test]
fn no_pointer_to_a_screen_the_app_does_not_have() {
    use ck_ui::model::Serves;
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    let buf = render(&App::new(Theme::test()), &s, 80, 24);
    let text: String = (0..24).map(|y| row(&buf, y) + "\n").collect();
    assert!(text.contains("more providers"), "{text}");
    assert!(!text.contains("on 4 Usage"), "{text}");
    let buf = render(&App::new(Theme::test()), &s, 160, 48);
    let text: String = (0..48).map(|y| row(&buf, y) + "\n").collect();
    assert!(!text.contains("Approvals"), "{text}");
}

// ---- broca's shape, empty health, counts, log errors ------------------------------

#[test]
fn the_restart_question_names_brocas_runs_in_flight() {
    use ck_ui::live::{assemble, State};
    // broca's real shape: dozens of top-level metrics, a runs_in_flight
    // number among them, no headline and no detail sentence.
    let mut metrics = serde_json::Map::new();
    for i in 0..36 {
        metrics.insert(format!("counter_{i:02}"), serde_json::json!(i));
    }
    metrics.insert("runs_in_flight".into(), serde_json::json!(37));
    let now = Snapshot::fixture_now();
    let mut state = State::empty("This Mac");
    let roster: Vec<subc_control::SupervisorEntry> = serde_json::from_value(serde_json::json!([
        { "module_id": "broca", "state": "running", "enabled": true, "live": true,
          "health": "ok", "drain_timeout_ms": 30000 }
    ]))
    .unwrap();
    state.record_roster(roster, now);
    state.health.set(
        vec![serde_json::from_value(serde_json::json!({
            "module_id": "broca", "status": "ok", "consecutive_failures": 0,
            "metrics": serde_json::Value::Object(metrics)
        }))
        .unwrap()],
        now,
    );
    let snap = assemble(&state, now);
    let broca = &snap.modules.data[0];
    assert!(broca.metrics.is_empty(), "no headline, nothing in the pane");
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    press(&mut app, &snap, KeyCode::Char('r'));
    let q = app.confirm.expect("asks first");
    assert!(
        q.consequence.contains("runs_in_flight 37"),
        "{}",
        q.consequence
    );
    assert!(q.consequence.contains("up to 30s"), "{}", q.consequence);
}

#[test]
fn health_with_nothing_reported_says_so() {
    let mut s = Snapshot::fixture();
    let broca = index(&s, "broca");
    s.modules.data[broca].summary.clear();
    s.modules.data[broca].metrics.clear();
    let app = modules_app(&s, "broca");
    let buf = render(&app, &s, 120, 36);
    let pane = |y: u16| -> String { row(&buf, y).chars().skip(77).collect() };
    let at = (0..36).find(|&y| pane(y).starts_with("Health")).unwrap();
    let line = pane(at + 2);
    assert!(line.starts_with("No details reported"), "{line:?}");
}

#[test]
fn counts_of_one_are_singular() {
    let mut s = Snapshot::fixture();
    s.agents.data.working = 1;
    s.modules.data.truncate(2);
    s.modules.data[1].status = Status::Degraded;
    let buf = render(&App::new(Theme::test()), &s, 120, 36);
    assert!(
        row(&buf, 3).contains("1 of 2 modules healthy"),
        "{}",
        row(&buf, 3)
    );
    assert!(row(&buf, 3).contains("1 agent working"), "{}", row(&buf, 3));
    let buf = render(&App::new(Theme::test()), &s, 160, 48);
    assert!(row(&buf, 3).contains("1 agent working"), "{}", row(&buf, 3));
}

#[test]
fn a_log_that_cant_be_read_shows_its_error_not_an_empty_log() {
    use ck_ui::model::{LogView, Panel};
    let mut s = Snapshot::fixture();
    let view = LogView {
        module: "broca".into(),
        lines: Vec::new(),
        source: String::new(),
    };
    s.log_view = Some(Panel {
        error: Some("permission denied".into()),
        ..Panel::waiting(view.clone(), Some(2.0))
    });
    let mut app = modules_app(&s, "broca");
    press(&mut app, &s, KeyCode::Char('l'));
    let buf = render(&app, &s, 120, 36);
    assert!(
        row(&buf, 5).contains("Couldn't read the log: permission denied"),
        "{}",
        row(&buf, 5)
    );
    // Still being read: says so.
    s.log_view = Some(Panel::waiting(view.clone(), Some(2.0)));
    let buf = render(&app, &s, 120, 36);
    assert!(
        row(&buf, 5).contains("Reading the log…"),
        "{}",
        row(&buf, 5)
    );
    // Read, and empty.
    s.log_view = Some(Panel::new(Snapshot::fixture_now(), view));
    let buf = render(&app, &s, 120, 36);
    assert!(row(&buf, 5).contains("No lines yet"), "{}", row(&buf, 5));
}
