//! Drawing helpers over ratatui's `Buffer`.
//!
//! Every screen paints cells directly: one glyph and one style per cell, at
//! exact coordinates. Each call replaces a cell's whole style, except
//! [`Canvas::paint`] and [`Canvas::dim_all`], which add to it. Coordinates are
//! signed so a layout that runs off an edge simply draws nothing there.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use unicode_width::UnicodeWidthChar;
use unicode_width::UnicodeWidthStr;

use crate::model::Status;
use crate::theme::{Bg, Fg, St, Theme};

/// Display width of a string in terminal columns.
pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

fn char_width(ch: char) -> usize {
    UnicodeWidthChar::width(ch).unwrap_or(0)
}

/// Cuts `s` to at most `n` columns, ending in an ellipsis when cut. A cut
/// that lands on an ellipsis already in the text keeps just one.
pub fn trunc(s: &str, n: usize) -> String {
    if width(s) <= n {
        return s.to_string();
    }
    if n == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in s.chars() {
        let w = char_width(ch);
        if used + w > n - 1 {
            break;
        }
        out.push(ch);
        used += w;
    }
    if out.ends_with('…') {
        out.pop();
    }
    out.push('…');
    out
}

/// `trunc` to `n` columns, then spaces on the right up to `n`.
pub fn pad_right(s: &str, n: usize) -> String {
    let t = trunc(s, n);
    let pad = n.saturating_sub(width(&t));
    t + &" ".repeat(pad)
}

/// `trunc` to `n` columns, then spaces on the left up to `n`.
pub fn pad_left(s: &str, n: usize) -> String {
    let t = trunc(s, n);
    let pad = n.saturating_sub(width(&t));
    " ".repeat(pad) + &t
}

/// A rectangle drawing is limited to, in canvas coordinates (inclusive start,
/// exclusive end).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Clip {
    pub x0: i32,
    pub y0: i32,
    pub x1: i32,
    pub y1: i32,
}

/// A drawing surface over a buffer, in coordinates relative to its area.
pub struct Canvas<'a> {
    buf: &'a mut Buffer,
    theme: Theme,
    pub w: i32,
    pub h: i32,
    clip: Clip,
}

impl<'a> Canvas<'a> {
    pub fn new(buf: &'a mut Buffer, theme: Theme) -> Self {
        let area = buf.area;
        let (w, h) = (i32::from(area.width), i32::from(area.height));
        Canvas {
            buf,
            theme,
            w,
            h,
            clip: Clip {
                x0: 0,
                y0: 0,
                x1: w,
                y1: h,
            },
        }
    }

    pub fn theme(&self) -> Theme {
        self.theme
    }

    /// Limits drawing to `clip` (within the canvas) and returns the previous
    /// limit, for [`Canvas::set_clip`] to restore.
    pub fn clip_to(&mut self, clip: Clip) -> Clip {
        let prev = self.clip;
        self.clip = Clip {
            x0: clip.x0.max(0),
            y0: clip.y0.max(0),
            x1: clip.x1.min(self.w),
            y1: clip.y1.min(self.h),
        };
        prev
    }

    pub fn set_clip(&mut self, clip: Clip) {
        self.clip = clip;
    }

    fn visible(&self, x: i32, y: i32) -> bool {
        x >= self.clip.x0 && x < self.clip.x1 && y >= self.clip.y0 && y < self.clip.y1
    }

    fn cell_mut(&mut self, x: i32, y: i32) -> Option<&mut ratatui::buffer::Cell> {
        if !self.visible(x, y) {
            return None;
        }
        let area: Rect = self.buf.area;
        let pos = (area.x + x as u16, area.y + y as u16);
        self.buf.cell_mut(pos)
    }

    /// The glyph at a cell, or "" off the canvas.
    pub fn symbol(&self, x: i32, y: i32) -> &str {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return "";
        }
        let area = self.buf.area;
        self.buf
            .cell((area.x + x as u16, area.y + y as u16))
            .map_or("", |c| c.symbol())
    }

    /// Writes text from (x, y) and returns the column after it. Cells off the
    /// canvas are skipped, so the returned column still counts them.
    pub fn put(&mut self, x: i32, y: i32, text: &str, st: St) -> i32 {
        let style = self.theme.style(st);
        let mut x = x;
        for ch in text.chars() {
            let w = char_width(ch).max(1) as i32;
            if let Some(cell) = self.cell_mut(x, y) {
                cell.reset();
                let mut tmp = [0u8; 4];
                cell.set_symbol(ch.encode_utf8(&mut tmp));
                cell.set_style(style);
            }
            // A wide glyph covers the next cell; blank it so nothing stale
            // shows through.
            for k in 1..w {
                if let Some(cell) = self.cell_mut(x + k, y) {
                    cell.reset();
                    cell.set_symbol("");
                    cell.set_style(style);
                }
            }
            x += w;
        }
        x
    }

    /// Writes text so that it ends just before column `end`. Returns its start.
    pub fn right(&mut self, end: i32, y: i32, text: &str, st: St) -> i32 {
        let start = end - width(text) as i32;
        self.put(start, y, text, st);
        start
    }

    /// Centers text in the `w` columns from `x`.
    pub fn center(&mut self, x: i32, w: i32, y: i32, text: &str, st: St) {
        let n = width(text) as i32;
        self.put(x + ((w - n) / 2).max(0), y, text, st);
    }

    /// Adds a style to `w` cells without changing their glyphs.
    pub fn paint(&mut self, x: i32, y: i32, w: i32, st: St) {
        let style = self.theme.patch(st);
        for i in x..x + w {
            if let Some(cell) = self.cell_mut(i, y) {
                cell.set_style(style);
            }
        }
    }

    pub fn fill(&mut self, x: i32, y: i32, w: i32, h: i32, ch: char, st: St) {
        let row = ch.to_string().repeat(w.max(0) as usize);
        for j in y..y + h {
            self.put(x, j, &row, st);
        }
    }

    /// A horizontal hairline in the faint role.
    pub fn hline(&mut self, x: i32, y: i32, w: i32) {
        self.hline_with(x, y, w, '─', St::F);
    }

    pub fn hline_with(&mut self, x: i32, y: i32, w: i32, ch: char, st: St) {
        let row = ch.to_string().repeat(w.max(0) as usize);
        self.put(x, y, &row, st);
    }

    /// A vertical hairline in the faint role.
    pub fn vline(&mut self, x: i32, y: i32, h: i32) {
        for j in y..y + h {
            self.put(x, j, "│", St::F);
        }
    }

    /// A rounded box, cleared inside, with an optional bold title on its top
    /// edge. Used only for overlays.
    pub fn boxed(&mut self, x: i32, y: i32, w: i32, h: i32, title: &str) {
        self.fill(x, y, w, h, ' ', St::NONE);
        let inner = "─".repeat((w - 2).max(0) as usize);
        self.put(x, y, &format!("╭{inner}╮"), St::F);
        for j in y + 1..y + h - 1 {
            self.put(x, j, "│", St::F);
            self.put(x + w - 1, j, "│", St::F);
        }
        self.put(x, y + h - 1, &format!("╰{inner}╯"), St::F);
        if !title.is_empty() {
            self.put(x + 2, y, &format!(" {title} "), St::B);
        }
    }

    /// Dims the cells of a rectangle without changing their glyphs or
    /// colors, for values that are only a last good reading.
    pub fn dim_rect(&mut self, x: i32, y: i32, w: i32, h: i32) {
        for j in y..y + h {
            for i in x..x + w {
                if let Some(cell) = self.cell_mut(i, j) {
                    cell.modifier.insert(Modifier::DIM);
                }
            }
        }
    }

    /// Dims everything drawn so far, so an overlay reads as the only live
    /// thing on screen.
    pub fn dim_all(&mut self) {
        let area = self.buf.area;
        for y in 0..area.height {
            for x in 0..area.width {
                if let Some(cell) = self.buf.cell_mut((area.x + x, area.y + y)) {
                    cell.modifier.insert(Modifier::DIM);
                }
            }
        }
    }
}

// ---- widgets ---------------------------------------------------------------

/// A status's glyph, its color role and its word. Every glyph sits next to a
/// word or a name, so status never depends on color alone.
pub fn status_glyph(status: Status) -> (&'static str, St) {
    match status {
        Status::Ok => ("●", St::O),
        Status::Degraded => ("▲", St::W),
        Status::Failed => ("✕", St::E),
        Status::Starting => ("◐", St::A),
        Status::Stopped => ("○", St::D),
        Status::Unknown => ("◌", St::D),
    }
}

pub fn status_word(status: Status) -> &'static str {
    match status {
        Status::Ok => "ok",
        Status::Degraded => "degraded",
        Status::Failed => "failed",
        Status::Starting => "starting",
        Status::Stopped => "stopped",
        Status::Unknown => "unknown",
    }
}

/// Severity for a used fraction: quiet until it matters.
pub fn use_fg(frac: f64) -> Fg {
    if frac >= 0.999 {
        Fg::Err
    } else if frac >= 0.85 {
        Fg::Warn
    } else {
        Fg::Acc
    }
}

/// A horizontal gauge with eighth-block resolution: filled cells in the fill
/// color, a quiet track behind, and one partial cell that blends the two.
pub fn bar(c: &mut Canvas, x: i32, y: i32, w: i32, frac: f64, fill: Fg) {
    let frac = frac.clamp(0.0, 1.0);
    let eighths = crate::time::js_round(frac * f64::from(w) * 8.0) as i32;
    let full = eighths / 8;
    let rem = eighths % 8;
    const PARTS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    for i in 0..w {
        if i < full {
            c.put(x + i, y, "█", St::fg(fill));
        } else if i == full && rem != 0 {
            c.put(x + i, y, PARTS[rem as usize], St::fg(fill).on(Bg::Track));
        } else {
            c.put(x + i, y, "█", St::fg(Fg::Track));
        }
    }
}

/// A block sparkline scaled between `min` and `max` (`max` defaults to the
/// largest value, at least 1). A raised `min` lets a slowly moving value such
/// as memory show its variation instead of a flat block.
pub fn spark(c: &mut Canvas, x: i32, y: i32, values: &[f64], fg: Fg, max: Option<f64>, min: f64) {
    const TICKS: [&str; 8] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    let m = max.unwrap_or_else(|| values.iter().copied().fold(1.0, f64::max));
    for (i, v) in values.iter().enumerate() {
        // A missing sample is a gap in the line, never a zero.
        if !v.is_finite() {
            c.put(x + i as i32, y, " ", St::NONE);
            continue;
        }
        let k = crate::time::js_round(((v - min) / (m - min).max(1e-9)) * 7.0).clamp(0.0, 7.0);
        c.put(x + i as i32, y, TICKS[k as usize], St::fg(fg));
    }
}

/// Section heading: bold title, dim meta on the right, a hairline beneath.
pub fn heading(c: &mut Canvas, x: i32, y: i32, w: i32, title: &str, meta: &str) {
    heading_st(c, x, y, w, title, meta, St::D);
}

/// A heading whose meta is drawn in `meta_st`, such as amber for an age
/// worth noticing. A meta too long for the space beside the title is cut.
pub fn heading_st(c: &mut Canvas, x: i32, y: i32, w: i32, title: &str, meta: &str, meta_st: St) {
    c.put(x, y, title, St::B);
    if !meta.is_empty() {
        let room = (w - width(title) as i32 - 2).max(0) as usize;
        c.right(x + w, y, &trunc(meta, room), meta_st);
    }
    c.hline(x, y + 1, w);
}

/// Columns one key hint takes, including the gap after it.
fn hint_width(key: &str, label: &str) -> i32 {
    width(key) as i32 + 2 + 1 + width(label) as i32 + 3
}

/// Key hints: a key cap, then its quiet label. Returns the column after the
/// gap following the last hint.
pub fn keys(c: &mut Canvas, x: i32, y: i32, items: &[(&str, &str)]) -> i32 {
    let mut x = x;
    for (k, label) in items {
        x = c.put(x, y, &format!(" {k} "), St::bg(Bg::Key));
        x = c.put(x + 1, y, label, St::D);
        x += 3;
    }
    x
}

/// How many footer hints fit, and whether the status text on the right still
/// fits beside them. Hints are in priority order. The status text gives way
/// first, then hints drop whole from the end, so a narrow window never shows
/// half a hint or a hint run into the status text.
pub fn footer_fit(w: i32, items: &[(&str, &str)], right: &str) -> (usize, bool) {
    // A hint's label must end at least two columns before the status text.
    const GAP: i32 = 2;
    let ends: Vec<i32> = items
        .iter()
        .scan(1, |x, (k, l)| {
            let end = *x + hint_width(k, l) - 3;
            *x += hint_width(k, l);
            Some(end)
        })
        .collect();
    let all_end = ends.last().copied().unwrap_or(1);
    let right_start = w - 1 - width(right) as i32;
    if !right.is_empty() && all_end + GAP <= right_start {
        return (items.len(), true);
    }
    // A hint must end before the last column, which stays blank as a margin.
    let n = ends.iter().take_while(|end| **end < w).count();
    (n, false)
}

/// The footer row: key hints on the left, status text on the right.
pub fn footer(c: &mut Canvas, items: &[(&str, &str)], right: &str, right_st: St) {
    let y = c.h - 1;
    let (n, show_right) = footer_fit(c.w, items, right);
    keys(c, 1, y, &items[..n]);
    if show_right && !right.is_empty() {
        c.right(c.w - 1, y, right, right_st);
    }
}

/// The hairline above the footer, then the footer.
pub fn frame_end(c: &mut Canvas, items: &[(&str, &str)], right: &str, right_st: St) {
    let y = c.h - 2;
    c.hline(0, y, c.w);
    footer(c, items, right, right_st);
}

/// The tabs, in order. Their numbers are their keys.
pub const TABS: [&str; 7] = [
    "Home", "Modules", "Activity", "Usage", "Agents", "Devices", "Settings",
];

/// Whether a window below 120 columns must shorten the tab bar to numbers
/// (the active tab keeps its name): only when the bar with every name
/// doesn't fit beside the clock.
fn tabs_shortened(w: i32, tabs: usize) -> bool {
    if w >= 120 {
        return false;
    }
    let names: i32 = TABS.iter().take(tabs).map(|t| 2 + width(t) as i32).sum();
    let gaps = 2 * (tabs.saturating_sub(1)) as i32;
    // The bar starts at column 4; the clock takes the last six columns.
    4 + names + gaps > w - 7
}

/// Where each of the first `tabs` tabs sits in the top bar: (start column,
/// width, tab index). Below 120 columns, when the names don't all fit, only
/// the active tab shows its name and the rest just their number.
pub fn tab_spans(w: i32, active: usize, tabs: usize) -> Vec<(i32, i32, usize)> {
    let mut spans = Vec::new();
    if w < 120 {
        let short = tabs_shortened(w, tabs);
        let mut x = 4;
        for (i, t) in TABS.iter().take(tabs).enumerate() {
            let start = x;
            x += if i == active || !short {
                2 + width(t) as i32
            } else {
                1
            };
            spans.push((start, x - start, i));
            x += 2;
        }
    } else {
        let mut x = 16;
        for (i, t) in TABS.iter().take(tabs).enumerate() {
            let start = x;
            x += 2 + width(t) as i32;
            spans.push((start, x - start, i));
            x += 3;
        }
    }
    spans
}

/// The top bar: the first `tabs` tabs (only the screens that exist) and the
/// clock, with the wordmark and the machine's name from 120 columns. The
/// active tab carries the accent and an accent rule beneath it.
pub fn chrome(c: &mut Canvas, active: usize, tabs: usize, clock: &str, machine: &str) {
    let compact = c.w < 120;
    let short = tabs_shortened(c.w, tabs);
    c.put(1, 0, "◆", St::A.bold());
    if !compact {
        c.put(3, 0, "CortexKit", St::B);
    }
    let spans = tab_spans(c.w, active, tabs);
    for &(x, _, i) in &spans {
        let on = i == active;
        let num = (i + 1).to_string();
        if short && !on {
            c.put(x, 0, &num, St::F);
        } else {
            c.put(x, 0, &num, if on { St::A } else { St::F });
            c.put(x + 2, 0, TABS[i], if on { St::B } else { St::D });
        }
    }
    c.right(c.w - 1, 0, clock, St::D);
    if !compact {
        c.right(c.w - 8, 0, machine, St::D);
    }
    c.hline(0, 1, c.w);
    for &(x, w, i) in &spans {
        if i == active {
            c.hline_with(x, 1, w, '━', St::A);
        }
    }
}

/// The live dot: it breathes once a second, so "live" shows without churning
/// any text.
pub fn live_dot(tick: u64) -> (&'static str, St) {
    if tick.is_multiple_of(2) {
        ("●", St::O)
    } else {
        ("●", St::O.dim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trunc_keeps_one_ellipsis() {
        assert_eq!(trunc("abcdef", 4), "abc…");
        assert_eq!(trunc("ab…def", 4), "ab…");
        assert_eq!(trunc("abc", 3), "abc");
        assert_eq!(pad_right("ab", 4), "ab  ");
        assert_eq!(pad_left("85%", 4), " 85%");
        assert_eq!(trunc("abc", 0), "");
    }

    #[test]
    fn footer_drops_status_text_then_whole_hints() {
        let items = [
            ("⏎", "open"),
            ("↑↓", "move"),
            (":", "commands"),
            ("?", "help"),
        ];
        assert_eq!(footer_fit(120, &items, "updated just now"), (4, true));
        // The labels end before columns 9, 21, 36 and 47.
        assert_eq!(footer_fit(40, &items, "updated just now"), (3, false));
        assert_eq!(footer_fit(22, &items, "x"), (2, false));
        assert_eq!(footer_fit(20, &items, "x"), (1, false));
        assert_eq!(footer_fit(5, &items, ""), (0, false));
    }
}
