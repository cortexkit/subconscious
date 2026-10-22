//! What a user sees and does: moving the selection, the palette, every
//! window size, the footer at narrow widths, NO_COLOR, restoring the
//! terminal, and choosing light or dark. Nothing here touches a real
//! terminal.

use std::cell::RefCell;
use std::io::{self, Write};
use std::rc::Rc;

use ck_ui::app::{App, Overlay, RawMode, Tab, TerminalGuard, TextField};
use ck_ui::draw::{footer, Canvas};
use ck_ui::theme::{choose_mode, parse_osc11, theme_for, Bg, Mode, Osc11Collector, St, Theme};
use ck_ui::Snapshot;
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

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

fn selected(buf: &Buffer, theme: &Theme, x: u16, y: u16) -> bool {
    buf[(x, y)].bg == theme.bg(Bg::Sel)
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) {
    app.handle_key(key(code), snap);
}

#[test]
fn arrows_move_the_modules_selection_and_the_pane_follows() {
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    app.mod_sel = 3;
    let theme = Theme::test();

    let before = render(&app, &snap, 120, 36);
    assert!(selected(&before, &theme, 1, 10), "broca's row is painted");
    assert_eq!(text(&before, 77, 3, 5), "broca");

    press(&mut app, &snap, KeyCode::Down);
    let after = render(&app, &snap, 120, 36);
    assert!(selected(&after, &theme, 1, 11), "callosum's row is painted");
    assert!(
        selected(&after, &theme, 72, 11),
        "the paint spans the table"
    );
    assert!(
        !selected(&after, &theme, 1, 10),
        "broca's row is no longer painted"
    );
    assert_eq!(text(&after, 77, 3, 8), "callosum");
    assert!(row(&after, 4).contains("pid 2214"));

    press(&mut app, &snap, KeyCode::Char('k'));
    press(&mut app, &snap, KeyCode::Up);
    let again = render(&app, &snap, 120, 36);
    assert!(selected(&again, &theme, 1, 9));
    assert_eq!(text(&again, 77, 3, 5), "basal");

    // The selection stays on screen in a short window by scrolling the table.
    app.mod_sel = 24;
    let short = render(&app, &snap, 120, 24);
    let rows: Vec<String> = (7..=20).map(|y| row(&short, y)).collect();
    let at = rows
        .iter()
        .position(|r| r.contains("wernicke"))
        .expect("wernicke shown");
    assert!(selected(&short, &theme, 1, 7 + at as u16));
}

#[test]
fn enter_on_home_opens_that_module_and_esc_comes_back() {
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::test());
    press(&mut app, &snap, KeyCode::Down);
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(app.tab, Tab::Modules);
    assert_eq!(snap.modules.data[app.mod_sel].id, "thalamus-harness");
    press(&mut app, &snap, KeyCode::Esc);
    assert_eq!(app.tab, Tab::Home);
}

#[test]
fn the_palette_filters_as_you_type_and_enter_navigates() {
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::test());
    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &snap,
    );
    assert_eq!(app.overlay, Some(Overlay::Palette));

    let listed = |app: &App| -> Vec<String> {
        let buf = render(app, &snap, 120, 36);
        (7..19)
            .map(|y| text(&buf, 29, y, 48).trim_end().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    };
    assert_eq!(listed(&app).len(), 12, "an empty query lists every command");

    for ch in "logs".chars() {
        press(&mut app, &snap, KeyCode::Char(ch));
    }
    assert_eq!(listed(&app), vec!["Open broca logs".to_string()]);
    let buf = render(&app, &snap, 120, 36);
    assert_eq!(text(&buf, 29, 5, 4), "logs", "the query is shown as typed");

    press(&mut app, &snap, KeyCode::Backspace);
    press(&mut app, &snap, KeyCode::Backspace);
    press(&mut app, &snap, KeyCode::Backspace);
    assert!(listed(&app).len() > 1, "deleting widens the list again");

    press(&mut app, &snap, KeyCode::Char('o'));
    press(&mut app, &snap, KeyCode::Char('g'));
    press(&mut app, &snap, KeyCode::Char('s'));
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(app.overlay, None);
    assert_eq!(app.tab, Tab::Modules);
    assert_eq!(snap.modules.data[app.mod_sel].id, "broca");

    press(&mut app, &snap, KeyCode::Char(':'));
    press(&mut app, &snap, KeyCode::Esc);
    assert_eq!(app.overlay, None, "esc closes the palette");
}

#[test]
fn every_size_renders_without_panicking() {
    let snap = Snapshot::fixture();
    let down = Snapshot::fixture_daemon_down();
    let mut apps = Vec::new();
    for (tab, overlay) in [
        (Tab::Home, None),
        (Tab::Modules, None),
        (Tab::Home, Some(Overlay::Palette)),
        (Tab::Modules, Some(Overlay::Help)),
        (Tab::Activity, None),
        (Tab::Usage, None),
        (Tab::Agents, None),
        (Tab::Devices, None),
        (Tab::Settings, None),
    ] {
        let mut app = App::new(Theme::test());
        app.tab = tab;
        app.overlay = overlay;
        app.mod_sel = 24;
        app.tick = 7;
        apps.push((app, &snap));
    }
    apps.push((App::new(Theme::test()), &down));
    // 15,400 sizes for each screen: spread the widths over threads.
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()) as u16;
    std::thread::scope(|scope| {
        for t in 0..threads {
            let apps = &apps;
            scope.spawn(move || {
                for w in (1..=220u16).filter(|w| w % threads == t) {
                    for h in 1..=70u16 {
                        for (app, snap) in apps {
                            let buf = render(app, snap, w, h);
                            if w < 80 || h < 24 {
                                let all: String = (0..h).map(|y| row(&buf, y)).collect();
                                assert!(
                                    all.trim().is_empty()
                                        || "Make the window at least 80×24".contains(all.trim()),
                                    "{w}×{h} draws only the size note"
                                );
                            }
                        }
                    }
                }
            });
        }
    });
}

#[test]
fn footer_hints_drop_whole_items_lowest_priority_first() {
    let hints = [
        ("↑↓", "select"),
        ("r", "restart"),
        ("s", "stop"),
        ("l", "logs"),
        ("e", "settings"),
    ];
    let full = "  ↑↓  select    r  restart    s  stop    l  logs    e  settings";
    let foot = |w: u16| {
        let mut buf = Buffer::empty(Rect::new(0, 0, w, 1));
        let mut c = Canvas::new(&mut buf, Theme::test());
        footer(&mut c, &hints, "updated just now", St::D);
        row(&buf, 0).trim_end().to_string()
    };
    assert!(foot(120).ends_with("updated just now"));
    assert!(foot(120).starts_with(full));
    // At 80 columns every hint stays and the status text gives way.
    assert_eq!(foot(80), full);
    let mut kept = Vec::new();
    for w in 1..80 {
        let line = foot(w);
        assert!(!line.contains("updated"), "{w}: no status text");
        assert!(
            full.starts_with(&line),
            "{w}: a prefix of the hints: {line:?}"
        );
        let n = hints
            .iter()
            .filter(|(_, l)| line.contains(&format!(" {l}")))
            .count();
        let expected_end = [" select", " restart", " stop", " logs", " settings"];
        if n > 0 {
            assert!(
                line.ends_with(expected_end[n - 1]),
                "{w}: whole hints only: {line:?}"
            );
        } else {
            assert!(line.is_empty(), "{w}: nothing partial: {line:?}");
        }
        kept.push(n);
    }
    assert!(
        kept.windows(2).all(|p| p[0] <= p[1]),
        "narrower never keeps more"
    );
    assert_eq!(kept[0], 0);
    assert_eq!(*kept.last().unwrap(), 5);
}

#[test]
fn no_color_yields_no_colors() {
    let theme = theme_for(Some(""), None, None);
    assert!(theme.is_no_color());
    let snap = Snapshot::fixture();
    let mut bold = 0;
    for (tab, overlay) in [
        (Tab::Home, None),
        (Tab::Modules, None),
        (Tab::Home, Some(Overlay::Palette)),
    ] {
        let mut app = App::new(theme);
        app.tab = tab;
        app.overlay = overlay;
        app.mod_sel = 3;
        for (w, h) in [(80, 24), (120, 36), (160, 48)] {
            let buf = render(&app, &snap, w, h);
            for cell in buf.content() {
                assert_eq!((cell.fg, cell.bg), (Color::Reset, Color::Reset));
                if cell.modifier.contains(Modifier::BOLD) {
                    bold += 1;
                }
            }
        }
    }
    assert!(bold > 0, "modifiers stay");
    // The color theme of the same screen does use color, so the check above
    // can tell the difference.
    let buf = render(&App::new(Theme::light()), &snap, 120, 36);
    assert!(buf.content().iter().any(|c| c.fg != Color::Reset));
}

#[test]
fn mouse_clicks_tabs_and_rows_and_scrolls_the_selection() {
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::test());
    let click = |x: u16, y: u16| {
        Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        })
    };
    let scroll = |kind| {
        Event::Mouse(MouseEvent {
            kind,
            column: 10,
            row: 10,
            modifiers: KeyModifiers::NONE,
        })
    };
    assert!(app.handle_event(&click(30, 0), &snap, (120, 36)));
    assert_eq!(app.tab, Tab::Modules);
    assert!(app.handle_event(&click(10, 12), &snap, (120, 36)));
    assert_eq!(app.mod_sel, 5);
    app.handle_event(&scroll(MouseEventKind::ScrollDown), &snap, (120, 36));
    assert_eq!(app.mod_sel, 6);
    app.handle_event(&scroll(MouseEventKind::ScrollUp), &snap, (120, 36));
    app.handle_event(&scroll(MouseEventKind::ScrollUp), &snap, (120, 36));
    assert_eq!(app.mod_sel, 4);
    assert!(app.handle_event(&click(18, 0), &snap, (120, 36)));
    assert_eq!(app.tab, Tab::Home);
    // The fixture serves every tab, so each one the bar draws opens.
    assert!(app.handle_event(&click(41, 0), &snap, (120, 36)));
    assert_eq!(app.tab, Tab::Activity);
    // The live app's bar has no Devices, so the place it has in the full
    // bar does nothing.
    let mut live = Snapshot::fixture();
    live.serves = ck_ui::model::Serves::LIVE;
    let mut app = App::new(Theme::test());
    assert!(!app.handle_event(&click(75, 0), &live, (120, 36)));
    assert_eq!(app.tab, Tab::Home);
    // Activity, Usage and Agents are served live, at the same places as in
    // the full bar.
    assert!(app.handle_event(&click(41, 0), &live, (120, 36)));
    assert_eq!(app.tab, Tab::Activity);
    assert!(app.handle_event(&click(54, 0), &live, (120, 36)));
    assert_eq!(app.tab, Tab::Usage);
    assert!(app.handle_event(&click(64, 0), &live, (120, 36)));
    assert_eq!(app.tab, Tab::Agents);
}

#[test]
fn quit_keys_and_later_screens_keys() {
    // Every number key reaches its own tab while that tab shows: the
    // fixture serves them all and its roster has every module they need.
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::light());
    for tab in Tab::ALL {
        assert!(app.handle_key(key(KeyCode::Char(tab.key())), &snap));
        assert_eq!(app.tab, tab);
    }
    // The live app serves Home, Modules, Activity, Usage and Agents: the
    // later screens' keys do nothing there.
    let mut live = Snapshot::fixture();
    live.serves = ck_ui::model::Serves::LIVE;
    let mut app = App::new(Theme::light());
    for k in '6'..='7' {
        assert!(!app.handle_key(key(KeyCode::Char(k)), &live));
        assert_eq!(app.tab, Tab::Home);
    }
    assert!(app.handle_key(key(KeyCode::Char('5')), &live));
    assert_eq!(app.tab, Tab::Agents);
    assert!(app.handle_key(key(KeyCode::Char('4')), &live));
    assert_eq!(app.tab, Tab::Usage);
    assert!(app.handle_key(key(KeyCode::Char('3')), &live));
    assert_eq!(app.tab, Tab::Activity);
    assert!(app.handle_key(key(KeyCode::Char('2')), &live));
    assert_eq!(app.tab, Tab::Modules);
    app.tab = Tab::Home;
    press(&mut app, &snap, KeyCode::Char('t'));
    assert_eq!(app.theme.mode(), Mode::Dark);
    press(&mut app, &snap, KeyCode::Char('q'));
    assert!(app.quit);
    let mut app = App::new(Theme::light());
    app.handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &snap,
    );
    assert!(app.quit);
}

// ---- the terminal guard ----------------------------------------------------------

#[derive(Clone, Default)]
struct Log(Rc<RefCell<Vec<u8>>>);

impl Write for Log {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Clone, Default)]
struct FakeRaw(Rc<RefCell<Vec<&'static str>>>);

impl RawMode for FakeRaw {
    fn enable(&mut self) -> io::Result<()> {
        self.0.borrow_mut().push("enable");
        Ok(())
    }
    fn disable(&mut self) -> io::Result<()> {
        self.0.borrow_mut().push("disable");
        Ok(())
    }
}

fn written(log: &Log) -> String {
    String::from_utf8(log.0.borrow().clone()).unwrap()
}

#[test]
fn the_terminal_guard_restores_the_terminal() {
    let (log, raw) = (Log::default(), FakeRaw::default());
    {
        let _guard = TerminalGuard::enter(log.clone(), raw.clone()).unwrap();
        let out = written(&log);
        assert!(out.contains("\x1b[?1049h"), "alternate screen on");
        // On Windows crossterm turns mouse reporting on through the console
        // API rather than an escape sequence, so nothing reaches the writer.
        if cfg!(not(windows)) {
            assert!(out.contains("\x1b[?1000h"), "mouse on");
        }
        assert!(out.contains("\x1b[?25l"), "cursor hidden");
        assert_eq!(*raw.0.borrow(), vec!["enable"]);
    }
    let out = written(&log);
    assert!(out.contains("\x1b[?1049l"), "alternate screen off");
    if cfg!(not(windows)) {
        assert!(out.contains("\x1b[?1000l"), "mouse off");
    }
    assert!(out.contains("\x1b[?25h"), "cursor shown");
    assert_eq!(*raw.0.borrow(), vec!["enable", "disable"]);

    // A panic while the app runs restores it too, exactly once.
    let (log, raw) = (Log::default(), FakeRaw::default());
    let (l2, r2) = (log.clone(), raw.clone());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut guard = TerminalGuard::enter(l2, r2).unwrap();
        guard.restore().unwrap();
        let _guard = TerminalGuard::enter(Log::default(), FakeRaw::default()).unwrap();
        panic!("boom");
    }));
    assert!(result.is_err());
    assert_eq!(*raw.0.borrow(), vec!["enable", "disable"]);
    assert_eq!(written(&log).matches("\x1b[?1049l").count(), 1);

    let (log, raw) = (Log::default(), FakeRaw::default());
    let (l2, r2) = (log.clone(), raw.clone());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let _guard = TerminalGuard::enter(l2, r2).unwrap();
        panic!("boom");
    }));
    assert!(result.is_err());
    assert_eq!(*raw.0.borrow(), vec!["enable", "disable"]);
    assert!(written(&log).contains("\x1b[?1049l"));
}

// ---- light or dark -------------------------------------------------------------

#[test]
fn osc11_replies_parse() {
    let white = parse_osc11("\x1b]11;rgb:ffff/ffff/ffff\x07").unwrap();
    assert_eq!(white, (1.0, 1.0, 1.0));
    let dark = parse_osc11("11;rgb:1c1c/1c1c/1f1f").unwrap();
    assert!((dark.0 - f64::from(0x1c1c) / 65535.0).abs() < 1e-12);
    assert_eq!(parse_osc11("\x1b]11;rgb:ff/80/00\x1b\\").unwrap().0, 1.0);
    assert!((parse_osc11("rgb:ff/80/00").unwrap().1 - 128.0 / 255.0).abs() < 1e-12);
    assert_eq!(parse_osc11(""), None);
    assert_eq!(parse_osc11("11;rgb:ffff/ffff"), None);
    assert_eq!(parse_osc11("11;rgb:fffff/0/0"), None);
    assert_eq!(parse_osc11("11;cmy:1/2/3"), None);
}

#[test]
fn light_or_dark_is_chosen_in_order() {
    let black = Some("11;rgb:0000/0000/0000");
    let white = Some("11;rgb:ffff/ffff/ffff");
    assert_eq!(choose_mode(None, black), Mode::Dark);
    assert_eq!(choose_mode(None, white), Mode::Light);
    assert_eq!(
        choose_mode(None, None),
        Mode::Light,
        "no answer means light"
    );
    assert_eq!(choose_mode(None, Some("garbage")), Mode::Light);
    assert_eq!(
        choose_mode(Some("light"), black),
        Mode::Light,
        "CK_THEME wins"
    );
    assert_eq!(choose_mode(Some("dark"), white), Mode::Dark);
    assert_eq!(choose_mode(Some("purple"), black), Mode::Dark);
    // Mid grey sits below 0.5 luminance (0x80 is about 0.22), so it is dark;
    // a light grey such as 0xc0 (about 0.53) is light.
    assert_eq!(choose_mode(None, Some("rgb:80/80/80")), Mode::Dark);
    assert_eq!(choose_mode(None, Some("rgb:c0/c0/c0")), Mode::Light);
    assert_eq!(theme_for(None, None, black), Theme::dark());
    assert_eq!(theme_for(None, Some("light"), black), Theme::light());
}

#[test]
fn the_reply_is_rebuilt_from_crossterm_key_events() {
    // How crossterm reports `ESC ] 11 ; rgb:ffff/ffff/ffff BEL`.
    let mut events = vec![Event::Key(KeyEvent::new(
        KeyCode::Char(']'),
        KeyModifiers::ALT,
    ))];
    for ch in "11;rgb:ffff/ffff/ffff".chars() {
        events.push(Event::Key(key(KeyCode::Char(ch))));
    }
    events.push(Event::Key(KeyEvent::new(
        KeyCode::Char('g'),
        KeyModifiers::CONTROL,
    )));
    let mut collector = Osc11Collector::default();
    let done: Vec<bool> = events.iter().map(|e| collector.feed(e)).collect();
    assert_eq!(done.iter().filter(|d| **d).count(), 1);
    assert!(*done.last().unwrap());
    assert_eq!(collector.reply(), Some("11;rgb:ffff/ffff/ffff"));
    assert_eq!(choose_mode(None, collector.reply()), Mode::Light);

    // Keys typed before the reply don't become part of it.
    let mut collector = Osc11Collector::default();
    collector.feed(&Event::Key(key(KeyCode::Char('x'))));
    assert_eq!(collector.reply(), None);
}

#[test]
fn a_text_field_takes_printable_keys_before_any_app_key() {
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::light());
    app.field = Some(TextField::default());
    for ch in "quit".chars() {
        assert!(app.handle_key(key(KeyCode::Char(ch)), &snap));
    }
    assert!(!app.quit, "q typed into the field doesn't quit");
    assert!(app.requests.is_empty(), "typing sends nothing");
    // Digits, the theme key, help and the palette key type too.
    for ch in "2t?:Q".chars() {
        assert!(app.handle_key(key(KeyCode::Char(ch)), &snap));
    }
    assert_eq!(app.field.as_ref().unwrap().text, "quit2t?:Q");
    assert_eq!(app.tab, Tab::Home);
    assert_eq!(app.overlay, None);
    assert_eq!(app.theme.mode(), Mode::Light);
    assert!(!app.quit);
    // Ctrl with a letter is swallowed rather than opening the palette.
    app.handle_key(
        KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
        &snap,
    );
    assert_eq!(app.overlay, None);
    press(&mut app, &snap, KeyCode::Backspace);
    assert_eq!(app.field.as_ref().unwrap().text, "quit2t?:");
    assert!(app.requests.is_empty());
    // Esc closes the field, and then the keys act again.
    press(&mut app, &snap, KeyCode::Esc);
    assert!(app.field.is_none());
    press(&mut app, &snap, KeyCode::Char('2'));
    assert_eq!(app.tab, Tab::Modules);
    // Ctrl-c always quits.
    app.field = Some(TextField::default());
    app.handle_key(
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
        &snap,
    );
    assert!(app.quit);
}

#[test]
fn the_sources_follow_the_tab_on_screen() {
    use ck_ui::live::schedule::Showing;
    let snap = Snapshot::fixture();
    let mut app = App::new(Theme::test());
    assert_eq!(app.showing(&snap), Showing::tab(Tab::Home));
    press(&mut app, &snap, KeyCode::Char('4'));
    assert_eq!(app.showing(&snap), Showing::tab(Tab::Usage));

    // A tab that doesn't show (its module left the roster) runs nothing of
    // its own: Home is what shows.
    let mut no_insula = snap.clone();
    no_insula.modules.data.retain(|m| m.id != "insula");
    assert_eq!(app.showing(&no_insula), Showing::tab(Tab::Home));

    // The live app serves Usage and Agents but not Devices: key 4 shows
    // Usage, key 6 leaves it on screen, and key 5 shows Agents.
    let mut live = snap.clone();
    live.serves = ck_ui::model::Serves::LIVE;
    let mut app = App::new(Theme::test());
    press(&mut app, &live, KeyCode::Char('4'));
    assert_eq!(app.showing(&live), Showing::tab(Tab::Usage));
    press(&mut app, &live, KeyCode::Char('6'));
    assert_eq!(app.showing(&live), Showing::tab(Tab::Usage));
    press(&mut app, &live, KeyCode::Char('5'));
    assert_eq!(app.showing(&live), Showing::tab(Tab::Agents));
    press(&mut app, &live, KeyCode::Char('2'));
    assert_eq!(app.showing(&live), Showing::tab(Tab::Modules));
}
