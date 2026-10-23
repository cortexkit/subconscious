//! The Activity screen: day labels, the list's last row, the kind chips and
//! the filter, the detail pane, the keys, and the list at every window size.

use ck_ui::app::{App, Request, Tab};
use ck_ui::model::{ActivityKey, ActivityLog, ActivityRow, ActivityRowKind, Serves};
use ck_ui::screens::activity::{
    detail_rows, details_view, layout, offset_label, pane_rows, Chip, DetailRow, DETAIL_KEY_MAX,
};
use ck_ui::theme::St;
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
        pane[..12],
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
            "  drain_ms  412",
            "",
        ]
    );
    // Keys are dim and values plain, as in the rows above.
    let theme = Theme::test();
    assert_eq!(Some(buf[(81, 15)].fg), theme.style(St::D).fg);
    assert_ne!(Some(buf[(91, 15)].fg), theme.style(St::D).fg);

    // Without a build the Build row is left out, and empty details draw no
    // block.
    let mut app = activity_app();
    press(&mut app, &snap, KeyCode::Down);
    let buf = render(&app, &snap, 120, 36);
    let pane = pane_text(&buf, 36);
    assert_eq!(pane[6], "Seq       48,212");
    assert!(pane[7..].iter().all(String::is_empty), "{pane:?}");
}

fn row_of(key: &str, value: Option<&str>) -> DetailRow {
    DetailRow {
        key: key.into(),
        value: value.map(str::to_string),
    }
}

/// The pane's lines from the one under "Details" to the last non-blank one.
fn details_drawn(buf: &Buffer, h: u16) -> Vec<String> {
    let pane = pane_text(buf, h);
    let at = pane.iter().position(|l| l == "Details").unwrap();
    let mut lines = pane[at + 1..].to_vec();
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines
}

#[test]
fn details_flatten_sorted_at_every_level_whatever_order_they_arrive_in() {
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
    assert_eq!(
        detail_rows(&details, None),
        [
            row_of("Mid", Some("x")),
            row_of("alpha.b[0].c", Some("2")),
            row_of("alpha.b[0].d", Some("1")),
            row_of("alpha.y", Some("2")),
            row_of("empty", None),
            row_of("zeta", Some("1")),
        ]
    );

    // Drawn, values start two past the longest key.
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(details);
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 40);
    assert_eq!(
        details_drawn(&buf, 40),
        [
            "  Mid           x",
            "  alpha.b[0].c  2",
            "  alpha.b[0].d  1",
            "  alpha.y       2",
            "  empty         –",
            "  zeta          1",
        ]
    );
    // A value that holds nothing is a dim "–".
    // "Details" is on row 13, so "empty" is on row 18.
    assert_eq!(buf[(95, 18)].symbol(), "–");
    assert_eq!(Some(buf[(95, 18)].fg), Theme::test().style(St::D).fg);
}

#[test]
fn detail_values_read_as_words_not_json() {
    let details = json!({
        "array_of_objects": [{"id": "a"}, {"id": "b", "n": 2}],
        "flags": [true, false, null],
        "float": 1.5,
        "negative": -3,
        "no": false,
        "none": null,
        "nothing": [],
        "path": "/Users/alex/Work/Projects/CortexKit/magic-context",
        "tags": ["fast", "local"],
        "yes": true,
    });
    let rows = detail_rows(details.as_object().unwrap(), Some("/Users/alex"));
    assert_eq!(
        rows,
        [
            row_of("array_of_objects[0].id", Some("a")),
            row_of("array_of_objects[1].id", Some("b")),
            row_of("array_of_objects[1].n", Some("2")),
            row_of("flags", Some("yes, no, –")),
            row_of("float", Some("1.5")),
            row_of("negative", Some("-3")),
            row_of("no", Some("no")),
            row_of("none", None),
            row_of("nothing", None),
            row_of("path", Some("~/Work/Projects/CortexKit/magic-context")),
            row_of("tags", Some("fast, local")),
            row_of("yes", Some("yes")),
        ]
    );

    // The home folder alone is "~"; a folder that only starts with the
    // home folder's name is not under it; a home written with a trailing
    // separator matches the same; without a home nothing changes.
    let paths = json!({
        "a": "/Users/alex",
        "b": "/Users/alexander/notes",
        "c": "/Users/alex/notes",
        "d": "C:\\Users\\alex\\notes",
    });
    let paths = paths.as_object().unwrap();
    let values = |home: Option<&str>| -> Vec<Option<String>> {
        detail_rows(paths, home)
            .into_iter()
            .map(|r| r.value)
            .collect()
    };
    let some = |s: &str| Some(s.to_string());
    assert_eq!(
        values(Some("/Users/alex/")),
        [
            some("~"),
            some("/Users/alexander/notes"),
            some("~/notes"),
            some("C:\\Users\\alex\\notes"),
        ]
    );
    assert_eq!(values(Some("C:\\Users\\alex"))[3], some("~\\notes"));
    assert_eq!(values(None)[2], some("/Users/alex/notes"));
    assert_eq!(values(Some("/"))[2], some("/Users/alex/notes"));

    // The pane reads the home folder from the host.
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(
        json!({"project": "/Users/alex/Work/magic-context"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let mut snap = with_rows(vec![r], true);
    snap.host.data.home = Some("/Users/alex".into());
    let buf = render(&activity_app(), &snap, 120, 36);
    assert_eq!(details_drawn(&buf, 36), ["  project  ~/Work/magic-context"]);
}

#[test]
fn long_values_wrap_under_the_value_column_and_are_never_cut() {
    let rows = [
        row_of(
            "path",
            Some("~/Library/Application Support/cortexkit/modules/engram/bin"),
        ),
        row_of("execution", Some("bg_d74d793677582c05d74d793677582c05d74d")),
    ];
    let view = details_view(&rows, 37);
    assert_eq!(view.value_col, 11);
    let lines: Vec<Vec<(&str, &str)>> = view
        .rows
        .iter()
        .map(|r| {
            r.iter()
                .map(|l| (l.key.as_str(), l.value.as_str()))
                .collect()
        })
        .collect();
    // At spaces where there are any, inside a word that is wider than the
    // room; no character is dropped.
    assert_eq!(
        lines,
        [
            vec![
                ("path", "~/Library/Application"),
                ("", "Support/cortexkit/modules/"),
                ("", "engram/bin"),
            ],
            vec![
                ("execution", "bg_d74d793677582c05d74d793"),
                ("", "677582c05d74d"),
            ]
        ]
    );

    // Drawn in the pane at 120 columns, 37 columns for the lines.
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(
        json!({"path": "/Users/alex/Library/Application Support/cortexkit/modules/engram/bin"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let mut snap = with_rows(vec![r], true);
    snap.host.data.home = Some("/Users/alex".into());
    let buf = render(&activity_app(), &snap, 120, 36);
    let drawn = details_drawn(&buf, 36);
    assert_eq!(
        drawn,
        [
            "  path  ~/Library/Application",
            "        Support/cortexkit/modules/engra",
            "        m/bin",
        ]
    );
    assert!(drawn.iter().all(|l| !l.contains('…')), "{drawn:?}");
}

#[test]
fn a_key_longer_than_the_value_column_takes_its_own_line() {
    let long = "a_detail_key_of_25_column";
    assert!(long.len() > DETAIL_KEY_MAX);
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    let mut details = Map::new();
    details.insert(long.into(), json!("value"));
    details.insert("id".into(), json!("bg_1"));
    r.details = Some(details);
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 36);
    // Values align to "id", the longest key that fits; the long key's value
    // starts on the next line, under the others.
    assert_eq!(
        details_drawn(&buf, 36),
        ["  a_detail_key_of_25_column", "      value", "  id  bg_1",]
    );

    // Aligned to the longest key up to the cap.
    let rows = [
        row_of("twenty_column_key_xx", Some("a")),
        row_of("twenty_one_column_key", Some("b")),
    ];
    let view = details_view(&rows, 37);
    assert_eq!(view.value_col, 22);
    assert_eq!(view.rows[0].len(), 1);
    assert_eq!(view.rows[1][0].key, "twenty_one_column_key");
    assert_eq!(view.rows[1][0].value, "");
    assert_eq!(view.rows[1][1].key, "");
    assert_eq!(view.rows[1][1].value, "b");

    // A key wider than the whole line wraps too.
    let rows = [row_of(&"k".repeat(45), Some("v"))];
    let view = details_view(&rows, 37);
    let keys: Vec<String> = view.rows[0].iter().map(|l| l.key.clone()).collect();
    assert_eq!(keys, ["k".repeat(37), "k".repeat(8), String::new()]);
    assert_eq!(view.value_col, 2);
}

#[test]
fn details_that_overflow_the_pane_end_with_how_many_rows_more() {
    let details: Map<String, Value> = (0..10).map(|i| (format!("key_{i:02}"), json!(i))).collect();
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(details);
    let snap = with_rows(vec![r], true);
    // At 120×24 the list ends on row 20: "Details" on 13, then 7 rows, so 6
    // rows and "+4 more".
    let buf = render(&activity_app(), &snap, 120, 24);
    let pane = pane_text(&buf, 24);
    let at = pane.iter().position(|l| l == "Details").unwrap();
    assert_eq!(pane[at + 1], "  key_00  0");
    assert_eq!(pane[at + 6], "  key_05  5");
    assert_eq!(pane[at + 7], "  +4 more");
    assert_eq!(at + 7, pane.len() - 1, "the note is the pane's last row");
    assert_eq!(
        Some(buf[(81, 5 + at as u16 + 7)].fg),
        Theme::test().style(St::D).fg
    );

    // At 120×36 thirty keys run out of room the same way: "Details" on row
    // 13, then rows 14 to 32, 18 rows and the note.
    let details: Map<String, Value> = (0..30).map(|i| (format!("key_{i:02}"), json!(i))).collect();
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(details);
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 36);
    let pane = pane_text(&buf, 36);
    assert_eq!(pane.last().unwrap(), "  +12 more");

    // A row shows whole or not at all: five short rows, then one whose
    // value wraps to three lines, which doesn't fit in the two left.
    let mut details: Map<String, Value> = ["a", "b", "c", "d", "e"]
        .iter()
        .map(|k| (k.to_string(), json!(1)))
        .collect();
    details.insert("z".into(), json!("x".repeat(80)));
    let mut r = entry(9, 60.0, "engram", "restarted", "subc.module_restarted");
    r.details = Some(details);
    let buf = render(&activity_app(), &with_rows(vec![r], true), 120, 24);
    assert_eq!(
        details_drawn(&buf, 24),
        [
            "  a  1",
            "  b  1",
            "  c  1",
            "  d  1",
            "  e  1",
            "  +1 more"
        ]
    );
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
        // From 150 columns the list adds kind and seq. The details show
        // only in the pane, at every width.
        let first = 6;
        let list = text(&buf, 0, first, l.lw as u16);
        assert!(!list.contains("drain_ms"), "{w}×{h}: {list}");
        if w >= 150 {
            assert!(l.wide);
            let kind = text(&buf, l.kind_x as u16, first, 21);
            assert_eq!(kind, "subc.module_restarted", "{w}×{h}");
            let seq = text(&buf, (l.seq_end - 7) as u16, first, 7);
            assert_eq!(seq, " 10,000", "{w}×{h}");
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
fn the_wide_list_has_no_details_column_and_gives_its_room_to_the_summary() {
    let what = "approved a tool permission for IRIS on the build server for the release of magic-context 0.9";
    let mut r = entry(
        48_213,
        60.0,
        "cingulate",
        what,
        "cingulate.consent_answered",
    );
    r.who = Some("you".into());
    r.details = Some(
        json!({
            "execution": "bg_d74d793677582c05",
            "model_route": "openai/gpt-6.1-sol",
            "project": "/Users/alex/Work/Projects/CortexKit/magic-context",
        })
        .as_object()
        .unwrap()
        .clone(),
    );
    let mut snap = with_rows(vec![r], true);
    snap.host.data.home = Some("/Users/alex".into());
    let (w, h) = (250, 60);
    let buf = render(&activity_app(), &snap, w, h);
    let l = layout(i32::from(w), i32::from(h));
    assert!(l.wide);
    let y = 6;
    let list = text(&buf, 0, y, l.lw as u16);
    for leak in ["execution", "bg_d74d", "model_route", "{", "\""] {
        assert!(!list.contains(leak), "{leak} in the list: {list}");
    }
    // The summary reads in full, and the kind and seq follow it.
    assert_eq!(l.what_w, l.lw - 68);
    assert_eq!(text(&buf, 27, y, what.len() as u16), what);
    assert_eq!(l.kind_x, 27 + l.what_w + 2);
    assert_eq!(text(&buf, l.kind_x as u16, y, 22), "cingulate.consent_ans…");
    assert_eq!(text(&buf, (l.seq_end - 7) as u16, y, 7), " 48,213");
    // Who did it ends at the list's right edge, one gap past the seq.
    assert_eq!(l.seq_end + 1 + l.who_w, l.lw);
    assert_eq!(text(&buf, (l.lw - 3) as u16, y, 3), "you");
    assert_eq!(text(&buf, l.lw as u16, y, 2), "  ");
    // The pane still shows the details.
    let x = (l.lw + 5) as u16;
    let pane = rows_between(&buf, x, w - x, 5, l.last as u16);
    let at = pane.iter().position(|t| t == "Details").unwrap();
    assert_eq!(
        pane[at + 1..at + 4],
        [
            "  execution    bg_d74d793677582c05",
            "  model_route  openai/gpt-6.1-sol",
            "  project      ~/Work/Projects/CortexKit/magic-context",
        ]
    );
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
