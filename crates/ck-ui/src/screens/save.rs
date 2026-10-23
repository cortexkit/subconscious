//! The Save overlay over Settings: the exact change to `subc.jsonc`, with
//! line numbers and comments, and what the daemon says saving it would do.
//!
//! Every change to the staged text makes a new candidate revision, and the
//! connected daemon previews each one (`supervisor.rescan_checked`). The
//! overlay shows only the preview of the revision on screen, and `⏎` and `s`
//! act only on that preview, so a save never rests on a description of other
//! text.

use subc_control::{
    RescanExpect, RescanModuleAction, RescanModuleActionKind, SupervisorRescanResult,
};

use super::overlays::wrap;
use crate::draw::{pad_left, trunc, under_home, width, Canvas};
use crate::live::settings::{PreviewOutcome, PreviewReply, PreviewTicket};
use crate::theme::{Bg, Fg, St};

/// What the overlay says while the daemon's preview of the revision on
/// screen hasn't arrived.
pub const CHECKING: &str = "Checking what saving does…";

/// Lines of unchanged text shown above a change: enough for the key that
/// holds it and the comment just above that key.
const CONTEXT_BEFORE: usize = 2;
/// Lines of unchanged text shown below a change.
const CONTEXT_AFTER: usize = 1;

/// Above this many line pairs the changed middle of the file is shown as
/// removed and re-added whole, rather than matched line by line, so a huge
/// rewrite can't stall a frame.
const DIFF_CELLS: usize = 4_000_000;

/// Which button the person pressed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SaveMode {
    /// `⏎`: write the file, then apply it.
    Apply,
    /// `s`: write the file only.
    Only,
}

/// A save the person asked for: the exact text the daemon previewed, the
/// value an apply must send so the daemon applies only that text, and the
/// modules the preview says need a restart, in its order.
#[derive(Clone, Debug, PartialEq)]
pub struct SaveOrder {
    pub mode: SaveMode,
    pub revision: u64,
    pub candidate: String,
    pub expect: RescanExpect,
    pub restarts: Vec<String>,
}

/// The candidate revisions, their previews and the overlay's view.
#[derive(Clone, Debug, Default)]
pub struct SaveState {
    /// Moves on every change to the staged text.
    pub revision: u64,
    /// The number of the last preview request sent.
    requests: u64,
    /// The request whose reply the overlay waits for, if any.
    pub pending: Option<PreviewTicket>,
    /// The reply for the revision on screen.
    pub outcome: Option<PreviewOutcome>,
    /// Preview requests not yet handed to the live layer: the ticket and
    /// the candidate text.
    pub outbox: Vec<(PreviewTicket, String)>,
    /// Whether the overlay is on screen.
    pub open: bool,
    /// The first diff line in view.
    pub scroll: usize,
    /// The daemon connection the previews were sent on.
    connection: u64,
    /// The latest save asked for with a preview that allowed it.
    pub order: Option<SaveOrder>,
}

impl SaveState {
    /// Starts a new revision. Any reply still on its way describes older
    /// text, so it is dropped when it arrives. `preview` says whether this
    /// revision is worth previewing: it differs from the file and the
    /// daemon can preview it.
    pub fn revise(&mut self, candidate: &str, preview: bool) {
        self.revision += 1;
        self.pending = None;
        self.outcome = None;
        self.order = None;
        if preview {
            self.request(candidate);
        } else {
            self.open = false;
        }
    }

    /// Asks for a preview of the current revision unless one is already
    /// asked for or answered.
    pub fn ensure(&mut self, candidate: &str) {
        if self.pending.is_none() && self.outcome.is_none() {
            self.request(candidate);
        }
    }

    fn request(&mut self, candidate: &str) {
        self.requests += 1;
        let ticket = PreviewTicket {
            revision: self.revision,
            request: self.requests,
        };
        self.pending = Some(ticket);
        self.outbox.push((ticket, candidate.to_string()));
    }

    /// Takes a reply. Only the reply to the request waited for counts;
    /// returns whether it did.
    pub fn absorb(&mut self, reply: &PreviewReply) -> bool {
        if self.pending != Some(reply.ticket) || reply.ticket.revision != self.revision {
            return false;
        }
        self.pending = None;
        self.outcome = Some(reply.outcome.clone());
        true
    }

    /// Notes the live layer's connection number. After a reconnect no
    /// earlier request can be relied on: its reply may never come, and the
    /// daemon may now hold other staged state, so the current revision is
    /// previewed again. Returns whether the connection changed.
    pub fn connected(&mut self, connection: u64, candidate: &str, preview: bool) -> bool {
        if connection == self.connection {
            return false;
        }
        self.connection = connection;
        self.pending = None;
        self.outcome = None;
        self.order = None;
        if preview {
            self.request(candidate);
        }
        true
    }

    /// The preview of the revision on screen, once it has arrived and the
    /// daemon could load the text.
    pub fn preview(&self) -> Option<&SupervisorRescanResult> {
        match (&self.pending, &self.outcome) {
            (None, Some(PreviewOutcome::Ready(result))) => Some(&**result),
            _ => None,
        }
    }

    /// The modules the preview says need a restart, in its order.
    pub fn restarts(&self) -> Vec<String> {
        self.preview()
            .map(|r| {
                r.module_actions
                    .iter()
                    .filter(|a| a.action == RescanModuleActionKind::Restart)
                    .map(|a| a.module_id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The save `mode` asks for, when the preview of this revision allows
    /// one: it has arrived, the daemon could load the text, and it carries
    /// the value an apply needs.
    pub fn order(&self, mode: SaveMode, candidate: &str) -> Option<SaveOrder> {
        let preview = self.preview()?;
        Some(SaveOrder {
            mode,
            revision: self.revision,
            candidate: candidate.to_string(),
            expect: preview.expect.clone()?,
            restarts: self.restarts(),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffKind {
    Same,
    Removed,
    Added,
    /// Unchanged lines between two changes that aren't shown.
    Gap,
}

/// One line of the diff: its kind, its line number (in the old text for a
/// removed line, in the new text otherwise; 0 for a gap) and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub number: usize,
    pub text: String,
}

enum Op {
    Same(usize),
    Removed(usize),
    Added(usize),
}

/// The changes from `old` to `new`, line by line, with a little unchanged
/// text around each change. Removed lines come before the lines that
/// replace them.
pub fn diff(old: &str, new: &str) -> Vec<DiffLine> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let mut pre = 0;
    while pre < a.len() && pre < b.len() && a[pre] == b[pre] {
        pre += 1;
    }
    let mut suf = 0;
    while suf < a.len() - pre && suf < b.len() - pre && a[a.len() - 1 - suf] == b[b.len() - 1 - suf]
    {
        suf += 1;
    }
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut ops: Vec<Op> = (0..pre).map(Op::Same).collect();
    let (n, m) = (am.len(), bm.len());
    if n.saturating_mul(m) <= DIFF_CELLS {
        // Longest common subsequence lengths of every pair of suffixes.
        let mut lcs = vec![0u32; (n + 1) * (m + 1)];
        let at = |i: usize, j: usize| i * (m + 1) + j;
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[at(i, j)] = if am[i] == bm[j] {
                    lcs[at(i + 1, j + 1)] + 1
                } else {
                    lcs[at(i + 1, j)].max(lcs[at(i, j + 1)])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if am[i] == bm[j] {
                ops.push(Op::Same(pre + j));
                i += 1;
                j += 1;
            } else if lcs[at(i + 1, j)] >= lcs[at(i, j + 1)] {
                ops.push(Op::Removed(pre + i));
                i += 1;
            } else {
                ops.push(Op::Added(pre + j));
                j += 1;
            }
        }
        ops.extend((i..n).map(|i| Op::Removed(pre + i)));
        ops.extend((j..m).map(|j| Op::Added(pre + j)));
    } else {
        ops.extend((0..n).map(|i| Op::Removed(pre + i)));
        ops.extend((0..m).map(|j| Op::Added(pre + j)));
    }
    ops.extend((b.len() - suf..b.len()).map(Op::Same));

    let mut keep = vec![false; ops.len()];
    for (k, op) in ops.iter().enumerate() {
        if !matches!(op, Op::Same(_)) {
            let from = k.saturating_sub(CONTEXT_BEFORE);
            let to = (k + CONTEXT_AFTER).min(ops.len() - 1);
            keep[from..=to].iter_mut().for_each(|x| *x = true);
        }
    }
    let mut out = Vec::new();
    let mut skipped = false;
    for (k, op) in ops.iter().enumerate() {
        if !keep[k] {
            skipped = true;
            continue;
        }
        if skipped && !out.is_empty() {
            out.push(DiffLine {
                kind: DiffKind::Gap,
                number: 0,
                text: "…".into(),
            });
        }
        skipped = false;
        let (kind, number, text) = match *op {
            Op::Same(j) => (DiffKind::Same, j + 1, b[j]),
            Op::Removed(i) => (DiffKind::Removed, i + 1, a[i]),
            Op::Added(j) => (DiffKind::Added, j + 1, b[j]),
        };
        out.push(DiffLine {
            kind,
            number,
            // A tab has no fixed width on screen.
            text: text.replace('\t', "    "),
        });
    }
    out
}

/// One row of the "After saving" list: pieces of text at column offsets
/// from the list's left edge.
#[derive(Clone, Debug, Default)]
struct Row(Vec<(i32, String, St)>);

impl Row {
    fn one(text: String, st: St) -> Row {
        Row(vec![(0, text, st)])
    }
}

/// Seconds for a drain budget given in milliseconds: "30", or "1.5".
fn seconds(ms: u64) -> String {
    if ms.is_multiple_of(1000) {
        (ms / 1000).to_string()
    } else {
        let text = format!("{:.2}", ms as f64 / 1000.0);
        text.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// The drain sentence for a restart or a stop, in two parts: how many runs
/// are in flight, and how long the drain waits for them. Both are left out
/// when the module declares no busy gauges, because then ck has no reading
/// of what is in flight to show.
fn drain(action: &RescanModuleAction, verb: &str) -> Option<(String, String)> {
    if action.busy_gauges.is_empty() {
        return None;
    }
    // The count is the sum of the declared busy gauges' last readings; one
    // missing or non-numeric reading makes the sum unknown.
    let sum = action.busy_gauges.iter().try_fold(0u64, |sum, g| {
        let v = g.value.as_ref()?;
        let n = v
            .as_u64()
            .or_else(|| v.as_f64().filter(|f| *f >= 0.0).map(|f| f.round() as u64))?;
        Some(sum + n)
    });
    let runs = match sum {
        Some(1) => "It has 1 run in flight;".to_string(),
        Some(n) => format!("It has {n} runs in flight;"),
        None => "Its runs in flight are unknown;".to_string(),
    };
    let wait = match action.drain_timeout_ms {
        Some(ms) => format!("{verb} waits up to {} s for them to finish.", seconds(ms)),
        None => format!("{verb} waits for them to finish."),
    };
    Some((runs, wait))
}

/// What a restart is needed for: the one launch input that changed, or
/// "settings" when several did.
fn what(changed: &[String]) -> &str {
    let inputs: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|c| matches!(*c, "program" | "arguments" | "environment" | "protocol"))
        .collect();
    match inputs.as_slice() {
        [one] => one,
        _ => "settings",
    }
}

/// One module's consequence: a glyph and the module's name, then its text
/// wrapped in the `w` columns after the name.
fn action_rows(action: &RescanModuleAction, w: i32) -> Vec<Row> {
    let id = &action.module_id;
    let (glyph, glyph_st, text, drain) = match &action.action {
        RescanModuleActionKind::Restart => (
            "↻",
            St::A,
            format!("needs a restart to use the new {}.", what(&action.changed)),
            drain(action, "a restart"),
        ),
        RescanModuleActionKind::Reload => (
            "✓",
            St::O,
            "applies the change without a restart.".to_string(),
            None,
        ),
        RescanModuleActionKind::Start => ("◉", St::O, "starts.".to_string(), None),
        RescanModuleActionKind::Stop => {
            ("○", St::F, "stops.".to_string(), drain(action, "stopping"))
        }
        RescanModuleActionKind::Unknown(kind) => ("·", St::D, format!("{kind}."), None),
    };
    let tx = 2 + width(id) as i32 + 2;
    let room = (w - tx).max(1) as usize;
    let (first, second) = match drain {
        Some((runs, wait)) => (format!("{text} {runs}"), Some(wait)),
        None => (text, None),
    };
    let mut rows = Vec::new();
    for (i, line) in wrap(&first, room).into_iter().enumerate() {
        let mut row = Row::default();
        if i == 0 {
            row.0.push((0, glyph.into(), glyph_st));
            row.0.push((2, id.clone(), St::B));
        }
        row.0.push((tx, line, St::NONE));
        rows.push(row);
    }
    for line in second.iter().flat_map(|s| wrap(s, room)) {
        rows.push(Row(vec![(tx, line, St::D)]));
    }
    rows
}

/// The "After saving" list for the revision on screen, wrapped to `w`
/// columns.
fn after_rows(save: &SaveState, w: i32) -> Vec<Row> {
    let room = w.max(1) as usize;
    let lines = |text: &str, st: St| -> Vec<Row> {
        wrap(text, room)
            .into_iter()
            .map(|l| Row::one(l, st))
            .collect()
    };
    match (&save.pending, &save.outcome) {
        (None, Some(PreviewOutcome::Refused(message))) => lines(message, St::E),
        (None, Some(PreviewOutcome::Failed(code))) => {
            lines(&format!("Couldn't check what saving does: {code}"), St::E)
        }
        (None, Some(PreviewOutcome::Ready(result))) => {
            let mut rows: Vec<Row> = result
                .module_actions
                .iter()
                .flat_map(|a| action_rows(a, w))
                .collect();
            if !result.earlier_pending.is_empty() {
                rows.extend(lines(
                    &format!(
                        "Also waiting for a restart from an earlier change: {}",
                        result.earlier_pending.join(", ")
                    ),
                    St::D,
                ));
            }
            for section in &result.restart_required {
                rows.extend(lines(
                    &format!("{section} changed; it applies only when the daemon restarts"),
                    St::W,
                ));
            }
            for warning in &result.capability_warnings {
                rows.extend(lines(warning, St::W));
            }
            if rows.is_empty() {
                rows.extend(lines("Saving changes no running module.", St::D));
            }
            rows
        }
        _ => lines(CHECKING, St::D),
    }
}

/// Where the overlay and its parts go in a window `cw` columns wide and `ch`
/// rows high.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    /// Rows for diff lines.
    pub diff_room: usize,
    /// Rows for the "After saving" list.
    pub after_room: usize,
}

/// The box takes the design's 96 × 22 at 120 × 36 and grows with the
/// window. Inside, the diff and the "After saving" list share the rows
/// above the buttons: the diff shows whole when both fit, and otherwise
/// keeps what the list leaves, but never fewer than three rows.
fn layout(cw: i32, ch: i32, diff_total: usize, after_total: usize) -> Layout {
    let w = if cw >= 120 { cw - 24 } else { (cw - 8).min(96) };
    let h = if ch >= 36 { ch - 14 } else { (ch - 4).min(22) };
    let x = ((cw - w) / 2).max(0);
    let y = ((ch - h) / 2 - 1).max(1);
    // Rows from the first diff line to the row above the buttons' gap.
    let room = (h - 8).max(0) as usize;
    // The list costs its lines plus a blank row, its title and a blank row.
    let after_need = after_total + 3;
    let diff_room = if diff_total + after_need <= room {
        diff_total
    } else {
        room.saturating_sub(after_need)
            .max(3)
            .min(diff_total)
            .min(room)
    };
    Layout {
        x,
        y,
        w,
        h,
        diff_room,
        after_room: room.saturating_sub(diff_room + 3),
    }
}

/// The diff's first line in view, the lines shown, and how many lines are
/// below the view. While lines are hidden below, the last row counts them.
fn window(total: usize, room: usize, scroll: usize) -> (usize, usize, usize) {
    if total <= room {
        return (0, total, 0);
    }
    let last = total - room;
    let start = scroll.min(last);
    if start == last {
        (start, room, 0)
    } else {
        let shown = room.saturating_sub(1);
        (start, shown, total - start - shown)
    }
}

/// The furthest the diff scrolls, and how far a page moves it, in a window
/// `cw` columns wide and `ch` rows high.
pub fn scroll_limits(
    save: &SaveState,
    original: &str,
    candidate: &str,
    cw: i32,
    ch: i32,
) -> (usize, usize) {
    let total = diff(original, candidate).len();
    let probe = layout(cw, ch, total, 0);
    let after = after_rows(save, probe.w - 6).len();
    let l = layout(cw, ch, total, after);
    (
        total.saturating_sub(l.diff_room),
        l.diff_room.saturating_sub(1).max(1),
    )
}

/// What the overlay needs from Settings.
pub struct Overlay<'a> {
    pub save: &'a SaveState,
    pub original: &'a str,
    pub candidate: &'a str,
    pub config_path: &'a str,
    pub home: Option<&'a str>,
    pub changes: usize,
    /// Whether `⏎` and `s` write the file in this build.
    pub serves_save: bool,
}

pub fn draw(c: &mut Canvas, o: &Overlay) {
    c.dim_all();
    let lines = diff(o.original, o.candidate);
    let probe = layout(c.w, c.h, lines.len(), 0);
    let after = after_rows(o.save, probe.w - 6);
    let l = layout(c.w, c.h, lines.len(), after.len());
    let (x, y, w, h) = (l.x, l.y, l.w, l.h);
    c.boxed(x, y, w, h, "Save subc.jsonc");

    let path = under_home(o.config_path, o.home);
    let end = c.put(x + 3, y + 2, &trunc(&path, (w - 6).max(0) as usize), St::B);
    let note = format!(
        "· {} change{} · comments kept · a copy of the old file is kept",
        o.changes,
        if o.changes == 1 { "" } else { "s" }
    );
    c.put(
        end + 1,
        y + 2,
        &trunc(&note, (x + w - 3 - end - 1).max(0) as usize),
        St::D,
    );

    let text_w = (w - 14).max(0) as usize;
    let (start, shown, below) = window(lines.len(), l.diff_room, o.save.scroll);
    let mut row = y + 4;
    for line in lines.iter().skip(start).take(shown) {
        let comment = line.kind == DiffKind::Same && {
            let t = line.text.trim_start();
            t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
        };
        let (fill, number_st, text_st, sign) = match line.kind {
            DiffKind::Removed => (St::bg(Bg::Del), St::D.on(Bg::Del), St::bg(Bg::Del), "-"),
            DiffKind::Added => (St::bg(Bg::Add), St::D.on(Bg::Add), St::bg(Bg::Add), "+"),
            DiffKind::Gap => (St::NONE, St::D, St::F, " "),
            DiffKind::Same if comment => (St::F, St::D, St::F, " "),
            DiffKind::Same => (St::NONE, St::D, St::NONE, " "),
        };
        c.fill(x + 2, row, w - 4, 1, ' ', fill);
        if line.kind != DiffKind::Gap {
            c.put(
                x + 3,
                row,
                &pad_left(&line.number.to_string(), 4),
                number_st,
            );
        }
        c.put(x + 9, row, sign, text_st);
        c.put(x + 11, row, &trunc(&line.text, text_w), text_st);
        row += 1;
    }
    if below > 0 {
        c.put(x + 11, row, &format!("↓ {below} more"), St::F);
        row += 1;
    }

    row += 1;
    c.put(x + 3, row, "After saving", St::B);
    row += 2;
    let (shown, hidden) = if after.len() > l.after_room {
        let shown = l.after_room.saturating_sub(1);
        (shown, after.len() - shown)
    } else {
        (after.len(), 0)
    };
    for r in after.iter().take(shown) {
        for (dx, text, st) in &r.0 {
            c.put(x + 3 + dx, row, text, *st);
        }
        row += 1;
    }
    if hidden > 0 {
        c.put(x + 3, row, &format!("+{hidden} more"), St::F);
    }

    let by = y + h - 4;
    let button = St::fg(Fg::OnAcc).on(Bg::Acc).bold();
    let mut bx = x + 3;
    if o.serves_save && o.save.order(SaveMode::Only, o.candidate).is_some() {
        let restarts = o.save.restarts();
        let label = match restarts.as_slice() {
            [] => "Save and apply".to_string(),
            [one] => format!("Save and restart {one}"),
            many => format!("Save and restart {} modules", many.len()),
        };
        bx = c.put(bx, by, &format!(" ⏎  {label} "), button) + 3;
        bx = c.put(bx, by, " s  Save only ", St::bg(Bg::Key)) + 3;
    }
    c.put(bx, by, " esc  Cancel ", St::bg(Bg::Key));
}
