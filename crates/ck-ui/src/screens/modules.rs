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
    width, Canvas, Clip,
};
use crate::model::{LogLevel, Module, ProcessEventKind, Status};
use crate::theme::{Bg, Fg, St};
use crate::time::{ago, js_round};

use super::overlays::wrap;
use super::{fmt_cpu, fmt_mem, window, View, UNKNOWN};

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

/// The widths of the table's fixed columns. Health starts at column 24 and
/// has a space before CPU; CPU, Memory and Up end at columns 39, 48 and 58.
const HEALTH_W: usize = 9;
const CPU_W: usize = 5;
const MEM_W: usize = 8;
const UP_W: usize = 8;

/// What a numeric cell shows when there is no value.
pub const MISSING: &str = "–";

/// Right-aligns `text` to end at `end`, cut to `width` columns.
fn cell(c: &mut Canvas, end: i32, y: i32, width: usize, text: &str, st: St) {
    c.right(end, y, &trunc(text, width), st);
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
    chrome(c, 1, snap.serves.tabs, &v.clock(), &snap.host.data.label);
    let mods = &snap.modules.data;
    let l = layout(c.w, c.h);
    let (lw, last) = (l.lw, l.last);
    let sel = v.mod_sel.min(mods.len().saturating_sub(1));

    c.put(2, 3, "Modules", St::B);
    c.put(10, 3, &mods.len().to_string(), St::D);
    if snap.serves.list_filter {
        c.right(lw, 3, "/ to filter", St::F);
    }
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
        // An action in flight shows its phase where the health word goes.
        let (g, gst, word) = match &m.action {
            Some(a) => ("◐", St::A, action_word(a)),
            None => (g, gst, status_word(m.status).to_string()),
        };
        c.put(2, y, g, gst);
        let x = c.put(4, y, &trunc(&m.id, 18), if ok { St::NONE } else { St::B });
        if m.pending_restart.is_some() {
            c.put(x + 1, y, "↻", St::A);
        }
        // Every column is cut to its own width, so no value runs into the
        // next one; a missing number is a dim dash.
        let word_st = if m.action.is_some() || !ok {
            gst
        } else {
            St::D
        };
        c.put(24, y, &trunc(&word, HEALTH_W), word_st);
        match m.cpu_percent {
            Some(cpu) => {
                let cpu_st = if cpu >= 10.0 { St::NONE } else { St::D };
                cell(c, 39, y, CPU_W, &fmt_cpu(cpu), cpu_st);
            }
            None => cell(c, 39, y, CPU_W, MISSING, St::D),
        }
        match m.memory_mb {
            Some(mb) => {
                let mem_st = if mb >= 1024.0 { St::NONE } else { St::D };
                cell(c, 48, y, MEM_W, &fmt_mem(mb), mem_st);
            }
            None => cell(c, 48, y, MEM_W, MISSING, St::D),
        }
        let up = uptime(v, m);
        cell(c, 58, y, UP_W, up.as_deref().unwrap_or(MISSING), St::D);
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
        let todays: Vec<_> = snap
            .process_events
            .data
            .iter()
            .filter(|e| v.civil(e.at).days == today)
            .collect();
        // With nothing from today, the section says so (or why it can't
        // tell) instead of leaving a heading over a blank block.
        if todays.is_empty() {
            let note = if snap.process_events.loaded {
                "No starts or exits today"
            } else {
                snap.process_events.error.as_deref().unwrap_or(UNKNOWN)
            };
            c.put(2, ev_top + 2, &trunc(note, (lw - 2).max(0) as usize), St::D);
        }
        let room = (last - ev_top - 1).max(0) as usize;
        for (i, e) in todays.into_iter().take(room).enumerate() {
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

    // The table is one source; when it is aging or failing the footer says
    // so in amber, and past five refresh intervals the rows dim.
    let meta = v.meta(&snap.modules, "");
    let (status, status_st) = if meta.text.is_empty() {
        (v.updated(&snap.modules), St::D)
    } else {
        (meta.text.clone(), meta.st)
    };
    if meta.dim {
        c.dim_rect(1, 7, lw, last - 6);
    }
    let selected = mods.get(sel);
    let s_label = match selected.map(|m| m.status) {
        Some(Status::Stopped) => "start",
        _ => "stop",
    };
    // A key is offered only for what exists: no Settings screen, no `e`.
    let mut hints = vec![
        ("↑↓", "select"),
        ("r", "restart"),
        ("s", s_label),
        ("l", "logs"),
    ];
    if snap.serves.all_metrics {
        hints.push(("m", "all metrics"));
    }
    if snap.serves.settings {
        hints.push(("e", "settings"));
    }
    frame_end(c, &hints, &status, status_st);
}

/// An action's phase in the table's health column, which is nine columns
/// wide.
fn action_word(a: &crate::model::ModuleAction) -> String {
    use crate::model::ActionPhase;
    match &a.phase {
        ActionPhase::Asking => a.kind.verb().to_string(),
        ActionPhase::Draining => "draining".into(),
        ActionPhase::Stopped => "stopped".into(),
        ActionPhase::Starting { .. } => "starting".into(),
    }
}

/// How long the module's process has been up, if known.
fn uptime(v: &View, m: &Module) -> Option<String> {
    m.started_at.map(|t| ago(v.age(t)))
}

/// The detail pane for the selected module.
fn pane(c: &mut Canvas, v: &View, sel: usize, lw: i32) {
    let m = &v.snap.modules.data[sel];
    let x = lw + 5;
    let dw = c.w - x - 2;
    let (g, gst) = status_glyph(m.status);
    c.put(x, 3, &m.id, St::B);
    c.right(x + dw, 3, &format!("{g} {}", status_word(m.status)), gst);
    let pid = m.pid.map_or_else(|| UNKNOWN.to_string(), |p| p.to_string());
    let up = match (uptime(v, m), m.status) {
        (Some(up), _) => up,
        (None, Status::Stopped) => "not running".into(),
        (None, _) => UNKNOWN.into(),
    };
    let facts = format!("pid {pid} · up {up} · build {}", m.build);
    c.put(x, 4, &trunc(&facts, dw.max(0) as usize), St::D);
    if let Some(a) = &m.action {
        let line = format!(
            "{} · {} · {}",
            a.kind.verb(),
            a.phase_text(),
            ago(v.age(a.started_at))
        );
        c.put(x, 5, "◐", St::A);
        c.put(x + 2, 5, &trunc(&line, (dw - 2).max(0) as usize), St::A);
    }

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

    let checked = m.health_checked_at.map_or_else(
        || "never checked".to_string(),
        |t| format!("checked {} ago", ago(v.age(t))),
    );
    heading(c, x, y, dw, "Health", &checked);
    y += 2;
    // The CPU and memory chart and the log always keep their rows: a blank,
    // a heading and two sparklines, a blank, a heading and at least two log
    // lines. The module's sentence and metrics share what is left.
    let last = c.h - 4;
    let keep = 10;
    let mut room = (last + 1 - y - keep).max(1);
    // The module's own sentence, in full and wrapped, as far as room allows.
    for line in wrap(&m.summary, dw.max(1) as usize) {
        if room == 0 {
            break;
        }
        c.put(x, y, &line, St::NONE);
        y += 1;
        room -= 1;
    }
    if m.summary.is_empty() && m.metrics.is_empty() {
        // A heading never stands over nothing.
        c.put(x, y, "No details reported", St::D);
        y += 1;
        room -= 1;
    } else if m.summary.is_empty() {
        y += 1;
    }
    // The metrics the module names as its headline: labels in a column wide
    // enough for the longest, at least 18 and leaving the value at least 10
    // columns (a longer label is cut), a gap, then the value.
    let col = m
        .metrics
        .iter()
        .map(|x| width(&x.label) as i32 + 2)
        .max()
        .unwrap_or(0)
        .clamp(18, (dw - 10).max(18));
    let fits = (room.max(0) as usize).min(m.metrics.len());
    let shown = if fits < m.metrics.len() {
        fits.saturating_sub(1)
    } else {
        fits
    };
    for metric in m.metrics.iter().take(shown) {
        c.put(
            x,
            y,
            &pad_right(&trunc(&metric.label, (col - 2) as usize), col as usize),
            St::D,
        );
        c.put(
            x + col,
            y,
            &trunc(&metric.value, (dw - col).max(0) as usize),
            St::NONE,
        );
        y += 1;
    }
    if shown < m.metrics.len() {
        let more = m.metrics.len() - shown;
        let note = if v.snap.serves.all_metrics {
            format!("+{more} more · m all metrics")
        } else {
            format!("+{more} more")
        };
        c.put(x, y, &note, St::D);
        y += 1;
    }

    y += 1;
    heading(c, x, y, dw, "CPU and memory", "last 10 min");
    y += 2;
    let n = (dw - 8).max(0) as usize;
    if m.cpu_history.iter().any(|c| c.is_finite()) {
        let max = m.cpu_history.iter().copied().fold(f64::MIN, f64::max) * 1.1;
        let cpu = window(&m.cpu_history, v.spark_offset(), n);
        spark(c, x, y, cpu, Fg::Acc, Some(max), 0.0);
    }
    c.right(
        x + dw,
        y,
        &m.cpu_percent.map_or_else(|| UNKNOWN.to_string(), fmt_cpu),
        St::NONE,
    );
    y += 1;
    let mem = window(&m.memory_history, 0, n);
    if !mem.is_empty() {
        let max = mem.iter().copied().fold(f64::MIN, f64::max);
        let min = mem.iter().copied().fold(f64::MAX, f64::min);
        spark(c, x, y, mem, Fg::Dim, Some(max), min * 0.97);
    }
    c.right(
        x + dw,
        y,
        &m.memory_mb.map_or_else(|| UNKNOWN.to_string(), fmt_mem),
        St::D,
    );

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
        if let Some(at) = line.at {
            c.put(x, y, &v.civil(at).hm(), St::D);
        }
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
