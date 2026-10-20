//! Renders each screen at the sizes the design was drawn at and compares it
//! cell for cell with the frames in `tests/golden/`: glyphs with `.txt`, and
//! style roles with `.styles`. The frames are the specification; there is no
//! switch to rewrite them from the app's output.

use ck_ui::app::{App, Overlay, Tab};
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

fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let now = Snapshot::fixture_now();
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
    let (w, h) = {
        let size = name.rsplit('-').next().unwrap();
        let (w, h) = size.split_once('x').unwrap();
        (w.parse().unwrap(), h.parse().unwrap())
    };
    let buf = render(app, snap, w, h);
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
