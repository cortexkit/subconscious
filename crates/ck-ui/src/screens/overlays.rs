//! Overlays drawn over a dimmed screen: the command palette and the keys page.

use crate::draw::{pad_right, trunc, Canvas, Clip};
use crate::model::PaletteCommand;
use crate::theme::{Bg, St};

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

/// The commands matching `query`, in their listed order.
pub fn matches<'a>(commands: &'a [PaletteCommand], query: &str) -> Vec<&'a PaletteCommand> {
    commands
        .iter()
        .filter(|c| fuzzy(query, &c.title))
        .take(PALETTE_ROWS)
        .collect()
}

pub fn palette(c: &mut Canvas, commands: &[PaletteCommand], query: &str, sel: usize) {
    c.dim_all();
    let (wd, hd) = (70, 18);
    let x = ((c.w - wd) / 2).max(0);
    let y = 4;
    c.boxed(x, y, wd, hd, "");
    c.put(x + 2, y + 1, ":", St::A.bold());
    let end = c.put(x + 4, y + 1, query, St::B);
    c.put(end, y + 1, "▏", St::A);
    c.hline(x + 1, y + 2, wd - 2);
    let list = matches(commands, query);
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

/// The keys page. In a window too small for the whole page, the box shrinks
/// to the window and its contents are cut at the box's edges.
pub fn help(c: &mut Canvas) {
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
    let col = |c: &mut Canvas, cx: i32, title: &str, rows: &[(&str, &str)]| {
        c.put(cx, y + 2, title, St::B);
        for (i, (k, t)) in rows.iter().enumerate() {
            c.put(cx, y + 4 + i as i32, &pad_right(k, 8), St::A);
            c.put(cx + 8, y + 4 + i as i32, t, St::NONE);
        }
    };
    col(
        c,
        x + 4,
        "Everywhere",
        &[
            ("1 – 7", "switch tab"),
            (":", "commands"),
            ("?", "this help"),
            ("esc", "back or close"),
            ("t", "light or dark"),
            ("q", "quit"),
        ],
    );
    col(
        c,
        x + 36,
        "Lists",
        &[
            ("↑↓ j k", "move"),
            ("⏎", "open"),
            ("/", "filter"),
            ("g G", "top, bottom"),
            ("space", "select"),
        ],
    );
    col(
        c,
        x + 66,
        "Modules",
        &[
            ("r", "restart"),
            ("w", "restart, no downtime"),
            ("s", "stop or start"),
            ("l", "logs"),
            ("e", "settings"),
        ],
    );
    c.put(x + 4, y + 13, "Mouse", St::B);
    c.put(
        x + 4,
        y + 15,
        "Click to select, double-click to open, scroll anywhere. Nothing needs the mouse.",
        St::D,
    );
    c.put(x + 4, y + 17, "Scripts and agents", St::B);
    c.put(
        x + 4,
        y + 19,
        "ckni runs any command without this app: plain text, or --json. Everything here has a",
        St::D,
    );
    c.put(x + 4, y + 20, "ckni form, and the palette shows it.", St::D);
    c.put(x + 4, y + 23, "esc closes", St::F);
    c.set_clip(prev);
}
