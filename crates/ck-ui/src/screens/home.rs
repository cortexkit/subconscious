//! Home: is everything fine, and what needs me?
//!
//! Three layouts. Below 120×36 the answer and the essentials; from 120
//! columns the full set of panels; from 150 columns the same panels with
//! more facts in them (CPU beside every module, account totals, this Mac's
//! memory, disk and load). Activity and agent lists run to the footer at any
//! height. A list with more entries than rows ends in a "+N more" note, so
//! nothing is dropped without saying so.

use crate::draw::{
    bar, chrome, frame_end, heading_st, live_dot, pad_left, pad_right, spark, status_glyph, trunc,
    use_fg, width, Canvas,
};
use crate::model::{ActivityEntry, Agent, Daemon, Panel, Provider, Status};
use crate::theme::{Bg, Fg, St};
use crate::time::{ago, ago_short, js_round, locale_number, thousands, to_fixed};

use super::modules::MISSING;
use super::{count, fmt_cpu, pct, spark_window, Meta, View, UNKNOWN};

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
            format!(
                "{} · up {}",
                d.version,
                d.started_at
                    .map_or_else(|| UNKNOWN.to_string(), |t| ago(v.age(t)))
            ),
            d.clients
                .map_or_else(|| UNKNOWN.to_string(), |n| format!("{n} connected")),
        ),
        Daemon::Down(_) => ("not running".to_string(), UNKNOWN.to_string()),
    }
}

fn approvals_text(v: &View) -> (String, St) {
    match v.snap.approvals.data.waiting {
        None => (UNKNOWN.to_string(), St::D),
        Some(0) => ("none waiting".to_string(), St::D),
        Some(n) => (format!("{n} waiting"), St::W),
    }
}

/// An error in a value cell: a machine code (one word, no spaces) never
/// shows as the value; it reads "couldn't read {what}" instead. A sentence
/// such as "motor isn't installed" shows as it is.
pub fn error_words(err: &str, what: &str) -> String {
    if err.trim().contains(' ') {
        err.to_string()
    } else {
        format!("couldn't read {what}")
    }
}

/// An error as a value cell shows it: a machine code becomes a muted
/// "couldn't read {what}" (the code stays in the model for a detail view); a
/// sentence shows as it is, in amber.
fn error_cell(err: &str, what: &str) -> (String, St) {
    let text = error_words(err, what);
    let st = if text == err { St::W } else { St::D };
    (text, st)
}

/// One fact row of This Mac: its label, then the value, or the source's
/// error in amber while it is failing without a reading to show. A row
/// whose source is only a last good reading draws dimmed.
fn fact_row<T>(
    c: &mut Canvas,
    v: &View,
    (x, y, w): (i32, i32, i32),
    k: &str,
    panel: &Panel<T>,
    value: &str,
    st: St,
) {
    c.put(x, y, k, St::D);
    let room = (w - 13).max(0) as usize;
    match (&panel.error, panel.loaded) {
        (Some(err), false) => {
            let (text, st) = error_cell(err, &k.to_lowercase());
            c.put(x + 13, y, &trunc(&text, room), st);
        }
        _ => {
            c.put(x + 13, y, &trunc(value, room), st);
        }
    }
    if v.meta(panel, "").dim {
        c.dim_rect(x + 13, y, w - 13, 1);
    }
}

fn backups(c: &mut Canvas, v: &View, x: i32, y: i32, w: i32) {
    let panel = &v.snap.backups;
    let b = &panel.data;
    c.put(x, y, "Backups", St::D);
    if let (Some(err), false) = (&panel.error, panel.loaded) {
        let (text, st) = error_cell(err, "backups");
        c.put(x + 13, y, &trunc(&text, (w - 13).max(0) as usize), st);
        return;
    }
    let (g, st) = status_glyph(b.status);
    c.put(x + 13, y, g, st);
    let last = b.last_upload_at.map_or_else(
        || format!("last upload {UNKNOWN}"),
        |t| format!("last upload {} ago", ago_short(v.age(t))),
    );
    c.put(x + 15, y, &trunc(&last, (w - 15).max(0) as usize), St::NONE);
    if b.uploading {
        c.put(x + 13, y + 1, "uploading now", St::D);
    } else if let Some(detail) = &b.detail {
        c.put(
            x + 13,
            y + 1,
            &trunc(detail, (w - 13).max(0) as usize),
            St::D,
        );
    }
    if v.meta(panel, "").dim {
        c.dim_rect(x + 13, y, w - 13, 2);
    }
}

/// Draws a heading with the panel's meta, and returns whether the panel's
/// values should then be dimmed as a last good reading.
fn panel_heading<T>(
    c: &mut Canvas,
    v: &View,
    (x, y, w): (i32, i32, i32),
    title: &str,
    panel: &Panel<T>,
    normal: &str,
) -> bool {
    let meta = v.meta(panel, normal);
    heading_st(c, x, y, w, title, &meta.text, meta.st);
    meta.dim
}

/// Paired devices in one row, as many as fit before `end`.
fn devices(c: &mut Canvas, v: &View, x: i32, y: i32, end: i32) {
    c.put(x, y, "Devices", St::D);
    let panel = &v.snap.devices;
    if !panel.loaded {
        let (text, st) = match panel.error.as_deref() {
            Some(err) => error_cell(err, "devices"),
            None => (UNKNOWN.to_string(), St::D),
        };
        c.put(x + 13, y, &trunc(&text, (end - x - 13).max(0) as usize), st);
        return;
    }
    // Online devices first, so the row agrees with the online count; chips
    // two spaces apart; when they don't all fit, the last ones give way to a
    // dim "+N" one space after the last chip shown.
    let mut list: Vec<_> = panel.data.list.iter().collect();
    list.sort_by_key(|d| d.status != Status::Ok);
    let start = x + 13;
    let online = list.iter().filter(|d| d.status == Status::Ok).count();
    let (names, shown) = device_chips(
        &list
            .iter()
            .map(|d| d.short_name.as_str())
            .collect::<Vec<_>>(),
        online,
        end - start,
    );
    let mut dx = start;
    for (d, name) in list.iter().zip(&names).take(shown) {
        let (g, st) = status_glyph(d.status);
        c.put(dx, y, g, st);
        let name_st = if d.status == Status::Ok {
            St::NONE
        } else {
            St::D
        };
        dx = c.put(dx + 2, y, name, name_st) + 2;
    }
    if shown < list.len() {
        let at = if shown == 0 { start } else { dx - 1 };
        c.put(at, y, &format!("+{}", list.len() - shown), St::D);
    }
    if v.meta(panel, "").dim {
        c.dim_rect(x + 13, y, end - x - 13, 1);
    }
}

/// The names to draw and how many chips fit in `room` columns, for devices
/// listed online first (`online` of them). Each chip is a glyph, a space and
/// the name, two spaces from the next; chips that don't fit give way to "+N"
/// one space after the last one shown. When long names would keep an online
/// device out, every name is cut, shorter step by step (to no fewer than
/// nine columns, the ellipsis included), keeping the cut that shows the most
/// devices; in a row too narrow even then, some online devices stay out.
pub fn device_chips(names: &[&str], online: usize, room: i32) -> (Vec<String>, usize) {
    // How many chips of these names fit, leaving room for the "+N" note when
    // some are left out.
    let fit = |names: &[String]| -> usize {
        let mut ends = Vec::new();
        let mut at = 0;
        for n in names {
            let end = at + 2 + width(n) as i32;
            ends.push(end);
            at = end + 2;
        }
        if ends.last().is_none_or(|e| *e <= room) {
            return names.len();
        }
        (0..names.len())
            .rev()
            .find(|&k| {
                let after = if k == 0 { 0 } else { ends[k - 1] + 1 };
                after + width(&format!("+{}", names.len() - k)) as i32 <= room
            })
            .unwrap_or(0)
    };
    let full: Vec<String> = names.iter().map(|n| n.to_string()).collect();
    let longest = names.iter().map(|n| width(n)).max().unwrap_or(0);
    let mut best = (full.clone(), fit(&full));
    // Nine columns at the shortest, the ellipsis included.
    for cap in (9..longest).rev() {
        if best.1 >= online {
            break;
        }
        let cut: Vec<String> = names.iter().map(|n| trunc(n, cap)).collect();
        let shown = fit(&cut);
        if shown > best.1 {
            best = (cut, shown);
        }
    }
    best
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

fn agent_glyph(a: &Agent) -> (&'static str, St) {
    if a.working() {
        ("●", St::A)
    } else if !a.state_known() {
        ("◌", St::D)
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
    // Rows 5 and 6, at every size: row 7 stays blank so the panels below
    // never sit directly under the list.
    let _ = layout(w, h);
    2
}

/// What a problem row says: the module's own sentence, or, when it gave
/// none, where it is in its lifecycle ("Restarting") or its status, in
/// muted words.
fn problem_text(m: &crate::model::Module) -> (String, St) {
    if !m.summary.is_empty() {
        return (m.summary.clone(), St::NONE);
    }
    let words = match (&m.lifecycle, m.status) {
        (Some(word), _) => word.clone(),
        (None, Status::Unknown) => "Health unknown".to_string(),
        (None, status) => {
            let w = crate::draw::status_word(status);
            let mut chars = w.chars();
            chars
                .next()
                .map(|f| f.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    };
    (words, St::D)
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
        let (text, text_st) = problem_text(m);
        match lay {
            Layout::Small => {
                c.put(5, y, &pad_right(&m.id, 17), St::B);
                c.put(22, y, &trunc(&text, (c.w - 24).max(0) as usize), text_st);
            }
            Layout::Regular => {
                c.put(5, y, &pad_right(&m.id, 18), St::B);
                c.put(24, y, &trunc(&text, 70), text_st);
                if let Some(t) = m.status_since {
                    c.right(c.w - 10, y, &format!("since {}", v.civil(t).hm()), St::D);
                }
            }
            Layout::Large => {
                let r = c.w - 2;
                c.put(5, y, &pad_right(&m.id, 18), St::B);
                c.put(24, y, &trunc(&text, (r - 24 - 30).max(0) as usize), text_st);
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
        // The CPU cell is the five columns before x + 27, after a space; a
        // missing reading is a dim dash.
        if with_cpu {
            match m.cpu_percent {
                Some(cpu) => {
                    let cpu_st = if cpu >= 10.0 { St::NONE } else { St::F };
                    c.right(x + 27, y, &trunc(&fmt_cpu(cpu), 5), cpu_st);
                }
                None => {
                    c.right(x + 27, y, MISSING, St::D);
                }
            }
        }
    }
    if overflow {
        let (x, y) = slot(shown);
        c.put(x, y, &format!("+{} more", mods.len() - shown), St::D);
    }
}

/// The line that replaces the providers that didn't fit, e.g. "+ 2 more
/// providers on 4 Usage", pointing at the tab that lists them all when the
/// app has that tab, else just "+ 2 more providers".
fn more_providers(n: usize, usage_tab: bool) -> String {
    let more = count(n, "more provider", "more providers");
    if usage_tab {
        format!("+ {more} on 4 Usage")
    } else {
        format!("+ {more}")
    }
}

/// A provider's share used, or the text that stands in its place: an error
/// in amber (never a bar at 0%), what is left of a credit pool, or
/// "unknown".
fn provider_frac(q: &Provider) -> Result<f64, (String, St)> {
    match (q.used, &q.error, &q.balance) {
        (Some(used), None, _) => Ok(used / f64::from(q.of.max(1))),
        (_, Some(err), _) => Err(error_cell(err, "usage")),
        (None, None, Some(left)) => Err((left.clone(), St::NONE)),
        (None, None, None) => Err((UNKNOWN.to_string(), St::D)),
    }
}

/// A credit balance with its soonest expiry, " · expires Oct 25", when the
/// whole of it fits in `room` columns.
fn with_expiry(v: &View, q: &Provider, text: String, room: usize) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (Some(_), Some(t)) = (&q.balance, q.balance_expires) else {
        return text;
    };
    let d = v.civil(t);
    let month = MONTHS[(d.month as usize).clamp(1, 12) - 1];
    let full = format!("{text} · expires {month} {}", d.day);
    if width(&full) <= room {
        full
    } else {
        text
    }
}

/// The width of the provider name column: the longest name shown, plus a
/// space, between 12 and 17 columns (a name longer than 16 is cut).
fn name_col(providers: &[&Provider]) -> i32 {
    let longest = providers.iter().map(|q| width(&q.name)).max().unwrap_or(0) as i32;
    (longest + 1).clamp(12, 17)
}

/// A provider's name in its column: cut to leave a space before what
/// follows.
fn provider_name(name: &str, col: i32) -> String {
    pad_right(&trunc(name, (col - 1).max(0) as usize), col.max(0) as usize)
}

fn resets_text(v: &View, q: &Provider) -> String {
    q.resets_at.map_or_else(
        || format!("resets {UNKNOWN}"),
        |t| format!("resets in {}", ago(-v.age(t))),
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
    let col = name_col(&providers.iter().take(shown).collect::<Vec<_>>());
    // A wider name column takes its extra width from the bar.
    let bar_w = bar_w - (col - 12);
    for (i, q) in providers.iter().take(shown).enumerate() {
        let y = 10 + i as i32 * 2;
        c.put(bx, y, &provider_name(&q.name, col), St::NONE);
        let f = match provider_frac(q) {
            Ok(f) => f,
            Err((text, st)) => {
                let room = (bw - col).max(0) as usize;
                let text = with_expiry(v, q, text, room);
                c.put(bx + col, y, &trunc(&text, room), st);
                continue;
            }
        };
        bar(c, bx + col, y, bar_w, f, use_fg(f));
        let pst = if f >= 0.85 { St::fg(use_fg(f)) } else { St::D };
        c.right(bx + bw, y, &pct(f), pst);
        let reset = resets_text(v, q);
        if totals {
            let total = if q.of == 1 {
                format!("{} {}", q.of, q.unit)
            } else {
                format!(
                    "{} of {} {}",
                    to_fixed(q.used.unwrap_or_default(), 1),
                    q.of,
                    q.unit
                )
            };
            c.put(bx + col, y + 1, &total, St::D);
            c.right(bx + bw, y + 1, &reset, St::D);
        } else {
            c.put(
                bx + col,
                y + 1,
                &trunc(&reset, (bw - col).max(0) as usize),
                St::D,
            );
        }
    }
    if overflow {
        let y = 10 + shown as i32 * 2;
        c.put(
            bx,
            y,
            &more_providers(providers.len() - shown, v.snap.serves.tabs >= 4),
            St::F,
        );
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
                // Each column is cut to its own width so none runs into the
                // next: what happened, then (when shown) the kind, then who.
                c.put(x, y, &v.civil(a.at).hm(), St::D);
                c.put(x + 7, y, &pad_right(&a.subject, 17), St::B);
                c.put(x + 25, y, &trunc(&a.what, what_w.max(0) as usize), St::NONE);
                let who = if with_kind {
                    let who = trunc(&a.who, 12);
                    let kind_w = (18 - width(&who) as i32 - 1).max(0) as usize;
                    c.put(x + dw - 18, y, &trunc(&a.kind, kind_w), St::F);
                    who
                } else {
                    trunc(&a.who, (dw - 25 - what_w - 1).max(0) as usize)
                };
                c.right(x + dw, y, &who, St::D);
            }
        }
    }
}

/// Agents from `top` down to `last`. With `project`, a dim project column
/// follows each name.
fn agent_list(c: &mut Canvas, v: &View, (cx, top, cw, last): (i32, i32, i32, i32), project: bool) {
    let rows = (last - top + 1).max(0) as usize;
    let list = &v.snap.agents.data.list;
    // Like every list here: when the agents outnumber the rows, the last row
    // says how many are left out.
    let overflow = list.len() > rows;
    let shown = if overflow {
        rows.saturating_sub(1)
    } else {
        list.len()
    };
    if overflow && rows > 0 {
        let y = top + shown as i32;
        c.put(cx, y, &format!("+{} more", list.len() - shown), St::D);
    }
    for (i, a) in list.iter().take(shown).enumerate() {
        let y = top + i as i32;
        let working = a.working();
        let (g, st) = agent_glyph(a);
        c.put(cx, y, g, st);
        let name_st = if working { St::B } else { St::NONE };
        if project {
            c.put(cx + 2, y, &pad_right(&a.name, 9), name_st);
            c.put(cx + 12, y, &pad_right(&a.project, 16), St::D);
        } else {
            c.put(cx + 2, y, &pad_right(&a.name, 12), name_st);
        }
        let state_st = if working { St::NONE } else { St::D };
        let name_w = if project { 29 } else { 15 };
        let state = trunc(&a.state, (cw - name_w).max(0) as usize);
        c.right(cx + cw, y, &state, state_st);
    }
}

// ---- 120 columns -------------------------------------------------------------

fn draw_regular(c: &mut Canvas, v: &View) {
    let snap = v.snap;
    chrome(c, 0, snap.serves.tabs, &v.clock(), &snap.host.data.label);
    let total = snap.modules.data.len();
    let last = c.h - 4;

    if let Some(x) = answer(c, v) {
        c.put(
            x,
            3,
            &format!(
                "   ·   {} of {} healthy   ·   {} working",
                ok_count(v),
                count(total, "module", "modules"),
                count(snap.agents.data.working as usize, "agent", "agents")
            ),
            St::D,
        );
    }
    attention_list(c, v, Layout::Regular);

    // Modules
    let (ax, aw, bx, bw, cx) = (2, 40, 45, 37, 85);
    let cw = c.w - 2 - cx;
    let dim = panel_heading(
        c,
        v,
        (ax, 8, aw),
        "Modules",
        &snap.modules,
        &module_tally(v),
    );
    module_grid(c, v, ax, 20, false);
    if dim {
        c.dim_rect(ax, 10, aw, 13);
    }

    // Usage
    usage_panel(c, v, (bx, bw, 18), 6, false);

    // This Mac
    this_mac_heading(c, v, cx, cw);
    let (daemon, clients) = daemon_line(v);
    fact_row(
        c,
        v,
        (cx, 10, cw),
        "Daemon",
        &snap.daemon,
        &daemon,
        St::NONE,
    );
    fact_row(
        c,
        v,
        (cx, 11, cw),
        "Clients",
        &snap.daemon,
        &clients,
        St::NONE,
    );
    let bs = &snap.build_server.data;
    fact_row(
        c,
        v,
        (cx, 13, cw),
        "Build server",
        &snap.build_server,
        &busy_text(bs.busy, bs.slots),
        St::NONE,
    );
    if snap.build_server.loaded || snap.build_server.error.is_none() {
        c.put(
            cx + 13,
            14,
            &trunc(
                &queue_text(bs.queued, bs.wait_secs),
                (cw - 13).max(0) as usize,
            ),
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
        if v.meta(&snap.build_server, "").dim {
            c.dim_rect(cx + 13, 14, cw - 13, 2);
        }
    }
    backups(c, v, cx, 17, cw);
    devices(c, v, cx, 20, cx + cw);
    // Approvals show only once something can say how many are waiting.
    if snap.serves.approvals {
        let (appr, appr_st) = approvals_text(v);
        fact_row(
            c,
            v,
            (cx, 21, cw),
            "Approvals",
            &snap.approvals,
            &appr,
            appr_st,
        );
    }

    // Activity
    let dw = 80;
    activity_heading(c, v, ax, 24, dw);
    activity_list(c, v, (ax, 26, dw, last), 46, false);

    // Agents, with each one's project once the panel has room for it.
    let agents = &snap.agents;
    let meta = format!("{} working", agents.data.working);
    let dim = panel_heading(c, v, (cx, 24, cw), "Agents", agents, &meta);
    agent_list(c, v, (cx, 26, cw, last), cw >= 44);
    if dim {
        c.dim_rect(cx, 26, cw, last - 25);
    }

    frame_end(c, &HINTS, &v.updated(&snap.modules), St::D);
}

/// Gigabytes of memory: one decimal under 100, none from 100 ("82.6",
/// "128").
fn memory_gb(gb: f64) -> String {
    if gb < 100.0 {
        locale_number(js_round(gb * 10.0) / 10.0)
    } else {
        locale_number(js_round(gb))
    }
}

/// Gigabytes of disk as a whole number with separators ("3,922").
fn disk_gb(gb: f64) -> String {
    locale_number(js_round(gb))
}

/// "80 of 80 busy", with "unknown" for whichever count is missing.
/// A missing count makes the whole figure one "unknown", so an unknown
/// never spreads across a row.
fn busy_text(busy: Option<u32>, slots: Option<u32>) -> String {
    match (busy, slots) {
        (Some(b), Some(s)) => format!("{b} of {s} busy"),
        _ => UNKNOWN.to_string(),
    }
}

/// "21 queued · wait 3m", with whichever part is unknown left out, or one
/// "unknown" when both are.
fn queue_text(queued: Option<u32>, wait_secs: Option<f64>) -> String {
    match (queued, wait_secs) {
        (Some(q), Some(w)) => format!("{q} queued · wait {}", ago(w)),
        (Some(q), None) => format!("{q} queued"),
        (None, Some(w)) => format!("wait {}", ago(w)),
        (None, None) => UNKNOWN.to_string(),
    }
}

/// This Mac's heading. Its rows come from several sources; while any of
/// them is aging or failing, the heading's meta names one of those (a
/// last good reading first) instead of the machine id, so the panel never
/// looks current when part of it isn't.
fn this_mac_heading(c: &mut Canvas, v: &View, cx: i32, cw: i32) {
    let snap = v.snap;
    let id = snap
        .host
        .data
        .machine_id
        .as_deref()
        .map_or_else(|| UNKNOWN.to_string(), short_id);
    let sources: [(&str, Meta); 5] = [
        ("daemon", v.meta(&snap.daemon, "")),
        ("build server", v.meta(&snap.build_server, "")),
        ("backups", v.meta(&snap.backups, "")),
        ("devices", v.meta(&snap.devices, "")),
        ("host", v.meta(&snap.host, "")),
    ];
    let worst = sources
        .iter()
        .filter(|(_, m)| m.st == St::W)
        .max_by_key(|(_, m)| m.dim);
    match worst {
        Some((name, m)) => heading_st(
            c,
            cx,
            8,
            cw,
            "This Mac",
            &format!("{name} {}", m.text),
            St::W,
        ),
        None => heading_st(c, cx, 8, cw, "This Mac", &id, St::D),
    }
}

fn activity_heading(c: &mut Canvas, v: &View, x: i32, y: i32, w: i32) {
    let meta = v.meta(&v.snap.activity, "");
    if meta.text.is_empty() {
        heading_st(c, x, y, w, "Activity", "", St::D);
        live(c, v, x, y, w);
    } else {
        heading_st(c, x, y, w, "Activity", &meta.text, meta.st);
    }
}

/// The Usage panel: its heading (with the source's age or error), then the
/// providers, or "unknown" while nothing has been read yet.
fn usage_panel(
    c: &mut Canvas,
    v: &View,
    (bx, bw, bar_w): (i32, i32, i32),
    max: usize,
    totals: bool,
) {
    let usage = &v.snap.usage;
    let dim = panel_heading(c, v, (bx, 8, bw), "Usage", usage, "this week");
    if !usage.loaded {
        c.put(bx, 10, UNKNOWN, St::D);
        return;
    }
    usage_list(c, v, (bx, bw, bar_w), max, totals);
    if dim {
        c.dim_rect(bx, 10, bw, 12);
    }
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
    chrome(c, 0, snap.serves.tabs, &v.clock(), &snap.host.data.label);
    let total = snap.modules.data.len();
    let r = c.w - 2; // right edge of content

    if let Some(x) = answer(c, v) {
        c.put(
            x,
            3,
            &format!(
                "   ·   {} of {} healthy   ·   {} working   ·   {}",
                ok_count(v),
                count(total, "module", "modules"),
                count(snap.agents.data.working as usize, "agent", "agents"),
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
    let dim = panel_heading(
        c,
        v,
        (ax, 8, aw),
        "Modules",
        &snap.modules,
        &format!("{} · CPU now", module_tally(v)),
    );
    module_grid(c, v, ax, 29, true);
    if dim {
        c.dim_rect(ax, 10, aw, 13);
    }

    usage_panel(c, v, (bx, bw, bw - 18), 5, true);
    let usage = &snap.usage.data;
    c.put(bx, 21, "Models", St::D);
    let priced = usage
        .models_priced
        .map_or_else(|| UNKNOWN.to_string(), thousands);
    let checked = usage
        .models_checked_at
        .map_or_else(|| UNKNOWN.to_string(), |t| format!("{} ago", ago(v.age(t))));
    let models = format!("{priced} priced · checked {checked}");
    c.put(
        bx + 12,
        21,
        &trunc(&models, (bw - 12).max(0) as usize),
        St::NONE,
    );

    let host_panel = &snap.host;
    let host = &host_panel.data;
    this_mac_heading(c, v, cx, cw);
    let (daemon, clients) = daemon_line(v);
    fact_row(
        c,
        v,
        (cx, 10, cw),
        "Daemon",
        &snap.daemon,
        &daemon,
        St::NONE,
    );
    fact_row(
        c,
        v,
        (cx, 11, cw),
        "Clients",
        &snap.daemon,
        &clients,
        St::NONE,
    );
    let bs = &snap.build_server.data;
    fact_row(
        c,
        v,
        (cx, 13, cw),
        "Build server",
        &snap.build_server,
        &match (bs.busy, bs.slots, bs.queued) {
            (Some(_), Some(_), Some(q)) => {
                format!("{} · {q} queued", busy_text(bs.busy, bs.slots))
            }
            _ => busy_text(bs.busy, bs.slots),
        },
        St::NONE,
    );
    if snap.build_server.loaded || snap.build_server.error.is_none() {
        // The sparkline takes the panel's width, less its span label and the
        // queue wait after it, and never more samples than the history holds.
        let wait = bs
            .wait_secs
            .map_or_else(|| format!("wait {UNKNOWN}"), |w| format!("wait {}", ago(w)));
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
        if v.meta(&snap.build_server, "").dim {
            c.dim_rect(cx + 13, 14, cw - 13, 1);
        }
    }
    backups(c, v, cx, 16, cw);
    devices(c, v, cx, 18, cx + cw);
    // Approvals show only once something can say how many are waiting.
    if snap.serves.approvals {
        let (appr, appr_st) = approvals_text(v);
        fact_row(
            c,
            v,
            (cx, 19, cw),
            "Approvals",
            &snap.approvals,
            &appr,
            appr_st,
        );
    }
    let right = cx + cw;
    let gauge = |c: &mut Canvas,
                 y: i32,
                 k: &str,
                 used: Option<f64>,
                 total: Option<f64>,
                 fmt: fn(f64) -> String| {
        c.put(cx, y, k, St::D);
        let (Some(used), Some(total)) = (used, total) else {
            c.put(cx + 13, y, UNKNOWN, St::D);
            return;
        };
        let f = if total > 0.0 { used / total } else { 0.0 };
        let text = format!("{} of {} GB", fmt(used), fmt(total));
        // The gauge is 16 wide, narrowing (to no less than 6) so the figures
        // after it fit the panel.
        let bar_w = (right - cx - 13 - 2 - width(&text) as i32).clamp(6, 16);
        bar(c, cx + 13, y, bar_w, f, use_fg(f));
        let tx = cx + 13 + bar_w + 2;
        c.put(tx, y, &trunc(&text, (right - tx).max(0) as usize), St::NONE);
    };
    gauge(
        c,
        20,
        "Memory",
        host.memory_used_gb,
        host.memory_total_gb,
        memory_gb,
    );
    gauge(
        c,
        21,
        "Disk",
        host.disk_used_gb,
        host.disk_total_gb,
        disk_gb,
    );
    c.put(cx, 22, "Load", St::D);
    match host.load {
        Some(load) => {
            let loads = load
                .iter()
                .map(|l| to_fixed(*l, 1))
                .collect::<Vec<_>>()
                .join(" · ");
            let end = c.put(
                cx + 13,
                22,
                &trunc(&loads, (right - cx - 13).max(0) as usize),
                St::NONE,
            );
            // The label only when it fits whole.
            const SPAN: &str = "1, 5 and 15 min";
            if end + 3 + width(SPAN) as i32 <= right {
                c.put(end + 3, 22, SPAN, St::F);
            }
        }
        None => {
            c.put(cx + 13, 22, UNKNOWN, St::D);
        }
    }
    if v.meta(host_panel, "").dim {
        c.dim_rect(cx + 13, 20, cw - 13, 3);
    }

    // Activity fills the height; older entries follow a day divider.
    let (top, last) = (25, c.h - 4);
    let dw = cx - 3 - ax;
    activity_heading(c, v, ax, top, dw);
    activity_list(c, v, (ax, top + 2, dw, last), dw - 25 - 20, true);

    let agents = &snap.agents.data;
    let meta = format!("{} working", agents.working);
    let dim = panel_heading(c, v, (cx, top, cw), "Agents", &snap.agents, &meta);
    // The agents leave a blank row and then the summary line above the
    // footer.
    let room = (last - top - 3).max(0) as usize;
    let shown = agents.list.len().min(room);
    agent_list(c, v, (cx, top + 2, cw, top + 1 + room as i32), true);
    let opt = |n: Option<u32>, one: &str, many: &str| match n {
        Some(n) => count(n as usize, one, many),
        None => format!("{UNKNOWN} {many}"),
    };
    let summary = format!(
        "{} · {} · {}",
        opt(agents.machines, "machine", "machines"),
        opt(agents.workspaces, "workspace", "workspaces"),
        opt(agents.projects, "project", "projects"),
    );
    c.put(cx, top + 3 + shown as i32, &summary, St::F);
    if dim {
        c.dim_rect(cx, top + 2, cw, last - top - 1);
    }

    frame_end(c, &HINTS, &v.updated(&snap.modules), St::D);
}

// ---- below 120×36 ------------------------------------------------------------

fn draw_small(c: &mut Canvas, v: &View) {
    let snap = v.snap;
    chrome(c, 0, snap.serves.tabs, &v.clock(), &snap.host.data.label);
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
    let modules_dim = panel_heading(
        c,
        v,
        (2, 8, c.w - 4),
        "Modules",
        &snap.modules,
        &module_tally(v),
    );
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
    if modules_dim {
        c.dim_rect(2, 10, c.w - 4, 1);
    }

    let usage_dim = panel_heading(c, v, (2, 12, c.w - 4), "Usage", &snap.usage, "this week");
    let providers: &[Provider] = if snap.usage.loaded {
        &snap.usage.data.providers
    } else {
        c.put(2, 14, UNKNOWN, St::D);
        &[]
    };
    let fit = (c.h - 20).max(0) as usize;
    let overflow = providers.len() > fit;
    let shown = if overflow {
        fit.saturating_sub(1)
    } else {
        providers.len()
    };
    let col = name_col(&providers.iter().take(shown).collect::<Vec<_>>());
    for (i, q) in providers.iter().take(shown).enumerate() {
        let y = 14 + i as i32;
        c.put(2, y, &provider_name(&q.name, col), St::NONE);
        let f = match provider_frac(q) {
            Ok(f) => f,
            Err((text, st)) => {
                let room = (c.w - 4 - col).max(0) as usize;
                let text = with_expiry(v, q, text, room);
                c.put(2 + col, y, &trunc(&text, room), st);
                continue;
            }
        };
        // A wider name column takes its extra width from the bar.
        bar(c, 2 + col, y, 30 - (col - 12), f, use_fg(f));
        let pst = if f >= 0.85 { St::fg(use_fg(f)) } else { St::D };
        c.put(46, y, &pad_left(&pct(f), 4), pst);
        c.put(52, y, &resets_text(v, q), St::D);
    }
    let mut usage_end = 13 + shown.max(usize::from(!snap.usage.loaded)) as i32;
    if usage_dim {
        c.dim_rect(2, 14, c.w - 4, usage_end - 13);
    }
    if overflow {
        usage_end += 1;
        c.put(
            2,
            usage_end,
            &more_providers(providers.len() - shown, v.snap.serves.tabs >= 4),
            St::F,
        );
    }

    // Below a blank row: an Activity section when it gets at least one row
    // under its heading, else just the latest entry.
    let top = usage_end + 2;
    if last - top + 1 >= 3 {
        let dw = c.w - 4;
        activity_heading(c, v, 2, top, dw);
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
