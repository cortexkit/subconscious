//! Activity: what happened on this Mac, newest first.
//!
//! The daemon's activity ledger merged with process exits, one row each, a
//! restart and the start that followed it as one row. Dim label rows mark
//! each day ("Today", "Yesterday", then the date) and every place where the
//! daemon no longer keeps the entries between two loaded ranges; labels
//! count toward no total. Kind chips and a `/` text filter narrow the list
//! to the loaded rows they match. When the rows reach the list's last row,
//! that row says how many loaded rows are below the view, or that older
//! entries are a scroll away, or that the list reached the start of the
//! history the daemon keeps.
//!
//! From 120 columns a pane follows the selection: what happened, when in
//! local time, who asked for it, the subject, the ledger's kind and seq,
//! the build a restart ran, and the entry's details printed as JSON with
//! keys in sorted order. From 150 columns the list adds each entry's kind,
//! seq and details on one line.

use serde_json::{Map, Value};

use crate::draw::{chrome, frame_end, live_dot, pad_right, trunc, Canvas, Clip};
use crate::model::{ActivityKey, ActivityLog, ActivityRow, ActivityRowKind, Tab};
use crate::theme::{Bg, St};
use crate::time::{thousands, Civil, UtcOffset};

use super::View;

/// The kind chips, in the order the heading draws them and `tab` visits
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Chip {
    #[default]
    All,
    Modules,
    Agents,
    Approvals,
    Settings,
}

impl Chip {
    pub const ALL: [Chip; 5] = [
        Chip::All,
        Chip::Modules,
        Chip::Agents,
        Chip::Approvals,
        Chip::Settings,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Chip::All => "All",
            Chip::Modules => "Modules",
            Chip::Agents => "Agents",
            Chip::Approvals => "Approvals",
            Chip::Settings => "Settings",
        }
    }

    /// The next chip to the right, wrapping to All after Settings.
    pub fn next(self) -> Chip {
        let i = Chip::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Chip::ALL[(i + 1) % Chip::ALL.len()]
    }

    /// The next chip to the left, wrapping to Settings before All.
    pub fn prev(self) -> Chip {
        let i = Chip::ALL.iter().position(|c| *c == self).unwrap_or(0);
        Chip::ALL[(i + Chip::ALL.len() - 1) % Chip::ALL.len()]
    }

    /// Whether the chip lists `row`. Modules lists the daemon's module
    /// lifecycle kinds (`subc.module_started`, `_restarted`, `_stopped`,
    /// `_disabled`, `_swapped`, `_rescanned`) and every process exit;
    /// Agents lists entries whose actor carries an agent id; Approvals
    /// lists cingulate's kinds, which the daemon only accepts from
    /// cingulate as `cingulate.<name>`; Settings lists `subc.config_`
    /// kinds. A kind no chip names shows only under All.
    pub fn admits(self, row: &ActivityRow) -> bool {
        let kind = row.kind.as_deref().unwrap_or("");
        match self {
            Chip::All => true,
            Chip::Modules => row.row == ActivityRowKind::Exit || kind.starts_with("subc.module_"),
            Chip::Agents => row.by_agent,
            Chip::Approvals => kind.starts_with("cingulate."),
            Chip::Settings => kind.starts_with("subc.config_"),
        }
    }
}

/// What the person has set on the Activity screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActivityState {
    /// The selected row; `None` for the first row, so the selection stays
    /// at the top as new entries arrive.
    pub sel: Option<ActivityKey>,
    pub chip: Chip,
    /// The `/` filter's text; empty for none.
    pub filter: String,
}

/// Whether `row` holds `query` in its subject, what happened, who did it or
/// its kind, ignoring case.
fn matches_text(row: &ActivityRow, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let query = query.to_lowercase();
    [
        Some(row.subject.as_str()),
        Some(row.what.as_str()),
        row.who.as_deref(),
        row.kind.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|text| text.to_lowercase().contains(&query))
}

/// The rows the list shows: every loaded row under All with no filter,
/// labels where entries are no longer kept included; otherwise the entries
/// and exits the chip admits and the filter matches.
pub fn shown<'a>(log: &'a ActivityLog, state: &ActivityState) -> Vec<&'a ActivityRow> {
    let narrowed = state.chip != Chip::All || !state.filter.is_empty();
    log.rows
        .iter()
        .filter(|r| {
            if !r.counts() {
                return !narrowed;
            }
            state.chip.admits(r) && matches_text(r, &state.filter)
        })
        .collect()
}

/// The rows the selection moves over: the entries and exits shown.
pub fn selectable<'a>(log: &'a ActivityLog, state: &ActivityState) -> Vec<&'a ActivityRow> {
    shown(log, state)
        .into_iter()
        .filter(|r| r.counts())
        .collect()
}

/// Where the selection is among `rows`: the selected row, or the first
/// when nothing is selected or the selected row is no longer shown.
pub fn selected_pos(rows: &[&ActivityRow], sel: Option<&ActivityKey>) -> usize {
    sel.and_then(|k| rows.iter().position(|r| r.is(k)))
        .unwrap_or(0)
}

/// How many loaded rows (entries and exits, whatever the chips and filter)
/// fall on today's local date.
pub fn today_count(v: &View, log: &ActivityLog) -> usize {
    let today = v.civil(v.now).days;
    log.rows
        .iter()
        .filter(|r| r.counts() && v.civil(r.at).days == today)
        .count()
}

/// The list's geometry at a window size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// Right edge of the list: who did it ends just before this column.
    pub lw: i32,
    /// Whether the detail pane is drawn.
    pub pane: bool,
    /// Whether the list has the kind, seq and details columns.
    pub wide: bool,
    /// Width of the "what happened" column, from column 27.
    pub what_w: i32,
    pub who_w: i32,
    pub kind_x: i32,
    /// The seq column ends just before this column.
    pub seq_end: i32,
    pub details_x: i32,
    pub details_w: i32,
    /// The last screen row the list draws on: the row above the blank row
    /// over the footer rule.
    pub last: i32,
}

const WHAT_X: i32 = 27;
const KIND_W: i32 = 22;
const SEQ_W: i32 = 7;
/// The details column's narrowest width; width past what "what happened"
/// takes goes to it.
const DETAILS_MIN_W: i32 = 12;

pub fn layout(w: i32, h: i32) -> Layout {
    let pane = w >= 120;
    let wide = w >= 150;
    // At 120 columns the pane takes the 46 columns from the divider on;
    // from 150 it takes 30% of the window, between 46 and 70 columns.
    let pane_part = if wide {
        (f64::from(w) * 0.3).round().clamp(46.0, 70.0) as i32
    } else {
        46
    };
    let lw = if pane { w - pane_part } else { w - 2 };
    let base = Layout {
        lw,
        pane,
        wide,
        what_w: 0,
        who_w: 6,
        kind_x: 0,
        seq_end: 0,
        details_x: 0,
        details_w: 0,
        last: h - 4,
    };
    if !wide {
        return Layout {
            what_w: (lw - WHAT_X - 1 - base.who_w).max(0),
            ..base
        };
    }
    // Columns from 27: what happened, a gap, the kind, a gap, the seq, a
    // gap, the details, a gap, then who did it.
    let who_w = 8;
    let fixed = WHAT_X + 2 + KIND_W + 1 + SEQ_W + 2 + 1 + who_w;
    let what_w = (lw - fixed - DETAILS_MIN_W).clamp(24, 40);
    let kind_x = WHAT_X + what_w + 2;
    let seq_end = kind_x + KIND_W + 1 + SEQ_W;
    Layout {
        what_w,
        who_w,
        kind_x,
        seq_end,
        details_x: seq_end + 2,
        details_w: (lw - fixed - what_w).max(0),
        ..base
    }
}

/// One line of the list: a day label or a row.
enum Line<'a> {
    Day(String),
    Row(&'a ActivityRow),
}

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// The label for the local day `c` falls on: "Today", "Yesterday", or
/// "<Weekday> <d> <Mon>".
fn day_label(v: &View, c: Civil) -> String {
    match v.civil(v.now).days - c.days {
        0 => "Today".to_string(),
        1 => "Yesterday".to_string(),
        _ => {
            // 1970-01-01, day 0, was a Thursday.
            let weekday = WEEKDAYS[(c.days + 4).rem_euclid(7) as usize];
            let month = MONTHS[(c.month as usize).clamp(1, 12) - 1];
            format!("{weekday} {} {month}", c.day)
        }
    }
}

/// The list's lines: a day label above the first row and wherever the
/// local date changes, then the rows. A label marking entries no longer
/// kept starts no day.
fn lines<'a>(v: &View, rows: &[&'a ActivityRow]) -> Vec<Line<'a>> {
    let mut out = Vec::with_capacity(rows.len() + 4);
    let mut day = None;
    for r in rows {
        if r.counts() {
            let c = v.civil(r.at);
            if day != Some(c.days) {
                day = Some(c.days);
                out.push(Line::Day(day_label(v, c)));
            }
        }
        out.push(Line::Row(r));
    }
    out
}

/// What the list's last row says when the rows reach it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Note {
    /// Loaded rows below the view.
    More(usize),
    /// Every loaded row below is in view, and older entries can be read.
    ScrollForOlder,
    /// Every loaded row below is in view, and nothing older is kept.
    StartOfHistory,
}

impl Note {
    pub fn text(&self) -> String {
        match self {
            Note::More(n) => format!("+{n} more"),
            Note::ScrollForOlder => "scroll for older entries".into(),
            Note::StartOfHistory => "start of history".into(),
        }
    }
}

/// Which lines the list shows: the first, how many, and the note on the
/// last row when the lines reach it.
struct Window {
    first: usize,
    count: usize,
    note: Option<Note>,
}

/// The lines in view for `slots` rows with the selected row on line
/// `sel_line`. Lines that end above the last row are all drawn, with
/// nothing below them; otherwise the last row is a note and the selection
/// stays in view above it.
fn window(lines: &[Line], slots: usize, sel_line: usize, exhausted: bool) -> Window {
    if lines.len() < slots {
        return Window {
            first: 0,
            count: lines.len(),
            note: None,
        };
    }
    let count = slots.saturating_sub(1);
    let first = if sel_line < count {
        0
    } else {
        sel_line + 1 - count
    }
    .min(lines.len() - count);
    let below = lines[first + count..]
        .iter()
        .filter(|l| matches!(l, Line::Row(r) if r.counts()))
        .count();
    let note = if below > 0 {
        Note::More(below)
    } else if exhausted {
        Note::StartOfHistory
    } else {
        Note::ScrollForOlder
    };
    Window {
        first,
        count,
        note: Some(note),
    }
}

/// A UTC offset as the pane's time line names it: "UTC", "UTC+2",
/// "UTC-3:30".
pub fn offset_label(offset: UtcOffset) -> String {
    let secs = offset.0;
    if secs == 0 {
        return "UTC".into();
    }
    let sign = if secs < 0 { '-' } else { '+' };
    let abs = secs.unsigned_abs();
    let (h, m) = (abs / 3600, abs % 3600 / 60);
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h}:{m:02}")
    }
}

/// An object's entries in sorted byte order of their keys. The order is
/// set here rather than taken from the map, because a map's iteration
/// order depends on whether serde_json's `preserve_order` feature is on,
/// and a workspace build turns it on for every crate while a build of
/// this crate alone may not.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<(&String, &Value)> = map.iter().collect();
    entries.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    entries
}

fn quoted(key: &str) -> String {
    serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
}

fn pretty_into(v: &Value, depth: usize, head: String, tail: &str, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    match v {
        Value::Object(map) if !map.is_empty() => {
            out.push(format!("{head}{{"));
            let entries = sorted(map);
            let n = entries.len();
            for (i, (k, val)) in entries.into_iter().enumerate() {
                let head = format!("{pad}  {}: ", quoted(k));
                pretty_into(val, depth + 1, head, if i + 1 < n { "," } else { "" }, out);
            }
            out.push(format!("{pad}}}{tail}"));
        }
        Value::Array(items) if !items.is_empty() => {
            out.push(format!("{head}["));
            let n = items.len();
            for (i, val) in items.iter().enumerate() {
                let tail = if i + 1 < n { "," } else { "" };
                pretty_into(val, depth + 1, format!("{pad}  "), tail, out);
            }
            out.push(format!("{pad}]{tail}"));
        }
        Value::Object(_) => out.push(format!("{head}{{}}{tail}")),
        Value::Array(_) => out.push(format!("{head}[]{tail}")),
        scalar => out.push(format!("{head}{scalar}{tail}")),
    }
}

/// An entry's details pretty-printed with a two-space indent, keys in
/// sorted byte order at every level, one string per line.
pub fn details_lines(details: &Map<String, Value>) -> Vec<String> {
    let mut out = Vec::new();
    pretty_into(
        &Value::Object(details.clone()),
        0,
        String::new(),
        "",
        &mut out,
    );
    out
}

fn compact_into(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            out.push('{');
            for (i, (k, val)) in sorted(map).into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&quoted(k));
                out.push(':');
                compact_into(val, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, val) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                compact_into(val, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// An entry's details on one line, keys in sorted byte order at every
/// level, for the wide list's details column.
pub fn details_line(details: &Map<String, Value>) -> String {
    let mut out = String::new();
    compact_into(&Value::Object(details.clone()), &mut out);
    out
}

/// How "what happened" is coloured: approvals in the accent, settings
/// changes in amber, exits dim.
fn what_style(row: &ActivityRow) -> St {
    if row.row == ActivityRowKind::Exit {
        return St::D;
    }
    let kind = row.kind.as_deref().unwrap_or("");
    if Chip::Approvals.admits(row) {
        St::A
    } else if kind.starts_with("subc.config_") {
        St::W
    } else {
        St::NONE
    }
}

pub fn draw(c: &mut Canvas, v: &View, state: &ActivityState, typing: bool) {
    let snap = v.snap;
    chrome(
        c,
        Tab::Activity,
        snap.visible_tabs(),
        &v.clock(),
        &snap.host.data.label,
    );
    let panel = &snap.activity_log;
    let log = &panel.data;
    let l = layout(c.w, c.h);

    // The heading: the title, the kind chips, the filter, and how the feed
    // is doing.
    c.put(2, 3, "Activity", St::B);
    let mut cx = 14;
    for chip in Chip::ALL {
        let text = format!(" {} ", chip.label());
        let st = if chip == state.chip {
            St::bg(Bg::Key).bold()
        } else {
            St::D
        };
        cx = c.put(cx, 3, &text, st) + 1;
    }
    let meta = v.meta(panel, "");
    let right_start = if meta.text.is_empty() {
        if panel.loaded {
            let (dot, dot_st) = live_dot(v.tick);
            c.put(c.w - 3, 3, dot, dot_st);
            c.right(c.w - 4, 3, "live", St::D)
        } else {
            c.right(c.w - 2, 3, "reading the ledger…", St::D)
        }
    } else {
        let room = (c.w - 2 - cx - 1).max(0) as usize;
        c.right(c.w - 2, 3, &trunc(&meta.text, room), meta.st)
    };
    let rows = shown(log, state);
    if !state.filter.is_empty() || typing {
        let prev = c.clip_to(Clip {
            x0: 0,
            y0: 0,
            x1: right_start - 1,
            y1: c.h,
        });
        let sx = cx + 1;
        c.put(sx, 3, "/", St::A);
        let end = c.put(sx + 2, 3, &state.filter, St::B.underline());
        if typing {
            c.put(end, 3, "▏", St::A);
        }
        let n = rows.iter().filter(|r| r.counts()).count();
        let count = if n == 1 {
            "1 match".to_string()
        } else {
            format!("{n} matches")
        };
        c.put(end + 1, 3, &count, St::D);
        c.set_clip(prev);
    }
    c.hline(2, 4, c.w - 4);

    // The list.
    let selectable: Vec<&ActivityRow> = rows.iter().copied().filter(|r| r.counts()).collect();
    let sel_row = selectable
        .get(selected_pos(&selectable, state.sel.as_ref()))
        .copied();
    let lines = lines(v, &rows);
    let sel_line = sel_row
        .and_then(|s| {
            lines
                .iter()
                .position(|line| matches!(line, Line::Row(r) if std::ptr::eq(*r, s)))
        })
        .unwrap_or(0);
    let slots = (l.last - 5 + 1).max(0) as usize;
    let win = window(&lines, slots, sel_line, log.history_exhausted);
    let mut y = 5;
    for line in lines.iter().skip(win.first).take(win.count) {
        match line {
            Line::Day(label) => {
                c.put(2, y, label, St::D);
            }
            Line::Row(r) if !r.counts() => {
                c.put(2, y, &trunc(&r.what, (l.lw - 2).max(0) as usize), St::D);
            }
            Line::Row(r) => {
                list_row(c, v, &l, y, r);
                if sel_row.is_some_and(|s| std::ptr::eq(*r, s)) {
                    c.paint(1, y, l.lw, St::bg(Bg::Sel));
                }
            }
        }
        y += 1;
    }
    if let Some(note) = &win.note {
        c.put(2, y, &note.text(), St::D);
    }
    if rows.iter().all(|r| !r.counts()) {
        let note = if !panel.loaded {
            "Reading the activity ledger…"
        } else if log.rows.iter().all(|r| !r.counts()) {
            "No activity recorded yet"
        } else {
            "No loaded entry matches"
        };
        c.put(2, 5, &trunc(note, (l.lw - 2).max(0) as usize), St::D);
    }
    if meta.dim {
        c.dim_rect(1, 5, l.lw, l.last - 4);
    }

    if l.pane {
        c.vline(l.lw + 2, 5, l.last - 4);
        if let Some(r) = sel_row {
            let prev = c.clip_to(Clip {
                x0: 0,
                y0: 0,
                x1: c.w,
                y1: l.last + 1,
            });
            pane(c, v, &l, r);
            c.set_clip(prev);
        }
    }

    let mut hints = vec![("↑↓", "move"), ("⏎", "open subject")];
    if snap.serves.list_filter.contains(Tab::Activity) {
        hints.push(("/", "filter"));
    }
    hints.push(("tab", "kind"));
    let status = format!("{} today · ledger and process events", today_count(v, log));
    frame_end(c, &hints, &status, St::D);
}

/// One entry or exit row of the list at screen row `y`.
fn list_row(c: &mut Canvas, v: &View, l: &Layout, y: i32, r: &ActivityRow) {
    c.put(2, y, &v.civil(r.at).hm(), St::D);
    c.put(9, y, &pad_right(&r.subject, 17), St::B);
    c.put(
        WHAT_X,
        y,
        &trunc(&r.what, l.what_w.max(0) as usize),
        what_style(r),
    );
    if l.wide {
        if let Some(kind) = &r.kind {
            c.put(l.kind_x, y, &trunc(kind, KIND_W as usize), St::F);
        }
        if let Some(seq) = r.seq {
            c.right(l.seq_end, y, &trunc(&thousands(seq), SEQ_W as usize), St::D);
        }
        if let Some(details) = r.details.as_ref().filter(|d| !d.is_empty()) {
            let text = trunc(&details_line(details), l.details_w.max(0) as usize);
            c.put(l.details_x, y, &text, St::F);
        }
    }
    if let Some(who) = &r.who {
        c.right(l.lw, y, &trunc(who, l.who_w.max(0) as usize), St::D);
    }
}

/// The rows the detail pane draws under the summary and time, as label and
/// value. An exit has no actor, seq or details: subc reports it.
pub fn pane_rows(r: &ActivityRow) -> Vec<(&'static str, String)> {
    let subject = format!("module {}", r.subject);
    if r.row == ActivityRowKind::Exit {
        return vec![
            ("By", "subc".into()),
            ("Subject", subject),
            ("Kind", "process exit".into()),
        ];
    }
    let by =
        r.by.clone()
            .or_else(|| r.who.clone())
            .unwrap_or_else(|| super::UNKNOWN.into());
    let mut rows = vec![("By", by), ("Subject", subject)];
    if let Some(kind) = &r.kind {
        rows.push(("Kind", kind.clone()));
    }
    if let Some(seq) = r.seq {
        rows.push(("Seq", thousands(seq)));
    }
    if let Some(build) = &r.build {
        rows.push(("Build", build.clone()));
    }
    rows
}

/// The detail pane for the selected row.
fn pane(c: &mut Canvas, v: &View, l: &Layout, r: &ActivityRow) {
    let x = l.lw + 5;
    let dw = (c.w - x - 2).max(0);
    let summary = format!("{} {}", r.subject, r.what);
    c.put(x, 5, &trunc(&summary, dw as usize), St::B);
    let when = format!(
        "{} local · {}",
        v.civil(r.at).hms(),
        offset_label(v.snap.host.data.utc_offset)
    );
    c.put(x, 6, &trunc(&when, dw as usize), St::D);
    let mut y = 8;
    for (label, value) in pane_rows(r) {
        c.put(x, y, &pad_right(label, 10), St::D);
        c.put(
            x + 10,
            y,
            &trunc(&value, (dw - 10).max(0) as usize),
            St::NONE,
        );
        y += 1;
    }
    let Some(details) = r.details.as_ref().filter(|d| !d.is_empty()) else {
        return;
    };
    y += 1;
    if y > l.last {
        return;
    }
    c.put(x, y, "Details", St::D);
    y += 1;
    let text = details_lines(details);
    let room = (l.last - y + 1).max(0) as usize;
    if room == 0 {
        return;
    }
    let shown = if text.len() > room {
        room - 1
    } else {
        text.len()
    };
    let line_w = (dw - 2).max(0) as usize;
    for line in text.iter().take(shown) {
        c.put(x + 2, y, &trunc(line, line_w), St::F);
        y += 1;
    }
    if shown < text.len() {
        let more = format!("+{} more lines", text.len() - shown);
        c.put(x + 2, y, &trunc(&more, line_w), St::D);
    }
}
