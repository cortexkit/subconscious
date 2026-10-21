//! The log view: one module's full log, newest at the bottom, filtered by
//! level and searched in place.
//!
//! The view follows new lines as they arrive unless paused. A search doesn't
//! hide lines: it highlights every match and counts the lines holding one,
//! and `n` moves to the next of them. The level chips do hide lines.

use crate::draw::{chrome, frame_end, live_dot, trunc, width, Canvas};
use crate::model::{LogLevel, LogLine, LogView, Panel};
use crate::theme::{Bg, St};

use super::View;

/// Which lines the level chips let through.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LevelFilter {
    #[default]
    All,
    /// Warnings and errors.
    Warnings,
    Errors,
}

impl LevelFilter {
    pub fn admits(self, level: LogLevel) -> bool {
        match self {
            LevelFilter::All => true,
            LevelFilter::Warnings => level != LogLevel::Info,
            LevelFilter::Errors => level == LogLevel::Error,
        }
    }

    /// The next chip to the right, wrapping.
    pub fn next(self) -> LevelFilter {
        match self {
            LevelFilter::All => LevelFilter::Warnings,
            LevelFilter::Warnings => LevelFilter::Errors,
            LevelFilter::Errors => LevelFilter::All,
        }
    }
}

/// What the user has set in the log view.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogState {
    pub levels: LevelFilter,
    pub query: String,
    /// Whether keys go to the search text.
    pub typing: bool,
    /// Paused: the view stays where it is instead of following new lines.
    pub paused: bool,
    /// How many lines above the newest the view's last row sits.
    pub scroll: usize,
}

/// How many log rows the view has in a window `h` rows tall.
pub fn rows(h: i32) -> usize {
    (h - 8).max(0) as usize
}

/// The lines the level chips let through, oldest first.
pub fn visible<'a>(view: &'a LogView, state: &LogState) -> Vec<&'a LogLine> {
    view.lines
        .iter()
        .filter(|l| state.levels.admits(l.level))
        .collect()
}

/// Indexes into `lines` of the lines holding the search text.
pub fn matches(lines: &[&LogLine], query: &str) -> Vec<usize> {
    if query.is_empty() {
        return Vec::new();
    }
    lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.message.contains(query))
        .map(|(i, _)| i)
        .collect()
}

/// The first line shown, for `len` lines in `rows` rows scrolled `scroll`
/// lines up from the newest.
pub fn first_shown(len: usize, rows: usize, scroll: usize) -> usize {
    len.saturating_sub(rows).saturating_sub(scroll)
}

pub fn draw(c: &mut Canvas, v: &View, panel: &Panel<LogView>, state: &LogState) {
    let view = &panel.data;
    chrome(
        c,
        1,
        v.snap.serves.tabs,
        &v.clock(),
        &v.snap.host.data.label,
    );
    let w = c.w;
    c.put(2, 3, "←", St::D);
    let x = c.put(4, 3, &view.module, St::B);
    let x = c.put(x + 1, 3, "· log", St::D);
    let mut cx = 22.max(x + 2);
    for (label, chip) in [
        ("all levels", LevelFilter::All),
        ("warnings", LevelFilter::Warnings),
        ("errors", LevelFilter::Errors),
    ] {
        let text = format!(" {label} ");
        let st = if state.levels == chip {
            St::bg(Bg::Key).bold()
        } else {
            St::D
        };
        cx = c.put(cx, 3, &text, st) + 1;
    }
    if state.paused {
        c.right(w - 4, 3, "paused", St::W);
        c.put(w - 3, 3, "○", St::D);
    } else {
        let (dot, dot_st) = live_dot(v.tick);
        c.right(w - 4, 3, "following", St::D);
        c.put(w - 3, 3, dot, dot_st);
    }

    let lines = visible(view, state);
    let hits = matches(&lines, &state.query);
    if !state.query.is_empty() || state.typing {
        let sx = 70.max(cx + 1);
        c.put(sx, 3, "/", St::A);
        let end = c.put(sx + 2, 3, &state.query, St::B.underline());
        if state.typing {
            c.put(end, 3, "▏", St::A);
        }
        let n = hits.len();
        let count = if n == 1 {
            "1 match".to_string()
        } else {
            format!("{n} matches")
        };
        c.put(end + 1, 3, &count, St::D);
    }
    c.hline(2, 4, w - 4);

    let rows = rows(c.h);
    let first = first_shown(lines.len(), rows, state.scroll);
    let msg_w = (w - 26).max(0) as usize;
    for (i, line) in lines.iter().skip(first).take(rows).enumerate() {
        let y = 5 + i as i32;
        if let Some(at) = line.at {
            let ms = at.unix_millis().rem_euclid(1000);
            c.put(2, y, &format!("{}.{ms:03}", v.civil(at).hms()), St::D);
        }
        let level_st = match line.level {
            LogLevel::Info => St::F,
            LogLevel::Warn => St::W,
            LogLevel::Error => St::E,
        };
        c.put(16, y, line.level.word(), level_st);
        let mx = 23;
        let text = trunc(&line.message, msg_w);
        let msg_st = if line.level == LogLevel::Error {
            St::E
        } else {
            St::NONE
        };
        c.put(mx, y, &text, msg_st);
        if !state.query.is_empty() {
            let mut from = 0;
            while let Some(pos) = text[from..].find(&state.query) {
                let at = from + pos;
                let col = mx + width(&text[..at]) as i32;
                c.paint(col, y, width(&state.query) as i32, St::bg(Bg::Hl));
                from = at + state.query.len();
            }
        }
    }
    if lines.is_empty() {
        // An error reads as an error, and a log still being read says so;
        // only a log read without error and holding nothing is "No lines yet".
        let (note, st) = match (&panel.error, panel.loaded, view.lines.is_empty()) {
            (Some(err), _, true) => (format!("Couldn't read the log: {err}"), St::W),
            (None, false, true) => ("Reading the log…".to_string(), St::D),
            (_, _, true) => ("No lines yet".to_string(), St::D),
            (_, _, false) => ("No lines at this level".to_string(), St::D),
        };
        c.put(2, 5, &trunc(&note, (w - 4).max(0) as usize), st);
    }

    let meta = v.meta(panel, "");
    let (right, right_st) = if meta.text.is_empty() {
        (view.source.clone(), St::D)
    } else {
        (meta.text, meta.st)
    };
    frame_end(
        c,
        &[
            ("f", if state.paused { "follow" } else { "pause" }),
            ("/", "search"),
            ("n", "next match"),
            ("m", "module"),
            ("esc", "back"),
        ],
        &right,
        right_st,
    );
}
