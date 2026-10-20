//! Home: is everything fine, and what needs me?
//!
//! Three layouts. Below 120×36 the answer and the essentials; from 120
//! columns the full set of panels; from 150 columns the same panels with
//! more facts in them (CPU beside every module, account totals, this Mac's
//! memory, disk and load). Activity and agent lists run to the footer at any
//! height. A list with more entries than rows ends in a "+N more" note, so
//! nothing is dropped without saying so.

use crate::draw::{
    bar, chrome, frame_end, heading, live_dot, pad_left, pad_right, spark, status_glyph, trunc,
    use_fg, width, Canvas,
};
use crate::model::{ActivityEntry, Daemon, Status};
use crate::theme::{Bg, Fg, St};
use crate::time::{ago, ago_short, locale_number, thousands, to_fixed};

use super::{count, fmt_cpu, pct, spark_window, View};

const HINTS: [(&str, &str); 4] = [
    ("⏎", "open"),
    ("↑↓", "move"),
    (":", "commands"),
    ("?", "help"),
];

pub fn draw(c: &mut Canvas, v: &View) {
    match layout(c.w, c.h) {
        Layout::Small => draw_small(c, v),
        Layout::Regular => draw_regular(c, v),
        Layout::Large => draw_large(c, v),
    }
}

fn ok_count(v: &View) -> usize {
    v.snap
        .modules
        .data
        .iter()
        .filter(|m| m.status == Status::Ok)
        .count()
}

/// "23 ok · 2 degraded": the healthy count, then each other status present.
fn module_tally(v: &View) -> String {
    let mut out = format!("{} ok", ok_count(v));
    for status in [
        Status::Degraded,
        Status::Failed,
        Status::Starting,
        Status::Stopped,
        Status::Unknown,
    ] {
        let n = v
            .snap
            .modules
            .data
            .iter()
            .filter(|m| m.status == status)
            .count();
        if n > 0 {
            out += &format!(" · {n} {}", crate::draw::status_word(status));
        }
    }
    out
}

fn attention_line(n: usize) -> String {
    if n == 1 {
        "1 thing needs attention".to_string()
    } else {
        format!("{n} things need attention")
    }
}

/// The answer line at row 3. When something needs attention, returns the
/// column after the bold part so the caller can add counts; otherwise None.
fn answer(c: &mut Canvas, v: &View) -> Option<i32> {
    let n = v.snap.attention().len();
    if n == 0 {
        c.put(2, 3, "●", St::O);
        c.put(4, 3, "Everything is running.", St::B);
        return None;
    }
    c.put(2, 3, "▲", St::W.bold());
    Some(c.put(4, 3, &attention_line(n), St::B))
}

fn daemon_line(v: &View) -> (String, String) {
    match &v.snap.daemon.data {
        Daemon::Running(d) => (
            format!("{} · up {}", d.version, ago(v.age(d.started_at))),
            format!("{} connected", d.clients),
        ),
        Daemon::Down(_) => ("not running".to_string(), "unknown".to_string()),
    }
}

fn approvals_text(v: &View) -> (String, St) {
    match v.snap.approvals.data.waiting {
        0 => ("none waiting".to_string(), St::D),
        n => (format!("{n} waiting"), St::W),
    }
}

fn backups(c: &mut Canvas, v: &View, x: i32, y: i32) {
    let b = &v.snap.backups.data;
    let (g, st) = if b.healthy {
        status_glyph(Status::Ok)
    } else {
        status_glyph(Status::Degraded)
    };
    c.put(x, y, "Backups", St::D);
    c.put(x + 13, y, g, st);
    c.put(
        x + 15,
        y,
        &format!("last upload {} ago", ago_short(v.age(b.last_upload_at))),
        St::NONE,
    );
    if b.uploading {
        c.put(x + 13, y + 1, "uploading now", St::D);
    }
}

/// Paired devices in one row, as many as fit before `end`.
fn devices(c: &mut Canvas, v: &View, x: i32, y: i32, end: i32) {
    c.put(x, y, "Devices", St::D);
    let mut dx = x + 13;
    for d in &v.snap.devices.data.list {
        let need = 2 + width(&d.short_name) as i32;
        if dx + need > end {
            break;
        }
        let (g, st) = status_glyph(d.status);
        c.put(dx, y, g, st);
        let name_st = if d.status == Status::Ok {
            St::NONE
        } else {
            St::D
        };
        dx = c.put(dx + 2, y, &d.short_name, name_st) + 2;
    }
}

enum Row<'a> {
    Entry(&'a ActivityEntry),
    Day(String),
}

/// Activity newest first, with a divider where the day changes.
fn activity_rows<'a>(v: &View<'a>) -> Vec<Row<'a>> {
    let today = v.civil(v.now).days;
    let mut rows = Vec::new();
    let mut day = today;
    for a in &v.snap.activity.data {
        let d = v.civil(a.at).days;
        if d != day {
            day = d;
            let label = if today - d == 1 {
                "Yesterday".to_string()
            } else {
                let c = v.civil(a.at);
                format!("{:04}-{:02}-{:02}", c.year, c.month, c.day)
            };
            rows.push(Row::Day(label));
        }
        rows.push(Row::Entry(a));
    }
    rows
}

fn live(c: &mut Canvas, v: &View, x: i32, y: i32, w: i32) {
    let (g, st) = live_dot(v.tick);
    c.right(x + w - 5, y, "live", St::D);
    c.put(x + w - 2, y, g, st);
}

fn agent_glyph(working: bool) -> (&'static str, St) {
    if working {
        ("●", St::A)
    } else {
        ("○", St::F)
    }
}

/// Which of Home's three layouts a window gets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Layout {
    Small,
    Regular,
    Large,
}

fn layout(w: i32, h: i32) -> Layout {
    if w < 120 || h < 36 {
        Layout::Small
    } else if w < 150 {
        Layout::Regular
    } else {
        Layout::Large
    }
}

/// How many rows the attention list has at a window size, counting the
/// "+N more" row that replaces the last one when there are more problems.
/// Selection moves over these rows; the "+N more" row opens the first
/// problem it stands for.
pub fn attention_rows(w: i32, h: i32) -> usize {
    match layout(w, h) {
        Layout::Small => 2,
        Layout::Regular | Layout::Large => 3,
    }
}

/// The attention list from row 5. When the problems outnumber the rows, the
/// last row says how many are left out instead of dropping them silently.
fn attention_list(c: &mut Canvas, v: &View, lay: Layout) {
    let att = v.snap.attention();
    let rows = attention_rows(c.w, c.h);
    let overflow = att.len() > rows;
    let shown = if overflow { rows - 1 } else { att.len() };
    let sel = v.home_sel.min(rows.min(att.len()).saturating_sub(1));
    let mark = |c: &mut Canvas, y: i32| {
        if lay != Layout::Small {
            c.right(c.w - 3, y, "⏎", St::A);
        }
        c.paint(2, y, c.w - 4, St::bg(Bg::Sel));
    };
    for (i, m) in att.iter().take(shown).enumerate() {
        let y = 5 + i as i32;
        let (g, st) = status_glyph(m.status);
        c.put(3, y, g, st);
        match lay {
            Layout::Small => {
                c.put(5, y, &pad_right(&m.id, 17), St::B);
                c.put(
                    22,
                    y,
                    &trunc(&m.summary, (c.w - 24).max(0) as usize),
                    St::NONE,
                );
            }
            Layout::Regular => {
                c.put(5, y, &pad_right(&m.id, 18), St::B);
                c.put(24, y, &trunc(&m.summary, 70), St::NONE);
                if let Some(t) = m.status_since {
                    c.right(c.w - 10, y, &format!("since {}", v.civil(t).hm()), St::D);
                }
            }
            Layout::Large => {
                let r = c.w - 2;
                c.put(5, y, &pad_right(&m.id, 18), St::B);
                c.put(
                    24,
                    y,
                    &trunc(&m.summary, (r - 24 - 30).max(0) as usize),
                    St::NONE,
                );
                if let Some(t) = m.status_since {
                    let text = format!("since {} · {}", v.civil(t).hm(), ago(v.age(t)));
                    c.right(r - 4, y, &text, St::D);
                }
            }
        }
        if i == sel {
            mark(c, y);
        }
    }
    if overflow {
        let y = 5 + shown as i32;
        let more = format!("+{} more · ⏎ opens Modules", att.len() - shown);
        c.put(3, y, &more, St::D);
        if sel == shown {
            mark(c, y);
        }
    }
}

/// The module grid: two columns of 13 from row 10. A 27th module or more
/// turns the last slot into "+N more".
fn module_grid(c: &mut Canvas, v: &View, x0: i32, col_w: i32, with_cpu: bool) {
    const SLOTS: usize = 26;
    let mods = &v.snap.modules.data;
    let overflow = mods.len() > SLOTS;
    let shown = if overflow { SLOTS - 1 } else { mods.len() };
    let slot = |i: usize| (x0 + (i as i32 / 13) * col_w, 10 + (i as i32 % 13));
    for (i, m) in mods.iter().take(shown).enumerate() {
        let (x, y) = slot(i);
        let (g, st) = status_glyph(m.status);
        c.put(x, y, g, st);
        let name_st = if m.status == Status::Ok {
            St::NONE
        } else {
            St::B
        };
        c.put(x + 2, y, &trunc(&m.id, 18), name_st);
        if with_cpu {
            let cpu_st = if m.cpu_percent >= 10.0 {
                St::NONE
            } else {
                St::F
            };
            c.right(x + 27, y, &fmt_cpu(m.cpu_percent), cpu_st);
        }
    }
    if overflow {
        let (x, y) = slot(shown);
        c.put(x, y, &format!("+{} more", mods.len() - shown), St::D);
    }
}

/// The line that replaces the providers that didn't fit, e.g. "+ 2 more
/// providers on 4 Usage", pointing at the tab that lists them all.
fn more_providers(n: usize) -> String {
    format!(
        "+ {} on 4 Usage",
        count(n, "more provider", "more providers")
    )
}

/// Providers two rows each from row 10, at most `max` of them; past that the
/// last slot says how many more there are. `totals` adds each provider's
/// accounts used and puts the reset on the right.
fn usage_list(
    c: &mut Canvas,
    v: &View,
    (bx, bw, bar_w): (i32, i32, i32),
    max: usize,
    totals: bool,
) {
    let providers = &v.snap.usage.data.providers;
    let overflow = providers.len() > max;
    let shown = if overflow { max - 1 } else { providers.len() };
    for (i, q) in providers.iter().take(shown).enumerate() {
        let y = 10 + i as i32 * 2;
        let f = q.used / f64::from(q.of.max(1));
        c.put(bx, y, &pad_right(&q.name, 12), St::NONE);
        bar(c, bx + 12, y, bar_w, f, use_fg(f));
        let pst = if f >= 0.85 { St::fg(use_fg(f)) } else { St::D };
        c.right(bx + bw, y, &pct(f), pst);
        let reset = format!("resets in {}", ago(-v.age(q.resets_at)));
        if totals {
            let total = if q.of == 1 {
                format!("{} {}", q.of, q.unit)
            } else {
                format!("{} of {} {}", to_fixed(q.used, 1), q.of, q.unit)
            };
            c.put(bx + 12, y + 1, &total, St::D);
            c.right(bx + bw, y + 1, &reset, St::D);
        } else {
            c.put(bx + 12, y + 1, &trunc(&reset, (bw - 12) as usize), St::D);
        }
    }
    if overflow {
        let y = 10 + shown as i32 * 2;
        c.put(bx, y, &more_providers(providers.len() - shown), St::F);
    }
}

/// Activity rows from `top` down to `last`: time, subject, what happened
/// (cut to `what_w`), optionally its kind, and who did it at the right edge
/// `x + dw`.
fn activity_list(
    c: &mut Canvas,
    v: &View,
    (x, top, dw, last): (i32, i32, i32, i32),
    what_w: i32,
    with_kind: bool,
) {
    for (y, row) in (top..=last).zip(activity_rows(v)) {
        match row {
            Row::Day(label) => {
                c.put(x, y, &label, St::F);
            }
            Row::Entry(a) => {
                c.put(x, y, &v.civil(a.at).hm(), St::D);
                c.put(x + 7, y, &pad_right(&a.subject, 17), St::B);
                c.put(x + 25, y, &trunc(&a.what, what_w.max(0) as usize), St::NONE);
                if with_kind {
                    c.put(x + dw - 18, y, &a.kind, St::F);
                }
                c.right(x + dw, y, &a.who, St::D);
            }
        }
    }
}

/// Agents from `top` down to `last`. With `project`, a dim project column
/// follows each name.
fn agent_list(c: &mut Canvas, v: &View, (cx, top, cw, last): (i32, i32, i32, i32), project: bool) {
    let rows = (last - top + 1).max(0) as usize;
    for (i, a) in v.snap.agents.data.list.iter().take(rows).enumerate() {
        let y = top + i as i32;
        let working = a.working();
        let (g, st) = agent_glyph(working);
        c.put(cx, y, g, st);
        let name_st = if working { St::B } else { St::NONE };
        if project {
            c.put(cx + 2, y, &pad_right(&a.name, 9), name_st);
            c.put(cx + 12, y, &pad_right(&a.project, 16), St::D);
        } else {
            c.put(cx + 2, y, &pad_right(&a.name, 12), name_st);
        }
        let state_st = if working { St::NONE } else { St::D };
        c.right(cx + cw, y, &a.state, state_st);
    }
}

// ---- 120 columns -------------------------------------------------------------

fn draw_regular(c: &mut Canvas, v: &View) {
    let snap = v.snap;
    chrome(c, 0, &v.clock(), &snap.host.data.label);
    let total = snap.modules.data.len();
    let last = c.h - 4;

    if let Some(x) = answer(c, v) {
        c.put(
            x,
            3,
            &format!(
                "   ·   {} of {total} modules healthy   ·   {} agents working",
                ok_count(v),
                snap.agents.data.working
            ),
            St::D,
        );
    }
    attention_list(c, v, Layout::Regular);

    // Modules
    let (ax, aw, bx, bw, cx) = (2, 40, 45, 37, 85);
    let cw = c.w - 2 - cx;
    heading(c, ax, 8, aw, "Modules", &module_tally(v));
    module_grid(c, v, ax, 20, false);

    // Usage
    heading(c, bx, 8, bw, "Usage", "this week");
    usage_list(c, v, (bx, bw, 18), 6, false);

    // This Mac
    let host = &snap.host.data;
    heading(c, cx, 8, cw, "This Mac", &short_id(&host.machine_id));
    let (daemon, clients) = daemon_line(v);
    let l = |c: &mut Canvas, y: i32, k: &str, val: &str, st: St| {
        c.put(cx, y, k, St::D);
        c.put(cx + 13, y, val, st);
    };
    l(c, 10, "Daemon", &daemon, St::NONE);
    l(c, 11, "Clients", &clients, St::NONE);
    let bs = &snap.build_server.data;
    l(
        c,
        13,
        "Build server",
        &format!("{} of {} busy", bs.busy, bs.slots),
        St::NONE,
    );
    c.put(
        cx + 13,
        14,
        &format!("{} queued · wait {}", bs.queued, ago(bs.wait_secs)),
        St::D,
    );
    spark(
        c,
        cx + 13,
        15,
        spark_window(&bs.load_history, v.spark_offset(), 16),
        Fg::Acc,
        Some(100.0),
        0.0,
    );
    c.put(cx + 30, 15, "1h", St::D);
    backups(c, v, cx, 17);
    devices(c, v, cx, 20, cx + cw);
    let (appr, appr_st) = approvals_text(v);
    l(c, 21, "Approvals", &appr, appr_st);

    // Activity
    let dw = 80;
    heading(c, ax, 24, dw, "Activity", "");
    live(c, v, ax, 24, dw);
    activity_list(c, v, (ax, 26, dw, last), 46, false);

    // Agents, with each one's project once the panel has room for it.
    let agents = &snap.agents.data;
    let meta = format!("{} working", agents.working);
    heading(c, cx, 24, cw, "Agents", &meta);
    agent_list(c, v, (cx, 26, cw, last), cw >= 44);

    frame_end(c, &HINTS, &v.updated(&snap.modules), St::D);
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

// ---- 150 columns and up ------------------------------------------------------

/// The widest the large layout's Usage panel gets. Past it the bars would
/// stretch without saying more; This Mac takes the extra width instead.
const USAGE_MAX_W: i32 = 64;

fn draw_large(c: &mut Canvas, v: &View) {
    let snap = v.snap;
    chrome(c, 0, &v.clock(), &snap.host.data.label);
    let total = snap.modules.data.len();
    let r = c.w - 2; // right edge of content

    if let Some(x) = answer(c, v) {
        c.put(
            x,
            3,
            &format!(
                "   ·   {} of {total} modules healthy   ·   {} agents working   ·   {}",
                ok_count(v),
                snap.agents.data.working,
                count(
                    snap.devices.data.online as usize,
                    "device online",
                    "devices online"
                ),
            ),
            St::D,
        );
    }
    attention_list(c, v, Layout::Large);

    // Three panels: modules with CPU, usage with totals, this Mac with host
    // facts. This Mac is at least 46 wide and takes whatever Usage leaves.
    let (ax, aw) = (2, 56);
    let bx = ax + aw + 3;
    let bw = (r - 46 - 3 - bx).min(USAGE_MAX_W);
    let cx = bx + bw + 3;
    let cw = r - cx;
    heading(
        c,
        ax,
        8,
        aw,
        "Modules",
        &format!("{} · CPU now", module_tally(v)),
    );
    module_grid(c, v, ax, 29, true);

    heading(c, bx, 8, bw, "Usage", "this week");
    usage_list(c, v, (bx, bw, bw - 18), 5, true);
    let usage = &snap.usage.data;
    c.put(bx, 21, "Models", St::D);
    let models = format!(
        "{} priced · checked {} ago",
        thousands(usage.models_priced),
        ago(v.age(usage.models_checked_at))
    );
    c.put(
        bx + 12,
        21,
        &trunc(&models, (bw - 12).max(0) as usize),
        St::NONE,
    );

    let host = &snap.host.data;
    heading(c, cx, 8, cw, "This Mac", &short_id(&host.machine_id));
    let (daemon, clients) = daemon_line(v);
    let l = |c: &mut Canvas, y: i32, k: &str, val: &str, st: St| {
        c.put(cx, y, k, St::D);
        c.put(cx + 13, y, val, st);
    };
    l(c, 10, "Daemon", &daemon, St::NONE);
    l(c, 11, "Clients", &clients, St::NONE);
    let bs = &snap.build_server.data;
    l(
        c,
        13,
        "Build server",
        &format!("{} of {} busy · {} queued", bs.busy, bs.slots, bs.queued),
        St::NONE,
    );
    // The sparkline takes the panel's width, less its span label and the
    // queue wait after it, and never more samples than the history holds.
    let wait = format!("wait {}", ago(bs.wait_secs));
    let n = ((cw - 13 - 6 - width(&wait) as i32).max(0) as usize).min(bs.load_history.len());
    spark(
        c,
        cx + 13,
        14,
        spark_window(&bs.load_history, v.spark_offset(), n),
        Fg::Acc,
        Some(100.0),
        0.0,
    );
    let end = cx + 13 + n as i32;
    c.put(end + 1, 14, "3h", St::D);
    c.put(end + 6, 14, &wait, St::D);
    backups(c, v, cx, 16);
    devices(c, v, cx, 18, cx + cw);
    let (appr, appr_st) = approvals_text(v);
    l(c, 19, "Approvals", &appr, appr_st);
    let gauge = |c: &mut Canvas, y: i32, k: &str, used: f64, total: f64| {
        c.put(cx, y, k, St::D);
        let f = if total > 0.0 { used / total } else { 0.0 };
        bar(c, cx + 13, y, 16, f, use_fg(f));
        let text = format!("{} of {} GB", locale_number(used), locale_number(total));
        c.put(cx + 31, y, &text, St::NONE);
    };
    gauge(c, 20, "Memory", host.memory_used_gb, host.memory_total_gb);
    gauge(c, 21, "Disk", host.disk_used_gb, host.disk_total_gb);
    let loads = host
        .load
        .iter()
        .map(|l| to_fixed(*l, 1))
        .collect::<Vec<_>>()
        .join(" · ");
    l(c, 22, "Load", &loads, St::NONE);
    c.put(
        cx + 13 + width(&loads) as i32 + 3,
        22,
        "1, 5 and 15 min",
        St::F,
    );

    // Activity fills the height; older entries follow a day divider.
    let (top, last) = (25, c.h - 4);
    let dw = cx - 3 - ax;
    heading(c, ax, top, dw, "Activity", "");
    live(c, v, ax, top, dw);
    activity_list(c, v, (ax, top + 2, dw, last), dw - 25 - 20, true);

    let agents = &snap.agents.data;
    let meta = format!("{} working", agents.working);
    heading(c, cx, top, cw, "Agents", &meta);
    // The agents leave a blank row and then the summary line above the
    // footer.
    let room = (last - top - 3).max(0) as usize;
    let shown = agents.list.len().min(room);
    agent_list(c, v, (cx, top + 2, cw, top + 1 + shown as i32), true);
    let summary = format!(
        "{} · {} · {}",
        count(agents.machines as usize, "machine", "machines"),
        count(agents.workspaces as usize, "workspace", "workspaces"),
        count(agents.projects as usize, "project", "projects"),
    );
    c.put(cx, top + 3 + shown as i32, &summary, St::F);

    frame_end(c, &HINTS, &v.updated(&snap.modules), St::D);
}

// ---- below 120×36 ------------------------------------------------------------

fn draw_small(c: &mut Canvas, v: &View) {
    let snap = v.snap;
    chrome(c, 0, &v.clock(), &snap.host.data.label);
    let total = snap.modules.data.len();
    let last = c.h - 4;

    if let Some(x) = answer(c, v) {
        c.put(
            x,
            3,
            &format!(" · {} of {total} healthy", ok_count(v)),
            St::D,
        );
    }
    attention_list(c, v, Layout::Small);

    // Modules as a row of status glyphs. When they don't all fit, the row
    // ends in "+N more".
    heading(c, 2, 8, c.w - 4, "Modules", &module_tally(v));
    let mods = &snap.modules.data;
    let room = (c.w - 4).max(0) as usize / 2;
    let shown = if mods.len() <= room {
        mods.len()
    } else {
        let note = width(&format!("+{} more", mods.len())) as i32;
        ((c.w - 4 - note).max(0) / 2) as usize
    };
    let mut x = 2;
    for m in mods.iter().take(shown) {
        let (g, st) = status_glyph(m.status);
        c.put(x, 10, g, st);
        x += 2;
    }
    if shown < mods.len() {
        c.put(x, 10, &format!("+{} more", mods.len() - shown), St::D);
    } else {
        let hint = "⏎ to list them";
        if x + 2 + width(hint) as i32 <= c.w - 2 {
            c.put(x + 2, 10, hint, St::F);
        }
    }

    // Usage gets rows 14 to h - 7. That leaves a blank row and at least two
    // rows for recent activity between it and the footer.
    heading(c, 2, 12, c.w - 4, "Usage", "this week");
    let providers = &snap.usage.data.providers;
    let fit = (c.h - 20).max(0) as usize;
    let overflow = providers.len() > fit;
    let shown = if overflow {
        fit.saturating_sub(1)
    } else {
        providers.len()
    };
    for (i, q) in providers.iter().take(shown).enumerate() {
        let y = 14 + i as i32;
        let f = q.used / f64::from(q.of.max(1));
        c.put(2, y, &pad_right(&q.name, 12), St::NONE);
        bar(c, 14, y, 30, f, use_fg(f));
        let pst = if f >= 0.85 { St::fg(use_fg(f)) } else { St::D };
        c.put(46, y, &pad_left(&pct(f), 4), pst);
        c.put(
            52,
            y,
            &format!("resets in {}", ago(-v.age(q.resets_at))),
            St::D,
        );
    }
    let mut usage_end = 13 + shown as i32;
    if overflow {
        usage_end += 1;
        c.put(
            2,
            usage_end,
            &more_providers(providers.len() - shown),
            St::F,
        );
    }

    // Below a blank row: an Activity section when it gets at least one row
    // under its heading, else just the latest entry.
    let top = usage_end + 2;
    if last - top + 1 >= 3 {
        let dw = c.w - 4;
        heading(c, 2, top, dw, "Activity", "");
        live(c, v, 2, top, dw);
        activity_list(c, v, (2, top + 2, dw, last), dw - 34, false);
    } else {
        c.put(2, top, "Latest", St::D);
        if let Some(a) = snap.activity.data.first() {
            let text = format!("{}  {} {}", v.civil(a.at).hm(), a.subject, a.what);
            c.put(10, top, &trunc(&text, (c.w - 12).max(0) as usize), St::NONE);
        }
    }

    frame_end(
        c,
        &[("⏎", "open"), (":", "commands"), ("?", "help")],
        "live",
        St::D,
    );
}
