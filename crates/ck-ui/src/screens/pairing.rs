//! Add a phone: the service's code stays beside every step, including confirmation.

use crate::draw::{self, Canvas};
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

fn common(s: &PairingState) -> Column {
    let mut col = Column::default();
    for (i, text) in [
        "Open CortexKit on your phone.",
        "Tap Add a computer and scan this code.",
        "Type the four words your phone shows.",
    ]
    .into_iter()
    .enumerate()
    {
        let done = s.claim.is_some() && i < 2;
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
    col.put(0, 8, "Access", St::D);
    let end_y = col.wrap(8, 8, &s.group, St::B);
    if let Some(count) = s.operation_count {
        let last = col.text.last().unwrap();
        let end_x = last.x + draw::width(&last.text) as i32;
        let count = format!(" · {count} operations");
        if end_x + draw::width(&count) as i32 <= STEPS_W {
            col.put(end_x, end_y - 1, count, St::D);
        } else {
            col.wrap(8, end_y, count.trim_start(), St::D);
        }
    }
    col
}

fn waiting(s: &PairingState, now: Timestamp) -> Column {
    let mut col = common(s);
    let access_end = col.text.last().unwrap();
    let hint_y = if col.height == 9 && access_end.x + draw::width(&access_end.text) as i32 <= 37 {
        8
    } else {
        col.height
    };
    col.put(37, hint_y, "tab changes", St::D);
    let status_y = col.height + 2;
    col.put(0, status_y, "⠸", St::A);
    col.put(2, status_y, "Waiting for your phone…", St::NONE);
    let seconds = ((s.expires_at.unix_millis() - now.unix_millis()).max(0) + 999) / 1000;
    col.put(
        2,
        status_y + 1,
        format!("closes in {}:{:02}", seconds / 60, seconds % 60),
        St::D,
    );
    let mut y = col.wrap(
        0,
        status_y + 4,
        &format!("The code works once, for {} minutes, and only", s.minutes()),
        St::D,
    );
    y = col.wrap(0, y, "on your own network. Anyone who sees it still", St::D);
    col.wrap(0, y, "needs your phone.", St::D);
    col
}

fn confirmation(s: &PairingState) -> Column {
    let mut col = common(s);
    let name = s
        .claim
        .as_ref()
        .map_or("Your phone", |c| c.device_name.as_str());
    let start = col.height + 2;
    let mut y = col.wrap(0, start, &format!("{name} scanned the code."), St::B);
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
        y = col.wrap(
            0,
            y,
            &format!("From {} · key {}", claim.source_addr, claim.fingerprint),
            St::D,
        );
    } else {
        y += 1;
    }
    if let Some(addr) = &s.foreign_attempt {
        y = col.wrap(0, y, &format!("Another phone tried from {addr}."), St::D);
    }
    y += 2;
    col.put(0, y, " ⏎  Pair ", St::fg(Fg::OnAcc).on(Bg::Acc).bold());
    col.put(11, y, " esc  Close ", St::bg(Bg::Key));
    col.wrap(0, y + 2, "Touch ID confirms the pairing.", St::D);
    col
}

pub fn draw(c: &mut Canvas, s: &PairingState, now: Timestamp) {
    let waiting = waiting(s, now);
    let confirmation = confirmation(s);
    let height = waiting.height.max(confirmation.height);
    c.dim_all();
    let Some(l) = pairing::layout(c.w, c.h, s.code(), height) else {
        let (w, h) = pairing::minimum(s.code(), height);
        let message = format!("Make the window at least {w}×{h} to show the code");
        let box_w = (draw::width(&message) as i32 + 6).min(c.w);
        let x = (c.w - box_w) / 2;
        c.boxed(x, 3, box_w, 5, "Add a phone");
        c.center(x, box_w, 5, &message, St::D);
        return;
    };
    c.boxed(l.x, l.y, l.width, l.height, "Add a phone");
    let symbol = s.code().size() + 2 * QUIET;
    for row in (0..symbol).step_by(2) {
        for x in 0..symbol {
            let top = s.code().get_module(x - QUIET, row - QUIET);
            let bottom = s.code().get_module(x - QUIET, row + 1 - QUIET);
            let ch = match (top, bottom) {
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
    let col = if s.claim.is_some() {
        confirmation
    } else {
        waiting
    };
    for text in col.text {
        c.put(
            l.steps_x + text.x,
            l.content_y + text.y,
            &text.text,
            text.style,
        );
    }
}
