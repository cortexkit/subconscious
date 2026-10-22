//! The Usage screen: what the live app serves and hides, failing accounts
//! and sources, hiding account identifiers on Usage and Home, reaching
//! every window and account from the keyboard, and the screen at every
//! window size.

use ck_ui::app::{App, Tab};
use ck_ui::model::{Account, FaultKind, Panel, Provider, Serves, Usage, UsageWindow};
use ck_ui::screens::usage::{layout, Focus};
use ck_ui::time::Timestamp;
use ck_ui::{Snapshot, Theme};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use serde_json::json;

fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buf, snap, Snapshot::fixture_now());
    buf
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn text(buf: &Buffer, x: u16, y: u16, n: u16) -> String {
    (x..(x + n).min(buf.area.width))
        .map(|x| buf[(x, y)].symbol())
        .collect()
}

fn screen(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row(buf, y) + "\n").collect()
}

/// Presses `code` in a window of `size`, as the run loop hands keys over.
fn press_at(app: &mut App, snap: &Snapshot, code: KeyCode, size: (u16, u16)) -> bool {
    let key = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    app.handle_event(&key, snap, size)
}

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) -> bool {
    press_at(app, snap, code, (120, 36))
}

fn now() -> Timestamp {
    Snapshot::fixture_now()
}

fn usage_window(name: &str, used: f64) -> UsageWindow {
    UsageWindow {
        name: name.into(),
        used,
        resets_at: Some(now().add_secs(3_600.0)),
    }
}

/// `n` windows named `<prefix>-1` and on, zero-padded to the width of `n`.
fn windows(prefix: &str, n: usize) -> Vec<UsageWindow> {
    let digits = n.to_string().len();
    (1..=n)
        .map(|i| usage_window(&format!("{prefix}-{i:0digits$}"), 0.1 + 0.04 * i as f64))
        .collect()
}

fn account(email: &str, windows: Vec<UsageWindow>) -> Account {
    Account {
        who: email.into(),
        redacted: ck_ui::quota::account_name(&json!({ "accountInfo": { "email": email } }), true),
        identifiers: vec![email.into()],
        windows,
        note: String::new(),
        error: None,
    }
}

fn failing(email: &str, error: &str) -> Account {
    Account {
        error: Some(error.into()),
        ..account(email, Vec::new())
    }
}

fn provider(name: &str, accounts: Vec<Account>) -> Provider {
    let n = accounts.len() as u32;
    Provider {
        name: name.into(),
        window: "week".into(),
        used: Some(0.5 * f64::from(n)),
        of: n,
        unit: if n == 1 { "account" } else { "accounts" }.into(),
        resets_at: Some(now().add_secs(3_600.0)),
        accounts,
        error: None,
        error_kind: FaultKind::Untyped,
        balance: None,
        balance_expires: None,
    }
}

fn live_with(providers: Vec<Provider>) -> Snapshot {
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    s.usage = Panel::new(
        now(),
        Usage {
            providers,
            models_priced: Some(10_258),
            models_counted: Some(10_400),
            models_checked_at: Some(now()),
        },
    );
    s
}

fn usage_app() -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Usage;
    app
}

/// Thirty providers. The first has twenty accounts: one with 6 windows,
/// one with 20, then eighteen with 2 each. The rest have one account each.
fn crowded() -> Snapshot {
    let mut accounts = vec![
        account("six@example.com", windows("w6", 6)),
        account("twenty@example.com", windows("w20", 20)),
    ];
    for i in 3..=20 {
        accounts.push(account(
            &format!("a{i:02}@example.com"),
            windows(&format!("a{i:02}"), 2),
        ));
    }
    let mut providers = vec![provider("Prov-01", accounts)];
    for i in 2..=30 {
        providers.push(provider(
            &format!("Prov-{i:02}"),
            vec![account(&format!("p{i:02}@example.com"), windows("w", 2))],
        ));
    }
    live_with(providers)
}

/// The counts in every "+N more" in `text`, with what follows the word
/// "more" ("" for a bare "+N more").
fn notes(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, w) in words.iter().enumerate() {
        if let Some(n) = w.strip_prefix('+').and_then(|n| n.parse::<usize>().ok()) {
            if words.get(i + 1) == Some(&"more") {
                let what = words
                    .get(i + 2)
                    .filter(|w| ["accounts", "account", "providers", "provider"].contains(w))
                    .map_or("", |w| *w);
                out.push((n, what.to_string()));
            }
        }
    }
    out
}

/// The footer's hints that show, each whole: drawn in full or not at all,
/// and the ones drawn a prefix of `hints`.
fn assert_whole_hints(buf: &Buffer, w: u16, h: u16, hints: &[&str]) {
    let footer = row(buf, h - 1);
    let shown: Vec<bool> = hints.iter().map(|hint| footer.contains(hint)).collect();
    let n = shown.iter().take_while(|s| **s).count();
    assert!(n >= 1, "{w}×{h}: {footer}");
    assert!(shown[n..].iter().all(|s| !s), "{w}×{h}: {footer}");
}

// ---- served and hidden ----------------------------------------------------------------

#[test]
fn the_live_footer_offers_no_refresh_and_r_does_nothing() {
    let snap = crowded();
    for (w, h) in [(80, 24), (120, 36), (200, 60)] {
        for focus in [Focus::Providers, Focus::Accounts] {
            let mut app = usage_app();
            app.usage.focus = focus;
            let buf = render(&app, &snap, w, h);
            let footer = row(&buf, h - 1);
            assert!(!footer.contains("refresh"), "{w}×{h}: {footer}");
            assert!(!footer.contains(" r "), "{w}×{h}: {footer}");
            assert!(!footer.contains('⏎'), "{w}×{h}: {footer}");
            assert!(!screen(&buf).contains("catalog"), "{w}×{h}");
        }
    }
    let mut app = usage_app();
    for code in [KeyCode::Char('r'), KeyCode::Enter, KeyCode::Char('m')] {
        assert!(!press(&mut app, &snap, code), "{code:?}");
    }
    assert!(app.requests.is_empty());
    assert!(app.confirm.is_none() && app.overlay.is_none() && app.metrics.is_none());
    assert_eq!(app.tab, Tab::Usage);

    // The design draws the keys the live app doesn't serve.
    let mut design = snap.clone();
    design.serves = Serves::DESIGN;
    let footer = row(&render(&usage_app(), &design, 120, 36), 35);
    assert!(footer.contains("r  refresh now"), "{footer}");
    assert!(footer.contains("⏎  account"), "{footer}");
}

#[test]
fn models_read_priced_and_counted_or_unknown() {
    let mut snap = crowded();
    let buf = render(&usage_app(), &snap, 120, 36);
    let left = |buf: &Buffer| -> String {
        (5..33)
            .map(|y| text(buf, 2, y, 26).trim_end().to_string() + "\n")
            .collect()
    };
    let col = left(&buf);
    assert!(col.contains("Models\n"), "{col}");
    assert!(col.contains("10,258 priced\nof 10,400 models\n"), "{col}");
    snap.usage.data.models_priced = None;
    snap.usage.data.models_counted = None;
    let col = left(&render(&usage_app(), &snap, 120, 36));
    assert!(col.contains("unknown priced\nof unknown models\n"), "{col}");
    assert!(!col.contains(" 0 "), "{col}");
}

// ---- failing ------------------------------------------------------------------------

#[test]
fn a_failing_account_shows_as_failing_never_as_zero_percent() {
    let mut codex = provider("Codex", vec![failing("kai@example.com", "upstream_failed")]);
    codex.used = None;
    codex.of = 0;
    codex.error = Some("upstream_failed".into());
    codex.error_kind = FaultKind::Code;
    let snap = live_with(vec![
        provider(
            "Claude",
            vec![
                account("alex@example.com", windows("w", 2)),
                failing("sam@example.org", "credential_expired"),
            ],
        ),
        codex,
    ]);
    let buf = render(&usage_app(), &snap, 120, 36);
    let at = (8..33)
        .find(|&y| row(&buf, y).contains("sam@example.org"))
        .expect("the failing account is listed");
    assert!(row(&buf, at).contains("▲ failing"), "{}", row(&buf, at));
    assert!(row(&buf, at + 1).contains("credential_expired"));
    for y in [at, at + 1, at + 2] {
        let detail = text(&buf, 33, y, 84);
        assert!(!detail.contains('%'), "{detail}");
    }

    // A provider whose every account fails reads as failing in the list
    // and on its side, never as a share.
    let codex_row = (5..20)
        .find(|&y| text(&buf, 2, y, 12).trim_end() == "Codex")
        .unwrap();
    let cell = text(&buf, 2, codex_row, 26);
    assert!(cell.contains("couldn't read"), "{cell}");
    assert!(!cell.contains('%'), "{cell}");
    let mut app = usage_app();
    assert!(press(&mut app, &snap, KeyCode::Down));
    let buf = render(&app, &snap, 120, 36);
    let summary = text(&buf, 33, 5, 84);
    assert!(
        summary.contains("couldn't read usage · upstream_failed"),
        "{summary}"
    );
    assert!(!summary.contains('%'), "{summary}");
    assert!(row(&buf, 8).contains("▲ failing"));
}

#[test]
fn a_failing_source_shows_one_line_and_no_zeros() {
    let mut snap = crowded();
    snap.usage = Panel {
        error: Some("insula unreachable · retrying".into()),
        error_kind: FaultKind::Sentence,
        ..Panel::waiting(
            Usage {
                providers: Vec::new(),
                models_priced: None,
                models_counted: None,
                models_checked_at: None,
            },
            Some(30.0),
        )
    };
    let buf = render(&usage_app(), &snap, 120, 36);
    let lines: Vec<u16> = (2..34)
        .filter(|&y| row(&buf, y).contains("insula unreachable"))
        .collect();
    assert_eq!(lines, [3], "{}", screen(&buf));
    assert_eq!(text(&buf, 2, 5, 7), "unknown");
    assert!(!screen(&buf).contains('%'));

    // With a last good reading the error stays on that line and the
    // reading shows with it.
    let mut stale = crowded();
    stale.usage.error = Some("insula unreachable · retrying".into());
    stale.usage.error_kind = FaultKind::Sentence;
    let buf = render(&usage_app(), &stale, 120, 36);
    assert!(row(&buf, 3).contains("insula unreachable · retrying"));
    assert!(row(&buf, 5).contains("Prov-01"));
}

// ---- hiding account identifiers ---------------------------------------------------

/// Claude reads fine; every Codex account fails with a sentence that
/// quotes the account's email.
fn quoting() -> Snapshot {
    let quote = "token for sam@example.org expired";
    let mut codex = provider("Codex", vec![failing("sam@example.org", quote)]);
    codex.used = None;
    codex.of = 0;
    codex.error = Some(quote.into());
    codex.error_kind = FaultKind::Sentence;
    live_with(vec![
        provider("Claude", vec![account("alex@example.com", windows("w", 2))]),
        codex,
    ])
}

#[test]
fn x_hides_account_identifiers_on_usage_and_shows_them_again() {
    let snap = quoting();
    let alex = snap.usage.data.providers[0].accounts[0].redacted.clone();
    let sam = snap.usage.data.providers[1].accounts[0].redacted.clone();
    assert!(alex.starts_with("account ") && sam.starts_with("account "));
    let mut app = usage_app();
    let shown = screen(&render(&app, &snap, 120, 36));
    assert!(shown.contains("alex@example.com"), "{shown}");
    assert!(row(&render(&app, &snap, 120, 36), 35).contains("x  hide emails"));

    assert!(press(&mut app, &snap, KeyCode::Char('x')));
    assert!(app.redact);
    let buf = render(&app, &snap, 120, 36);
    let hidden = screen(&buf);
    assert!(!hidden.contains("@example"), "{hidden}");
    assert!(hidden.contains(&alex), "{hidden}");
    assert!(row(&buf, 35).contains("x  show emails"));
    // The failing provider's side: its error and its account's error name
    // the account by its pseudonym.
    assert!(press(&mut app, &snap, KeyCode::Down));
    let hidden = screen(&render(&app, &snap, 120, 36));
    assert!(!hidden.contains("@example"), "{hidden}");
    assert!(
        hidden.contains(&format!("token for {sam} expired")),
        "{hidden}"
    );

    assert!(press(&mut app, &snap, KeyCode::Char('x')));
    assert!(!app.redact);
    let back = screen(&render(&app, &snap, 120, 36));
    assert!(back.contains("token for sam@example.org expired"), "{back}");
    assert!(back.contains("sam@example.org\n") || back.contains("sam@example.org "));
}

#[test]
fn homes_usage_panel_masks_identifiers_in_a_providers_error_while_hidden() {
    let snap = quoting();
    let sam = snap.usage.data.providers[1].accounts[0].redacted.clone();
    // Hidden on Usage, then back to Home: the setting is the app's, so
    // Home follows it.
    let mut app = usage_app();
    assert!(press(&mut app, &snap, KeyCode::Char('x')));
    assert!(press(&mut app, &snap, KeyCode::Char('1')));
    assert_eq!(app.tab, Tab::Home);
    for (w, h) in [(80, 24), (160, 48)] {
        let hidden = screen(&render(&app, &snap, w, h));
        assert!(
            hidden.contains(&format!("token for {sam} expired")),
            "{w}×{h}: {hidden}"
        );
        assert!(!hidden.contains("sam@example.org"), "{w}×{h}: {hidden}");
    }
    app.redact = false;
    for (w, h) in [(80, 24), (160, 48)] {
        let shown = screen(&render(&app, &snap, w, h));
        assert!(
            shown.contains("token for sam@example.org expired"),
            "{w}×{h}: {shown}"
        );
    }
    // Home has no `x` of its own: its footer is the design's.
    let mut home = App::new(Theme::test());
    assert!(!press(&mut home, &snap, KeyCode::Char('x')));
    assert!(!home.redact);
}

// ---- reaching every window and account --------------------------------------------

#[test]
fn the_keyboard_reaches_the_last_window_and_the_last_account_at_80x24() {
    let snap = crowded();
    let size = (80, 24);
    let sel = Theme::test().bg(ck_ui::theme::Bg::Sel);
    let mut app = App::new(Theme::test());
    assert!(press_at(&mut app, &snap, KeyCode::Char('4'), size));
    assert_eq!(app.tab, Tab::Usage);
    // The 20 windows don't all fit: three show and a "+17 more" row.
    let buf = render(&app, &snap, 80, 24);
    let shown = screen(&buf);
    assert!(
        shown.contains("w20-03") && !shown.contains("w20-04"),
        "{shown}"
    );
    assert!(notes(&shown).contains(&(17, String::new())), "{shown}");

    // tab moves to the accounts; ↓ selects the 20-window account.
    assert!(press_at(&mut app, &snap, KeyCode::Tab, size));
    assert_eq!(app.usage.focus, Focus::Accounts);
    assert!(press_at(&mut app, &snap, KeyCode::Down, size));
    let mut presses = 0;
    while press_at(&mut app, &snap, KeyCode::Right, size) {
        presses += 1;
        assert!(presses <= 20, "→ never stops");
    }
    assert_eq!(presses, 17);
    let buf = render(&app, &snap, 80, 24);
    let shown = screen(&buf);
    for label in ["w20-18", "w20-19", "w20-20"] {
        assert!(shown.contains(label), "{label}: {shown}");
    }
    assert!(!shown.contains("w20-17"), "{shown}");
    assert!(notes(&shown).contains(&(17, String::new())), "{shown}");
    // ← goes back.
    assert!(press_at(&mut app, &snap, KeyCode::Left, size));
    assert!(screen(&render(&app, &snap, 80, 24)).contains("w20-17"));

    // ↓ reaches the provider's last account, with the same provider still
    // selected.
    while press_at(&mut app, &snap, KeyCode::Down, size) {}
    assert_eq!(app.usage.account, 19);
    assert_eq!(app.usage.provider, 0);
    let buf = render(&app, &snap, 80, 24);
    let at = (8..21)
        .find(|&y| row(&buf, y).contains("a20@example.com"))
        .expect("the last account is in view");
    assert_eq!(buf[(33, at)].bg, sel, "the last account is selected");
    assert_eq!(text(&buf, 2, 5, 7), "Prov-01");
    assert_eq!(buf[(2, 5)].bg, sel, "and its provider still is");
    // The accounts above it that no longer fit are counted.
    let hidden = notes(&screen(&buf))
        .into_iter()
        .find(|(_, what)| what == "accounts")
        .expect("+N more accounts");
    let listed = (8..21)
        .filter(|&y| text(&buf, 33, y, 44).contains("@example.com"))
        .count();
    assert_eq!(hidden.0, 20 - listed);

    // tab goes back to the providers, where ↓ moves the provider.
    assert!(press_at(&mut app, &snap, KeyCode::Tab, size));
    assert_eq!(app.usage.focus, Focus::Providers);
    assert!(press_at(&mut app, &snap, KeyCode::Down, size));
    assert_eq!(app.usage.provider, 1);
    assert_eq!((app.usage.account, app.usage.scroll), (0, 0));
}

// ---- sizes ------------------------------------------------------------------------

#[test]
fn every_size_says_how_many_it_leaves_out() {
    let snap = crowded();
    for (w, h) in [(80, 24), (100, 30), (140, 40), (160, 48), (200, 60)] {
        let app = usage_app();
        let buf = render(&app, &snap, w, h);
        let l = layout(i32::from(w), i32::from(h));
        let last = l.last as u16;
        let left: Vec<String> = (5..=last)
            .map(|y| text(&buf, 2, y, l.pw as u16).trim_end().to_string())
            .collect();
        let detail: Vec<String> = (8..=last)
            .map(|y| {
                text(&buf, l.x as u16, y, l.dw as u16)
                    .trim_end()
                    .to_string()
            })
            .collect();
        let detail_text = detail.join("\n");

        // Providers: those listed and "+N more providers" for the rest.
        let listed = left.iter().filter(|t| t.starts_with("Prov-")).count();
        let note_at = left
            .iter()
            .position(|t| t.starts_with('+'))
            .unwrap_or_else(|| panic!("{w}×{h}: {left:?}"));
        assert_eq!(
            notes(&left[note_at]),
            [(30 - listed, "providers".to_string())],
            "{w}×{h}"
        );
        // The list runs to the Models panel: every row down to its note
        // is drawn, and the note sits over the panel's two blank rows (one
        // row higher when the two-row providers leave one over).
        assert!(
            left[..=note_at].iter().all(|t| !t.is_empty()),
            "{w}×{h}: {left:?}"
        );
        let models = left.iter().position(|t| t == "Models").unwrap();
        assert_eq!(
            models,
            left.len() - 4,
            "{w}×{h}: the panel ends on the last row"
        );
        assert!(
            models - note_at == 3 || models - note_at == 4,
            "{w}×{h}: {left:?}"
        );
        assert!(note_at + 1 >= 3, "{w}×{h}: at least 3 rows");

        // Accounts: those listed and "+N more accounts" on the last row.
        let accounts = detail
            .iter()
            .filter(|t| t.ends_with("@example.com") || t.contains("@example.com "))
            .count();
        assert!(accounts >= 1, "{w}×{h}");
        assert_eq!(
            notes(detail.last().unwrap()),
            [(20 - accounts, "accounts".to_string())],
            "{w}×{h}: {detail:?}"
        );

        // Windows: the 6- and 20-window accounts each end with "+N more",
        // N the windows not drawn.
        for (prefix, n) in [("w6-", 6), ("w20-", 20)] {
            let drawn = (1..=n)
                .filter(|i| {
                    let digits = n.to_string().len();
                    detail_text.contains(&format!("{prefix}{i:0digits$}"))
                })
                .count();
            assert!(drawn >= 1 && drawn < n, "{w}×{h}: {prefix} {drawn}");
            assert!(
                notes(&detail_text).contains(&(n - drawn, String::new())),
                "{w}×{h}: {prefix} shows {drawn}: {detail_text}"
            );
        }

        // From 150 columns: windows side by side, and each provider's
        // totals row.
        let six = detail
            .iter()
            .position(|t| t.starts_with("six@example.com"))
            .unwrap();
        let totals = &left[1];
        if w >= 150 {
            assert!(l.wide);
            let cells = &detail[six + 1];
            assert!(
                cells.contains("w6-1") && cells.contains("w6-2"),
                "{w}×{h}: {cells}"
            );
            assert!(
                totals.starts_with("10.0 of 20 accounts") && totals.ends_with("resets in 1h 0m"),
                "{w}×{h}: {totals}"
            );
        } else {
            assert!(!l.wide);
            assert!(!detail[six + 1].contains("w6-2"), "{w}×{h}");
            assert_eq!(totals, "20 accounts", "{w}×{h}");
        }

        assert!(row(&buf, h - 2).contains('─'), "{w}×{h}: the footer rule");
        assert_whole_hints(
            &buf,
            w,
            h,
            &["↑↓  provider", "tab  accounts", "x  hide emails"],
        );
    }
}

#[test]
fn every_size_draws_without_panicking() {
    let snap = crowded();
    let mut apps = vec![usage_app()];
    let mut deep = usage_app();
    deep.usage.provider = 11;
    apps.push(deep);
    let mut accounts = usage_app();
    accounts.usage.focus = Focus::Accounts;
    accounts.usage.account = 1;
    accounts.usage.scroll = 50;
    apps.push(accounts);
    let mut redacted = usage_app();
    redacted.redact = true;
    apps.push(redacted);
    let mut empty = snap.clone();
    empty.usage.data.providers.clear();
    for w in 60..=220u16 {
        for h in [10, 23, 24, 25, 30, 36, 48, 60] {
            for app in &apps {
                render(app, &snap, w, h);
                render(app, &empty, w, h);
            }
        }
    }
}
