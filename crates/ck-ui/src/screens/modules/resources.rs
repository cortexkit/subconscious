//! The tree-accounting table. Legacy daemon frames stay in the parent module.

use super::*;
use crate::model::{ResourceRow, ResourceRowKind, ResourceUsage, Snapshot};

pub fn resource_mode(snap: &Snapshot) -> bool {
    !snap.resource_rows.is_empty() || snap.modules.data.iter().any(|m| m.resources.is_some())
}

pub fn row_count(snap: &Snapshot) -> usize {
    snap.modules.data.len() + snap.resource_rows.len()
}

fn stack_rows(h: i32, len: usize, sel: usize) -> Rows {
    let capacity = (h - 12).max(2) as usize;
    if len <= capacity {
        return Rows {
            first: 0,
            count: len,
            above: false,
            below: false,
        };
    }
    if sel < capacity {
        return Rows {
            first: 0,
            count: capacity,
            above: false,
            below: true,
        };
    }
    let first = (sel + 2 - capacity).min(len - (capacity - 1));
    Rows {
        first,
        count: capacity - 1,
        above: true,
        below: first + capacity - 1 < len,
    }
}

/// Mouse rows include accounting rows, but never the pinned total or notes.
pub fn row_at(w: i32, h: i32, snap: &Snapshot, sel: usize, y: i32) -> Option<usize> {
    if !resource_mode(snap) {
        return module_at(w, h, snap.modules.data.len(), sel, y);
    }
    let r = stack_rows(
        h,
        row_count(snap),
        sel.min(row_count(snap).saturating_sub(1)),
    );
    let slot = usize::try_from(y - 7).ok()?;
    let slot = if r.above { slot.checked_sub(1)? } else { slot };
    (slot < r.count).then_some(r.first + slot)
}

pub(super) fn cpu_text(r: &ResourceUsage) -> String {
    r.cpu_percent
        .map_or_else(|| MISSING.into(), |cpu| bounded(fmt_cpu(cpu), r.lower))
}

fn bounded(text: String, lower: bool) -> String {
    if lower {
        format!("≥{text}")
    } else {
        text
    }
}

#[derive(Clone, Copy)]
struct HourFigure {
    cores: f64,
    lower: bool,
    short: bool,
}

fn hour(r: &ResourceUsage) -> Option<HourFigure> {
    let h = r.last_hour.as_ref()?;
    (h.covered_ms > 0).then(|| HourFigure {
        cores: h.cpu_ms as f64 / h.covered_ms as f64,
        lower: h.bound.is_some(),
        short: h.covered_ms < 3_600_000,
    })
}

fn figures(c: &mut Canvas, y: i32, r: &ResourceUsage, total: bool) {
    figures_with_hour(c, y, r, total, hour(r));
}

fn figures_with_hour(
    c: &mut Canvas,
    y: i32,
    r: &ResourceUsage,
    total: bool,
    reading: Option<HourFigure>,
) {
    let style = |dim| {
        let st = if dim { St::D } else { St::NONE };
        if total {
            st.bold()
        } else {
            st
        }
    };
    cell(
        c,
        39,
        y,
        6,
        &cpu_text(r),
        style(r.cpu_percent.is_none_or(|n| n < 10.0)),
    );
    let (cores, dim) = reading.map_or_else(
        || (MISSING.into(), true),
        |HourFigure {
             cores,
             lower,
             short,
         }| (bounded(format!("{cores:.2}"), lower), short || cores < 0.1),
    );
    cell(c, 49, y, 9, &cores, style(dim));
    cell(
        c,
        58,
        y,
        8,
        &r.memory_mb.map_or_else(|| MISSING.into(), fmt_mem),
        style(r.memory_mb.is_none_or(|n| n < 1024.0)),
    );
}

pub(super) fn last_hour(c: &mut Canvas, x: i32, y: i32, dw: i32, r: &ResourceUsage) {
    c.put(x, y, "Last hour", St::D);
    let (text, st) = hour(r).map_or_else(
        || ("not measured yet".into(), St::D),
        |HourFigure {
             cores: n,
             lower,
             short,
         }| {
            let text = if n < 0.005 && !lower {
                "under 0.01 cores on average".into()
            } else {
                format!("{} cores on average", bounded(format!("{n:.2}"), lower))
            };
            (text, if short { St::D } else { St::NONE })
        },
    );
    c.right(x + dw, y, &trunc(&text, (dw - 10).max(0) as usize), st);
}

fn unavailable(r: &ResourceUsage) -> &str {
    match r.unavailable.as_deref() {
        Some("not_yet_measured") | None => "not measured yet",
        Some("not_running") => "not running",
        Some(reason) => reason,
    }
}

pub(super) fn processes(c: &mut Canvas, x: i32, mut y: i32, dw: i32, r: &ResourceUsage) -> i32 {
    let count = r
        .process_count
        .map_or_else(String::new, |n| format!("{n} running"));
    heading(c, x, y, dw, "Processes", &count);
    y += 2;
    if r.process_count.is_none() {
        c.put(x, y, &trunc(unavailable(r), dw.max(0) as usize), St::D);
        return y + 1;
    }
    if let Some(own) = &r.own {
        process_row(c, x, y, dw, own);
        y += 1;
    }
    let mut groups: Vec<_> = r.groups.iter().collect();
    groups.sort_by(|a, b| {
        b.cpu_percent
            .partial_cmp(&a.cpu_percent)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.name.cmp(&b.name))
    });
    for g in groups.iter().take(4) {
        if y > c.h - 8 {
            break;
        }
        process_row(c, x, y, dw, g);
        y += 1;
    }
    if groups.is_empty() {
        let note = if r.own.is_some() && r.process_count == Some(1) {
            "No child processes"
        } else {
            "No program groups reported"
        };
        c.put(x, y, note, St::D);
        y += 1;
    }
    y
}

fn process_row(c: &mut Canvas, x: i32, y: i32, dw: i32, p: &crate::model::ProgramUsage) {
    let name = if p.count > 1 {
        format!("{} ×{}", p.name, p.count)
    } else {
        p.name.clone()
    };
    c.put(x, y, &trunc(&name, (dw - 17).max(0) as usize), St::NONE);
    let cpu = p
        .cpu_percent
        .map_or_else(|| MISSING.into(), |n| bounded(fmt_cpu(n), p.lower));
    c.right(
        x + dw - 9,
        y,
        &cpu,
        if p.cpu_percent.is_some_and(|n| n >= 10.0) {
            St::NONE
        } else {
            St::D
        },
    );
    c.right(x + dw, y, &fmt_mem(p.memory_mb), St::D);
}

fn total(snap: &Snapshot) -> (ResourceUsage, Option<HourFigure>) {
    let readings: Vec<_> = snap
        .modules
        .data
        .iter()
        .map(|m| m.resources.as_ref())
        .chain(snap.resource_rows.iter().map(|r| Some(&r.usage)))
        .collect();
    let complete = snap
        .resource_rows
        .iter()
        .any(|r| r.kind == ResourceRowKind::Daemon);
    let sum = |get: fn(&ResourceUsage) -> Option<f64>| {
        complete.then_some(())?;
        readings
            .iter()
            .try_fold(0.0, |sum, r| Some(sum + get((*r)?)?))
    };
    let cores = sum(|r| hour(r).map(|h| h.cores));
    let lower = readings.iter().flatten().any(|r| r.lower);
    let hour_lower = readings
        .iter()
        .flatten()
        .any(|r| hour(r).is_some_and(|h| h.lower));
    let short = readings
        .iter()
        .flatten()
        .any(|r| hour(r).is_some_and(|h| h.short));
    // Sum rates, not counters with different coverage denominators.
    (
        ResourceUsage {
            cpu_percent: sum(|r| r.cpu_percent),
            memory_mb: sum(|r| r.memory_mb),
            lower,
            ..ResourceUsage::default()
        },
        cores.map(|cores| HourFigure {
            cores,
            lower: hour_lower,
            short,
        }),
    )
}

pub(super) fn draw(c: &mut Canvas, v: &View) {
    let snap = v.snap;
    chrome(
        c,
        Tab::Modules,
        snap.visible_tabs(),
        &v.clock(),
        &snap.host.data.label,
    );
    let l = layout(c.w, c.h);
    let lw = l.lw;
    let wide = c.w >= 150;
    let count = row_count(snap);
    let sel = v.mod_sel.min(count.saturating_sub(1));
    c.put(2, 3, "Modules", St::B);
    c.put(10, 3, &snap.modules.data.len().to_string(), St::D);
    if snap.serves.list_filter.contains(Tab::Modules) {
        c.right(lw, 3, "/ to filter", St::F);
    }
    for (end, text) in [(39, "CPU"), (49, "Cores 1h"), (58, "Memory"), (67, "Up")] {
        c.right(end, 5, text, St::D);
    }
    c.put(4, 5, "Module", St::D);
    c.put(24, 5, "Health", St::D);
    if wide {
        c.put(71, 5, "Build", St::D);
        c.put(83, 5, "Summary", St::D);
    }
    c.hline(2, 6, lw - 2);
    let r = stack_rows(c.h, count, sel);
    let mut y = 7;
    if r.above {
        c.put(2, y, &format!("↑ {} more", r.first), St::D);
        y += 1;
    }
    for i in r.first..r.first + r.count {
        if let Some(m) = snap.modules.data.get(i) {
            let (g, gst) = status_glyph(m.status);
            let ok = m.status == Status::Ok;
            let (g, gst, word) = m.action.as_ref().map_or_else(
                || (g, gst, status_word(m.status).into()),
                |a| ("◐", St::A, action_word(a)),
            );
            c.put(2, y, g, gst);
            let x = c.put(4, y, &trunc(&m.id, 18), if ok { St::NONE } else { St::B });
            if m.pending_restart.is_some() {
                c.put(x + 1, y, "↻", St::A);
            }
            c.put(
                24,
                y,
                &trunc(&word, HEALTH_W),
                if ok && m.action.is_none() { St::D } else { gst },
            );
            figures(
                c,
                y,
                m.resources.as_ref().unwrap_or(&ResourceUsage::default()),
                false,
            );
            cell(
                c,
                67,
                y,
                UP_W,
                uptime(v, m).as_deref().unwrap_or(MISSING),
                St::D,
            );
            if wide {
                c.put(71, y, &trunc(&m.build, 10), St::D);
                c.put(
                    83,
                    y,
                    &trunc(&m.summary, (lw - 83).max(0) as usize),
                    if ok { St::D } else { gst },
                );
            }
        } else {
            let extra = &snap.resource_rows[i - snap.modules.data.len()];
            let (label, health) = match &extra.kind {
                ResourceRowKind::Daemon => ("subc daemon", "ok".into()),
                ResourceRowKind::Agents { via } => ("agents' commands", format!("via {via}")),
            };
            c.put(2, y, "●", St::O);
            c.put(4, y, label, St::D);
            c.put(24, y, &trunc(&health, HEALTH_W), St::D);
            figures(c, y, &extra.usage, false);
            if let Some(at) = extra.started_at {
                cell(c, 67, y, UP_W, &ago(v.age(at)), St::D);
            }
            if wide {
                c.put(71, y, &trunc(&extra.build, 10), St::D);
            }
        }
        if i == sel {
            c.paint(1, y, lw, St::bg(Bg::Sel));
        }
        y += 1;
    }
    let fits = !r.above && !r.below;
    let ty = if fits { y + 1 } else { l.last };
    c.hline(2, ty - 1, lw - 2);
    if r.below {
        c.right(
            lw,
            ty - 1,
            &format!(" ↓ {} more ", count - r.first - r.count),
            St::F,
        );
    }
    c.put(4, ty, "All of CortexKit", St::B);
    let (total, hour) = total(snap);
    figures_with_hour(c, ty, &total, true, hour);
    events(c, v, ty + 2, lw, l.last);
    if l.pane && count > 0 {
        c.vline(lw + 2, 3, l.last - 2);
        let prev = c.clip_to(Clip {
            x0: 0,
            y0: 0,
            x1: c.w,
            y1: l.last + 1,
        });
        if sel < snap.modules.data.len() {
            pane(c, v, sel, lw);
        } else {
            extra_pane(c, &snap.resource_rows[sel - snap.modules.data.len()], lw);
        }
        c.set_clip(prev);
    }
    let meta = v.meta(&snap.modules, "");
    let (status, st) = if meta.text.is_empty() {
        (v.updated(&snap.modules), St::D)
    } else {
        (meta.text, meta.st)
    };
    if meta.dim {
        c.dim_rect(1, 7, lw, l.last - 6);
    }
    let mut hints = vec![("↑↓", "select")];
    if let Some(m) = snap.modules.data.get(sel) {
        hints.extend([
            ("r", "restart"),
            (
                "s",
                if m.status == Status::Stopped {
                    "start"
                } else {
                    "stop"
                },
            ),
            ("l", "logs"),
        ]);
        if snap.serves.all_metrics {
            hints.push(("m", "all metrics"));
        }
        if snap.serves.settings {
            hints.push(("e", "settings"));
        }
    }
    frame_end(c, &hints, &status, st);
}

fn events(c: &mut Canvas, v: &View, top: i32, lw: i32, last: i32) {
    if last - top < 4 {
        return;
    }
    heading(c, 2, top, lw - 2, "Starts and exits", "today");
    let today = v.civil(v.now).days;
    let events: Vec<_> = v
        .snap
        .process_events
        .data
        .iter()
        .filter(|e| v.civil(e.at).days == today)
        .collect();
    if events.is_empty() {
        let note = if v.snap.process_events.loaded {
            "No starts or exits today"
        } else {
            v.snap.process_events.error.as_deref().unwrap_or(UNKNOWN)
        };
        c.put(2, top + 2, &trunc(note, (lw - 2).max(0) as usize), St::D);
    }
    for (i, e) in events
        .into_iter()
        .take((last - top - 1).max(0) as usize)
        .enumerate()
    {
        let y = top + 2 + i as i32;
        c.put(2, y, &v.civil(e.at).hms(), St::D);
        c.put(12, y, &pad_right(&e.module, 18), St::B);
        c.put(
            31,
            y,
            &trunc(&e.what, (lw - 31).max(0) as usize),
            if e.kind == ProcessEventKind::Exited {
                St::D
            } else {
                St::NONE
            },
        );
    }
}

fn extra_pane(c: &mut Canvas, r: &ResourceRow, lw: i32) {
    let x = lw + 5;
    let dw = c.w - x - 2;
    let label = match &r.kind {
        ResourceRowKind::Daemon => "subc daemon".into(),
        ResourceRowKind::Agents { via } => format!("agents' commands · via {via}"),
    };
    c.put(x, 3, &trunc(&label, dw.max(0) as usize), St::B);
    if !r.build.is_empty() {
        c.put(x, 4, &format!("build {}", r.build), St::D);
    }
    last_hour(c, x, 6, dw, &r.usage);
    processes(c, x, 8, dw, &r.usage);
}
