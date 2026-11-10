//! Resource wire mappings and accounting-row interaction, independent of the
//! byte-compared design frames in golden.rs.

#[path = "fixtures/resource_snapshot.rs"]
#[allow(dead_code)] // The golden test also uses this fixture's frame variants.
mod fixture;

use ck_ui::app::{App, Tab};
use ck_ui::live::{assemble, State};
use ck_ui::model::ResourceUsage;
use ck_ui::screens::modules::{resource_mode, row_at, row_count};
use ck_ui::time::Timestamp;
use ck_ui::{Snapshot, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{backend::TestBackend, buffer::Buffer, Terminal};
use serde_json::{json, Value};
use subc_client_rs::SupervisorListResponse;
use subc_control::ClientControlResponse;

fn reply(v: Value) -> SupervisorListResponse {
    let ClientControlResponse::SupervisorList {
        generation,
        modules,
        daemon,
        agent_commands,
    } = serde_json::from_value(v).unwrap()
    else {
        panic!("not a roster")
    };
    SupervisorListResponse {
        generation,
        modules,
        daemon,
        agent_commands,
    }
}

fn wire(cpu: u64) -> Value {
    json!({"op":"supervisor.list","generation":1,"modules":[{
        "module_id":"worker","state":"running","enabled":true,"live":true,"health":"ok",
        "resources":{"status":"measured","cpu_user_ms":10,"cpu_system_ms":10,"memory_bytes":1048576,"memory_kind":"resident_set"},
        "tree":{"cpu_ms":cpu,"memory_bytes":104857600,"memory_kind":"resident_set","process_count":4,"source":"process_tree","bound":"lower",
            "own":{"cpu_ms":cpu / 4,"memory_bytes":1048576,"bound":"lower"},
            "groups":[{"name":"compiler","count":3,"cpu_ms":cpu / 2,"memory_bytes":103809024,"bound":"lower"}]},
        "cpu_last_hour":{"cpu_ms":900000,"covered_ms":3600000}
    }],"daemon":{"pid":7,"start_time_ms":1700000000000_u64,"version":"0.41.1",
        "resources":{"status":"measured","cpu_user_ms":cpu / 10,"cpu_system_ms":0,"memory_bytes":2097152,"memory_kind":"resident_set"},
        "cpu_last_hour":{"cpu_ms":72000,"covered_ms":3600000}},
        "agent_commands":[{"module_id":"worker","cpu_ms":cpu * 2,"memory_bytes":3145728,"process_count":2,
            "cpu_last_hour":{"cpu_ms":1800000,"covered_ms":1800000,"bound":"lower"}}]})
}

fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| app.render(f.buffer_mut(), snap, Snapshot::fixture_now()))
        .unwrap();
    term.backend().buffer().clone()
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn text(buf: &Buffer) -> String {
    (0..buf.area.height)
        .map(|y| row(buf, y))
        .collect::<Vec<_>>()
        .join("\n")
}

fn app() -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    app
}

#[test]
fn resource_wire_uses_tree_counters_and_keeps_daemon_and_agents_apart() {
    let mut state = State::empty("test");
    let at = Timestamp::from_unix_millis(1_800_000_000_000);
    state.record_supervisor_list(reply(wire(1000)), at);
    let first = assemble(&state, at);
    let usage = first.modules.data[0].resources.as_ref().unwrap();
    assert_eq!(usage.cpu_percent, None);
    assert_eq!(usage.memory_mb, Some(100.0));
    state.record_supervisor_list(reply(wire(1600)), at.add_secs(2.0));
    let snap = assemble(&state, at.add_secs(2.0));
    let usage = snap.modules.data[0].resources.as_ref().unwrap();
    assert_eq!(usage.cpu_percent, Some(30.0));
    assert!(usage.lower);
    assert!(usage.last_hour.as_ref().unwrap().bound.is_none());
    assert_eq!(usage.groups[0].cpu_percent, Some(15.0));
    assert!(usage.groups[0].lower);
    let own = usage.own.as_ref().unwrap();
    assert_eq!(own.cpu_percent, Some(7.5));
    assert_eq!(own.memory_mb, 1.0);
    assert!(own.lower);
    assert_eq!(snap.resource_rows.len(), 2);
    assert_eq!(snap.resource_rows[0].usage.cpu_percent, Some(60.0));
    assert_eq!(snap.resource_rows[1].usage.cpu_percent, Some(3.0));
    assert_eq!(snap.resource_rows[1].build, "0.41.1");
}

#[test]
fn resource_groups_clamp_membership_decreases_and_missing_measurements_break_the_series() {
    let mut state = State::empty("test");
    let at = Timestamp::from_unix_millis(1_800_000_000_000);
    state.record_supervisor_list(reply(wire(1000)), at);
    let mut next = wire(1600);
    next["modules"][0]["tree"]["groups"][0]["cpu_ms"] = json!(100);
    next["modules"][0]["tree"]["own"]["cpu_ms"] = json!(100);
    state.record_supervisor_list(reply(next), at.add_secs(2.0));
    let snap = assemble(&state, at.add_secs(2.0));
    assert_eq!(
        snap.modules.data[0].resources.as_ref().unwrap().groups[0].cpu_percent,
        Some(0.0)
    );
    assert_eq!(
        snap.modules.data[0]
            .resources
            .as_ref()
            .unwrap()
            .own
            .as_ref()
            .unwrap()
            .cpu_percent,
        Some(0.0)
    );
    let mut missing = wire(1800);
    missing["modules"][0]
        .as_object_mut()
        .unwrap()
        .remove("tree");
    missing["modules"][0]["tree_unavailable"] = json!("not_yet_measured");
    state.record_supervisor_list(reply(missing), at.add_secs(4.0));
    let snap = assemble(&state, at.add_secs(4.0));
    let usage = snap.modules.data[0].resources.as_ref().unwrap();
    assert_eq!(usage.cpu_percent, None);
    assert_eq!(usage.memory_mb, None);
    assert!(usage.groups.is_empty());
    state.record_supervisor_list(reply(wire(2000)), at.add_secs(6.0));
    assert_eq!(
        assemble(&state, at).modules.data[0]
            .resources
            .as_ref()
            .unwrap()
            .cpu_percent,
        None
    );
}

#[test]
fn resource_legacy_wire_never_enables_new_columns_or_synthesizes_accounting_zeros() {
    let mut state = State::empty("test");
    let at = Snapshot::fixture_now();
    state.record_supervisor_list(
        reply(json!({"op":"supervisor.list","generation":1,"modules":[{
            "module_id":"worker","state":"running","enabled":true,"live":true,"health":"ok"
        }]})),
        at,
    );
    let snap = assemble(&state, at);
    assert!(!resource_mode(&snap));
    assert!(snap.modules.data[0].resources.is_none());
    assert!(snap.resource_rows.is_empty());
    let legacy = Snapshot::fixture();
    let screen = text(&render(&app(), &legacy, 120, 36));
    assert!(screen.contains("Build"));
    assert!(!screen.contains("Cores 1h"));
    assert!(!screen.contains("All of CortexKit"));
    assert!(!screen.contains("Last hour"));
}

#[test]
fn resource_total_sums_rates_and_preserves_independent_lower_bounds() {
    let mut snap = fixture::snapshot();
    snap.modules.data.truncate(2);
    let first = snap.modules.data[0].resources.as_mut().unwrap();
    first.lower = true;
    first.last_hour = Some(fixture::hour_window(900000, 3600000, false));
    let second = snap.modules.data[1].resources.as_mut().unwrap();
    second.last_hour = Some(fixture::hour_window(1800000, 1800000, true));
    let screen = render(&app(), &snap, 120, 36);
    // .25 + 1.00 + 1.41 + .02 = 2.68, not a coverage-weighted average.
    let total = (0..36)
        .map(|y| row(&screen, y))
        .find(|r| r.contains("All of CortexKit"))
        .unwrap();
    assert!(total.contains("≥199%"), "{total}");
    assert!(total.contains("≥2.68"), "{total}");
    assert!(!row(&screen, 7).contains("≥0.25"));
    assert!(row(&screen, 8).contains("≥1.00"));
    let total_y = (0..36)
        .find(|y| row(&screen, *y).contains("All of CortexKit"))
        .unwrap();
    assert_eq!(
        Theme::test_roles(
            screen[(48, total_y)].fg,
            screen[(48, total_y)].bg,
            screen[(48, total_y)].modifier
        )
        .0,
        "dim"
    );
}

#[test]
fn resource_total_does_not_turn_an_unavailable_part_into_zero() {
    let mut snap = fixture::snapshot();
    snap.modules.data[0].resources = Some(ResourceUsage {
        unavailable: Some("not_yet_measured".into()),
        ..ResourceUsage::default()
    });
    let buf = render(&app(), &snap, 120, 36);
    let line = row(&buf, 32);
    assert!(line.contains("All of CortexKit"));
    assert_eq!(line.chars().nth(38), Some('–'));
    assert_eq!(line.chars().nth(48), Some('–'));
    assert_eq!(line.chars().nth(57), Some('–'));
    let screen = text(&buf);
    assert!(screen.contains("not measured yet"));
    assert!(!screen.contains("not_yet_measured"));
}

#[test]
fn resource_rows_scroll_above_pinned_total_and_auxiliary_rows_cannot_control_modules() {
    let snap = fixture::snapshot();
    let mut app = app();
    for _ in 0..row_count(&snap) {
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &snap);
    }
    assert_eq!(app.mod_sel, row_count(&snap) - 1);
    let screen = render(&app, &snap, 80, 24);
    assert!(row(&screen, 20).contains("All of CortexKit"));
    assert!(text(&screen).contains("agents' commands"));
    assert!(text(&screen).contains("subc daemon"));
    assert_eq!(row_at(80, 24, &snap, app.mod_sel, 20), None);
    assert_eq!(row_at(80, 24, &snap, app.mod_sel, 18), Some(app.mod_sel));
    app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &snap);
    app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &snap);
    assert!(app.confirm.is_none());
    assert!(app.log_focus(&snap).is_none());
    app.handle_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE), &snap);
    app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), &snap);
    assert!(app.log.is_none());
    assert!(app.metrics.is_none());
}

#[test]
fn resource_hour_zero_coverage_is_missing_and_under_hour_is_dimmed() {
    let mut snap = fixture::snapshot();
    let usage = snap.modules.data[0].resources.as_mut().unwrap();
    usage.last_hour.as_mut().unwrap().covered_ms = 0;
    let buf = render(&app(), &snap, 120, 36);
    assert_eq!(buf[(48, 7)].symbol(), "–");
    assert!(text(&buf).contains("not measured yet"));
    let usage = snap.modules.data[0].resources.as_mut().unwrap();
    usage.last_hour = Some(fixture::hour_window(360000, 1800000, false));
    let buf = render(&app(), &snap, 120, 36);
    assert!(row(&buf, 7).contains("0.20"));
    assert_eq!(
        Theme::test_roles(buf[(48, 7)].fg, buf[(48, 7)].bg, buf[(48, 7)].modifier).0,
        "dim"
    );
}

#[test]
fn short_lower_bound_hours_render_without_a_tree_and_below_rounding_precision() {
    let mut value = wire(1000);
    let mut idle = value["modules"][0].clone();
    value["modules"][0]["module_id"] = json!("busy-worker");
    value["modules"][0].as_object_mut().unwrap().remove("tree");
    value["modules"][0]["tree_unavailable"] = json!("unreadable");
    value["modules"][0]["cpu_last_hour"] =
        json!({"bound":"lower","covered_ms":120001,"cpu_ms":159491});
    idle["module_id"] = json!("idle-worker");
    idle["cpu_last_hour"] = json!({"bound":"lower","covered_ms":60000,"cpu_ms":6});
    value["modules"].as_array_mut().unwrap().push(idle);
    let mut state = State::empty("test");
    let at = Timestamp::from_unix_millis(1_800_000_000_000);
    state.record_supervisor_list(reply(value), at);
    let screen = render(&app(), &assemble(&state, at), 140, 40);
    for (name, figure) in [
        ("busy-worker", "≥1.33"),
        ("idle-worker", "≥0.00"),
        ("All of CortexKit", "≥2.35"),
    ] {
        let y = (7..40).find(|y| row(&screen, *y).contains(name)).unwrap();
        assert!(row(&screen, y).contains(figure));
        assert_eq!(
            Theme::test_roles(
                screen[(48, y)].fg,
                screen[(48, y)].bg,
                screen[(48, y)].modifier
            )
            .0,
            "dim"
        );
    }
}

#[test]
fn resource_processes_sort_by_cpu_and_keep_the_own_process_first() {
    let mut snap = fixture::snapshot();
    let usage = snap.modules.data[0].resources.as_mut().unwrap();
    usage.groups = vec![
        fixture::program("quiet", 2, 1.0, 2000.0),
        fixture::program("busy", 1, 20.0, 1.0),
    ];
    usage.groups[1].lower = true;
    let screen = text(&render(&app(), &snap, 160, 48));
    assert!(screen.find("own process").unwrap() < screen.find("busy").unwrap());
    assert!(screen.find("busy").unwrap() < screen.find("quiet ×2").unwrap());
    assert!(screen.contains("≥20%"));
    snap.modules.data[0].resources.as_mut().unwrap().own = None;
    let screen = text(&render(&app(), &snap, 160, 48));
    assert!(!screen.contains("own process"));
    assert!(screen.contains("Processes"));
    assert!(screen.contains("quiet ×2"));
}

#[test]
fn resource_wire_without_own_keeps_groups_without_guessing_a_root_share() {
    let mut state = State::empty("test");
    let at = Snapshot::fixture_now();
    state.record_supervisor_list(reply(wire(1000)), at);
    let mut no_own = wire(1600);
    no_own["modules"][0]["tree"]
        .as_object_mut()
        .unwrap()
        .remove("own");
    state.record_supervisor_list(reply(no_own), at.add_secs(2.0));
    let snap = assemble(&state, at);
    let usage = snap.modules.data[0].resources.as_ref().unwrap();
    assert!(usage.own.is_none());
    assert_eq!(usage.groups[0].cpu_percent, Some(15.0));
    state.record_supervisor_list(reply(wire(2000)), at.add_secs(4.0));
    let snap = assemble(&state, at);
    assert_eq!(
        snap.modules.data[0]
            .resources
            .as_ref()
            .unwrap()
            .own
            .as_ref()
            .unwrap()
            .cpu_percent,
        None
    );
}
