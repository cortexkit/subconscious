//! Paired devices and the addresses and feeds this Mac offers.
//!
//! The list keeps three rows before dropping either panel below it, lowest
//! first. When it overflows, its last row counts every device not on screen.
//! Account, Vault, Recent and the unsupported action hints are design-only:
//! callosum has no live source or operation for them.

use crate::draw::{chrome, frame_end, heading, heading_st, pad_right, trunc, width, Canvas};
use crate::model::{Daemon, Device, Serves, Status, Tab};
use crate::theme::{Bg, St};
use crate::time::ago;

use super::{View, UNKNOWN};

pub const LIST_TOP: i32 = 14;
pub const MIN_LIST: i32 = 3;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DevicesState {
    pub sel: Option<usize>,
}

/// Column starts, with the extra width going to the device's name.
#[derive(Clone, Copy, Debug)]
pub struct Layout {
    pub kind: i32,
    pub connection: i32,
    pub feeds: i32,
    pub seen: i32,
    pub fingerprint: Option<i32>,
    pub trust: i32,
    pub right: i32,
}

pub fn layout(w: i32) -> Layout {
    if w >= 150 {
        let extra = w - 150;
        Layout {
            kind: 47 + extra,
            connection: 58 + extra,
            feeds: 70 + extra,
            seen: 90 + extra,
            fingerprint: Some(118 + extra),
            trust: 136 + extra,
            right: w - 2,
        }
    } else {
        let kind = if w >= 120 {
            w - 94
        } else {
            4 + (w - 98).clamp(12, 22)
        };
        let connection = kind + if w < 100 { 11 } else { 14 };
        let feeds = connection + if w < 100 { 12 } else { 18 };
        Layout {
            kind,
            connection,
            feeds,
            seen: feeds + if w < 100 { 14 } else { 18 },
            fingerprint: None,
            trust: 0,
            right: w - 2,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Place {
    pub list: i32,
    pub phone: Option<i32>,
    pub recent: Option<i32>,
}

/// Each lower panel includes its two leading blank rows. Keeping that
/// height makes an overflowing list end at the same gap as a short list.
pub fn place(h: i32, n: usize, design: bool) -> Place {
    let avail = (h - 2 - LIST_TOP).max(0);
    let phone_h = if avail >= MIN_LIST + 3 { 3 } else { 0 };
    let recent_h = if design && avail >= MIN_LIST + phone_h + 9 {
        9
    } else {
        0
    };
    let list = (n.max(1) as i32).min(avail - phone_h - recent_h);
    let phone = (phone_h > 0).then_some(LIST_TOP + list + 2);
    let recent = (recent_h > 0).then_some(LIST_TOP + list + phone_h + 2);
    Place {
        list,
        phone,
        recent,
    }
}

/// Joins only whole values. The suffix counts every value left out rather
/// than silently cutting the final one in the middle.
pub fn values(values: &[String], room: usize) -> String {
    if values.is_empty() {
        return "—".into();
    }
    for n in (0..=values.len()).rev() {
        let mut text = values[..n].join(", ");
        if n < values.len() {
            if n > 0 {
                text.push(' ');
            }
            text.push_str(&format!("+{} more", values.len() - n));
        }
        if width(&text) <= room {
            return text;
        }
    }
    String::new()
}

pub fn draw(c: &mut Canvas, v: &View, state: &DevicesState) {
    let snap = v.snap;
    let design = snap.serves == Serves::DESIGN;
    let devices = &snap.devices;
    chrome(
        c,
        Tab::Devices,
        snap.visible_tabs(),
        &v.clock(),
        &snap.host.data.label,
    );
    let rx = c.w / 2 + 2;
    heading(c, 2, 3, c.w / 2 - 6, "This Mac", "");
    heading(c, rx, 3, c.w - rx - 2, "Reachable as", "");
    let left_room = (rx - 18).max(0) as usize;
    let field = |c: &mut Canvas, y, label, value: &str| {
        c.put(2, y, &pad_right(label, 14), St::D);
        c.put(16, y, &trunc(value, left_room), St::NONE);
    };
    field(
        c,
        5,
        "Machine id",
        snap.host.data.machine_id.as_deref().unwrap_or(UNKNOWN),
    );
    let daemon = match &snap.daemon.data {
        Daemon::Running(d) => match d.started_at {
            Some(at) => format!("{} · up {}", d.version, ago(v.age(at))),
            None => d.version.clone(),
        },
        _ => UNKNOWN.into(),
    };
    field(c, 6, "Daemon", &daemon);
    if design {
        field(c, 7, "Account", "signed in · relay connected");
        field(c, 8, "Vault", "88 credentials · Touch ID protected");
    }
    let room = (c.w - rx - 16).max(0) as usize;
    let lan = devices
        .data
        .lan_addrs
        .as_ref()
        .map(|v| values(v, room))
        .unwrap_or_else(|| UNKNOWN.into());
    let offered = devices
        .data
        .watch_ops_offered
        .map(|n| format!("{n} offered to phones"))
        .unwrap_or_else(|| UNKNOWN.into());
    for (y, key, val) in [
        (5, "LAN", lan.as_str()),
        (
            6,
            "Relay",
            devices.data.rendezvous.as_deref().unwrap_or(UNKNOWN),
        ),
        (7, "Live feeds", offered.as_str()),
    ] {
        c.put(rx, y, key, St::D);
        c.put(rx + 14, y, &trunc(val, room), St::NONE);
    }
    let meta = v.meta(devices, "from callosum");
    let count = if devices.loaded {
        devices.data.list.len().to_string()
    } else {
        UNKNOWN.into()
    };
    heading_st(c, 2, 11, c.w - 4, "Paired devices", &count, St::D);
    let l = layout(c.w);
    let mut heads = vec![
        (4, "Device"),
        (l.kind, "Kind"),
        (l.connection, "Connection"),
        (l.feeds, "Live feeds"),
        (l.seen, "Last seen"),
    ];
    if let Some(x) = l.fingerprint {
        heads.extend([(x, "Fingerprint"), (l.trust, "Trust")]);
    }
    for (x, title) in heads {
        c.put(x, 13, title, St::D);
    }
    let at = place(c.h, devices.data.list.len(), design);
    let (first, k, hidden) = super::agents::window(devices.data.list.len(), at.list, state.sel);
    if devices.loaded {
        for (i, d) in devices.data.list.iter().enumerate().skip(first).take(k) {
            let y = LIST_TOP + (i - first) as i32;
            device_row(c, &l, y, d);
            if state.sel == Some(i) {
                c.paint(1, y, c.w - 3, St::bg(Bg::Sel));
            }
            if meta.dim {
                c.dim_rect(1, y, c.w - 2, 1);
            }
        }
        if hidden > 0 {
            c.put(2, LIST_TOP + k as i32, &format!("+{hidden} more"), St::D);
        }
        if devices.data.list.is_empty() {
            c.put(4, LIST_TOP, "No paired devices", St::D);
        }
    } else {
        c.put(
            4,
            LIST_TOP,
            if devices.error.is_some() {
                UNKNOWN
            } else {
                "Reading callosum…"
            },
            St::D,
        );
    }
    if let Some(y) = at.phone {
        c.put(4, y, "a", St::bg(Bg::Key).bold());
        c.put(6, y, "Add a phone", St::NONE);
        if design {
            c.put(22, y, "c", St::bg(Bg::Key).bold());
            c.put(24, y, "Add a computer", St::NONE);
        }
    }
    if let Some(y) = at.recent {
        recent(c, y);
    }
    let mut hints = vec![("a", "add a phone")];
    if design {
        hints.extend([("⏎", "details"), ("x", "forget")]);
    }
    hints.push(("↑↓", "move"));
    frame_end(c, &hints, &meta.text, meta.st);
}

fn device_row(c: &mut Canvas, l: &Layout, y: i32, d: &Device) {
    let (glyph, st) = if d.status == Status::Ok {
        ("●", St::O)
    } else {
        ("○", St::D)
    };
    c.put(2, y, glyph, st);
    c.put(
        4,
        y,
        &pad_right(&d.name, (l.kind - 5).max(0) as usize),
        St::B,
    );
    let seen_end = l.fingerprint.unwrap_or(l.right);
    for (x, end, text, st) in [
        (l.kind, l.connection, d.kind.as_str(), St::D),
        (
            l.connection,
            l.feeds,
            d.path.as_str(),
            if d.path == "—" { St::F } else { St::NONE },
        ),
        (
            l.feeds,
            l.seen,
            d.feeds.as_str(),
            if d.feeds == "—" { St::F } else { St::NONE },
        ),
        (
            l.seen,
            seen_end,
            d.seen.as_str(),
            if d.seen == "now" { St::NONE } else { St::D },
        ),
    ] {
        c.put(x, y, &trunc(text, (end - x - 1).max(0) as usize), st);
    }
    if let Some(x) = l.fingerprint {
        c.put(
            x,
            y,
            &trunc(
                d.fingerprint.as_deref().unwrap_or(UNKNOWN),
                (l.trust - x - 1) as usize,
            ),
            St::D,
        );
        c.put(
            l.trust,
            y,
            &trunc(
                d.trust.as_deref().unwrap_or(UNKNOWN),
                (l.right - l.trust - 1) as usize,
            ),
            St::D,
        );
    }
}

fn recent(c: &mut Canvas, y: i32) {
    heading(c, 2, y, c.w - 4, "Recent", "today");
    for (i, (t, who, what)) in [
        (
            "20:49",
            "iPhone",
            "opened live feeds: approvals, location, identity",
        ),
        ("20:41", "iPhone", "connected directly over the LAN"),
        ("18:02", "iPhone", "answered an approval for IRIS"),
        (
            "11:31",
            "CortexKit Mac 2",
            "reconnected after its daemon restarted",
        ),
        (
            "09:36",
            "CortexKit Mac 2",
            "connected directly over the LAN",
        ),
    ]
    .iter()
    .enumerate()
    {
        let row = y + 2 + i as i32;
        c.put(2, row, t, St::D);
        c.put(9, row, &pad_right(who, 17), St::B);
        c.put(27, row, &trunc(what, (c.w - 29).max(0) as usize), St::NONE);
    }
}
