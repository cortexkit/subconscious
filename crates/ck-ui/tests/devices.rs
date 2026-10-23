//! Devices mapping, live-only rows and actions, list/panel placement and
//! multi-value LAN overflow. Replies include a neutralized live capture.

use ck_ui::app::{App, Tab};
use ck_ui::live::map;
use ck_ui::model::{Panel, Serves, Snapshot, Status};
use ck_ui::screens::devices::{layout, place, values, LIST_TOP};
use ck_ui::theme::St;
use ck_ui::time::Timestamp;
use ck_ui::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};
use serde_json::{json, Value};

fn now() -> Timestamp {
    Snapshot::fixture_now()
}
fn app() -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Devices;
    app
}
fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buf, snap, now());
    buf
}
fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}
fn screen(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row(buf, y) + "\n").collect()
}
fn press(app: &mut App, snap: &Snapshot, key: KeyCode) -> bool {
    app.handle_key(KeyEvent::new(key, KeyModifiers::NONE), snap)
}
fn reply(n: usize) -> Value {
    json!({"result": {
        "devices": (0..n).map(|i| json!({
            "name": format!("Device-{i:02}"), "fingerprint": format!("key-{i:02}"),
            "device_class": "phone", "trust": "verified", "tunnel": "established",
            "carrier": "relay", "since_unix_ms": 0,
            "last_seen_unix_ms": null, "subscriptions_live": 3
        })).collect::<Vec<_>>(),
        "self": {"lan_addrs": ["192.0.2.1:7841", "192.0.2.2:7841"],
                 "rendezvous": "relay.example.com",
                 "watch_ops_offered": 20}
    }})
}

fn captured_reply() -> Value {
    json!({"devices":[
     {"name":"alex-iphone","device_class":"phone","fingerprint":"ac2a520b6dd57fc8e1a6b7c0d9f81234","trust":"verified","tunnel":"established","carrier":"direct","since_unix_ms":1792699016651_i64,"last_seen_unix_ms":1792700407537_i64,"subscriptions_live":0},
     {"name":"alex-iphone-sim","device_class":"sim","fingerprint":"006736a74d3c37d83e6b12bc6a381b27","trust":"verified","tunnel":"awaiting_dial","since_unix_ms":1792699016162_i64,"last_seen_unix_ms":null,"subscriptions_live":0}],
     "self":{"lan_addrs":["10.211.55.2:7841","192.168.1.33:7841"],"listen_addr":"0.0.0.0:7841","rendezvous":"connected","wan_reachability":{"verdict":"reachable"},"watch_ops_offered":11,
      "expose_profiles":[{"name":"phone","operation_count":54,"peers_using":2},{"name":"sim","operation_count":36,"peers_using":3}]}})
}
fn live(reply: &Value) -> Snapshot {
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    s.devices = Panel::new(now(), map::devices(reply, now()).unwrap());
    s.devices.refresh_secs = Some(15.0);
    s
}

#[test]
fn kind_words_are_computer_simulator_phone_or_unknown() {
    let mut r = reply(6);
    for (d, class) in r["result"]["devices"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(["computer", "mac", "sim", "phone", "future", "unknown"])
    {
        d["device_class"] = json!(class);
    }
    let s = live(&r);
    assert_eq!(
        s.devices
            .data
            .list
            .iter()
            .map(|d| d.kind.as_str())
            .collect::<Vec<_>>(),
        [
            "computer",
            "computer",
            "simulator",
            "phone",
            "unknown",
            "unknown"
        ]
    );
}

#[test]
fn only_established_tunnels_are_online() {
    let mut r = reply(5);
    for (d, tunnel) in r["result"]["devices"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip([
            "dormant",
            "dialing",
            "awaiting_dial",
            "closed:idle",
            "established",
        ])
    {
        d["tunnel"] = json!(tunnel);
    }
    let s = live(&r);
    assert_eq!(s.devices.data.online, 1);
    assert_eq!(s.devices.data.list[0].name, "Device-04");
    assert_eq!(s.devices.data.list[0].status, Status::Ok);
    assert!(s.devices.data.list[1..]
        .iter()
        .all(|d| d.status == Status::Stopped));
}

#[test]
fn tombstones_are_not_listed_or_counted() {
    let mut r = reply(3);
    r["result"]["devices"][1]["trust"] = json!("tombstoned");
    let s = live(&r);
    assert_eq!(s.devices.data.list.len(), 2);
    assert_eq!(s.devices.data.online, 2);
    let buf = render(&app(), &s, 120, 36);
    assert!(row(&buf, 11).trim_end().ends_with('2'));
    assert!(!screen(&buf).contains("Device-01"));
}

#[test]
fn connection_is_the_carrier_not_a_lan_guess() {
    let mut r = reply(3);
    r["result"]["devices"][0]["carrier"] = json!("direct");
    r["result"]["devices"][2]
        .as_object_mut()
        .unwrap()
        .remove("carrier");
    let s = live(&r);
    assert_eq!(
        s.devices
            .data
            .list
            .iter()
            .map(|d| d.path.as_str())
            .collect::<Vec<_>>(),
        ["direct", "relay", "unknown"]
    );
    let buf = render(&app(), &s, 120, 36);
    assert!(row(&buf, 14).contains("direct"));
    assert!(!screen(&buf).contains("· LAN"));
}

#[test]
fn null_last_seen_means_not_since_callosum_started() {
    let mut r = reply(3);
    r["result"]["devices"][1]["last_seen_unix_ms"] = json!(now().unix_millis() - 120_000);
    r["result"]["devices"][2]
        .as_object_mut()
        .unwrap()
        .remove("last_seen_unix_ms");
    let s = live(&r);
    assert_eq!(s.devices.data.list[0].seen, "not since callosum started");
    assert_eq!(s.devices.data.list[1].seen, "2m ago");
    assert_eq!(s.devices.data.list[2].seen, "unknown");
    assert!(row(&render(&app(), &s, 120, 36), 14).contains("not since callosum started"));
}

#[test]
fn reachable_as_comes_from_self_and_keeps_missing_values_unknown() {
    let s = live(&reply(1));
    assert_eq!(
        s.devices.data.lan_addrs.as_ref().unwrap(),
        &["192.0.2.1:7841", "192.0.2.2:7841"]
    );
    assert_eq!(
        s.devices.data.rendezvous.as_deref(),
        Some("relay.example.com")
    );
    assert_eq!(s.devices.data.watch_ops_offered, Some(20));
    let buf = render(&app(), &s, 120, 36);
    assert!(row(&buf, 5).contains("192.0.2.1:7841, 192.0.2.2:7841"));
    assert!(row(&buf, 6).contains("relay.example.com"));
    let mut r = reply(1);
    r["result"].as_object_mut().unwrap().remove("self");
    let buf = render(&app(), &live(&r), 120, 36);
    for y in 5..=7 {
        assert!(row(&buf, y).contains("unknown"), "{}", row(&buf, y));
    }
}

#[test]
fn live_hides_mock_panels_and_unsupported_actions() {
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    let mut a = app();
    let text = screen(&render(&a, &s, 120, 36));
    for absent in [
        "Account",
        "Vault",
        "Recent",
        "relay connected",
        "Add a computer",
        "details",
        "forget",
    ] {
        assert!(!text.contains(absent), "{absent}:\n{text}");
    }
    assert!(text.contains("a Add a phone"));
    for key in [
        KeyCode::Char('c'),
        KeyCode::Enter,
        KeyCode::Char('x'),
        KeyCode::Char('/'),
    ] {
        assert!(!press(&mut a, &s, key), "{key:?}");
        assert!(a.requests.is_empty() && a.confirm.is_none() && a.overlay.is_none());
    }
}

#[test]
fn every_size_counts_hidden_devices_and_keeps_the_phone_row_below_the_list() {
    let s = live(&reply(40));
    for (w, h) in [(80, 24), (100, 30), (140, 40), (160, 48), (200, 60)] {
        let buf = render(&app(), &s, w, h);
        let lines: Vec<_> = (0..h).map(|y| row(&buf, y)).collect();
        let shown = lines.iter().filter(|line| line.contains("Device-")).count();
        let phone = lines
            .iter()
            .position(|line| line.contains("a Add a phone"))
            .unwrap();
        assert!(shown >= 3, "{w}x{h}: {shown}");
        let capacity = h as usize - 19;
        if 40 > capacity {
            assert_eq!(shown, capacity - 1, "{w}x{h}");
            assert!(lines[14 + shown].contains(&format!("+{} more", 40 - shown)));
            assert_eq!(phone, h as usize - 3);
            assert!(lines[14..14 + shown]
                .iter()
                .all(|line| line.contains("Device-")));
        } else {
            assert_eq!(shown, 40);
            assert_eq!(phone, 14 + 40 + 2);
            assert!(lines[phone + 1..h as usize - 2]
                .iter()
                .all(|line| line.trim().is_empty()));
        }
        let footer = &lines[h as usize - 1];
        assert!(footer.contains("a  add a phone"));
        assert!(footer.contains("↑↓  move"));
        assert!(!footer.contains('?'));
    }
}

#[test]
fn panels_keep_their_height_and_drop_lowest_first() {
    let s = Snapshot::fixture();
    let buf = render(&app(), &s, 120, 36);
    assert!(row(&buf, 19).contains("a Add a phone"));
    assert!(row(&buf, 22).contains("Recent"));
    assert!(row(&buf, 28).contains("09:36"));
    let buf = render(&app(), &s, 80, 24);
    assert!(!screen(&buf).contains("Recent"));
    assert!(row(&buf, 19).contains("a Add a phone"));
    // Shorter layouts exercise the whole-panel fallback independently of
    // the app's minimum-window message.
    let p = place(19, 40, true);
    assert!(p.recent.is_none() && p.phone.is_none());
    assert_eq!(p.list, 3);
}

#[test]
fn multi_value_lan_cells_count_every_hidden_value() {
    let mut r = reply(40);
    r["result"]["self"]["lan_addrs"] = json!((0..20)
        .map(|i| format!("192.0.2.{}:7841", i + 1))
        .collect::<Vec<_>>());
    let s = live(&r);
    for (w, h) in [(80, 24), (100, 30), (140, 40), (160, 48), (200, 60)] {
        let buf = render(&app(), &s, w, h);
        let line = row(&buf, 5);
        let shown = line.matches("192.0.2.").count();
        assert!(shown < 20);
        assert!(
            line.contains(&format!("+{} more", 20 - shown)),
            "{w}x{h}: {line}"
        );
    }
    assert_eq!(values(&["one".into(), "two".into()], 12), "one, two");
    assert_eq!(values(&vec!["192.0.2.100:7841".into(); 3], 8), "+3 more");
}

#[test]
fn wide_columns_start_at_150_and_keep_the_mapped_values() {
    let s = live(&reply(40));
    assert_eq!(
        s.devices.data.list[0].fingerprint.as_deref(),
        Some("key-00")
    );
    assert_eq!(s.devices.data.list[0].trust.as_deref(), Some("verified"));
    for w in [120, 149, 150, 160, 200] {
        let buf = render(&app(), &s, w, 48);
        let head = row(&buf, 13);
        for title in ["Fingerprint", "Trust"] {
            assert_eq!(head.contains(title), w >= 150, "{w}: {head}");
        }
        if w >= 150 {
            let line = row(&buf, LIST_TOP as u16);
            for value in ["key-00", "verified"] {
                assert!(line.contains(value), "{w}: {line}");
            }
        }
    }
}

#[test]
fn real_reply_reports_a_numeric_offered_feed_count() {
    let mut r = captured_reply();
    let s = live(&r);
    assert_eq!(s.devices.data.watch_ops_offered, Some(11));
    for (w, h) in [(120, 36), (160, 48)] {
        let line = row(&render(&app(), &s, w, h), 7);
        assert!(
            line.contains("Live feeds    11 offered to phones"),
            "{line}"
        );
    }
    r["self"]
        .as_object_mut()
        .unwrap()
        .remove("watch_ops_offered");
    let s = live(&r);
    assert_eq!(s.devices.data.watch_ops_offered, None);
    assert!(row(&render(&app(), &s, 120, 36), 7).contains("Live feeds    unknown"));
    r["self"]["watch_ops_offered"] = json!(0);
    assert!(row(&render(&app(), &live(&r), 120, 36), 7).contains("0 offered to phones"));
    assert_eq!(Snapshot::fixture().devices.data.watch_ops_offered, Some(10));
}

#[test]
fn real_reply_does_not_label_tunnel_state_time_as_paired_since() {
    let s = live(&captured_reply());
    for (w, h, kind_x) in [(120, 36, 26), (150, 48, 47), (160, 48, 57), (200, 60, 97)] {
        let buf = render(&app(), &s, w, h);
        let text = screen(&buf);
        assert!(!text.contains("Paired since"), "{w}:\n{text}");
        assert!(!text.contains("2026-10-22"), "{w}:\n{text}");
        assert_eq!(row(&buf, 13).find("Kind"), Some(kind_x));
        if w >= 150 {
            assert!(row(&buf, 13).contains("Fingerprint"));
            assert!(row(&buf, 13).contains("Trust"));
            assert!(row(&buf, 14).contains("ac2a520b6dd57fc8"));
            assert!(row(&buf, 14).contains("verified"));
        }
    }
}

#[test]
fn real_reply_without_an_established_tunnel_has_a_faint_connection_dash() {
    let mut r = captured_reply();
    let s = live(&r);
    assert_eq!(s.devices.data.list[0].path, "direct");
    assert_eq!(s.devices.data.list[1].path, "—");
    for (w, h) in [(120, 36), (160, 48)] {
        let buf = render(&app(), &s, w, h);
        let cell = &buf[(layout(i32::from(w)).connection as u16, 15)];
        assert_eq!(cell.symbol(), "—");
        assert_eq!(Some(cell.fg), Theme::test().style(St::F).fg);
    }
    r["devices"][1]["tunnel"] = json!("established");
    assert_eq!(live(&r).devices.data.list[1].path, "unknown");
}

#[test]
fn keys_reach_every_device_and_leave_details_and_forget_unserved() {
    let s = live(&reply(40));
    let mut a = App::new(Theme::test());
    assert!(press(&mut a, &s, KeyCode::Char('6')));
    assert_eq!(a.tab, Tab::Devices);
    for i in 0..40 {
        assert!(press(&mut a, &s, KeyCode::Down));
        assert_eq!(a.devices.sel, Some(i));
        assert!(screen(&render(&a, &s, 80, 24)).contains(&format!("Device-{i:02}")));
    }
    assert!(!press(&mut a, &s, KeyCode::Down));
    assert!(press(&mut a, &s, KeyCode::Up));
    assert_eq!(a.devices.sel, Some(38));
    assert!(!press(&mut a, &s, KeyCode::Enter));
    assert!(!press(&mut a, &s, KeyCode::Char('x')));
    assert!(a.requests.is_empty());
}

#[test]
fn live_devices_tab_still_requires_callosum_in_the_roster() {
    let mut s = live(&reply(1));
    let mut a = App::new(Theme::test());
    s.modules.data.retain(|m| m.id != "callosum");
    assert!(!s.visible_tabs().contains(Tab::Devices));
    assert!(!press(&mut a, &s, KeyCode::Char('6')));
    assert_eq!(a.tab, Tab::Home);
}
