//! Window sizes between and beyond the design frames: lists that overflow
//! say how much they leave out, and bigger windows put the room to use.

use ck_ui::app::{App, Tab};
use ck_ui::model::Status;
use ck_ui::{Snapshot, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

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

fn selected(buf: &Buffer, x: u16, y: u16) -> bool {
    buf[(x, y)].bg == Theme::test().bg(ck_ui::theme::Bg::Sel)
}

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap);
}

/// The fixture with more of everything: four problems (aft failed,
/// cerebellum, engram and thalamus-harness degraded), 30 modules and 7
/// providers.
fn crowded() -> Snapshot {
    let mut s = Snapshot::fixture();
    let since = s.modules.data[10].status_since;
    s.modules.data[0].status = Status::Failed;
    s.modules.data[5].status = Status::Degraded;
    for i in [0, 5] {
        s.modules.data[i].status_since = since;
    }
    let template = s.modules.data[1].clone();
    for i in 1..=5 {
        let mut m = template.clone();
        m.id = format!("zz-extra-{i}");
        s.modules.data.push(m);
    }
    let provider = s.usage.data.providers[1].clone();
    for name in ["Mistral", "Groq"] {
        let mut q = provider.clone();
        q.name = name.to_string();
        s.usage.data.providers.push(q);
    }
    assert_eq!(s.attention().len(), 4);
    s
}

#[test]
fn home_attention_list_says_how_many_more() {
    let snap = crowded();
    let app = App::new(Theme::test());

    for (w, h) in [(120, 36), (160, 48)] {
        let buf = render(&app, &snap, w, h);
        assert!(row(&buf, 5).contains("aft"), "{w}×{h}");
        assert!(row(&buf, 6).contains("cerebellum"), "{w}×{h}");
        assert_eq!(
            text(&buf, 3, 7, 25),
            "+2 more · ⏎ opens Modules",
            "{w}×{h}: the last row stands for the rest"
        );
    }
    let small = render(&app, &snap, 80, 24);
    assert!(row(&small, 5).contains("aft"));
    assert_eq!(text(&small, 3, 6, 25), "+3 more · ⏎ opens Modules");

    // The note row can be selected, and ⏎ there opens the first problem it
    // stands for.
    let mut app = App::new(Theme::test());
    app.size = (120, 36);
    for _ in 0..5 {
        press(&mut app, &snap, KeyCode::Down);
    }
    assert_eq!(app.home_sel, 2, "selection stops on the note row");
    let buf = render(&app, &snap, 120, 36);
    assert!(selected(&buf, 3, 7));
    press(&mut app, &snap, KeyCode::Enter);
    assert_eq!(app.tab, Tab::Modules);
    assert_eq!(snap.modules.data[app.mod_sel].id, "engram");
}

#[test]
fn home_module_grid_and_usage_say_how_many_more() {
    let snap = crowded();
    let app = App::new(Theme::test());

    let regular = render(&app, &snap, 120, 36);
    assert!(text(&regular, 22, 21, 20).contains("wernicke"));
    assert_eq!(text(&regular, 22, 22, 7), "+5 more");
    assert!(row(&regular, 18).contains("Cursor"));
    assert_eq!(text(&regular, 45, 20, 29), "+ 2 more providers on 4 Usage");

    let large = render(&app, &snap, 160, 48);
    assert!(text(&large, 31, 21, 20).contains("wernicke"));
    assert_eq!(text(&large, 31, 22, 7), "+5 more");
    assert!(row(&large, 16).contains("Grok"));
    assert_eq!(text(&large, 61, 18, 29), "+ 3 more providers on 4 Usage");

    let small = render(&app, &snap, 80, 24);
    assert_eq!(text(&small, 2, 17, 29), "+ 4 more providers on 4 Usage");
}

#[test]
fn modules_table_says_how_many_rows_are_out_of_view() {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;

    // 25 modules, 20 table rows: 19 modules and a note.
    let snap = Snapshot::fixture();
    let buf = render(&app, &snap, 100, 30);
    assert!(row(&buf, 25).contains("prefrontal-core"));
    assert_eq!(text(&buf, 2, 26, 10), "↓ 6 more  ");

    app.mod_sel = 24;
    let buf = render(&app, &snap, 100, 30);
    assert_eq!(text(&buf, 2, 7, 10), "↑ 6 more  ");
    assert!(row(&buf, 26).contains("wernicke"));
    assert!(selected(&buf, 1, 26));

    // In the middle of a long list, both ends say what they hide.
    let crowded = crowded();
    app.mod_sel = 15;
    let buf = render(&app, &crowded, 120, 24);
    assert_eq!(text(&buf, 2, 7, 10), "↑ 4 more  ");
    assert_eq!(text(&buf, 2, 20, 11), "↓ 14 more  ");
    assert!(row(&buf, 19).contains(&crowded.modules.data[15].id));
    assert!(selected(&buf, 1, 19));
}

#[test]
fn home_100x30_lists_every_provider_and_recent_activity() {
    let buf = render(&App::new(Theme::test()), &Snapshot::fixture(), 100, 30);
    for (i, name) in ["Claude", "Codex", "Antigravity", "Grok", "Cursor"]
        .iter()
        .enumerate()
    {
        assert_eq!(text(&buf, 2, 14 + i as u16, name.len() as u16), *name);
    }
    assert!(!row(&buf, 19).contains("more providers"));
    assert_eq!(text(&buf, 2, 20, 8), "Activity");
    assert!(row(&buf, 22).contains("engram"));
    assert!(
        row(&buf, 26).contains("17:53"),
        "activity runs to the footer"
    );
}

#[test]
fn home_140x40_agents_show_their_projects() {
    let buf = render(&App::new(Theme::test()), &Snapshot::fixture(), 140, 40);
    // The Agents panel starts at column 85; projects follow the names.
    assert_eq!(text(&buf, 87, 26, 5), "ARGUS");
    assert_eq!(text(&buf, 97, 26, 7), "web-app");
}

#[test]
fn home_200x60_caps_usage_bars_and_grows_the_sparkline() {
    let buf = render(&App::new(Theme::test()), &Snapshot::fixture(), 200, 60);
    let bar = |c: &str| "█▏▎▍▌▋▊▉".contains(c);
    // Usage starts at column 61; This Mac follows it.
    let mut widest = 0;
    for y in 10..21 {
        let mut run = 0;
        for x in 61..125 {
            if bar(buf[(x, y)].symbol()) {
                run += 1;
                widest = widest.max(run);
            } else {
                run = 0;
            }
        }
    }
    assert_eq!(widest, 46, "usage bars stop at 46 cells");
    assert!(row(&buf, 8).contains("This Mac"));
    // This Mac starts at 128 and the sparkline at 141, so it gets
    // 70 - 13 - 6 - "wait 3m" = 44 samples, then "3h" and the wait.
    let line = text(&buf, 141, 14, 44);
    assert!(line.chars().all(|c| "▁▂▃▄▅▆▇█".contains(c)), "{line:?}");
    assert_eq!(text(&buf, 186, 14, 2), "3h");
    assert_eq!(text(&buf, 191, 14, 7), "wait 3m");
}

#[test]
fn modules_100x30_shows_the_summary_column() {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Modules;
    let buf = render(&app, &Snapshot::fixture(), 100, 30);
    assert_eq!(text(&buf, 73, 5, 7), "Summary");
    assert_eq!(text(&buf, 73, 7, 10), "Warming 57");
    // At 90 columns the column would get only 15, so it stays out.
    let narrow = render(&app, &Snapshot::fixture(), 90, 30);
    assert!(!row(&narrow, 5).contains("Summary"));
}

#[test]
fn the_crowded_fixture_renders_at_every_size() {
    let snap = crowded();
    let mut apps = Vec::new();
    for (tab, sel) in [(Tab::Home, 2), (Tab::Modules, 15), (Tab::Modules, 29)] {
        let mut app = App::new(Theme::test());
        app.tab = tab;
        app.home_sel = sel;
        app.mod_sel = sel;
        apps.push(app);
    }
    let threads = std::thread::available_parallelism().map_or(2, |n| n.get()) as u16;
    std::thread::scope(|scope| {
        for t in 0..threads {
            let (apps, snap) = (&apps, &snap);
            scope.spawn(move || {
                for w in (1..=220u16).filter(|w| w % threads == t) {
                    for h in 1..=70u16 {
                        for app in apps {
                            render(app, snap, w, h);
                        }
                    }
                }
            });
        }
    });
}
