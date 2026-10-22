//! Usage: how much quota each provider's accounts have used, and when it
//! comes back.
//!
//! The left column lists the providers, each with a bar for the window
//! closest to its limit (or, without one, its error, its remaining credit or
//! "unknown") and how many accounts it has, and below them how many models
//! the pricing module prices. The rest of the screen shows the selected
//! provider: the share of its accounts' capacity used and the soonest reset
//! (or the same stand-in text), then every account with its usage windows.
//! A failing account says that it is failing and why; it never reads 0%.
//!
//! `↑↓` select a provider. `tab` moves to its accounts and back; there `↑↓`
//! select an account and `←→` scroll its windows when it has more than fit.
//! `x` hides account identifiers behind the pseudonyms `ck quota --redact`
//! prints, here and in Home's usage panel.
//!
//! Lists that run out of room end with "+N more", N being how many are left
//! out: providers, accounts, and an account's windows. From 150 columns each
//! provider's second row is its totals row (accounts' worth used and the
//! soonest reset, or its account count when nothing could be read), and an
//! account's windows sit side by side, as many as fit.

use crate::draw::{
    bar, bar_fill, chrome, frame_end, pad_right, trunc, use_fg, width, Canvas, Clip, USAGE_MAX_W,
};
use crate::model::{Account, Provider, Snapshot, Tab, UsageWindow};
use crate::quota::{mask_identifiers, USAGE_MODULE};
use crate::theme::{Bg, Fg, St};
use crate::time::{ago, thousands, to_fixed};

use super::home::{error_cell, is_code, resets_text, with_expiry};
use super::{count, pct, Meta, View, MIN_H, UNKNOWN};

/// Which list `↑↓` move through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Focus {
    #[default]
    Providers,
    /// The selected provider's accounts.
    Accounts,
}

/// What the person has selected on the Usage screen. Positions are clamped
/// to the data whenever they are read, because a new reading can list fewer
/// providers, accounts or windows than the last.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageState {
    pub provider: usize,
    pub account: usize,
    pub focus: Focus,
    /// The first window shown of the selected account, while it has more
    /// windows than fit.
    pub scroll: usize,
}

/// The providers the screen lists: none until the first reading arrives.
pub fn providers(snap: &Snapshot) -> &[Provider] {
    if snap.usage.loaded {
        &snap.usage.data.providers
    } else {
        &[]
    }
}

/// The selected provider and its position, if there is any provider.
pub fn selected_provider<'a>(
    snap: &'a Snapshot,
    state: &UsageState,
) -> Option<(usize, &'a Provider)> {
    let list = providers(snap);
    let i = state.provider.min(list.len().checked_sub(1)?);
    Some((i, &list[i]))
}

/// The screen's geometry at a window size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// From 150 columns: totals rows and windows side by side.
    pub wide: bool,
    /// Width of the provider column, from column 2.
    pub pw: i32,
    /// Where the selected provider's side starts, and its width.
    pub x: i32,
    pub dw: i32,
    /// The last row a list draws on: the row above the blank row over the
    /// footer rule.
    pub last: i32,
}

pub fn layout(w: i32, h: i32) -> Layout {
    let wide = w >= 150;
    // The wider column has room for "4.3 of 5 accounts" and "resets in
    // 2d 23h" side by side.
    let pw = if wide { 38 } else { 26 };
    let x = 2 + pw + 5;
    Layout {
        wide,
        pw,
        x,
        dw: (w - 3 - x).max(0),
        last: h - 4,
    }
}

/// The most rows an account's windows take below 150 columns. An account
/// with more windows shows one fewer and a "+N more" row.
const WINDOW_ROWS: usize = 4;

/// One window side by side with others, from 150 columns: its name, a bar,
/// the share used and "resets 19h 25m".
const CELL_LABEL: i32 = 12;
const CELL_BAR: i32 = 8;
const CELL_RESET: i32 = 14;
const CELL_W: i32 = CELL_LABEL + 1 + CELL_BAR + 5 + 2 + CELL_RESET;
const CELL_GAP: i32 = 3;

/// The rows two blank rows, the Models heading, its rule and its two lines
/// take below the provider list.
const MODELS_H: i32 = 6;

fn more(n: usize) -> String {
    format!("+{n} more")
}

/// How many of an account's `n` windows show at once in a window `w`
/// columns wide; when fewer than `n`, a "+N more" follows them.
pub fn windows_shown(w: i32, n: usize) -> usize {
    shown_of(&layout(w, MIN_H), n)
}

fn shown_of(l: &Layout, n: usize) -> usize {
    if !l.wide {
        return if n <= WINDOW_ROWS { n } else { WINDOW_ROWS - 1 };
    }
    let avail = l.dw - 2;
    let stride = CELL_W + CELL_GAP;
    let fit = ((avail + CELL_GAP) / stride).max(0) as usize;
    if n <= fit {
        return n;
    }
    let mut k = fit;
    while k > 0 && k as i32 * stride + width(&more(n - k)) as i32 > avail {
        k -= 1;
    }
    k
}

/// Rows an account takes: its name, its windows (or the reason it is
/// failing), and a blank row after.
fn block_height(l: &Layout, a: &Account) -> i32 {
    let n = a.windows.len();
    let body = if a.error.is_some() {
        1
    } else if n == 0 {
        0
    } else if l.wide {
        1
    } else {
        let k = shown_of(l, n);
        (k + usize::from(k < n)) as i32
    };
    body + 2
}

/// Which accounts show in `slots` rows with account `sel` in view: the
/// first shown, how many, and how many are left out. When they don't all
/// fit, the last row is kept for the "+N more accounts" line.
fn account_window(heights: &[i32], slots: i32, sel: usize) -> (usize, usize, usize) {
    let n = heights.len();
    // The last account needs no blank row after it.
    if heights.iter().sum::<i32>() - 1 <= slots {
        return (0, n, 0);
    }
    let sum = |a: usize, b: usize| heights[a..b].iter().sum::<i32>();
    let sel = sel.min(n.saturating_sub(1));
    let mut first = 0;
    while first < sel && sum(first, sel + 1) > slots {
        first += 1;
    }
    let mut shown = 1;
    while first + shown < n && sum(first, first + shown + 1) <= slots {
        shown += 1;
    }
    (first, shown, n - shown)
}

/// The name an account shows under: its own, or its pseudonym while
/// account identifiers are hidden.
fn account_name<'a>(v: &View, a: &'a Account) -> &'a str {
    if v.redact {
        &a.redacted
    } else {
        &a.who
    }
}

/// `text` from one of `q`'s accounts, such as the provider's error. While
/// account identifiers are hidden, each identifier any of the provider's
/// accounts carries is replaced with that account's pseudonym.
pub fn provider_text(v: &View, q: &Provider, text: &str) -> String {
    if !v.redact {
        return text.to_string();
    }
    q.accounts.iter().fold(text.to_string(), |t, a| {
        mask_identifiers(&t, &a.identifiers, &a.redacted)
    })
}

/// A provider's share used, or the text in its place when it has none: its
/// error (never a bar at 0%), what is left of its credit, or "unknown".
/// `room` decides whether a machine-code error fits "couldn't read usage".
fn share(v: &View, q: &Provider, room: usize) -> Result<f64, (String, St)> {
    match (q.used, &q.error, &q.balance) {
        (Some(used), None, _) => Ok(used / f64::from(q.of.max(1))),
        (_, Some(err), _) => Err(error_cell(
            &provider_text(v, q, err),
            q.error_kind,
            "usage",
            room,
        )),
        (None, None, Some(left)) => Err((with_expiry(v, q, left.clone(), room), St::NONE)),
        (None, None, None) => Err((UNKNOWN.to_string(), St::D)),
    }
}

/// A share's percent in its alarm colour from 85%, else in `quiet`.
fn pct_st(f: f64, quiet: St) -> St {
    if f >= 0.85 {
        St::fg(use_fg(f))
    } else {
        quiet
    }
}

pub fn draw(c: &mut Canvas, v: &View, state: &UsageState) {
    let snap = v.snap;
    chrome(
        c,
        Tab::Usage,
        snap.visible_tabs(),
        &v.clock(),
        &snap.host.data.label,
    );
    let l = layout(c.w, c.h);
    let panel = &snap.usage;
    let list = providers(snap);
    let selected = selected_provider(snap, state);
    let accounts_focus =
        state.focus == Focus::Accounts && selected.is_some_and(|(_, q)| !q.accounts.is_empty());
    let meta = if panel.loaded || panel.error.is_some() {
        v.meta(
            panel,
            &format!("refreshed {} ago", ago(v.age(panel.received_at))),
        )
    } else {
        Meta::plain(&format!("reading {USAGE_MODULE}…"))
    };

    c.put(2, 3, "Providers", St::B);
    c.hline(2, 4, l.pw);
    let list_rows = provider_list(c, v, &l, list, selected.map_or(0, |(i, _)| i));
    c.vline(l.x - 3, 3, l.last - 2);

    // The selected provider's heading: its name, accounts and window, and on
    // the right how fresh the reading is or why it fails.
    let right = l.x + l.dw;
    let name = selected.map_or(String::new(), |(_, q)| trunc(&q.name, l.dw as usize));
    let name_end = l.x + width(&name) as i32;
    let meta_text = trunc(&meta.text, (right - name_end - 2).max(0) as usize);
    c.right(right, 3, &meta_text, meta.st);
    if let Some((_, q)) = selected {
        c.put(l.x, 3, &name, St::B);
        let mut what = format!("· {}", count(q.accounts.len(), "account", "accounts"));
        if !q.window.is_empty() {
            what += &format!(" · {}", q.window);
        }
        let room = right - name_end - 1 - width(&meta_text) as i32 - 2;
        c.put(name_end + 1, 3, &trunc(&what, room.max(0) as usize), St::D);
    } else if panel.loaded {
        let note = format!("{USAGE_MODULE} reports no accounts");
        c.put(l.x, 5, &trunc(&note, l.dw as usize), St::D);
    }

    if let Some((_, q)) = selected {
        provider_summary(c, v, &l, q);
        c.hline(l.x, 7, l.dw);
        let asel = if accounts_focus {
            state.account.min(q.accounts.len().saturating_sub(1))
        } else {
            0
        };
        let heights: Vec<i32> = q.accounts.iter().map(|a| block_height(&l, a)).collect();
        let (first, shown, hidden) = account_window(&heights, l.last - 8 + 1, asel);
        let prev = c.clip_to(Clip {
            x0: 0,
            y0: 0,
            x1: c.w,
            y1: l.last + 1,
        });
        let mut y = 8;
        for (i, a) in q.accounts.iter().enumerate().skip(first).take(shown) {
            let on = accounts_focus && i == asel;
            account_block(c, v, &l, y, a, if on { state.scroll } else { 0 });
            if on {
                c.paint(l.x - 1, y, l.dw + 2, St::bg(Bg::Sel));
            }
            y += heights[i];
        }
        if hidden > 0 {
            c.put(
                l.x,
                l.last,
                &format!("+{}", count(hidden, "more account", "more accounts")),
                St::D,
            );
        }
        c.set_clip(prev);
    }
    if meta.dim {
        c.dim_rect(1, 5, l.pw + 2, list_rows);
        c.dim_rect(l.x, 5, l.dw, l.last - 4);
    }

    let hide = if v.redact {
        "show emails"
    } else {
        "hide emails"
    };
    let mut hints: Vec<(&str, &str)> = if accounts_focus {
        vec![("↑↓", "account"), ("←→", "windows"), ("tab", "providers")]
    } else if snap.serves.usage_detail {
        vec![("↑↓", "provider"), ("⏎", "account")]
    } else {
        vec![("↑↓", "provider"), ("tab", "accounts")]
    };
    if snap.serves.usage_refresh {
        hints.push(("r", "refresh now"));
    }
    hints.push(("x", hide));
    frame_end(c, &hints, &format!("from {USAGE_MODULE}"), St::D);
}

/// The provider column: two rows per provider, then the Models panel.
/// While every provider fits, the panel follows the list after two blank
/// rows; otherwise the list runs down to where the panel's last row sits on
/// the list area's last row, its last row says how many providers are left
/// out, and the selected provider stays in view. When that would leave the
/// list fewer than 3 rows, the panel is left out. Returns the rows the list
/// takes.
fn provider_list(c: &mut Canvas, v: &View, l: &Layout, list: &[Provider], sel: usize) -> i32 {
    let top = 5;
    let avail = l.last - top + 1;
    let need = 2 * list.len().max(1) as i32;
    let (rows, models) = if need + MODELS_H <= avail {
        (need, true)
    } else if avail - MODELS_H >= 3 {
        (avail - MODELS_H, true)
    } else {
        (avail, false)
    };
    let fits = 2 * list.len() as i32 <= rows;
    let k = if fits {
        list.len()
    } else {
        ((rows - 1) / 2).max(0) as usize
    };
    let first = if sel < k { 0 } else { sel + 1 - k };
    for (i, q) in list.iter().enumerate().skip(first).take(k) {
        provider_rows(c, v, l, top + 2 * (i - first) as i32, q, i == sel);
    }
    if !fits {
        // The note follows the last provider shown. Providers take two rows
        // each, so it can sit one row above the panel's blank rows.
        let note = count(list.len() - k, "more provider", "more providers");
        let y = top + 2 * k as i32;
        c.put(2, y, &trunc(&format!("+{note}"), l.pw as usize), St::D);
    }
    if !v.snap.usage.loaded {
        c.put(2, top, UNKNOWN, St::D);
    }
    if models {
        models_panel(c, v, l, top + rows + 2);
    }
    rows
}

/// One provider's two rows from row `y`: its name, then its bar and percent
/// or the text in their place, then how many accounts it has, or from 150
/// columns its totals: accounts' worth used and the soonest reset.
fn provider_rows(c: &mut Canvas, v: &View, l: &Layout, y: i32, q: &Provider, selected: bool) {
    let right = 2 + l.pw;
    let name_st = if selected { St::B } else { St::NONE };
    c.put(2, y, &pad_right(&trunc(&q.name, 11), 12), name_st);
    let bar_w = l.pw - 18;
    // A bar's partial cell keeps its track behind it on a selected row.
    let mut partial: Option<(i32, Fg)> = None;
    match share(v, q, (l.pw - 12) as usize) {
        Ok(f) => {
            bar(c, 14, y, bar_w, f, use_fg(f));
            c.right(right, y, &pct(f), pct_st(f, St::D));
            let (full, rem) = bar_fill(bar_w, f);
            if rem != 0 {
                partial = Some((14 + full, use_fg(f)));
            }
        }
        Err((text, st)) => {
            c.put(14, y, &trunc(&text, (l.pw - 12) as usize), st);
        }
    }
    if l.wide {
        let resets = q.used.map(|_| resets_text(v, q)).unwrap_or_default();
        let totals = match q.used {
            Some(used) => format!("{} of {} {}", to_fixed(used, 1), q.of, q.unit),
            None => count(q.accounts.len(), "account", "accounts"),
        };
        let room = l.pw - width(&resets) as i32 - 2;
        c.put(2, y + 1, &trunc(&totals, room.max(0) as usize), St::D);
        c.right(right, y + 1, &resets, St::D);
    } else {
        c.put(
            2,
            y + 1,
            &count(q.accounts.len(), "account", "accounts"),
            St::D,
        );
    }
    if selected {
        c.paint(1, y, l.pw + 2, St::bg(Bg::Sel));
        c.paint(1, y + 1, l.pw + 2, St::bg(Bg::Sel));
        if let Some((x, fg)) = partial {
            let glyph = c.symbol(x, y).to_string();
            c.put(x, y, &glyph, St::fg(fg).on(Bg::Track));
        }
    }
}

/// How many models the pricing module prices, and how many it knows of in
/// all; "unknown" for a count it hasn't reported. The design's catalog key
/// takes the second line where the app serves the catalog.
fn models_panel(c: &mut Canvas, v: &View, l: &Layout, y: i32) {
    let usage = &v.snap.usage.data;
    let n = |count: Option<u64>| count.map_or_else(|| UNKNOWN.to_string(), thousands);
    c.put(2, y, "Models", St::B);
    c.hline(2, y + 1, l.pw);
    c.put(
        2,
        y + 2,
        &format!("{} priced", n(usage.models_priced)),
        St::D,
    );
    if v.snap.serves.usage_detail {
        c.put(2, y + 3, "m opens the catalog", St::F);
    } else {
        c.put(
            2,
            y + 3,
            &format!("of {} models", n(usage.models_counted)),
            St::D,
        );
    }
}

/// The selected provider's share of its accounts' capacity used and the
/// soonest reset, or the reason there is none.
fn provider_summary(c: &mut Canvas, v: &View, l: &Layout, q: &Provider) {
    let right = l.x + l.dw;
    let bar_w = (l.dw - 32).clamp(8, USAGE_MAX_W);
    match (q.used, &q.error) {
        (Some(used), None) => {
            let f = used / f64::from(q.of.max(1));
            bar(c, l.x, 5, bar_w, f, use_fg(f));
            let tx = l.x + bar_w + 2;
            let room = (right - tx).max(0) as usize;
            let text = format!("{} of {} {} used", to_fixed(used, 1), q.of, q.unit);
            c.put(tx, 5, &trunc(&text, room), St::B);
            let soonest = q.resets_at.map_or_else(
                || format!("soonest reset {UNKNOWN}"),
                |t| format!("soonest reset in {}", ago(-v.age(t))),
            );
            c.put(tx, 6, &trunc(&soonest, room), St::D);
        }
        _ => {
            let room = l.dw.max(0) as usize;
            let (mut text, st) = match share(v, q, room) {
                Err(cell) => cell,
                // Not reached: a share with no error takes the arm above.
                Ok(_) => (UNKNOWN.to_string(), St::D),
            };
            // The provider's own screen has room for the code itself.
            if let Some(err) = &q.error {
                if is_code(err, q.error_kind) {
                    text = format!("{text} · {err}");
                }
            }
            c.put(l.x, 5, &trunc(&text, room), st);
        }
    }
}

/// Whether an account's note says its reading is old.
fn warns(note: &str) -> bool {
    note.starts_with("refresh failing") || note.contains("last good reading")
}

/// One account from row `y`: its name with its note on the right, then its
/// windows from `scroll` on, or why it is failing.
fn account_block(c: &mut Canvas, v: &View, l: &Layout, y: i32, a: &Account, scroll: usize) {
    let right = l.x + l.dw;
    let name = trunc(account_name(v, a), l.dw.max(0) as usize);
    let end = c.put(l.x, y, &name, St::B);
    let (note, note_st) = if a.error.is_some() {
        ("▲ failing".to_string(), St::W)
    } else if warns(&a.note) {
        (format!("▲ {}", a.note), St::W)
    } else {
        (a.note.clone(), St::D)
    };
    if !note.is_empty() {
        let room = (right - end - 2).max(0) as usize;
        c.right(right, y, &trunc(&note, room), note_st);
    }
    if let Some(err) = &a.error {
        let err = if v.redact {
            mask_identifiers(err, &a.identifiers, &a.redacted)
        } else {
            err.clone()
        };
        c.put(
            l.x + 2,
            y + 1,
            &trunc(&err, (l.dw - 2).max(0) as usize),
            St::W,
        );
        return;
    }
    let n = a.windows.len();
    let k = shown_of(l, n);
    let first = scroll.min(n - k);
    let shown = &a.windows[first..first + k];
    if l.wide {
        let mut x = l.x + 2;
        for w in shown {
            cell(c, v, x, y + 1, w);
            x += CELL_W + CELL_GAP;
        }
        if k < n {
            c.put(x, y + 1, &more(n - k), St::D);
        }
    } else {
        let g = lines(l);
        for (i, w) in shown.iter().enumerate() {
            window_line(c, v, &g, y + 1 + i as i32, w);
        }
        if k < n {
            c.put(l.x + 2, y + 1 + k as i32, &more(n - k), St::D);
        }
    }
}

/// Where a window row's parts go below 150 columns. At 120 columns: the
/// name in 20 columns, a 30-column bar, the percent, then the reset.
struct Lines {
    label_w: i32,
    bar_x: i32,
    bar_w: i32,
    pct_end: i32,
    reset_x: i32,
    right: i32,
}

fn lines(l: &Layout) -> Lines {
    // What the name and bar share after the indent, the percent and the
    // reset ("resets in 19h 25m") take theirs.
    let avail = l.dw - 28;
    let bar_w = (avail - 20).clamp(8, 30);
    let label_w = (avail - bar_w).clamp(6, 20);
    let bar_x = l.x + 2 + label_w;
    Lines {
        label_w,
        bar_x,
        bar_w,
        pct_end: bar_x + bar_w + 6,
        reset_x: bar_x + bar_w + 9,
        right: l.x + l.dw,
    }
}

fn window_line(c: &mut Canvas, v: &View, g: &Lines, y: i32, w: &UsageWindow) {
    c.put(
        g.bar_x - g.label_w,
        y,
        &pad_right(&w.name, g.label_w as usize),
        St::D,
    );
    bar(c, g.bar_x, y, g.bar_w, w.used, use_fg(w.used));
    c.right(g.pct_end, y, &pct(w.used), pct_st(w.used, St::NONE));
    if let Some(t) = w.resets_at {
        let room = (g.right - g.reset_x).max(0) as usize;
        let text = format!("resets in {}", ago(-v.age(t)));
        c.put(g.reset_x, y, &trunc(&text, room), St::D);
    }
}

/// One window as a cell beside others, from column `x`.
fn cell(c: &mut Canvas, v: &View, x: i32, y: i32, w: &UsageWindow) {
    c.put(x, y, &pad_right(&w.name, CELL_LABEL as usize), St::D);
    let bx = x + CELL_LABEL + 1;
    bar(c, bx, y, CELL_BAR, w.used, use_fg(w.used));
    c.right(bx + CELL_BAR + 5, y, &pct(w.used), pct_st(w.used, St::NONE));
    if let Some(t) = w.resets_at {
        let text = format!("resets {}", ago(-v.age(t)));
        c.put(
            bx + CELL_BAR + 7,
            y,
            &trunc(&text, CELL_RESET as usize),
            St::D,
        );
    }
}
