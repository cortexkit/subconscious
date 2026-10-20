//! Modules: what each part is doing.
//!
//! A table of every module with a detail pane that follows the selection:
//! health in the module's own words, a pending restart, CPU and memory, and
//! the live log, newest first. From 150 columns the table gains each
//! module's one-line summary, and a tall window shows today's process starts
//! and exits under it. Below 120 columns the pane has no room: the table
//! takes the width, with the summary column once it gets 24 columns. When
//! the modules don't all fit, "↑ N more" and "↓ N more" rows say how many
//! are out of view.

use crate::draw::{
    chrome, frame_end, heading, live_dot, pad_right, spark, status_glyph, status_word, trunc,
    Canvas, Clip,
};
use crate::model::{LogLevel, ProcessEventKind, Status};
use crate::theme::{Bg, Fg, St};
use crate::time::{ago, js_round};

use super::{fmt_cpu, fmt_mem, window, View};

/// The table's geometry at a window size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Right edge of the table.
    pub lw: i32,
    /// Whether the detail pane is drawn.
    pub pane: bool,
    /// Whether the table has the summary column.
    pub summary: bool,
    /// The last screen row the table, and the lists under it, may draw on:
    /// the row just above the footer.
    pub last: i32,
}

/// Where the Summary column starts, two columns after Build's ten.
const SUMMARY_X: i32 = 73;

/// The fewest columns worth giving the Summary column when there is no pane.
const SUMMARY_MIN_W: i32 = 24;

pub fn layout(w: i32, h: i32) -> Layout {
    let wide = w >= 150;
    let pane = w >= 120;
    let lw = if wide {
        let pane_w = js_round(f64::from(w) * 0.3).clamp(45.0, 70.0) as i32;
        w - pane_w - 5
    } else if pane {
        72
    } else {
        w - 2
    };
    Layout {
        lw,
        pane,
        summary: wide || (!pane && lw - SUMMARY_X >= SUMMARY_MIN_W),
        last: h - 4,
    }
}

/// Which modules the table shows and whether it says how many it leaves
/// out above and below.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rows {
    /// Index of the first module shown.
    pub first: usize,
    /// How many modules are shown.
    pub count: usize,
    /// Whether the first row is an "↑ N more" note.
    pub above: bool,
    /// Whether the last row is a "↓ N more" note.
    pub below: bool,
}

/// The table's rows for a window height, keeping the selection in view.
/// When the modules don't all fit, a note replaces the top or bottom row
/// (or both) and says how many modules are out of view on that side.
pub fn rows(h: i32, len: usize, sel: usize) -> Rows {
    let v = (layout(0, h).last - 7 + 1).max(3) as usize;
    let sel = sel.min(len.saturating_sub(1));
    if len <= v {
        return Rows {
            first: 0,
            count: len,
            above: false,
            below: false,
        };
    }
    if sel < v - 1 {
        return Rows {
            first: 0,
            count: v - 1,
            above: false,
            below: true,
        };
    }
    let last_page = len - (v - 1);
    if sel >= last_page {
        return Rows {
            first: last_page,
            count: v - 1,
            above: true,
            below: false,
        };
    }
    // Notes at both ends; the selection sits on the last module row.
    Rows {
        first: sel + 1 - (v - 2),
        count: v - 2,
        above: true,
        below: true,
    }
}

/// The module drawn at screen row `y`, for mouse clicks. The note rows hold
/// no module.
pub fn module_at(w: i32, h: i32, len: usize, sel: usize, y: i32) -> Option<usize> {
    let l = layout(w, h);
    if y < 7 || y > l.last {
        return None;
    }
    let r = rows(h, len, sel);
    let slot = (y - 7) as usize;
    let slot = if r.above { slot.checked_sub(1)? } else { slot };
    (slot < r.count).then_some(r.first + slot)
}

pub fn draw(c: &mut Canvas, v: &View) {
    let snap = v.snap;
    chrome(c, 1, &v.clock(), &snap.host.data.label);
    let mods = &snap.modules.data;
    let l = layout(c.w, c.h);
    let (lw, last) = (l.lw, l.last);
    let sel = v.mod_sel.min(mods.len().saturating_sub(1));

    c.put(2, 3, "Modules", St::B);
    c.put(10, 3, &mods.len().to_string(), St::D);
    c.right(lw, 3, "/ to filter", St::F);
    c.put(4, 5, "Module", St::D);
    c.put(24, 5, "Health", St::D);
    c.right(39, 5, "CPU", St::D);
    c.right(48, 5, "Memory", St::D);
    c.right(58, 5, "Up", St::D);
    c.put(61, 5, "Build", St::D);
    if l.summary {
        c.put(SUMMARY_X, 5, "Summary", St::D);
    }
    c.hline(2, 6, lw - 2);

    let r = rows(c.h, mods.len(), sel);
    let mut y = 7;
    if r.above {
        c.put(2, y, &format!("↑ {} more", r.first), St::D);
        y += 1;
    }
    for (i, m) in mods.iter().enumerate().skip(r.first).take(r.count) {
        let (g, gst) = status_glyph(m.status);
        let ok = m.status == Status::Ok;
        c.put(2, y, g, gst);
        let x = c.put(4, y, &trunc(&m.id, 18), if ok { St::NONE } else { St::B });
        if m.pending_restart.is_some() {
            c.put(x + 1, y, "↻", St::A);
        }
        c.put(24, y, status_word(m.status), if ok { St::D } else { gst });
        let cpu_st = if m.cpu_percent >= 10.0 {
            St::NONE
        } else {
            St::D
        };
        c.right(39, y, &fmt_cpu(m.cpu_percent), cpu_st);
        let mem_st = if m.memory_mb >= 1024.0 {
            St::NONE
        } else {
            St::D
        };
        c.right(48, y, &fmt_mem(m.memory_mb), mem_st);
        c.right(58, y, &ago(v.age(m.started_at)), St::D);
        c.put(61, y, &trunc(&m.build, 10), St::D);
        if l.summary {
            let text = trunc(&m.summary, (lw - SUMMARY_X).max(0) as usize);
            c.put(SUMMARY_X, y, &text, if ok { St::D } else { gst });
        }
        if i == sel {
            c.paint(1, y, lw, St::bg(Bg::Sel));
        }
        y += 1;
    }
    if r.below {
        let hidden = mods.len() - r.first - r.count;
        c.put(2, y, &format!("↓ {hidden} more"), St::D);
    }

    // Below the table, when every module fits and there is room left:
    // today's process starts and exits, newest first.
    let ev_top = 7 + mods.len() as i32 + 2;
    if !r.above && !r.below && last - ev_top >= 4 {
        heading(c, 2, ev_top, lw - 2, "Starts and exits", "today");
        let today = v.civil(v.now).days;
        let events = snap
            .process_events
            .data
            .iter()
            .filter(|e| v.civil(e.at).days == today)
            .take((last - ev_top - 1) as usize);
        for (i, e) in events.enumerate() {
            let y = ev_top + 2 + i as i32;
            c.put(2, y, &v.civil(e.at).hms(), St::D);
            c.put(12, y, &pad_right(&e.module, 18), St::B);
            let st = match e.kind {
                ProcessEventKind::Exited => St::D,
                ProcessEventKind::Started => St::NONE,
            };
            c.put(31, y, &trunc(&e.what, (lw - 31).max(0) as usize), st);
        }
    }

    if l.pane && !mods.is_empty() {
        c.vline(lw + 2, 3, last - 2);
        let prev = c.clip_to(Clip {
            x0: 0,
            y0: 0,
            x1: c.w,
            y1: last + 1,
        });
        pane(c, v, sel, lw);
        c.set_clip(prev);
    }

    frame_end(
        c,
        &[
            ("↑↓", "select"),
            ("r", "restart"),
            ("s", "stop"),
            ("l", "logs"),
            ("e", "settings"),
        ],
        &v.updated(&snap.modules),
        St::D,
    );
}

/// The detail pane for the selected module.
fn pane(c: &mut Canvas, v: &View, sel: usize, lw: i32) {
    let m = &v.snap.modules.data[sel];
    let x = lw + 5;
    let dw = c.w - x - 2;
    let (g, gst) = status_glyph(m.status);
    c.put(x, 3, &m.id, St::B);
    c.right(x + dw, 3, &format!("{g} {}", status_word(m.status)), gst);
    let facts = format!(
        "pid {} · up {} · build {}",
        m.pid,
        ago(v.age(m.started_at)),
        m.build
    );
    c.put(x, 4, &facts, St::D);

    let mut y = 6;
    if let Some(pending) = &m.pending_restart {
        c.fill(x - 1, y, dw + 2, 1, ' ', St::bg(Bg::AccBg));
        c.put(x, y, "↻", St::A.on(Bg::AccBg));
        c.put(
            x + 2,
            y,
            &trunc(pending, (dw - 6).max(0) as usize),
            St::bg(Bg::AccBg),
        );
        c.right(x + dw, y, " r ", St::bg(Bg::Key).bold());
        y += 2;
    }

    let checked = format!("checked {} ago", ago(v.age(m.health_checked_at)));
    heading(c, x, y, dw, "Health", &checked);
    y += 2;
    c.put(x, y, &trunc(&m.summary, dw.max(0) as usize), St::NONE);
    y += 1;
    for metric in &m.metrics {
        c.put(x, y, &pad_right(&metric.label, 18), St::D);
        c.put(
            x + 18,
            y,
            &trunc(&metric.value, (dw - 18).max(0) as usize),
            St::NONE,
        );
        y += 1;
    }

    y += 1;
    heading(c, x, y, dw, "CPU and memory", "last 10 min");
    y += 2;
    let n = (dw - 8).max(0) as usize;
    if !m.cpu_history.is_empty() {
        let max = m.cpu_history.iter().copied().fold(f64::MIN, f64::max) * 1.1;
        let cpu = window(&m.cpu_history, v.spark_offset(), n);
        spark(c, x, y, cpu, Fg::Acc, Some(max), 0.0);
    }
    c.right(x + dw, y, &fmt_cpu(m.cpu_percent), St::NONE);
    y += 1;
    let mem = window(&m.memory_history, 0, n);
    if !mem.is_empty() {
        let max = mem.iter().copied().fold(f64::MIN, f64::max);
        let min = mem.iter().copied().fold(f64::MAX, f64::min);
        spark(c, x, y, mem, Fg::Dim, Some(max), min * 0.97);
    }
    c.right(x + dw, y, &fmt_mem(m.memory_mb), St::D);

    y += 2;
    heading(c, x, y, dw, "Log", "");
    let (dot, dot_st) = live_dot(v.tick);
    c.right(x + dw - 2, y, "live", St::D);
    c.put(x + dw - 1, y, dot, dot_st);
    y += 2;
    // A tall window shows more of the log: the newest lines that fit.
    for line in m.log.iter().rev() {
        if y >= c.h {
            break;
        }
        c.put(x, y, &v.civil(line.at).hm(), St::D);
        let (flag, flag_st) = match line.level {
            LogLevel::Warn => ("▲", St::W),
            LogLevel::Error => ("✕", St::E),
            LogLevel::Info => (" ", St::NONE),
        };
        c.put(x + 6, y, flag, flag_st);
        let msg_st = if line.level == LogLevel::Info {
            St::NONE
        } else {
            flag_st
        };
        c.put(
            x + 8,
            y,
            &trunc(&line.message, (dw - 8).max(0) as usize),
            msg_st,
        );
        y += 1;
    }
}
