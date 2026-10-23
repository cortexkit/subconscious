//! Add a phone: the service's code stays beside every step, including confirmation.
//!
//! Before a code arrives the overlay keeps the code's place as a blank light
//! square, and its column says what it waits for: the access groups, the
//! person's choice of group, or the code itself.

use crate::draw::{self, Canvas};
use crate::pairing::flow::{Flow, Stage, NO_NAME};
use crate::pairing::{self, PairingState, QUIET, STEPS_W};
use crate::theme::{Bg, Fg, St};
use crate::time::Timestamp;

struct Text {
    x: i32,
    y: i32,
    text: String,
    style: St,
}

#[derive(Default)]
struct Column {
    text: Vec<Text>,
    height: i32,
}

impl Column {
    fn put(&mut self, x: i32, y: i32, text: impl Into<String>, style: St) {
        self.text.push(Text {
            x,
            y,
            text: text.into(),
            style,
        });
        self.height = self.height.max(y + 1);
    }

    fn wrap(&mut self, x: i32, mut y: i32, text: &str, style: St) -> i32 {
        let room = (STEPS_W - x) as usize;
        let mut line = String::new();
        for word in text.split_whitespace() {
            if !line.is_empty() && draw::width(&line) + 1 + draw::width(word) > room {
                self.put(x, y, std::mem::take(&mut line), style);
                y += 1;
            }
            if !line.is_empty() {
                line.push(' ');
            }
            // Addresses, fingerprints and device names can contain unbroken runs.
            for ch in word.chars() {
                if draw::width(&line) + draw::width(&ch.to_string()) > room {
                    self.put(x, y, std::mem::take(&mut line), style);
                    y += 1;
                }
                line.push(ch);
            }
        }
        if !line.is_empty() {
            self.put(x, y, line, style);
            y += 1;
        }
        y
    }
}

/// The three numbered steps, the first `done` of them ticked.
fn steps(done: usize) -> Column {
    let mut col = Column::default();
    for (i, text) in [
        "Open CortexKit on your phone.",
        "Tap Add a computer and scan this code.",
        "Type the four words your phone shows.",
    ]
    .into_iter()
    .enumerate()
    {
        let done = i < done;
        col.put(
            0,
            1 + i as i32 * 2,
            if done {
                "✓".into()
            } else {
                (i + 1).to_string()
            },
            if done { St::O.bold() } else { St::A.bold() },
        );
        col.wrap(
            2,
            1 + i as i32 * 2,
            text,
            if done { St::D } else { St::NONE },
        );
    }
    col
}

/// The Access line: the group's name, and its operation count while the
/// latest list gives one.
fn access(col: &mut Column, y: i32, group: &str, count: Option<u64>, style: St) -> i32 {
    let end_y = col.wrap(8, y, group, style);
    match count {
        Some(count) => {
            let last = col.text.last().unwrap();
            let end_x = last.x + draw::width(&last.text) as i32;
            let count = format!(" · {count} operations");
            if end_x + draw::width(&count) as i32 <= STEPS_W {
                col.put(end_x, end_y - 1, count, St::D);
                end_y
            } else {
                col.wrap(8, end_y, count.trim_start(), St::D)
            }
        }
        None => end_y,
    }
}

fn common(s: &PairingState, done: usize) -> Column {
    let mut col = steps(done);
    col.put(0, 8, "Access", St::D);
    access(&mut col, 8, &s.group, s.operation_count, St::B);
    col
}

/// A status line in the column, wrapped; failures in the warning colour.
fn status(col: &mut Column, y: i32, flow: &Flow, text: &str) -> i32 {
    let style = match flow.stage {
        Stage::Waiting
        | Stage::TouchId
        | Stage::Opening
        | Stage::ReadingGroups
        | Stage::Choose
        | Stage::Granted { .. }
            if !flow.group_gone() =>
        {
            St::NONE
        }
        _ => St::W,
    };
    col.wrap(0, y, text, style)
}

fn foreign(col: &mut Column, y: i32, s: &PairingState) -> i32 {
    match &s.foreign_attempt {
        Some(addr) => col.wrap(0, y, &format!("Someone at {addr} tried this code"), St::W),
        None => y,
    }
}

fn waiting(flow: &Flow, s: &PairingState, now: Timestamp) -> Column {
    let mut col = common(s, 0);
    let access_end = col.text.last().unwrap();
    let hint_y = if col.height == 9 && access_end.x + draw::width(&access_end.text) as i32 <= 37 {
        8
    } else {
        col.height
    };
    col.put(37, hint_y, "tab changes", St::D);
    let status_y = col.height + 2;
    let live = !flow.stage.closed();
    let mut y = if flow.stage == Stage::Waiting && !flow.group_gone() {
        col.put(0, status_y, "⠸", St::A);
        col.put(2, status_y, "Waiting for your phone…", St::NONE);
        status_y + 1
    } else {
        let text = flow.status().unwrap_or_default();
        status(&mut col, status_y, flow, &text)
    };
    if live {
        let seconds = ((s.expires_at.unix_millis() - now.unix_millis()).max(0) + 999) / 1000;
        col.put(
            2,
            y,
            format!("closes in {}:{:02}", seconds / 60, seconds % 60),
            St::D,
        );
        y = col.wrap(
            0,
            y + 3,
            &format!("The code works once, for {} minutes, and only", s.minutes()),
            St::D,
        );
        y = col.wrap(0, y, "on your own network. Anyone who sees it still", St::D);
        y = col.wrap(0, y, "needs your phone.", St::D);
    }
    foreign(&mut col, y + 1, s);
    col
}

/// "From <address> · key <fingerprint>", broken only after the "·" and,
/// within the fingerprint, at the spaces callosum sends, or every 16
/// characters when it sends none. No character is dropped.
pub fn from_lines(source_addr: &str, fingerprint: &str) -> Vec<String> {
    // Each piece with the text that joins it to the piece before when both
    // share a line; a line may break only before a breakable piece.
    let mut pieces: Vec<(&str, String, bool)> = vec![
        ("", "From".to_string(), false),
        (" ", source_addr.to_string(), false),
        (" ", "·".to_string(), false),
        (" ", "key".to_string(), true),
    ];
    if fingerprint.contains(char::is_whitespace) {
        for part in fingerprint.split_whitespace() {
            pieces.push((" ", part.to_string(), true));
        }
    } else {
        let chars: Vec<char> = fingerprint.chars().collect();
        for (i, chunk) in chars.chunks(16).enumerate() {
            let glue = if i == 0 { " " } else { "" };
            pieces.push((glue, chunk.iter().collect(), true));
        }
    }
    let room = STEPS_W as usize;
    let mut lines = vec![String::new()];
    for (glue, text, breakable) in pieces {
        let line = lines.last_mut().unwrap();
        if !line.is_empty()
            && breakable
            && draw::width(line) + glue.len() + draw::width(&text) > room
        {
            lines.push(text);
        } else {
            if !line.is_empty() {
                line.push_str(glue);
            }
            line.push_str(&text);
        }
    }
    // An address too long for the column still shows whole, over more lines.
    let mut out = Vec::new();
    for line in lines {
        let mut rest = line.as_str();
        while draw::width(rest) > room {
            let mut cut = 0;
            for (i, ch) in rest.char_indices() {
                if draw::width(&rest[..i + ch.len_utf8()]) > room {
                    break;
                }
                cut = i + ch.len_utf8();
            }
            out.push(rest[..cut].to_string());
            rest = &rest[cut..];
        }
        out.push(rest.to_string());
    }
    out
}

fn confirmation(flow: &Flow, s: &PairingState) -> Column {
    let mut col = common(s, if s.claim.is_some() { 2 } else { 0 });
    let start = col.height + 2;
    let mut y = match s.claim.as_ref().map(|c| c.device_name.as_deref()) {
        Some(Some(name)) => col.wrap(0, start, &format!("{name} scanned the code."), St::B),
        Some(None) => named(&mut col, 0, start, None, "scanned the code."),
        None => col.wrap(0, start, "Your phone scanned the code.", St::B),
    };
    y = col.wrap(0, y, "Type the four words it shows:", St::NONE) + 1;
    for (i, word) in s.words.iter().enumerate() {
        let x = i as i32 * 12;
        let word = draw::trunc(word, 9);
        col.put(
            x,
            y,
            format!(" {}", draw::pad_right(&word, 9)),
            St::bg(Bg::Key),
        );
        if i == s.word_sel {
            col.put(
                x + 1 + draw::width(&word).min(8) as i32,
                y,
                "▏",
                St::A.on(Bg::Key),
            );
        }
    }
    y += 2;
    if let Some(claim) = &s.claim {
        for line in from_lines(&claim.source_addr, &claim.fingerprint) {
            col.put(0, y, line, St::D);
            y += 1;
        }
    } else {
        y += 1;
    }
    y = foreign(&mut col, y, s);
    if s.claim.is_some() {
        if let Some(text) = flow.status() {
            y = status(&mut col, y, flow, &text);
        }
    }
    y += 2;
    col.put(0, y, " ⏎  Pair ", St::fg(Fg::OnAcc).on(Bg::Acc).bold());
    col.put(11, y, " esc  Close ", St::bg(Bg::Key));
    col.wrap(0, y + 2, "Touch ID confirms the pairing.", St::D);
    col
}

/// A line naming the phone. A phone callosum's registry has no name for
/// reads as a dim "(no name)", never as a made-up name.
fn named(col: &mut Column, x: i32, y: i32, name: Option<&str>, rest: &str) -> i32 {
    match name {
        Some(name) => col.wrap(x, y, &format!("{name} {rest}"), St::B),
        None => {
            col.put(x, y, NO_NAME, St::D);
            col.wrap(x + draw::width(NO_NAME) as i32 + 1, y, rest, St::B)
        }
    }
}

/// The last step: the phone is paired.
fn paired(s: &PairingState, device_name: Option<&str>) -> Column {
    let mut col = common(s, 3);
    let y = col.height + 2;
    col.put(0, y, "✓", St::O.bold());
    named(&mut col, 2, y, device_name, "is paired.");
    // "You can remove it any time from Devices" waits for a forget
    // operation in callosum.
    let y = col.wrap(
        0,
        y + 2,
        &format!("It has {} access to this Mac.", s.group),
        St::D,
    );
    col.put(0, y + 2, " ⏎  Done ", St::fg(Fg::OnAcc).on(Bg::Acc).bold());
    col
}

/// Before a code: the access groups or the chosen one, and the status.
fn before_code(flow: &Flow) -> Column {
    let mut col = steps(0);
    let mut y = col.height;
    if let Some(group) = flow.picked() {
        col.put(0, 8, "Access", St::D);
        y = access(&mut col, 8, group, flow.count(group), St::B);
    } else if !flow.offered().is_empty() {
        col.put(0, 8, "Access", St::D);
        y = 8;
        for g in flow.offered() {
            y = access(&mut col, y, &g.name, g.operation_count, St::NONE);
        }
    }
    if let Some(text) = flow.status() {
        let at = y.max(col.height) + 2;
        status(&mut col, at, flow, &text);
    }
    col
}

/// Draws the overlay's box at `l`, with the code's place light and blank
/// unless `code` is given.
fn frame(c: &mut Canvas, l: &pairing::Layout, modules: i32, code: Option<&qrcodegen::QrCode>) {
    c.boxed(l.x, l.y, l.width, l.height, "Add a phone");
    let symbol = modules + 2 * QUIET;
    let dark = |x: i32, y: i32| code.is_some_and(|q| q.get_module(x - QUIET, y - QUIET));
    for row in (0..symbol).step_by(2) {
        for x in 0..symbol {
            let ch = match (dark(x, row), dark(x, row + 1)) {
                (true, true) => "█",
                (true, false) => "▀",
                (false, true) => "▄",
                _ => " ",
            };
            c.put(
                l.code_x + x,
                l.content_y + row / 2,
                ch,
                St::fg(Fg::QrDark).on(Bg::QrLight),
            );
        }
    }
}

fn too_small(c: &mut Canvas, w: i32, h: i32) {
    let message = format!("Make the window at least {w}×{h} to show the code");
    let box_w = (draw::width(&message) as i32 + 6).min(c.w);
    let x = (c.w - box_w) / 2;
    c.boxed(x, 3, box_w, 5, "Add a phone");
    c.center(x, box_w, 5, &message, St::D);
}

fn put_column(c: &mut Canvas, l: &pairing::Layout, col: Column) {
    for text in col.text {
        c.put(
            l.steps_x + text.x,
            l.content_y + text.y,
            &text.text,
            text.style,
        );
    }
}

pub fn draw(c: &mut Canvas, flow: &Flow, now: Timestamp) {
    c.dim_all();
    let Some(s) = &flow.window else {
        let col = before_code(flow);
        let modules = pairing::TYPICAL_MODULES;
        let height = col.height;
        match pairing::layout_for(c.w, c.h, modules, height) {
            Some(l) => {
                frame(c, &l, modules, None);
                put_column(c, &l, col);
            }
            None => {
                let (w, h) = pairing::minimum_for(modules, height);
                too_small(c, w, h);
            }
        }
        return;
    };
    let waiting_col = waiting(flow, s, now);
    let confirmation_col = confirmation(flow, s);
    let paired_col = match &flow.stage {
        Stage::Granted { device_name } => Some(paired(s, device_name.as_deref())),
        _ => None,
    };
    let height = waiting_col
        .height
        .max(confirmation_col.height)
        .max(paired_col.as_ref().map_or(0, |p| p.height));
    let Some(l) = pairing::layout(c.w, c.h, s.code(), height) else {
        let (w, h) = pairing::minimum(s.code(), height);
        too_small(c, w, h);
        return;
    };
    frame(c, &l, s.code().size(), flow.code_shown().then(|| s.code()));
    let col = match paired_col {
        Some(p) => p,
        None if s.claim.is_some() => confirmation_col,
        None => waiting_col,
    };
    put_column(c, &l, col);
}
