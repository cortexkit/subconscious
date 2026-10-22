//! The Activity screen: day labels, the list's last row, the kind chips and
//! the filter, the detail pane, the keys, and the list at every window size.

use ck_ui::app::{App, Request, Tab};
use ck_ui::model::{ActivityKey, ActivityLog, ActivityRow, ActivityRowKind, Serves};
use ck_ui::screens::activity::{
    details_line, details_lines, layout, offset_label, pane_rows, Chip,
};
use ck_ui::time::UtcOffset;
use ck_ui::{Snapshot, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::{json, Map, Value};

fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buf, snap, Snapshot::fixture_now());
    buf
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn text(buf: &Buffer, x: u16, y: u16, n: u16) -> String {
    (x..x + n).map(|x| buf[(x, y)].symbol()).collect()
}

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) -> bool {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap)
}

fn activity_app() -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Activity;
    app
}

/// A ledger entry recorded `secs_ago` seconds before the fixture's now.
fn entry(seq: u64, secs_ago: f64, subject: &str, what: &str, kind: &str) -> ActivityRow {
    ActivityRow {
        key: ActivityKey::Entry {
            incarnation: "inc-1".into(),
            seq,
        },
        partner: None,
        row: ActivityRowKind::Entry,
        at: Snapshot::fixture_now().add_secs(-secs_ago),
        subject: subject.into(),
        what: what.into(),
        who: Some(subject.into()),
        by: None,
        by_agent: false,
        kind: Some(kind.into()),
        seq: Some(seq),
        build: None,
        details: Some(Map::new()),
    }
}

/// A process exit `secs_ago` seconds before the fixture's now.
fn exit(secs_ago: f64, module: &str) -> ActivityRow {
    let at = Snapshot::fixture_now().add_secs(-secs_ago);
    ActivityRow {
        key: ActivityKey::Exit {
            module: module.into(),
            at,
        },
        partner: None,
        row: ActivityRowKind::Exit,
        at,
        subject: module.into(),
        what: "exited with status 1".into(),
        who: None,
        by: None,
        by_agent: false,
        kind: None,
        seq: None,
        build: None,
        details: None,
    }
}

fn with_rows(rows: Vec<ActivityRow>, exhausted: bool) -> Snapshot {
    let mut s = Snapshot::fixture();
    s.activity_log.data = ActivityLog {
        rows,
        history_exhausted: exhausted,
        newer_unloaded: false,
    };
    s
}

/// `n` restarts, one every `step` seconds back from now, newest first, with
/// seqs counting down from 10,000.
fn many(n: usize, step: f64) -> Vec<ActivityRow> {
    (0..n)
        .map(|i| {
            let mut r = entry(
                10_000 - i as u64,
                30.0 + i as f64 * step,
                &format!("mod-{i:03}"),
                "restarted on build 0a1b2c3d",
                "subc.module_restarted",
            );
            r.details = Some(json!({"drain_ms": i}).as_object().unwrap().clone());
            r
        })
        .collect()
}

/// The text of every screen row from `top` to `bottom`, trimmed.
fn rows_between(buf: &Buffer, x: u16, w: u16, top: u16, bottom: u16) -> Vec<String> {
    (top..=bottom)
        .map(|y| text(buf, x, y, w).trim_end().to_string())
        .collect()
}

// ---- day labels and the footer's count ------------------------------------------

#[test]
fn day_labels_read_today_yesterday_then_the_date_and_count_toward_nothing() {
    const DAY: f64 = 86_400.0;
    let rows = vec![
        entry(5, 3600.0, "engram", "restarted", "subc.module_restarted"),
        exit(4000.0, "motor"),
        entry(4, DAY, "motor", "restarted", "subc.module_restarted"),
        entry(3, 3.0 * DAY, "aft", "restarted", "subc.module_restarted"),
    ];
    let snap = with_rows(rows, true);
    let buf = render(&activity_app(), &snap, 120, 36);
    let list = rows_between(&buf, 2, 70, 5, 12);
    // The fixture's now is Tuesday 20 October 2026, 20:48 UTC.
    assert_eq!(list[0], "Today");
    assert!(list[1].starts_with("19:48  engram"), "{}", list[1]);
    assert!(list[2].starts_with("19:41  motor"), "{}", list[2]);
    assert_eq!(list[3], "Yesterday");
    assert!(list[4].starts_with("20:48  motor"), "{}", list[4]);
    assert_eq!(list[5], "Saturday 17 Oct");
    assert!(list[6].starts_with("20:48  aft"), "{}", list[6]);
    assert_eq!(list[7], "", "nothing below the last entry");
    // "N today" counts today's loaded rows, the exit included, and no
    // label; a filter doesn't change it.
    assert!(row(&buf, 35).contains("2 today · ledger and process events"));
    let mut app = activity_app();
    app.activity.filter = "engram".into();
    let buf = render(&app, &snap, 120, 36);
    assert!(row(&buf, 35).contains("2 today · ledger and process events"));
}

#[test]
fn the_day_counts_in_local_time() {
    // 20:48 UTC on the 20th is 00:48 on the 21st four hours east: half an
    // hour ago is Today there, an hour ago is Yesterday, and two days ago
    // is Monday the 19th.
    let mut snap = with_rows(
        vec![
            entry(3, 1800.0, "engram", "restarted", "subc.module_restarted"),
            entry(2, 3600.0, "motor", "restarted", "subc.module_restarted"),
            entry(
                1,
                2.0 * 86_400.0,
                "aft",
                "restarted",
                "subc.module_restarted",
            ),
        ],
        true,
    );
    snap.host.data.utc_offset = UtcOffset(4 * 3600);
    let buf = render(&activity_app(), &snap, 120, 36);
    let list = rows_between(&buf, 2, 70, 5, 10);
    assert_eq!(list[0], "Today");
    assert!(list[1].starts_with("00:18  engram"), "{}", list[1]);
    assert_eq!(list[2], "Yesterday");
    assert!(list[3].starts_with("23:48  motor"), "{}", list[3]);
    assert_eq!(list[4], "Monday 19 Oct");
    assert!(list[5].starts_with("00:48  aft"), "{}", list[5]);
    assert!(row(&buf, 35).contains(" 1 today"));
}

// ---- the list's last row ---------------------------------------------------------

#[test]
fn two_hundred_entries_end_with_how_many_more() {
    // All today: "Today", 26 entries, then "+174 more" on the last row.
    let snap = with_rows(many(200, 5.0), false);
    let buf = render(&activity_app(), &snap, 120, 36);
    assert_eq!(text(&buf, 2, 5, 5), "Today");
    assert!(row(&buf, 31).contains("mod-025"), "{}", row(&buf, 31));
    assert_eq!(text(&buf, 2, 32, 12), "+174 more   ");

    // Ten today and the rest yesterday: the "Yesterday" label takes a row
    // and counts toward nothing, so 25 entries show and 175 are below.
    let mut rows = many(200, 5.0);
    for r in rows.iter_mut().skip(10) {
        r.at = r.at.add_secs(-86_400.0);
    }
    let buf = render(&activity_app(), &with_rows(rows, false), 120, 36);
    assert_eq!(text(&buf, 2, 16, 9), "Yesterday");
    assert_eq!(text(&buf, 2, 32, 12), "+175 more   ");
}

#[test]
fn the_bottom_reads_start_of_history_or_scroll_for_older_entries() {
    for (exhausted, note) in [
        (true, "start of history"),
        (false, "scroll for older entries"),
    ] {
        let snap = with_rows(many(200, 5.0), exhausted);
        let mut app = activity_app();
        press(&mut app, &snap, KeyCode::Char('G'));
        let buf = render(&app, &snap, 120, 36);
        assert!(row(&buf, 31).contains("mod-199"), "{}", row(&buf, 31));
        assert_eq!(
            text(&buf, 2, 32, note.len() as u16 + 2).trim_end(),
            note,
            "exhausted: {exhausted}"
        );
    }
    // Rows that exactly reach the last row leave it to a note too: 27 rows
    // and their label fill the 28 rows, so one is below the view.
    let buf = render(&activity_app(), &with_rows(many(27, 5.0), true), 120, 36);
    assert_eq!(text(&buf, 2, 32, 10).trim_end(), "+1 more");
    // One fewer and they all fit, with nothing drawn below.
    let buf = render(&activity_app(), &with_rows(many(26, 5.0), true), 120, 36);
    assert!(row(&buf, 31).contains("mod-025"));
    assert_eq!(text(&buf, 1, 32, 74).trim(), "");
}

// ---- chips and the filter -------------------------------------------------------

fn kinds_snapshot() -> Snapshot {
    let mut agent = entry(
        6,
        60.0,
        "prefrontal-core",
        "started a worker",
        "prefrontal-core.worker_started",
    );
    agent.by_agent = true;
    agent.who = Some("IRIS".into());
    with_rows(
        vec![
            entry(7, 30.0, "engram", "restarted", "subc.module_restarted"),
            agent,
            entry(
                5,
                90.0,
                "cingulate",
                "approved a tool permission",
                "cingulate.consent_answered",
            ),
            entry(4, 120.0, "subc", "settings changed", "subc.config_changed"),
            entry(
                3,
                150.0,
                "basal",
                "flow delivered 3 notices",
                "basal.flow_delivered",
            ),
            exit(180.0, "motor"),
        ],
        true,
    )
}

fn list_subjects(buf: &Buffer) -> Vec<String> {
    (6..32)
        .map(|y| text(buf, 9, y, 17).trim_end().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

#[test]
fn tab_cycles_the_kind_chips_and_wraps() {
    let snap = kinds_snapshot();
    let mut app = activity_app();
    let mut seen = Vec::new();
    for _ in 0..6 {
        let buf = render(&app, &snap, 120, 36);
        seen.push((app.activity.chip, list_subjects(&buf)));
        assert!(press(&mut app, &snap, KeyCode::Tab));
    }
    let want: Vec<(Chip, Vec<&str>)> = vec![
        (
            Chip::All,
            vec![
                "engram",
                "prefrontal-core",
                "cingulate",
                "subc",
                "basal",
                "motor",
            ],
        ),
        (Chip::Modules, vec!["engram", "motor"]),
        (Chip::Agents, vec!["prefrontal-core"]),
        (Chip::Approvals, vec!["cingulate"]),
        (Chip::Settings, vec!["subc"]),
        // Back to All: basal's kind matches no chip and shows only here.
        (
            Chip::All,
            vec![
                "engram",
                "prefrontal-core",
                "cingulate",
                "subc",
                "basal",
                "motor",
            ],
        ),
    ];
    let got: Vec<(Chip, Vec<&str>)> = seen
        .iter()
        .map(|(c, s)| (*c, s.iter().map(String::as_str).collect()))
        .collect();
    assert_eq!(got, want);
    // Shift-tab goes the other way, wrapping too.
    assert!(press(&mut app, &snap, KeyCode::BackTab));
    assert_eq!(app.activity.chip, Chip::All);
    assert!(press(&mut app, &snap, KeyCode::BackTab));
    assert_eq!(app.activity.chip, Chip::Settings);
}

#[test]
fn the_active_chip_is_drawn_as_a_key() {
    let snap = kinds_snapshot();
    let mut app = activity_app();
    app.activity.chip = Chip::Approvals;
    let buf = render(&app, &snap, 120, 36);
    let key = Theme::test().bg(ck_ui::theme::Bg::Key);
    assert_eq!(text(&buf, 39, 3, 11), " Approvals ");
    assert!((39..50).all(|x| buf[(x, 3)].bg == key));
    assert!(buf[(14, 3)].bg != key, "All is not the active chip");
}

#[test]
fn slash_filters_the_loaded_entries_by_text() {
    let snap = kinds_snapshot();
    let mut app = activity_app();
    assert!(press(&mut app, &snap, KeyCode::Char('/')));
    // Typed letters go to the filter, never to the app's own keys.
    for ch in "TOOL".chars() {
        assert!(press(&mut app, &snap, KeyCode::Char(ch)));
    }
    assert!(!app.quit);
    let buf = render(&app, &snap, 120, 36);
    assert_eq!(list_subjects(&buf), ["cingulate"]);
    assert!(row(&buf, 3).contains("/ TOOL▏1 match"), "{}", row(&buf, 3));
    // The filter matches who did it and the kind too, and takes the chip
    // into account.
    app.activity.filter = "iris".into();
    let buf = render(&app, &snap, 120, 36);
    assert_eq!(list_subjects(&buf), ["prefrontal-core"]);
    app.activity.filter = "subc.module".into();
    let buf = render(&app, &snap, 120, 36);
    assert_eq!(list_subjects(&buf), ["engram"]);
    app.activity.chip = Chip::Approvals;
    let buf = render(&app, &snap, 120, 36);
    assert!(list_subjects(&buf).is_empty());
    assert!(row(&buf, 5).contains("No loaded entry matches"));
    // ⏎ keeps the filter and closes the field; esc then clears it.
    app.activity.chip = Chip::All;
    assert!(press(&mut app, &snap, KeyCode::Enter));
    assert!(app.field.is_none());
    assert_eq!(app.activity.filter, "subc.module");
    assert!(press(&mut app, &snap, KeyCode::Esc));
    assert!(app.activity.filter.is_empty());
}

// ---- the detail pane ------------------------------------------------------------

/// The pane's rows from 5 to the list's last row, trimmed, with blank rows
/// kept.
fn pane_text(buf: &Buffer, h: u16) -> Vec<String> {
    rows_between(buf, 79, 39, 5, h - 4)
}

#[test]
fn an_exit_row_shows_only_its_summary_time_by_subject_and_kind() {
    let snap = with_rows(
        vec![
            exit(60.0, "motor"),
            entry(1, 90.0, "engram", "restarted", "subc.module_restarted"),
        ],
        true,
    );
    let buf = render(&activity_app(), &snap, 120, 36);
    let pane = pane_text(&buf, 36);
    let drawn: Vec<&str> = pane
        .iter()
        .map(String::as_str)
        .filter(|l| !l.is_empty())
        .collect();
    assert_eq!(
        drawn,
        [
            "motor exited with status 1",
            "20:47:00 local · UTC",
            "By        subc",
            "Subject   module motor",
            "Kind      process exit",
        ]
    );
    assert_eq!(
        pane_rows(&snap.activity_log.data.rows[0])
            .iter()
            .map(|(k, _)| *k)
            .collect::<Vec<_>>(),
        ["By", "Subject", "Kind"]
    );
}

#[test]
fn an_entry_shows_by_subject_kind_seq_build_and_its_details() {
    let mut r = entry(
        48_213,
        60.0,
        "engram",
        "restarted on build 9d8a178c",
        "subc.module_restarted",
    );
    r.who = Some("direct".into());
    r.build = Some("7c3a47b9 → 9d8a178c".into());
    r.details = Some(json!({"drain_ms": 412}).as_object().unwrap().clone());
    let mut plain = entry(48_212, 90.0, "motor", "restarted", "subc.module_restarted");
    plain.details = Some(Map::new());
    let snap = with_rows(vec![r, plain], true);
    let buf = render(&activity_app(), &snap, 120, 36);
    let pane = pane_text(&buf, 36);
    assert_eq!(
        pane[..13],
        [
            "engram restarted on build 9d8a178c",
            "20:47:00 local · UTC",
            "",
            "By        direct",
            "Subject   module engram",
            "Kind      subc.module_restarted",
            "Seq       48,213",
            "Build     7c3a47b9 → 9d8a178c",
            "",
            "Details",
            "  {",
            "    \"drain_ms\": 412",
            "  }",
        ]
    );
    // The details print in the faint role.
    let faint = Theme::test().style(ck_ui::theme::St::F).fg;
    assert_eq!(Some(buf[(83, 16)].fg), faint);

    // Without a build the Build row is left out, and empty details draw no
    // block.
    let mut app = activity_app();
    press(&mut app, &snap, KeyCode::Down);
    let buf = render(&app, &snap, 120, 36);
    let pane = pane_text(&buf, 36);
    assert_eq!(pane[6], "Seq       48,212");
    assert!(pane[7..].iter().all(String::is_empty), "{pane:?}");
}

#[test]
fn details_print_sorted_at_every_level_whatever_order_they_arrive_in() {
    // Inserted out of order. A workspace build turns on serde_json's
    // `preserve_order`, where the map keeps this order; the pane must still
    // sort.
    let mut inner = Map::new();
    inner.insert("y".into(), json!(2));
    let mut deep = Map::new();
    deep.insert("d".into(), json!(1));
    deep.insert("c".into(), json!(2));
    inner.insert("b".into(), Value::Array(vec![Value::Object(deep)]));
    let mut details = Map::new();
    details.insert("zeta".into(), json!(1));
    details.insert("alpha".into(), Value::Object(inner));
    details.insert("Mid".into(), json!("x"));
    details.insert("empty".into(), json!({}));
    let want = [
        "{",
        "  \"Mid\": \"x\",",
        "  \"alpha\": {",
        "    \"b\": [",
        "      {",
        "        \"c\": 2,",
        "        \"d\": 1",
        "      }",
        "    ],",
        "    \"y\": 2",
        "  },",
        "  \"empty\": {},",
        "  \"zeta\": 1",
        "}",
    ];
    assert_eq!(details_lines(&details), want);
    assert_eq!(
        details_line(&details),
        r#"{"Mid":"x","alpha":{"b":[{"c":2,"d":1}],"y":2},"empty":{},"zeta":1}"#
    );

    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(details);
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 40);
    let pane = pane_text(&buf, 40);
    let at = pane.iter().position(|l| l == "Details").unwrap();
    let drawn: Vec<String> = pane[at + 1..at + 1 + want.len()]
        .iter()
        .map(|l| l.strip_prefix("  ").unwrap_or(l).to_string())
        .collect();
    assert_eq!(drawn, want);
}

#[test]
fn details_that_overflow_the_pane_end_with_how_many_lines_more() {
    // Ten keys: twelve lines with the braces.
    let details: Map<String, Value> = (0..10).map(|i| (format!("key_{i:02}"), json!(i))).collect();
    assert_eq!(details_lines(&details).len(), 12);
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(details);
    let snap = with_rows(vec![r], true);
    // At 120×24 the list ends on row 20: "Details" on 13, then 7 rows, so 6
    // lines and "+6 more lines".
    let buf = render(&activity_app(), &snap, 120, 24);
    let pane = pane_text(&buf, 24);
    let at = pane.iter().position(|l| l == "Details").unwrap();
    assert_eq!(pane[at + 1], "  {");
    assert_eq!(pane[at + 6], "    \"key_04\": 4,");
    assert_eq!(pane[at + 7], "  +6 more lines");
    assert_eq!(at + 7, pane.len() - 1, "the note is the pane's last row");

    // At 120×36 thirty keys run out of room the same way.
    let details: Map<String, Value> = (0..30).map(|i| (format!("key_{i:02}"), json!(i))).collect();
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(details);
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 36);
    let pane = pane_text(&buf, 36);
    // "Details" on row 13, then rows 14 to 32: 18 lines and the note.
    assert_eq!(pane.last().unwrap(), "  +14 more lines");

    // A line wider than the pane ends with "…".
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(
        json!({"path": "/Users/alex/Library/Application Support/cortexkit/modules/engram/bin"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 36);
    let pane = pane_text(&buf, 36);
    let long = pane.iter().find(|l| l.contains("\"path\"")).unwrap();
    assert!(long.ends_with('…'), "{long}");
    assert_eq!(ck_ui::draw::width(long), 39);
}

#[test]
fn null_details_draw_no_block() {
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = None;
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 36);
    assert!(!pane_text(&buf, 36).iter().any(|l| l == "Details"));
}

#[test]
fn the_pane_names_the_utc_offset() {
    assert_eq!(offset_label(UtcOffset(0)), "UTC");
    assert_eq!(offset_label(UtcOffset(7200)), "UTC+2");
    assert_eq!(offset_label(UtcOffset(-12_600)), "UTC-3:30");
    assert_eq!(offset_label(UtcOffset(20_700)), "UTC+5:45");
}

// ---- keys -----------------------------------------------------------------------

#[test]
fn enter_on_a_module_subject_selects_it_on_modules() {
    let snap = Snapshot::fixture();
    let mut app = activity_app();
    // The fixture's fourth row is motor's settings change.
    for _ in 0..3 {
        assert!(press(&mut app, &snap, KeyCode::Down));
    }
    assert!(press(&mut app, &snap, KeyCode::Enter));
    assert_eq!(app.tab, Tab::Modules);
    assert_eq!(snap.modules.data[app.mod_sel].id, "motor");
    // esc comes back to the same row.
    assert!(press(&mut app, &snap, KeyCode::Esc));
    assert_eq!(app.tab, Tab::Activity);
    let buf = render(&app, &snap, 120, 36);
    assert!(text(&buf, 79, 5, 39).starts_with("motor settings changed"));

    // The last row's subject, the daemon, is no module: ⏎ does nothing.
    let mut app = activity_app();
    press(&mut app, &snap, KeyCode::Char('G'));
    assert!(!press(&mut app, &snap, KeyCode::Enter));
    assert_eq!(app.tab, Tab::Activity);
}

#[test]
fn moving_tells_the_live_layer_where_the_view_is() {
    let snap = Snapshot::fixture();
    let rows = &snap.activity_log.data.rows;
    let mut app = activity_app();
    press(&mut app, &snap, KeyCode::Down);
    assert_eq!(
        app.requests,
        [Request::ActivityView {
            anchor: Some(rows[1].key.clone()),
            older: false
        }]
    );
    app.requests.clear();
    // Back at the top the view follows the newest entries.
    press(&mut app, &snap, KeyCode::Up);
    assert_eq!(
        app.requests,
        [Request::ActivityView {
            anchor: None,
            older: false
        }]
    );
    app.requests.clear();
    // The oldest loaded row asks for the next older page, and so does
    // pressing on past it.
    press(&mut app, &snap, KeyCode::Char('G'));
    let last = Some(rows.last().unwrap().key.clone());
    assert_eq!(
        app.requests,
        [Request::ActivityView {
            anchor: last.clone(),
            older: true
        }]
    );
    app.requests.clear();
    assert!(press(&mut app, &snap, KeyCode::Down));
    assert_eq!(
        app.requests,
        [Request::ActivityView {
            anchor: last,
            older: true
        }]
    );
    // Once history is exhausted there is nothing older to ask for.
    let mut done = snap.clone();
    done.activity_log.data.history_exhausted = true;
    app.requests.clear();
    assert!(!press(&mut app, &done, KeyCode::Down));
    assert!(app.requests.is_empty());
}

#[test]
fn a_selection_stays_on_its_row_as_newer_entries_arrive() {
    let snap = with_rows(many(50, 5.0), true);
    let mut app = activity_app();
    for _ in 0..5 {
        press(&mut app, &snap, KeyCode::Down);
    }
    // Three newer entries arrive above the loaded ones.
    let mut rows: Vec<ActivityRow> = (0..3)
        .map(|i| {
            entry(
                20_000 - i,
                1.0 + i as f64,
                &format!("new-{i}"),
                "restarted",
                "subc.module_restarted",
            )
        })
        .collect();
    rows.extend(many(50, 5.0));
    let buf = render(&app, &with_rows(rows, true), 120, 36);
    assert!(text(&buf, 79, 5, 39).starts_with("mod-005"));
}

// ---- sizes ----------------------------------------------------------------------

/// 240 entries over three days, with exits among them.
fn crowded() -> Snapshot {
    let mut rows = many(240, 1200.0);
    for i in (7..240).step_by(20) {
        let at = rows[i].at;
        let mut x = exit(0.0, "motor");
        x.at = at.add_secs(1.0);
        x.key = ActivityKey::Exit {
            module: "motor".into(),
            at: x.at,
        };
        rows.insert(i, x);
    }
    with_rows(rows, false)
}

/// The footer's hints that show, each whole: every hint is either drawn in
/// full or not at all, and the ones drawn are a prefix of the list.
fn assert_whole_hints(buf: &Buffer, w: u16, h: u16) {
    let footer = row(buf, h - 1);
    let hints = ["↑↓  move", "⏎  open subject", "/  filter", "tab  kind"];
    let shown: Vec<bool> = hints.iter().map(|hint| footer.contains(hint)).collect();
    let n = shown.iter().take_while(|s| **s).count();
    assert!(shown[n..].iter().all(|s| !s), "{w}×{h}: {footer}");
    assert!(n >= 1, "{w}×{h}: {footer}");
    for word in ["move", "open", "subject", "filter", "kind"] {
        let cut = footer
            .split_whitespace()
            .any(|t| t != word && word.starts_with(t) && t.len() > 1);
        assert!(!cut, "{w}×{h}: {word} cut: {footer}");
    }
}

#[test]
fn the_list_at_every_size_says_how_many_rows_it_leaves_out() {
    let snap = crowded();
    let total = snap.activity_log.data.counted();
    assert!(total >= 200);
    let app = activity_app();
    for (w, h) in [(80, 24), (100, 30), (140, 40), (160, 48), (200, 60)] {
        let buf = render(&app, &snap, w, h);
        let l = layout(i32::from(w), i32::from(h));
        let last = l.last as u16;
        // Every row from the heading's rule to the last row is drawn: the
        // list fills its space while rows remain hidden.
        let lines = rows_between(&buf, 2, 30, 5, last);
        assert!(lines.iter().all(|t| !t.is_empty()), "{w}×{h}: {lines:?}");
        // The rows shown, labels left out; the last row says the rest.
        let entries = lines[..lines.len() - 1]
            .iter()
            .filter(|t| t.as_bytes().get(2) == Some(&b':'))
            .count();
        assert!(entries >= 3, "{w}×{h}: {entries} rows");
        let note = lines.last().unwrap();
        assert_eq!(*note, format!("+{} more", total - entries), "{w}×{h}");
        assert!(row(&buf, h - 2).contains('─'), "{w}×{h}: the footer rule");
        assert_whole_hints(&buf, w, h);
        // From 150 columns the list adds kind, seq and details.
        let first = 6;
        if w >= 150 {
            assert!(l.wide);
            let kind = text(&buf, l.kind_x as u16, first, 21);
            assert_eq!(kind, "subc.module_restarted", "{w}×{h}");
            let seq = text(&buf, (l.seq_end - 7) as u16, first, 7);
            assert_eq!(seq, " 10,000", "{w}×{h}");
            let details = text(&buf, l.details_x as u16, first, 12);
            assert!(details.starts_with(r#"{"drain_ms""#), "{w}×{h}: {details}");
        } else {
            assert!(
                !row(&buf, first).contains("subc.module_restarted"),
                "{w}×{h}"
            );
        }
    }
    // At 80 columns the status text gives way and the hints stay whole.
    let buf = render(&app, &snap, 80, 24);
    assert!(!row(&buf, 23).contains("today"));
}

#[test]
fn every_size_draws_without_panicking() {
    let snap = crowded();
    let mut apps = Vec::new();
    for (chip, filter, down) in [
        (Chip::All, "", 0),
        (Chip::Modules, "", 30),
        (Chip::All, "mod-1", 5),
        (Chip::All, "", 400),
    ] {
        let mut app = activity_app();
        app.activity.chip = chip;
        app.activity.filter = filter.into();
        for _ in 0..down {
            press(&mut app, &snap, KeyCode::Down);
        }
        apps.push(app);
    }
    for w in 60..=220u16 {
        for h in [10, 23, 24, 25, 30, 36, 48, 60, 70] {
            for app in &apps {
                render(app, &snap, w, h);
            }
        }
    }
}

#[test]
fn the_live_app_serves_activity_and_its_filter() {
    let mut snap = Snapshot::fixture();
    snap.serves = Serves::LIVE;
    let mut app = App::new(Theme::test());
    assert!(press(&mut app, &snap, KeyCode::Char('3')));
    let buf = render(&app, &snap, 120, 36);
    assert!(row(&buf, 0).contains("3 Activity"));
    assert!(row(&buf, 35).contains("/  filter"));
    assert!(press(&mut app, &snap, KeyCode::Char('/')));
    assert!(app.field.is_some());
    // A build that doesn't serve the filter neither offers nor opens it.
    snap.serves.list_filter = ck_ui::model::TabSet::EMPTY;
    let mut app = activity_app();
    let buf = render(&app, &snap, 120, 36);
    assert!(!row(&buf, 35).contains("filter"));
    assert!(!press(&mut app, &snap, KeyCode::Char('/')));
    assert!(app.field.is_none());
}
