//! The app's screens, each drawn cell by cell from a [`View`].

pub mod down;
pub mod home;
pub mod modules;
pub mod overlays;

use crate::draw::Canvas;
use crate::model::{Panel, Snapshot};
use crate::theme::St;
use crate::time::{ago, js_round, to_fixed, Civil, Timestamp};

/// The smallest window the app draws in.
pub const MIN_W: i32 = 80;
pub const MIN_H: i32 = 24;

/// What a renderer needs besides the canvas: the data, the time to format it
/// against, and the selection state.
#[derive(Clone, Copy)]
pub struct View<'a> {
    pub snap: &'a Snapshot,
    pub now: Timestamp,
    /// Seconds since the app started; drives the live dot and moves the
    /// sparkline windows along.
    pub tick: u64,
    pub home_sel: usize,
    pub mod_sel: usize,
}

impl View<'_> {
    /// A time as the host's wall clock reads it.
    pub fn civil(&self, t: Timestamp) -> Civil {
        t.civil(self.snap.host.data.utc_offset)
    }

    pub fn clock(&self) -> String {
        self.civil(self.now).hm()
    }

    /// Seconds from `t` until now.
    pub fn age(&self, t: Timestamp) -> f64 {
        self.now.secs_since(t)
    }

    /// The footer's note on how fresh a panel is.
    pub fn updated<T>(&self, panel: &Panel<T>) -> String {
        let age = self.age(panel.received_at);
        if age < 5.0 {
            "updated just now".to_string()
        } else {
            format!("updated {} ago", ago(age))
        }
    }

    /// "today", "yesterday", or the date, for `t` relative to now.
    pub fn day_word(&self, t: Timestamp) -> String {
        let c = self.civil(t);
        match self.civil(self.now).days - c.days {
            0 => "today".to_string(),
            1 => "yesterday".to_string(),
            _ => format!("{:04}-{:02}-{:02}", c.year, c.month, c.day),
        }
    }

    /// Where a sparkline window starts: it moves one sample a second and
    /// wraps every 20 seconds.
    pub fn spark_offset(&self) -> usize {
        (self.tick % 20) as usize
    }
}

/// CPU percent: one decimal below 10%, none from 10%.
pub fn fmt_cpu(c: f64) -> String {
    format!("{}%", to_fixed(c, if c >= 10.0 { 0 } else { 1 }))
}

/// Memory in MB, or GB with one decimal from 1024 MB.
pub fn fmt_mem(mb: f64) -> String {
    if mb >= 1024.0 {
        format!("{} GB", to_fixed(mb / 1024.0, 1))
    } else {
        format!("{} MB", js_round(mb))
    }
}

/// A used fraction as a whole percent.
pub fn pct(frac: f64) -> String {
    format!("{}%", js_round(frac * 100.0))
}

/// `n` items, with the noun made plural unless `n` is 1.
pub fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// `n` values of `values` from `start`, or fewer at its end.
pub fn window(values: &[f64], start: usize, n: usize) -> &[f64] {
    let a = start.min(values.len());
    let b = start.saturating_add(n).min(values.len());
    &values[a..b]
}

/// A sparkline's samples: `n` of them from `offset`, with the offset pulled
/// back so the window never runs past the end of the history and the line
/// keeps its width as the offset moves.
pub fn spark_window(values: &[f64], offset: usize, n: usize) -> &[f64] {
    let n = n.min(values.len());
    window(values, offset.min(values.len() - n), n)
}

/// The note drawn instead of a screen when the window is too small.
pub fn too_small(c: &mut Canvas) {
    let y = c.h / 2;
    c.center(0, c.w, y, "Make the window at least 80×24", St::D);
}
