//! The code and the entire confirmation column must fit without covering chrome.

use ck_ui::app::{App, Overlay, Tab};
use ck_ui::pairing::{self, Claim, PairingState};
use ck_ui::theme::{Bg, Fg};
use ck_ui::{Snapshot, Theme};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::Terminal;

fn payload() -> String {
    format!("ckpair:1/{}", "a".repeat(212))
}

fn app() -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Devices;
    app.overlay = Some(Overlay::Pairing);
    let now = Snapshot::fixture_now();
    app.pairing.window = Some(
        PairingState::new(
            &payload(),
            "phone".into(),
            Some(38),
            now,
            now.add_millis(599_800),
        )
        .unwrap(),
    );
    app
}

fn render(app: &App, w: u16, h: u16) -> Buffer {
    let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
    term.draw(|f| {
        app.render(
            f.buffer_mut(),
            &Snapshot::fixture(),
            Snapshot::fixture_now(),
        )
    })
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

fn claim(app: &mut App) {
    let s = app.pairing.window.as_mut().unwrap();
    s.claim = Some(Claim {
        device_name: "iPhone".into(),
        source_addr: "192.168.1.41".into(),
        fingerprint: "4744 56dd 9c0e 21b8".into(),
    });
    s.words = ["maple", "orbit", "candle", "river"].map(String::from);
    s.word_sel = 3;
}

#[test]
fn maximum_payload_keeps_the_whole_code_and_countdown_inside_chrome() {
    assert_eq!(payload().len(), 221);
    let app = app();
    let code = app.pairing.window.as_ref().unwrap().code();
    assert_eq!(code.size(), 53);
    let buf = render(&app, 120, 36);
    assert_eq!(buf[(2, 2)].symbol(), "╭");
    assert_eq!(buf[(117, 33)].symbol(), "╯");
    assert!(text(&buf).contains("closes in 10:00"));
    let qr_style = Theme::test().style(ck_ui::theme::St::fg(Fg::QrDark).on(Bg::QrLight));
    let mut cells = 0;
    for y in 0..36 {
        for x in 0..120 {
            if buf[(x, y)].bg == qr_style.bg.unwrap() {
                assert!((5..62).contains(&x) && (4..33).contains(&y), "{x},{y}");
                cells += 1;
            }
        }
    }
    assert_eq!(cells, 57 * 29);
    for y in [0, 1, 34, 35] {
        let mut plain = app.clone();
        plain.overlay = None;
        let plain = render(&plain, 120, 36);
        assert_eq!(row(&buf, y), row(&plain, y));
    }
}

#[test]
fn maximum_payload_confirmation_keeps_four_fields_and_pair_beside_the_code() {
    let mut app = app();
    claim(&mut app);
    let buf = render(&app, 120, 36);
    let rows: Vec<_> = (4..33).map(|y| row(&buf, y)).collect();
    for word in [
        "maple",
        "orbit",
        "candle",
        "river",
        "Pair",
        "From 192.168.1.41",
        "Touch ID confirms",
    ] {
        let line = rows
            .iter()
            .find(|r| r.contains(word))
            .unwrap_or_else(|| panic!("missing {word}"));
        let x = line.chars().collect::<String>().find(word).unwrap();
        // All preceding QR glyphs are multibyte, so use character columns, not byte offsets.
        let column = line[..x].chars().count();
        assert!((66..114).contains(&column), "{word} at {column}");
    }
    assert!(rows.iter().any(|r| r.contains("⏎  Pair")));
    assert_eq!(buf[(5, 4)].symbol(), " ");
    assert_eq!(buf[(5, 4)].bg, Theme::test().bg(Bg::QrLight));
}

#[test]
fn half_blocks_and_quiet_zone_use_explicit_dark_on_light_in_both_themes() {
    for theme in [Theme::light(), Theme::dark()] {
        let mut app = app();
        app.theme = theme;
        let code = app.pairing.window.as_ref().unwrap().code();
        let buf = render(&app, 120, 36);
        for y in 0..29 {
            for x in 0..57 {
                let cell = &buf[((x + 5) as u16, (y + 4) as u16)];
                let module = |dy| code.get_module(x - 2, y * 2 - 2 + dy);
                let glyph = match (module(0), module(1)) {
                    (true, true) => "█",
                    (true, false) => "▀",
                    (false, true) => "▄",
                    _ => " ",
                };
                assert_eq!(cell.symbol(), glyph, "{x},{y}");
                assert_eq!(cell.fg, ratatui::style::Color::Rgb(0, 0, 0));
                assert_eq!(cell.bg, ratatui::style::Color::Rgb(255, 255, 255));
                if !(2..55).contains(&x) || y == 0 || y == 28 {
                    assert_eq!(cell.symbol(), " ", "quiet zone {x},{y}");
                }
            }
        }
    }
}

#[test]
fn small_window_names_the_computed_minimum_and_resize_keeps_words() {
    let mut app = app();
    claim(&mut app);
    app.pairing.window.as_mut().unwrap().word_sel = 0;
    app.pairing.window.as_mut().unwrap().words = std::array::from_fn(|_| String::new());
    for word in ["quit", "t1?:", "candle", "river"] {
        for ch in word.chars() {
            app.handle_key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &Snapshot::fixture(),
            );
        }
        app.handle_key(
            KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
            &Snapshot::fixture(),
        );
    }
    assert!(!app.quit);
    assert_eq!(app.tab, Tab::Devices);
    assert!(app.requests.is_empty());
    let saved = app.pairing.window.as_ref().unwrap().words.clone();
    app.handle_event(&Event::Resize(80, 24), &Snapshot::fixture(), (80, 24));
    let small = render(&app, 80, 24);
    assert!(text(&small).contains("Make the window at least 116×35 to show the code"));
    assert!(!text(&small).contains("⏎  Pair"));
    app.handle_event(&Event::Resize(120, 36), &Snapshot::fixture(), (120, 36));
    assert_eq!(app.pairing.window.as_ref().unwrap().words, saved);
    let large = text(&render(&app, 120, 36));
    for word in saved {
        assert!(large.contains(&word));
    }
}

#[test]
fn esc_closes_the_overlay_and_forgets_the_code_and_the_words() {
    let mut app = app();
    claim(&mut app);
    app.handle_key(
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        &Snapshot::fixture(),
    );
    assert_eq!(app.overlay, None);
    assert!(app.pairing.window.is_none());
}

#[test]
fn debug_output_shows_neither_the_payload_nor_the_typed_words() {
    let mut app = app();
    claim(&mut app);
    let shown = format!("{:?}", app.pairing.window.as_ref().unwrap());
    assert!(!shown.contains("ckpair:"), "{shown}");
    for word in ["maple", "orbit", "candle", "river"] {
        assert!(!shown.contains(word), "{shown}");
    }
}

#[test]
fn layout_removes_padding_in_order_and_reserves_all_four_chrome_rows() {
    let code = pairing::encode(&payload()).unwrap();
    for (height, top, content_y, box_height) in [
        (39, 3, 5, 34),
        (38, 2, 4, 34),
        (37, 2, 4, 32),
        (36, 2, 4, 32),
        (35, 2, 3, 31),
    ] {
        let l = pairing::layout(120, height, &code, 22).unwrap();
        assert_eq!((l.y, l.content_y, l.height), (top, content_y, box_height));
        assert!(l.y >= 2 && l.y + l.height <= height - 2);
    }
    assert!(pairing::layout(120, 34, &code, 22).is_none());
    assert!(pairing::layout(115, 60, &code, 22).is_none());
}

#[test]
fn long_claim_metadata_wraps_in_the_right_column_at_every_size() {
    let mut app = app();
    claim(&mut app);
    let s = app.pairing.window.as_mut().unwrap();
    s.claim.as_mut().unwrap().fingerprint = "abcd ".repeat(20).trim().into();
    s.foreign_attempt = Some("192.0.2.123:45678".into());
    s.group = "access-group-".repeat(5);
    for (w, h) in [
        (80, 24),
        (100, 30),
        (120, 36),
        (140, 40),
        (160, 48),
        (200, 60),
    ] {
        let buf = render(&app, w, h);
        let body = text(&buf);
        if body.contains("Make the window") {
            continue;
        }
        assert!(body.contains("⏎  Pair"));
        assert!(body.contains("Someone at 192.0.2.123:45678 tried this code"));
        let left = (i32::from(w) - 116) / 2;
        let steps = left + 64;
        let top = (2..h - 2)
            .find(|&y| buf[(left as u16, y)].symbol() == "╭")
            .unwrap();
        let bottom = (2..h - 2)
            .find(|&y| buf[(left as u16, y)].symbol() == "╰")
            .unwrap();
        for y in top + 1..bottom {
            for x in (steps + 48)..(left + 115) {
                assert_eq!(buf[(x as u16, y)].symbol(), " ");
            }
        }
        let rows = body.lines().collect::<Vec<_>>();
        let from = rows.iter().position(|r| r.contains("From ")).unwrap();
        let button = rows.iter().position(|r| r.contains("⏎  Pair")).unwrap();
        assert!(button > from + 2);
    }
}

#[test]
fn validity_minutes_round_to_the_nearest_minute_and_never_zero() {
    let mut app = app();
    let s = app.pairing.window.as_mut().unwrap();
    assert_eq!(s.minutes(), 10);
    s.expires_at = s.opened_at.add_millis(1);
    assert_eq!(s.minutes(), 1);
}
