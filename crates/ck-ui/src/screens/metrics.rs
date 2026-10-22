//! All of one module's metrics: every key the module reports, in full, with
//! a gap before each value, scrolled with the arrow keys. The Modules pane
//! shows only the few the module names as its headline; this is the rest.

use crate::draw::{chrome, frame_end, pad_right, trunc, width, Canvas};
use crate::model::{Module, Tab};
use crate::theme::St;
use crate::time::ago;

use super::View;

/// How many metric rows the screen has in a window `h` rows tall.
pub fn rows(h: i32) -> usize {
    (h - 8).max(0) as usize
}

pub fn draw(c: &mut Canvas, v: &View, m: &Module, scroll: usize) {
    chrome(
        c,
        Tab::Modules,
        v.snap.visible_tabs(),
        &v.clock(),
        &v.snap.host.data.label,
    );
    c.put(2, 3, "←", St::D);
    let x = c.put(4, 3, &m.id, St::B);
    c.put(x + 1, 3, "· all metrics", St::D);
    c.hline(2, 4, c.w - 4);

    let list = &m.all_metrics;
    if list.is_empty() {
        c.put(2, 5, "No metrics from this module", St::D);
    }
    // Keys take what they need, up to half the width, so each value has room
    // after a two-column gap.
    let key_w = list
        .iter()
        .map(|x| width(&x.label) as i32)
        .max()
        .unwrap_or(0)
        .min((c.w - 4) / 2)
        .max(1);
    let first = scroll.min(list.len().saturating_sub(rows(c.h)));
    for (i, metric) in list.iter().skip(first).take(rows(c.h)).enumerate() {
        let y = 5 + i as i32;
        c.put(2, y, &pad_right(&metric.label, key_w as usize), St::D);
        let vx = 2 + key_w + 2;
        c.put(
            vx,
            y,
            &trunc(&metric.value, (c.w - 2 - vx).max(0) as usize),
            St::NONE,
        );
    }

    let checked = m.health_checked_at.map_or_else(
        || "never checked".to_string(),
        |t| format!("checked {} ago", ago(v.age(t))),
    );
    let right = format!(
        "{} · {checked}",
        super::count(list.len(), "metric", "metrics")
    );
    frame_end(c, &[("↑↓", "scroll"), ("esc", "back")], &right, St::D);
}
