//! The screen shown instead of an error when the daemon isn't running.

use crate::draw::{footer, pad_right, Canvas};
use crate::model::DaemonDown;
use crate::theme::{Bg, Fg, St};

use super::View;

pub fn draw(c: &mut Canvas, v: &View, d: &DaemonDown) {
    c.put(1, 0, "◆", St::A.bold());
    c.put(3, 0, "CortexKit", St::B);
    c.right(c.w - 1, 0, &v.clock(), St::D);
    c.right(c.w - 8, 0, "daemon not running", St::W);
    c.hline(0, 1, c.w);

    // The message is an 18-row block centered between the top bar and the
    // footer.
    let top = ((c.h - 2 - 18) / 2).max(2);
    c.center(0, c.w, top, "○", St::D.bold());
    c.center(0, c.w, top + 2, "The CortexKit daemon isn't running", St::B);
    c.center(
        0,
        c.w,
        top + 4,
        "Nothing is supervising your modules right now, so agents can't reach",
        St::D,
    );
    c.center(0, c.w, top + 5, "AFT, Magic Context or the vault.", St::D);

    let x = ((c.w - 44) / 2).max(0);
    let stopped = v.civil(d.stopped_at);
    let rows = [
        (
            "Stopped",
            format!(
                "{} {} · {}",
                v.day_word(d.stopped_at),
                stopped.hms(),
                d.stopped_by
            ),
            St::NONE,
        ),
        ("Last line", d.last_line.clone(), St::F),
        ("Service", d.service.clone(), St::NONE),
    ];
    for (i, (k, val, st)) in rows.iter().enumerate() {
        let y = top + 8 + i as i32;
        c.put(x, y, &pad_right(k, 12), St::D);
        c.put(x + 12, y, val, *st);
    }

    let by = top + 13;
    c.put(x, by, " ⏎  Start it ", St::fg(Fg::OnAcc).on(Bg::Acc).bold());
    c.put(x + 16, by, " l  Show the log ", St::bg(Bg::Key));
    c.put(x + 36, by, " q  Quit ", St::bg(Bg::Key));
    c.center(
        0,
        c.w,
        top + 17,
        "ck reconnects by itself as soon as the daemon is back.",
        St::F,
    );

    c.hline(0, c.h - 2, c.w);
    footer(c, &[("⏎", "start"), ("l", "log"), ("q", "quit")], "", St::D);
}
