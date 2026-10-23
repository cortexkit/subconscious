//! The Add a phone exchange against a scripted callosum: keys go in,
//! scripted replies and watch events come back through snapshots, and each
//! test asserts the status line, the screen and the calls sent.

use ck_ui::app::{App, Overlay, Request, Tab};
use ck_ui::live::map;
use ck_ui::model::{AccessGroup, AccessGroups, Devices, Panel, Snapshot};
use ck_ui::pairing::flow::{
    Command, ConfirmFailure, Event, Opened, Reading, Refusal, Verdict, WatchEvent,
};
use ck_ui::pairing::Claim;
use ck_ui::theme::Bg;
use ck_ui::time::Timestamp;
use ck_ui::Theme;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;
use serde_json::json;

fn groups(list: &[(&str, u64)]) -> AccessGroups {
    AccessGroups::Listed(
        list.iter()
            .map(|(name, n)| AccessGroup {
                name: (*name).into(),
                operation_count: Some(*n),
            })
            .collect(),
    )
}

/// The app on Devices, a snapshot whose devices panel the test sets, and
/// every pairing call the app has asked for.
struct Rig {
    app: App,
    snap: Snapshot,
    seq: u64,
    now: Timestamp,
    sent: Vec<Command>,
}

impl Rig {
    fn with(access: Option<AccessGroups>) -> Rig {
        let mut rig = Rig {
            app: App::new(Theme::test()),
            snap: Snapshot::fixture(),
            seq: 0,
            now: Snapshot::fixture_now(),
            sent: Vec::new(),
        };
        rig.app.tab = Tab::Devices;
        match access {
            Some(access) => rig.set_groups(access),
            None => rig.snap.devices = Panel::waiting(Devices::default(), Some(15.0)),
        }
        rig.sync();
        rig
    }

    fn new() -> Rig {
        Rig::with(Some(groups(&[("phone", 38), ("sim", 9)])))
    }

    fn set_groups(&mut self, access: AccessGroups) {
        let mut devices = Snapshot::fixture().devices;
        devices.data.access_groups = access;
        self.snap.devices = devices;
        self.sync();
    }

    fn fail_read(&mut self, code: &str) {
        self.snap.devices = Panel::waiting(Devices::default(), Some(15.0));
        self.snap.devices.error = Some(code.into());
        self.sync();
    }

    /// Hands the app the snapshot as the run loop does, then collects what
    /// it asked for.
    fn sync(&mut self) {
        self.app.on_snapshot(&self.snap, self.now);
        self.collect();
    }

    fn collect(&mut self) {
        for r in self.app.requests.drain(..) {
            if let Request::Pairing(c) = r {
                self.sent.push(c);
            }
        }
    }

    fn deliver(&mut self, event: Event) {
        self.seq += 1;
        self.snap.pairing.push((self.seq, event));
        self.sync();
    }

    fn advance(&mut self, ms: i64) {
        self.now = self.now.add_millis(ms);
        self.sync();
    }

    fn press(&mut self, code: KeyCode) {
        self.app
            .handle_key(KeyEvent::new(code, KeyModifiers::NONE), &self.snap);
        self.collect();
    }

    fn type_text(&mut self, text: &str) {
        for ch in text.chars() {
            self.press(KeyCode::Char(ch));
        }
    }

    fn open_overlay(&mut self) {
        self.press(KeyCode::Char('a'));
        assert_eq!(self.app.overlay, Some(Overlay::Pairing));
    }

    fn status(&self) -> Option<String> {
        self.app.pairing.status()
    }

    fn opens(&self) -> Vec<(u64, String)> {
        self.sent
            .iter()
            .filter_map(|c| match c {
                Command::Open { gen, group } => Some((*gen, group.clone())),
                _ => None,
            })
            .collect()
    }

    fn confirms(&self) -> Vec<(String, [String; 4])> {
        self.sent
            .iter()
            .filter_map(|c| match c {
                Command::Confirm {
                    window_id, words, ..
                } => Some((window_id.clone(), words.clone())),
                _ => None,
            })
            .collect()
    }

    fn records(&self) -> Vec<String> {
        self.sent
            .iter()
            .filter_map(|c| match c {
                Command::Record { text, .. } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn last_open(&self) -> u64 {
        self.opens().last().expect("an open was sent").0
    }

    /// Answers the latest open with window `window_id` and a typical code.
    fn reply(&mut self, window_id: &str) {
        self.reply_to(self.last_open(), window_id, &typical_payload(), 600_000);
    }

    fn reply_to(&mut self, gen: u64, window_id: &str, payload: &str, valid_ms: i64) {
        let opened = Opened {
            window_id: window_id.into(),
            qr_payload: payload.into(),
            expires_at_ms: self.now.unix_millis() + valid_ms,
            received_at: self.now,
        };
        self.deliver(Event::Opened {
            gen,
            result: Ok(opened),
        });
    }

    fn watch(&mut self, window_id: &str, event: WatchEvent) {
        self.deliver(Event::Watch {
            window_id: window_id.into(),
            event,
        });
    }

    fn claim(&mut self, window_id: &str, device: &str) {
        self.watch(
            window_id,
            WatchEvent::Claimed(Claim {
                device_name: Some(device.into()),
                source_addr: "192.168.1.41".into(),
                fingerprint: "4744 56dd 9c0e 21b8".into(),
            }),
        );
    }

    fn confirmed(&mut self, window_id: &str, result: Result<Verdict, ConfirmFailure>) {
        self.deliver(Event::Confirmed {
            window_id: window_id.into(),
            result,
        });
    }

    /// Opens the overlay, answers the phone open with `window_id`, claims
    /// it and types four words.
    fn claimed(window_id: &str) -> Rig {
        let mut rig = Rig::new();
        rig.open_overlay();
        rig.reply(window_id);
        rig.claim(window_id, "iPhone");
        rig.type_text("maple orbit candle river");
        rig
    }

    fn render(&self, w: u16, h: u16) -> Buffer {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| self.app.render(f.buffer_mut(), &self.snap, self.now))
            .unwrap();
        term.backend().buffer().clone()
    }

    fn screen(&self) -> String {
        let buf = self.render(120, 36);
        (0..buf.area.height)
            .map(|y| row(&buf, y))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn typical_payload() -> String {
    format!("ckpair:1/{}", "b".repeat(123))
}

// ---- access groups -------------------------------------------------------------

#[test]
fn a_reply_with_three_groups_offers_those_three_in_order_with_their_counts() {
    let reply = json!({"devices": [], "self": {"expose_profiles": [
        {"name": "family", "operation_count": 12, "peers_using": 1},
        {"name": "guest", "operation_count": 3, "peers_using": 0},
        {"name": "work", "operation_count": 41, "peers_using": 2}
    ]}});
    let devices = map::devices(&reply, Snapshot::fixture_now()).unwrap();
    let mut rig = Rig::with(Some(devices.access_groups));
    rig.open_overlay();
    assert_eq!(
        rig.status().as_deref(),
        Some("Choose this phone's access · tab")
    );
    let screen = rig.screen();
    let at = |text: &str| screen.find(text).unwrap_or_else(|| panic!("no {text}"));
    assert!(at("family · 12 operations") < at("guest · 3 operations"));
    assert!(at("guest · 3 operations") < at("work · 41 operations"));
    assert!(!screen.contains("phone ·"), "no group is invented");
    assert_eq!(rig.opens(), []);
}

#[test]
fn a_reply_without_the_field_offers_phone_and_sim_with_no_count() {
    let reply = json!({"devices": [], "self": {"lan_addrs": []}});
    let devices = map::devices(&reply, Snapshot::fixture_now()).unwrap();
    assert_eq!(devices.access_groups, AccessGroups::Unlisted);
    let mut rig = Rig::with(Some(devices.access_groups));
    rig.open_overlay();
    assert_eq!(rig.opens(), [(1, "phone".to_string())]);
    rig.reply("w-1");
    let screen = rig.screen();
    assert!(screen.contains("Access  phone "), "{screen}");
    assert!(!screen.contains("operations"), "{screen}");
    rig.press(KeyCode::Tab);
    assert_eq!(rig.opens().last().unwrap().1, "sim");
}

#[test]
fn a_malformed_field_offers_nothing_and_opens_no_window() {
    for field in [
        json!("phone"),
        json!([{"name": 3, "operation_count": 1, "peers_using": 0}]),
        json!([{"name": "phone", "operation_count": "38", "peers_using": 0}]),
        json!([{"name": "phone", "operation_count": 38}]),
        json!(null),
    ] {
        let reply = json!({"devices": [], "self": {"expose_profiles": field}});
        let devices = map::devices(&reply, Snapshot::fixture_now()).unwrap();
        assert_eq!(devices.access_groups, AccessGroups::Malformed, "{field}");
        let mut rig = Rig::with(Some(devices.access_groups));
        rig.open_overlay();
        rig.press(KeyCode::Tab);
        rig.press(KeyCode::Enter);
        assert_eq!(
            rig.status().as_deref(),
            Some("Couldn't read this computer's access groups")
        );
        assert!(rig
            .screen()
            .contains("Couldn't read this computer's access groups"));
        assert_eq!(rig.opens(), [], "{field}");
        assert!(rig.app.pairing.window.is_none());
    }
}

// ---- before any open --------------------------------------------------------------

#[test]
fn with_no_reply_yet_the_overlay_reads_at_once_and_opens_nothing() {
    let mut rig = Rig::with(None);
    rig.open_overlay();
    assert_eq!(
        rig.status().as_deref(),
        Some("Reading this computer's access groups…")
    );
    assert!(rig
        .screen()
        .contains("Reading this computer's access groups…"));
    assert_eq!(rig.sent, [Command::ReadGroups]);
    assert_eq!(rig.opens(), []);
    rig.set_groups(groups(&[("phone", 38)]));
    assert_eq!(rig.opens(), [(1, "phone".to_string())]);
}

#[test]
fn a_failed_read_shows_its_code_and_enter_reads_again() {
    let mut rig = Rig::with(None);
    rig.fail_read("fed_not_authorized");
    rig.open_overlay();
    assert_eq!(
        rig.status().as_deref(),
        Some("Couldn't read this computer's access groups: fed_not_authorized")
    );
    assert_eq!(rig.opens(), []);
    rig.sent.clear();
    rig.press(KeyCode::Enter);
    assert_eq!(rig.sent, [Command::ReadGroups]);
    assert_eq!(rig.opens(), []);
}

#[test]
fn an_empty_list_offers_no_group() {
    let mut rig = Rig::with(Some(groups(&[])));
    rig.open_overlay();
    rig.press(KeyCode::Tab);
    assert_eq!(
        rig.status().as_deref(),
        Some("This computer has no access groups")
    );
    assert!(rig.screen().contains("This computer has no access groups"));
    assert_eq!(rig.opens(), []);
}

#[test]
fn a_list_without_phone_selects_nothing_until_tab_picks_one() {
    let mut rig = Rig::with(Some(groups(&[("family", 12), ("sim", 9)])));
    rig.open_overlay();
    assert_eq!(rig.app.pairing.picked(), None);
    assert_eq!(
        rig.status().as_deref(),
        Some("Choose this phone's access · tab")
    );
    assert_eq!(rig.opens(), []);
    rig.press(KeyCode::Tab);
    assert_eq!(rig.opens(), [(1, "family".to_string())]);
    assert_eq!(rig.status().as_deref(), Some("Opening a code…"));
}

#[test]
fn a_reorder_during_an_unresolved_open_changes_nothing() {
    let mut rig = Rig::with(Some(groups(&[("family", 12), ("phone", 38), ("sim", 9)])));
    rig.open_overlay();
    assert_eq!(rig.opens(), [(1, "phone".to_string())]);
    rig.set_groups(groups(&[("sim", 9), ("phone", 40), ("family", 12)]));
    assert_eq!(rig.status().as_deref(), Some("Opening a code…"));
    assert_eq!(rig.opens().len(), 1);
    rig.reply("w-1");
    let w = rig.app.pairing.window.as_ref().unwrap();
    assert_eq!((w.window_id.as_str(), w.group.as_str()), ("w-1", "phone"));
    assert_eq!(rig.status().as_deref(), Some("Waiting for your phone…"));
    assert!(rig.screen().contains("phone · 40 operations"));
    assert_eq!(rig.opens().len(), 1);
}

#[test]
fn a_group_removed_after_claimed_keeps_its_name_and_says_so() {
    let mut rig = Rig::claimed("w-1");
    rig.set_groups(groups(&[("sim", 9)]));
    assert_eq!(
        rig.status().as_deref(),
        Some("phone is no longer offered on this computer · tab picks another")
    );
    let screen = rig.screen();
    assert!(screen.contains("Access  phone "), "{screen}");
    assert!(!screen.contains("phone · 38 operations"), "{screen}");
    assert!(screen.contains("phone is no longer offered on this computer"));
    assert_eq!(rig.opens().len(), 1);
    // A confirm is still sent as typed.
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 1);
    assert_eq!(rig.confirms()[0].0, "w-1");
}

#[test]
fn tab_picks_another_group_once_the_window_group_is_gone() {
    let mut rig = Rig::claimed("w-1");
    rig.set_groups(groups(&[("family", 12), ("sim", 9)]));
    rig.press(KeyCode::Tab);
    assert_eq!(rig.opens().last().unwrap().1, "family");
    assert!(rig.sent.contains(&Command::Unwatch {
        window_id: "w-1".into()
    }));
}

// ---- serialized opens ---------------------------------------------------------------

#[test]
fn closing_and_reopening_during_an_open_then_three_group_changes_send_two_opens() {
    let mut rig = Rig::with(Some(groups(&[
        ("family", 12),
        ("phone", 38),
        ("sim", 9),
        ("work", 41),
    ])));
    let in_flight = |rig: &Rig, replies: usize| rig.opens().len() - replies;
    rig.open_overlay();
    assert_eq!(rig.opens(), [(1, "phone".to_string())]);
    rig.press(KeyCode::Esc);
    rig.open_overlay();
    for _ in 0..3 {
        rig.press(KeyCode::Tab);
        assert!(in_flight(&rig, 0) <= 1, "two opens in flight at once");
    }
    assert_eq!(rig.opens().len(), 1);
    assert_eq!(rig.status().as_deref(), Some("Opening a code…"));
    rig.reply_to(1, "w-old", &typical_payload(), 600_000);
    assert!(
        rig.app.pairing.window.is_none(),
        "a superseded code is drawn"
    );
    assert_eq!(
        rig.opens(),
        [(1, "phone".to_string()), (2, "family".to_string())]
    );
    assert!(in_flight(&rig, 1) <= 1);
    rig.reply_to(2, "w-new", &typical_payload(), 600_000);
    let w = rig.app.pairing.window.as_ref().unwrap();
    assert_eq!(
        (w.window_id.as_str(), w.group.as_str()),
        ("w-new", "family")
    );
    assert_eq!(rig.opens().len(), 2);
    assert!(rig.screen().contains("family · 12 operations"));
}

#[test]
fn until_the_reply_only_the_light_square_and_opening_show() {
    let mut rig = Rig::new();
    rig.open_overlay();
    let buf = rig.render(120, 36);
    let screen: String = (0..36).map(|y| row(&buf, y) + "\n").collect();
    assert!(screen.contains("Opening a code…"));
    let light = Theme::test().bg(Bg::QrLight);
    let mut square = 0;
    for y in 0..36 {
        for x in 0..120 {
            let cell = &buf[(x, y)];
            if cell.bg == light {
                assert_eq!(cell.symbol(), " ", "a module is drawn at {x},{y}");
                square += 1;
            }
        }
    }
    assert_eq!(square, 45 * 23);
}

// ---- windows and their results --------------------------------------------------------

#[test]
fn a_late_result_for_an_earlier_window_changes_only_devices() {
    let mut rig = Rig::claimed("w-a");
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 1);
    rig.press(KeyCode::Esc);
    rig.open_overlay();
    rig.reply("w-b");
    rig.claim("w-b", "Pixel");
    rig.type_text("one two");
    let before = rig.screen();
    rig.watch("w-a", WatchEvent::Granted);
    rig.confirmed("w-a", Ok(Verdict::Granted));
    let w = rig.app.pairing.window.as_ref().unwrap();
    assert_eq!(w.window_id, "w-b");
    assert_eq!(w.words[..2], ["one".to_string(), "two".to_string()]);
    assert_eq!(rig.status(), None);
    assert_eq!(rig.screen(), before, "B's overlay changed");
    assert_eq!(rig.records(), ["iPhone is paired"]);
    assert_eq!(rig.confirms().len(), 1);
}

#[test]
fn esc_during_the_confirm_with_its_reply_lost_then_granted_records_the_pairing() {
    let mut rig = Rig::claimed("w-a");
    rig.press(KeyCode::Enter);
    rig.press(KeyCode::Esc);
    assert!(
        !rig.sent.contains(&Command::Unwatch {
            window_id: "w-a".into()
        }),
        "the watch was closed while its confirm was in flight"
    );
    assert_eq!(rig.app.pairing.watched(), 1);
    rig.watch("w-a", WatchEvent::Granted);
    assert_eq!(rig.records(), ["iPhone is paired"]);
    assert!(rig.sent.contains(&Command::Unwatch {
        window_id: "w-a".into()
    }));
    rig.open_overlay();
    assert_eq!(rig.status().as_deref(), Some("Opening a code…"));
    assert!(rig.app.pairing.window.is_none());
    rig.advance(700_000);
    assert_eq!(rig.confirms().len(), 1, "a confirm was sent again");
}

#[test]
fn a_held_watch_ends_at_the_confirm_deadline() {
    let mut rig = Rig::claimed("w-a");
    rig.press(KeyCode::Enter);
    rig.press(KeyCode::Esc);
    rig.advance(299_000);
    assert_eq!(rig.app.pairing.watched(), 1);
    rig.advance(1_000);
    assert_eq!(rig.app.pairing.watched(), 0);
    assert_eq!(
        rig.records(),
        ["Whether pairing iPhone finished is unknown · check Devices"]
    );
}

#[test]
fn the_code_works_for_ten_minutes_from_599800_ms() {
    let mut rig = Rig::new();
    rig.open_overlay();
    rig.reply_to(1, "w-1", &typical_payload(), 599_800);
    let screen = rig.screen();
    assert!(screen.contains("for 10 minutes"), "{screen}");
    assert!(screen.contains("closes in 10:00"), "{screen}");
    rig.advance(60_000);
    assert!(rig.screen().contains("closes in 9:00"));
}

#[test]
fn step_one_wraps_the_from_line_and_fingerprint_without_losing_a_character() {
    let mut rig = Rig::new();
    rig.open_overlay();
    let payload = format!("ckpair:1/{}", "a".repeat(212));
    rig.reply_to(1, "w-1", &payload, 600_000);
    assert_eq!(rig.app.pairing.window.as_ref().unwrap().code().size(), 53);
    let addr = "2001:0db8:85a3:0000:0000:8a2e:0370:7334";
    assert_eq!(addr.len(), 39);
    let fingerprint = "0123456789abcdef".repeat(4);
    rig.watch(
        "w-1",
        WatchEvent::Claimed(Claim {
            device_name: Some("iPhone".into()),
            source_addr: addr.into(),
            fingerprint: fingerprint.clone(),
        }),
    );
    let buf = rig.render(120, 36);
    // The steps column starts beside the code's 57-module area, quiet zone included.
    let column = |y: u16| -> String {
        row(&buf, y)
            .chars()
            .skip(66)
            .take(48)
            .collect::<String>()
            .trim_end()
            .to_string()
    };
    let from = (0..36)
        .find(|&y| column(y).starts_with("From "))
        .expect("a From line");
    assert_eq!(column(from), format!("From {addr} ·"));
    let mut joined = String::new();
    for y in from + 1..36 {
        let line = column(y);
        if line.is_empty() {
            break;
        }
        joined.push_str(&line);
    }
    assert_eq!(joined.strip_prefix("key ").unwrap(), fingerprint);
    let screen: String = (0..36).map(|y| row(&buf, y) + "\n").collect();
    for word in [
        "⏎  Pair",
        "esc  Close",
        "You confirm the pairing on this computer.",
    ] {
        assert!(screen.contains(word), "{word}");
    }
    assert!(
        !screen.contains("Pair \""),
        "callosum's own prompt is shown"
    );
}

#[test]
fn words_advance_with_space_or_tab_and_enter_focuses_the_first_empty_field() {
    let mut rig = Rig::new();
    rig.open_overlay();
    rig.reply("w-1");
    rig.claim("w-1", "iPhone");
    rig.type_text("maple orbit");
    rig.press(KeyCode::Tab);
    rig.press(KeyCode::Tab);
    rig.type_text("river");
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms(), []);
    assert_eq!(rig.app.pairing.window.as_ref().unwrap().word_sel, 2);
    rig.type_text("candle");
    rig.app.pairing.window.as_mut().unwrap().words[0] = "  maple ".into();
    rig.press(KeyCode::Enter);
    rig.press(KeyCode::Enter);
    assert_eq!(
        rig.confirms(),
        [(
            "w-1".to_string(),
            ["maple", "orbit", "candle", "river"].map(String::from)
        )]
    );
    // Backspace in an empty field moves back.
    let mut rig = Rig::new();
    rig.open_overlay();
    rig.reply("w-1");
    rig.claim("w-1", "iPhone");
    rig.type_text("ab ");
    rig.press(KeyCode::Backspace);
    rig.press(KeyCode::Backspace);
    let w = rig.app.pairing.window.as_ref().unwrap();
    assert_eq!((w.word_sel, w.words[0].as_str()), (0, "a"));
}

// ---- status lines ---------------------------------------------------------------------

#[test]
fn an_open_refused_for_an_unknown_group_reads_as_no_such_group() {
    for code in ["fed_bad_request", "fed_target_unavailable"] {
        let mut rig = Rig::new();
        rig.open_overlay();
        rig.deliver(Event::Opened {
            gen: 1,
            result: Err(Refusal {
                code: code.into(),
                message: "unknown expose profile \"phone\"".into(),
                retry_after_ms: None,
            }),
        });
        assert_eq!(
            rig.status().as_deref(),
            Some("This computer has no access group named phone · tab changes the group"),
            "{code}"
        );
        assert!(rig.app.pairing.window.is_none());
        rig.press(KeyCode::Tab);
        assert_eq!(rig.opens().last().unwrap().1, "sim");
    }
    let mut rig = Rig::new();
    rig.open_overlay();
    rig.deliver(Event::Opened {
        gen: 1,
        result: Err(Refusal {
            code: "fed_target_unavailable".into(),
            message: "no LAN listener".into(),
            retry_after_ms: None,
        }),
    });
    assert_eq!(
        rig.status().as_deref(),
        Some("Couldn't open a code: fed_target_unavailable · ⏎ try again")
    );
    rig.press(KeyCode::Enter);
    assert_eq!(rig.opens().len(), 2);
}

#[test]
fn confirm_refusals_read_as_plain_sentences() {
    for (code, line) in [
        (
            "fed_not_authorized",
            "Pairing failed: callosum didn't authorize ck to confirm it",
        ),
        (
            "fed_bad_request",
            "Pairing failed: callosum couldn't read the confirm ck sent",
        ),
        (
            "fed_target_unavailable",
            "Pairing failed: callosum is missing its rendezvous credential, LAN listener or store",
        ),
        ("fed_not_ready", "Pairing failed: callosum isn't ready yet"),
        (
            "fed_pairing_not_pending",
            "The claim expired or was cancelled · ⏎ scan a new code",
        ),
        (
            "fed_operator_confirmation_refused",
            "Confirmation declined · ⏎ try again",
        ),
        (
            "fed_operator_confirmation_backoff",
            "Confirmation is cooling down · try again shortly",
        ),
        // A code callosum doesn't document still shows as it came.
        ("fed_something_new", "Pairing failed: fed_something_new"),
    ] {
        let mut rig = Rig::claimed("w-1");
        rig.press(KeyCode::Enter);
        rig.confirmed("w-1", Err(ConfirmFailure::Refused(Refusal::code(code))));
        assert_eq!(rig.status().as_deref(), Some(line), "{code}");
        let first_words: String = line.chars().take(30).collect();
        assert!(rig.screen().contains(&first_words), "{code}");
        if code != "fed_something_new" {
            assert!(!rig.screen().contains(code), "{code} shown raw");
        }
    }
}

#[test]
fn every_state_has_its_status_line() {
    let mut rig = Rig::new();
    rig.open_overlay();
    assert_eq!(rig.status().as_deref(), Some("Opening a code…"));
    rig.reply("w-1");
    assert_eq!(rig.status().as_deref(), Some("Waiting for your phone…"));
    rig.watch("w-1", WatchEvent::ForeignAttempt("192.0.2.9".into()));
    assert!(rig
        .screen()
        .contains("Someone at 192.0.2.9 tried this code"));
    rig.claim("w-1", "iPhone");
    assert_eq!(rig.status(), None);
    rig.type_text("maple orbit candle river");
    rig.press(KeyCode::Enter);
    assert_eq!(
        rig.status().as_deref(),
        Some("Waiting for confirmation on this computer")
    );
    assert!(rig
        .screen()
        .contains("Waiting for confirmation on this computer"));

    rig.confirmed(
        "w-1",
        Err(ConfirmFailure::Refused(Refusal::code(
            "fed_operator_confirmation_refused",
        ))),
    );
    assert_eq!(
        rig.status().as_deref(),
        Some("Confirmation declined · ⏎ try again")
    );
    assert_eq!(rig.app.pairing.window.as_ref().unwrap().words[3], "river");

    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 2);
    rig.confirmed(
        "w-1",
        Err(ConfirmFailure::Refused(Refusal {
            code: "fed_operator_confirmation_backoff".into(),
            message: String::new(),
            retry_after_ms: Some(12_300),
        })),
    );
    assert_eq!(
        rig.status().as_deref(),
        Some("Confirmation is cooling down · try again in 13 s")
    );
    rig.press(KeyCode::Enter);
    assert_eq!(
        rig.confirms().len(),
        2,
        "⏎ during the backoff sent a confirm"
    );
    rig.advance(12_300);
    assert_eq!(rig.status(), None);
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 3);
    rig.confirmed(
        "w-1",
        Err(ConfirmFailure::Refused(Refusal::code(
            "fed_operator_confirmation_backoff",
        ))),
    );
    assert_eq!(
        rig.status().as_deref(),
        Some("Confirmation is cooling down · try again shortly")
    );
    rig.advance(29_000);
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 3, "the unnamed backoff lasts 30 s");
    rig.advance(1_000);
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 4);
    rig.confirmed(
        "w-1",
        Err(ConfirmFailure::Refused(Refusal::code(
            "fed_pairing_not_pending",
        ))),
    );
    assert_eq!(
        rig.status().as_deref(),
        Some("The claim expired or was cancelled · ⏎ scan a new code")
    );
    assert_eq!(rig.app.pairing.window.as_ref().unwrap().words[0], "");

    for (event, line) in [
        (WatchEvent::Declined, "Pairing was declined · ⏎ new code"),
        (WatchEvent::Expired, "The code expired · ⏎ new code"),
    ] {
        let mut rig = Rig::new();
        rig.open_overlay();
        rig.reply("w-1");
        rig.watch("w-1", event);
        assert_eq!(rig.status().as_deref(), Some(line));
        rig.press(KeyCode::Enter);
        assert_eq!(rig.opens().len(), 2, "⏎ opens a new code");
    }

    let mut rig = Rig::new();
    rig.open_overlay();
    rig.reply("w-1");
    rig.deliver(Event::WatchEnded {
        window_id: "w-1".into(),
    });
    assert_eq!(
        rig.status().as_deref(),
        Some("Lost contact with callosum · ⏎ new code")
    );

    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.confirmed("w-1", Ok(Verdict::Mismatch));
    assert_eq!(
        rig.status().as_deref(),
        Some("The words didn't match · this code is closed · ⏎ new code")
    );

    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.watch("w-1", WatchEvent::Granted);
    assert_eq!(rig.status().as_deref(), Some("iPhone is paired."));
    let screen = rig.screen();
    assert!(screen.contains("iPhone is paired."));
    assert!(screen.contains("It has phone access to this computer."));
    assert!(screen.contains("⏎  Done"));
    assert!(!screen.contains("remove it any time"));
    rig.press(KeyCode::Enter);
    assert_eq!(rig.app.overlay, None, "⏎ Done closes the overlay");
}

// ---- timing and races -----------------------------------------------------------------

#[test]
fn the_confirm_call_may_run_for_300_s() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    let timeout = rig.sent.iter().find_map(|c| match c {
        Command::Confirm { timeout, .. } => Some(*timeout),
        _ => None,
    });
    assert_eq!(timeout, Some(std::time::Duration::from_secs(300)));
}

#[test]
fn a_confirm_waits_past_the_window_expiry() {
    let mut rig = Rig::new();
    rig.open_overlay();
    rig.reply_to(1, "w-1", &typical_payload(), 30_000);
    rig.claim("w-1", "iPhone");
    rig.type_text("maple orbit candle river");
    rig.press(KeyCode::Enter);
    // Past the window's expiry and the claim's 120 s, with confirmation still
    // pending: the confirm is still waited for.
    rig.advance(200_000);
    assert_eq!(
        rig.status().as_deref(),
        Some("Waiting for confirmation on this computer")
    );
    rig.confirmed("w-1", Ok(Verdict::Granted));
    assert_eq!(rig.status().as_deref(), Some("iPhone is paired."));
    // Without a reply, the confirm counts as lost at 300 s.
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.advance(299_000);
    assert_eq!(
        rig.status().as_deref(),
        Some("Waiting for confirmation on this computer")
    );
    rig.advance(1_000);
    assert_eq!(
        rig.status().as_deref(),
        Some("Whether pairing finished is unknown · check Devices · ⏎ new code")
    );
}

#[test]
fn a_double_enter_sends_one_confirm() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 1);
}

#[test]
fn granted_and_expired_decide_before_the_confirm_reply() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.watch("w-1", WatchEvent::Granted);
    assert_eq!(rig.status().as_deref(), Some("iPhone is paired."));
    // The reply racing in after the event changes nothing.
    rig.confirmed("w-1", Ok(Verdict::Granted));
    assert_eq!(rig.status().as_deref(), Some("iPhone is paired."));
    assert!(rig.records().is_empty());

    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.watch("w-1", WatchEvent::Expired);
    assert_eq!(
        rig.status().as_deref(),
        Some("The code expired · ⏎ new code")
    );
}

#[test]
fn a_reply_racing_ahead_of_the_terminal_event_decides_once() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.confirmed("w-1", Ok(Verdict::Granted));
    rig.watch("w-1", WatchEvent::Granted);
    assert_eq!(rig.status().as_deref(), Some("iPhone is paired."));
    let unwatches = rig
        .sent
        .iter()
        .filter(|c| matches!(c, Command::Unwatch { .. }))
        .count();
    assert_eq!(unwatches, 1);
}

#[test]
fn declined_during_the_confirm_waits_and_the_mismatch_reply_decides() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.watch("w-1", WatchEvent::Declined);
    assert_eq!(
        rig.status().as_deref(),
        Some("Waiting for confirmation on this computer")
    );
    rig.confirmed("w-1", Ok(Verdict::Mismatch));
    assert_eq!(
        rig.status().as_deref(),
        Some("The words didn't match · this code is closed · ⏎ new code")
    );
}

#[test]
fn a_lost_confirm_reply_then_granted_shows_granted() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.confirmed("w-1", Err(ConfirmFailure::Lost));
    assert_eq!(
        rig.status().as_deref(),
        Some("Whether pairing finished is unknown · check Devices · ⏎ new code")
    );
    rig.watch("w-1", WatchEvent::Granted);
    assert_eq!(rig.status().as_deref(), Some("iPhone is paired."));
    assert_eq!(rig.confirms().len(), 1);
}

#[test]
fn both_channels_lost_reads_unknown_and_sends_no_second_confirm() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    rig.deliver(Event::WatchEnded {
        window_id: "w-1".into(),
    });
    assert_eq!(
        rig.status().as_deref(),
        Some("Waiting for confirmation on this computer")
    );
    rig.confirmed("w-1", Err(ConfirmFailure::Lost));
    assert_eq!(
        rig.status().as_deref(),
        Some("Whether pairing finished is unknown · check Devices · ⏎ new code")
    );
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 1);
    assert_eq!(rig.opens().len(), 2, "⏎ asks for a new code instead");
}

#[test]
fn with_two_watches_held_reads_and_confirms_still_complete() {
    let mut rig = Rig::claimed("w-a");
    rig.press(KeyCode::Enter);
    rig.press(KeyCode::Esc);
    rig.open_overlay();
    rig.reply("w-b");
    rig.claim("w-b", "Pixel");
    rig.type_text("one two three four");
    rig.press(KeyCode::Enter);
    rig.press(KeyCode::Esc);
    assert_eq!(rig.app.pairing.watched(), 2, "both watches are held");
    rig.open_overlay();
    rig.set_groups(groups(&[("phone", 40), ("sim", 9)]));
    rig.reply("w-c");
    assert!(rig.screen().contains("phone · 40 operations"));
    rig.claim("w-c", "iPad");
    rig.type_text("five six seven eight");
    rig.press(KeyCode::Enter);
    assert_eq!(rig.confirms().len(), 3);
    rig.confirmed("w-c", Ok(Verdict::Granted));
    assert_eq!(rig.status().as_deref(), Some("iPad is paired."));
    rig.confirmed("w-a", Ok(Verdict::Mismatch));
    rig.watch("w-b", WatchEvent::Granted);
    assert_eq!(
        rig.records(),
        ["Pairing iPhone: the words didn't match", "Pixel is paired"]
    );
    assert_eq!(rig.status().as_deref(), Some("iPad is paired."));
}

// ---- esc --------------------------------------------------------------------------------

#[test]
fn esc_closes_at_every_step_and_reopening_opens_again() {
    let steps: [fn(&mut Rig); 4] = [
        |_| {},
        |rig| rig.reply("w-1"),
        |rig| {
            rig.reply("w-1");
            rig.claim("w-1", "iPhone");
        },
        |rig| {
            rig.reply("w-1");
            rig.claim("w-1", "iPhone");
            rig.type_text("a b c d");
            rig.press(KeyCode::Enter);
            rig.watch("w-1", WatchEvent::Granted);
        },
    ];
    for (i, step) in steps.into_iter().enumerate() {
        let mut rig = Rig::new();
        rig.open_overlay();
        step(&mut rig);
        let opens = rig.opens().len();
        rig.press(KeyCode::Esc);
        assert_eq!(rig.app.overlay, None, "step {i}");
        assert!(rig.app.pairing.window.is_none(), "step {i}");
        assert!(
            rig.sent.iter().all(|c| matches!(
                c,
                Command::Open { .. }
                    | Command::Watch { .. }
                    | Command::Unwatch { .. }
                    | Command::Confirm { .. }
                    | Command::ReadGroups
            )),
            "step {i}: esc asked for something else: {:?}",
            rig.sent
        );
        rig.open_overlay();
        if i == 0 {
            // The first open is still in flight; the new one waits for it.
            rig.reply_to(1, "w-0", &typical_payload(), 600_000);
        }
        assert_eq!(rig.opens().len(), opens + 1, "step {i}");
    }
}

#[test]
fn the_debug_form_of_a_confirm_hides_the_words() {
    let mut rig = Rig::claimed("w-1");
    rig.press(KeyCode::Enter);
    let shown = format!("{:?}", rig.sent);
    for word in ["maple", "orbit", "candle", "river"] {
        assert!(!shown.contains(word), "{shown}");
    }
    let opened = Opened {
        window_id: "w".into(),
        qr_payload: typical_payload(),
        expires_at_ms: 0,
        received_at: Timestamp::from_unix_millis(0),
    };
    assert!(!format!("{opened:?}").contains("ckpair:"));
}

#[test]
fn the_reading_prefers_a_failed_latest_read() {
    let mut panel = Snapshot::fixture().devices;
    panel.error = Some("fed_not_ready".into());
    assert_eq!(Reading::of(&panel), Reading::Failed("fed_not_ready".into()));
}
