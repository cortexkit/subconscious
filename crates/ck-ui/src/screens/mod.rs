//! The app's screens, each drawn cell by cell from a [`View`].

pub mod activity;
pub mod agents;
pub mod devices;
pub mod down;
pub mod home;
pub mod logs;
pub mod metrics;
pub mod modules;
pub mod overlays;
pub mod settings;
pub mod usage;

use crate::draw::{chrome, frame_end, trunc, Canvas};
use crate::model::{Freshness, Panel, Snapshot, Tab};
use crate::theme::{Bg, Fg, St};
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
    /// Account identifiers are hidden: Usage names each account by the
    /// pseudonym `ck quota --redact` prints, and Home's usage panel masks
    /// any identifier quoted in a provider's error.
    pub redact: bool,
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

/// The text shown for a value no source has reported.
pub const UNKNOWN: &str = "unknown";

/// How a panel's heading describes its data: the usual meta when fresh, the
/// age in amber past twice the refresh interval, "last good reading" past
/// five times, or the source's error while it is failing (with the age and
/// "last good reading" as the data behind it grows old).
#[derive(Clone, Debug, PartialEq)]
pub struct Meta {
    pub text: String,
    pub st: St,
    /// Whether the panel's values are a last good reading and draw dimmed.
    pub dim: bool,
}

impl Meta {
    pub fn plain(text: &str) -> Meta {
        Meta {
            text: text.to_string(),
            st: St::D,
            dim: false,
        }
    }
}

impl View<'_> {
    /// The heading meta for `panel`, given what it says when all is well.
    pub fn meta<T>(&self, panel: &Panel<T>, normal: &str) -> Meta {
        let fresh = panel.freshness(self.now);
        let stale = fresh == Freshness::Stale;
        let age = || ago(self.age(panel.received_at));
        if let Some(err) = &panel.error {
            let text = match (panel.loaded, fresh) {
                (true, Freshness::Stale) => format!("{err} · last good reading · {} ago", age()),
                (true, Freshness::Aging) => format!("{err} · {} ago", age()),
                _ => err.clone(),
            };
            return Meta {
                text,
                st: St::W,
                dim: stale,
            };
        }
        match fresh {
            Freshness::Fresh => Meta::plain(normal),
            Freshness::Aging => Meta {
                text: format!("{} ago", age()),
                st: St::W,
                dim: false,
            },
            Freshness::Stale => Meta {
                text: format!("last good reading · {} ago", age()),
                st: St::W,
                dim: true,
            },
        }
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

/// CPU percent, or "unknown".
pub fn opt_cpu(c: Option<f64>) -> String {
    c.map_or_else(|| UNKNOWN.to_string(), fmt_cpu)
}

/// Memory, or "unknown".
pub fn opt_mem(mb: Option<f64>) -> String {
    mb.map_or_else(|| UNKNOWN.to_string(), fmt_mem)
}

/// A count, or "unknown".
pub fn opt_count(n: Option<u32>) -> String {
    n.map_or_else(|| UNKNOWN.to_string(), |n| n.to_string())
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

/// How long the line reporting an action's result stays on screen.
pub const RESULT_SECS: f64 = 12.0;

/// The line reporting what the last action actually did, above the footer
/// at the right, for [`RESULT_SECS`] after it finished.
pub fn result_toast(c: &mut Canvas, v: &View) {
    let Some(r) = &v.snap.last_result else {
        return;
    };
    let age = v.age(r.at);
    if !(0.0..=RESULT_SECS).contains(&age) {
        return;
    }
    let (glyph, gst) = if r.ok {
        ("✓", St::fg(Fg::Ok).on(Bg::Key))
    } else {
        ("✕", St::fg(Fg::Err).on(Bg::Key))
    };
    let text = trunc(&r.text, (c.w - 10).max(0) as usize);
    let start = c.right(
        c.w - 2,
        c.h - 3,
        &format!(" {glyph} {text} "),
        St::bg(Bg::Key),
    );
    c.put(start + 1, c.h - 3, glyph, gst);
}

/// The note drawn instead of a screen when the window is too small.
pub fn too_small(c: &mut Canvas) {
    let y = c.h / 2;
    c.center(0, c.w, y, "Make the window at least 80×24", St::D);
}

/// A tab whose screen isn't built yet: the tab bar, a note saying so, and
/// the footer rule with no hints, so it offers no key it can't take.
pub fn unbuilt(c: &mut Canvas, v: &View, tab: Tab) {
    chrome(
        c,
        tab,
        v.snap.visible_tabs(),
        &v.clock(),
        &v.snap.host.data.label,
    );
    let note = format!("{} has no screen in this build", tab.name());
    c.center(0, c.w, c.h / 2, &note, St::D);
    frame_end(c, &[], "", St::D);
}
