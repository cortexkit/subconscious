//! Overlays drawn over a dimmed screen: the command palette, the keys page
//! and confirmations.

use crate::draw::{pad_right, trunc, width, Canvas, Clip};
use crate::model::{PaletteCommand, Serves};
use crate::theme::{Bg, Fg, St};

/// Whether every character of `query` (spaces ignored) appears in `text` in
/// order, ignoring case.
pub fn fuzzy(query: &str, text: &str) -> bool {
    let q: Vec<char> = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let mut i = 0;
    for ch in text.to_lowercase().chars() {
        if i < q.len() && ch == q[i] {
            i += 1;
        }
    }
    i == q.len()
}

/// The palette shows at most this many matches.
pub const PALETTE_ROWS: usize = 12;

/// How well `query` matches `text`, best first: 0 when the text starts with
/// it, 1 when a word in the text does, 2 when its letters only appear in
/// order; `None` when they don't. Case and spaces in the query are ignored.
pub fn match_rank(query: &str, text: &str) -> Option<u8> {
    if !fuzzy(query, text) {
        return None;
    }
    let q: String = query
        .to_lowercase()
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    let t = text.to_lowercase();
    if t.starts_with(&q) {
        return Some(0);
    }
    let word_start = t
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| !w.is_empty() && w.starts_with(&q));
    Some(if word_start { 1 } else { 2 })
}

/// The commands matching `query`, best matches first: prefix matches, then
/// word-start matches, then scattered letters. Within each, commands naming
/// `preferred` (the selected module) come first, then the listed order.
pub fn matches<'a>(
    commands: &'a [PaletteCommand],
    query: &str,
    preferred: Option<&str>,
) -> Vec<&'a PaletteCommand> {
    let names =
        |c: &PaletteCommand| preferred.is_some_and(|p| c.title.split_whitespace().any(|w| w == p));
    let mut ranked: Vec<(u8, bool, &PaletteCommand)> = commands
        .iter()
        .filter_map(|c| match_rank(query, &c.title).map(|r| (r, !names(c), c)))
        .collect();
    ranked.sort_by_key(|(rank, other, _)| (*rank, *other));
    ranked
        .into_iter()
        .map(|(_, _, c)| c)
        .take(PALETTE_ROWS)
        .collect()
}

pub fn palette(
    c: &mut Canvas,
    commands: &[PaletteCommand],
    query: &str,
    sel: usize,
    preferred: Option<&str>,
) {
    c.dim_all();
    let (wd, hd) = (70, 18);
    let x = ((c.w - wd) / 2).max(0);
    let y = 4;
    c.boxed(x, y, wd, hd, "");
    c.put(x + 2, y + 1, ":", St::A.bold());
    let end = c.put(x + 4, y + 1, query, St::B);
    c.put(end, y + 1, "▏", St::A);
    c.hline(x + 1, y + 2, wd - 2);
    let list = matches(commands, query, preferred);
    if list.is_empty() {
        c.put(x + 4, y + 4, "No matching command", St::D);
    }
    let chosen = sel.min(list.len().saturating_sub(1));
    for (i, cmd) in list.iter().enumerate() {
        let row = y + 3 + i as i32;
        let on = i == chosen;
        c.put(
            x + 4,
            row,
            &trunc(&cmd.title, 48),
            if on { St::B } else { St::NONE },
        );
        c.right(x + wd - 3, row, &cmd.area, St::D);
        if on {
            c.paint(x + 1, row, wd - 2, St::bg(Bg::Sel));
        }
    }
    c.put(x + 2, y + hd - 2, "↑↓ choose · ⏎ run · esc close", St::F);
}

/// The keys page's three columns: (title, rows of key and action). Only
/// keys the app serves are listed.
pub fn help_columns(serves: Serves) -> [(&'static str, Vec<(String, &'static str)>); 3] {
    let tabs = if serves.tabs > 1 {
        format!("1 – {}", serves.tabs)
    } else {
        "1".to_string()
    };
    let everywhere = vec![
        (tabs, "switch tab"),
        (":".into(), "commands"),
        ("?".into(), "this help"),
        ("esc".into(), "back or close"),
        ("t".into(), "light or dark"),
        ("q".into(), "quit"),
    ];
    let mut lists = vec![("↑↓ j k".into(), "move"), ("⏎".into(), "open")];
    if serves.list_filter {
        lists.push(("/".into(), "filter"));
    }
    lists.push(("g G".into(), "top, bottom"));
    if serves.multi_select {
        lists.push(("space".into(), "select"));
    }
    let mut modules = vec![("r".into(), "restart")];
    if serves.swap {
        modules.push(("w".into(), "restart, no downtime"));
    }
    modules.push(("s".into(), "stop or start"));
    modules.push(("l".into(), "logs"));
    if serves.all_metrics {
        modules.push(("m".into(), "all metrics"));
    }
    if serves.settings {
        modules.push(("e".into(), "settings"));
    }
    [
        ("Everywhere", everywhere),
        ("Lists", lists),
        ("Modules", modules),
    ]
}

/// The keys page. In a window too small for the whole page, the box shrinks
/// to the window and its contents are cut at the box's edges. Keys for
/// features `serves` doesn't have are left out.
pub fn help(c: &mut Canvas, serves: Serves) {
    c.dim_all();
    let wd = 100.min(c.w - 4);
    let hd = 26.min(c.h - 2);
    let x = ((c.w - wd) / 2).max(0);
    let y = if c.h > 4 + 26 { 4 } else { 1 };
    c.boxed(x, y, wd, hd, "Keys");
    let prev = c.clip_to(Clip {
        x0: x + 1,
        y0: y + 1,
        x1: x + wd - 1,
        y1: y + hd - 1,
    });
    for ((title, rows), cx) in help_columns(serves).iter().zip([x + 4, x + 36, x + 66]) {
        c.put(cx, y + 2, title, St::B);
        for (i, (k, t)) in rows.iter().enumerate() {
            c.put(cx, y + 4 + i as i32, &pad_right(k, 8), St::A);
            c.put(cx + 8, y + 4 + i as i32, t, St::NONE);
        }
    }
    c.put(x + 4, y + 13, "Mouse", St::B);
    let mouse = if serves.double_click {
        "Click to select, double-click to open, scroll anywhere. Nothing needs the mouse."
    } else {
        "Click to select, scroll anywhere. Nothing needs the mouse."
    };
    c.put(x + 4, y + 15, mouse, St::D);
    if serves.ckni {
        c.put(x + 4, y + 17, "Scripts and agents", St::B);
        c.put(
            x + 4,
            y + 19,
            "ckni runs any command without this app: plain text, or --json. Everything here has a",
            St::D,
        );
        c.put(x + 4, y + 20, "ckni form, and the palette shows it.", St::D);
    }
    c.put(x + 4, y + 23, "esc closes", St::F);
    c.set_clip(prev);
}

/// A question asked once before an action that is hard to undo: what will
/// happen, then the key that does it and the key that doesn't.
pub fn confirm(c: &mut Canvas, title: &str, body: &[String], yes: &str) {
    c.dim_all();
    let wd = 64.min(c.w - 4);
    let inner = (wd - 8).max(0) as usize;
    let lines: Vec<String> = body.iter().flat_map(|l| wrap(l, inner)).collect();
    let hd = lines.len() as i32 + 7;
    let x = ((c.w - wd) / 2).max(0);
    let y = ((c.h - hd) / 2).max(1);
    c.boxed(x, y, wd, hd, title);
    for (i, line) in lines.iter().enumerate() {
        c.put(x + 4, y + 2 + i as i32, line, St::NONE);
    }
    let by = y + hd - 3;
    let yes = format!(" ⏎  {yes} ");
    let end = c.put(x + 4, by, &yes, St::fg(Fg::OnAcc).on(Bg::Acc).bold());
    c.put(end + 2, by, " esc  Cancel ", St::bg(Bg::Key));
}

/// Splits `text` into lines of at most `n` columns at spaces; a word longer
/// than a line is cut.
pub fn wrap(text: &str, n: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let need = if line.is_empty() {
            width(word)
        } else {
            width(&line) + 1 + width(word)
        };
        if need > n && !line.is_empty() {
            out.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
        if width(&line) > n {
            out.push(trunc(&line, n));
            line.clear();
        }
    }
    if !line.is_empty() {
        out.push(line);
    }
    out
}
