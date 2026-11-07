//! Named-computer picker, pairing ceremony and details frames.

use crate::computer::{platform, short_id, Flow, Session, Watch};
use crate::draw::{chrome, frame_end, heading, pad_right, trunc, Canvas};
use crate::model::{Status, Tab};
use crate::theme::{Bg, St};

use super::View;

fn spinner(tick: u64) -> &'static str {
    ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][(tick % 10) as usize]
}

fn button(c: &mut Canvas, x: i32, y: i32, text: &str, primary: bool) {
    c.put(
        x,
        y,
        text,
        if primary {
            St::fg(crate::theme::Fg::OnAcc).bold().on(Bg::Acc)
        } else {
            St::NONE.on(Bg::Key)
        },
    );
}

pub fn picker(c: &mut Canvas, v: &View, flow: &Flow) {
    c.dim_all();
    let x = (c.w - 104) / 2;
    let y = (c.h - 30) / 2;
    c.boxed(x, y, 104, 28, "Add a computer");
    let l = x + 4;
    let top = y + 5;
    let by = y + 21;
    let mut sx = l;
    let at = flow.session.as_ref().map_or(0, |s| match s.state {
        Watch::Waiting | Watch::Connecting | Watch::Expired { .. } => 1,
        Watch::Paired { .. } => 3,
        _ => 2,
    });
    let ended = flow
        .session
        .as_ref()
        .is_some_and(|s| s.state.terminal() && !matches!(s.state, Watch::Paired { .. }));
    for (i, text) in [
        "Choose the computer",
        "Start pairing on it too",
        "Compare the code",
    ]
    .iter()
    .enumerate()
    {
        sx = c.put(
            sx,
            y + 2,
            &if i < at {
                "✓".into()
            } else {
                (i + 1).to_string()
            },
            if i < at {
                St::O.bold()
            } else if i == at && !ended {
                St::A.bold()
            } else {
                St::F
            },
        );
        sx = c.put(
            sx + 1,
            y + 2,
            text,
            if i == at && !ended { St::B } else { St::D },
        ) + 5;
    }
    c.hline(x + 2, y + 3, 100);
    if let Some(session) = &flow.session {
        ceremony(c, v, flow, session, l, top, by);
        return;
    }
    if flow.list_failed {
        if let Some(error) = &flow.error {
            c.put(l, top, &trunc(error, 96), St::W);
        }
        button(c, l, by, " esc  Close ", false);
        return;
    }
    if flow.waiting {
        c.put(l, top, spinner(v.tick), St::A);
        c.put(
            l + 2,
            top,
            "Waiting for your account's device list",
            St::NONE,
        );
        c.put(
            l + 2,
            top + 1,
            "This computer hasn't received it yet. It arrives once this computer is signed in",
            St::D,
        );
        c.put(
            l + 2,
            top + 2,
            "and online, and ck checks again every few seconds.",
            St::D,
        );
    } else if flow.error.is_some() && flow.candidates.is_empty() {
        // A failed read is never presented as an empty account.
    } else if flow.candidates.is_empty() {
        c.put(l, top, "There's no other computer on your account.", St::B);
        c.put(
            l,
            top + 2,
            "Sign in to CortexKit on the other computer with the same account. It shows up here",
            St::D,
        );
        c.put(
            l,
            top + 3,
            "once it has. To add a phone, use Add a phone on Devices.",
            St::D,
        );
    } else {
        c.put(
            l,
            top,
            "Choose the computer to pair with. It has to be signed in to your account too.",
            St::NONE,
        );
        for (dx, text) in [(2, "Computer"), (28, "System"), (40, "Id"), (52, "Status")] {
            c.put(l + dx, top + 2, text, St::D);
        }
        c.hline(l, top + 3, 96);
        let selected = flow
            .candidates
            .iter()
            .position(|d| Some(&d.fingerprint) == flow.selected.as_ref());
        let (first, count, _) = super::agents::window(flow.candidates.len(), 7, selected);
        for (i, d) in flow.candidates.iter().enumerate().skip(first).take(count) {
            let row = top + 4 + (i - first) as i32;
            let available = d.state == "available";
            let sel = Some(&d.fingerprint) == flow.selected.as_ref();
            c.put(
                l + 2,
                row,
                &pad_right(&trunc(&d.name, 24), 24),
                if !available {
                    St::D
                } else if sel {
                    St::B
                } else {
                    St::NONE
                },
            );
            c.put(
                l + 28,
                row,
                platform(&d.platform),
                if available { St::NONE } else { St::D },
            );
            c.put(l + 40, row, &short_id(&d.fingerprint), St::D);
            let (word, style) = match d.state.as_str() {
                "available" => ("not paired yet", St::NONE),
                "paired" => ("already paired", St::D),
                _ => ("unavailable", St::F),
            };
            c.put(l + 52, row, word, style);
            if sel {
                c.paint(l - 1, row, 98, St::bg(Bg::Sel));
            }
        }
        c.put(
            l,
            top + 5 + count as i32,
            "Two computers can have the same name. The system and id tell them apart.",
            St::F,
        );
        if let Some(error) = &flow.error {
            c.put(l, top + 7 + count as i32, &trunc(error, 96), St::W);
        }
        if let Some(message) = &flow.refreshed {
            c.put(l, top + 7 + count as i32, "▲", St::W);
            c.put(l + 2, top + 7 + count as i32, message, St::W);
        }
        button(c, l, by, " ⏎  Pair ", true);
    }
    button(
        c,
        l + if flow.candidates.is_empty() { 0 } else { 11 },
        by,
        " esc  Close ",
        false,
    );
}

fn countdown(deadline: i64, now: i64) -> String {
    let seconds = ((deadline - now).max(0) + 999) / 1000;
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

fn ceremony(c: &mut Canvas, v: &View, flow: &Flow, session: &Session, l: i32, top: i32, by: i32) {
    let n = &session.target.name;
    let px = c.put(l, top, "Pairing with", St::D);
    let px = c.put(px + 1, top, n, St::B);
    c.put(
        px,
        top,
        &format!(
            " · {} · id {}",
            platform(&session.target.platform),
            short_id(&session.target.fingerprint)
        ),
        St::D,
    );
    let y = top + 2;
    let now = v.now.unix_millis();
    match &session.state {
        Watch::Waiting | Watch::Connecting => {
            let waiting = matches!(session.state, Watch::Waiting);
            c.put(l, y, spinner(v.tick), St::A);
            c.put(
                l + 2,
                y,
                &format!(
                    "{} {n}…",
                    if waiting {
                        "Waiting for"
                    } else {
                        "Connecting to"
                    }
                ),
                St::NONE,
            );
            c.put(
                l + 2,
                y + 1,
                &format!("stops in {}", countdown(session.opened.expires_at_ms, now)),
                St::D,
            );
            c.put(l, y + 3, &format!("Start pairing on {n} too"), St::B);
            let me = v
                .snap
                .devices
                .data
                .inventory
                .account_name
                .as_deref()
                .unwrap_or("this computer");
            c.put(
                l,
                y + 4,
                &format!("On {n}, open ck, go to Devices, press c and choose {me}."),
                St::NONE,
            );
            let id = v
                .snap
                .devices
                .data
                .inventory
                .fingerprint
                .as_deref()
                .map(short_id)
                .unwrap_or_else(|| "unknown".into());
            c.put(
                l,
                y + 5,
                &format!("Its id there is {id}, the Pairing id on this computer's Devices."),
                St::D,
            );
            c.put(
                l,
                y + 6,
                "Pairing works only while both computers are doing it.",
                St::D,
            );
            if waiting {
                c.put(l, y + 8, &format!("{n} can't be reached yet. Check that it's turned on and online. The two computers"), St::D);
                c.put(l, y + 9, "connect over your network, or through the relay when they're on different networks.", St::D);
            } else {
                c.put(
                    l,
                    y + 8,
                    &format!("{n} can be reached. The code appears once it is pairing too."),
                    St::D,
                );
            }
            button(c, l, by, " esc  Cancel ", false);
        }
        Watch::Code {
            code,
            confirm_by_ms,
        }
        | Watch::PeerConfirmed {
            code,
            confirm_by_ms,
        } => {
            let deadline = format!(
                "If no one answers, pairing stops in {}.",
                countdown(*confirm_by_ms, now)
            );
            c.put(l, y, "Both screens must show the same code.", St::B);
            for (i, group) in [code.get(..3).unwrap_or(""), code.get(3..).unwrap_or("")]
                .iter()
                .enumerate()
            {
                let gx = l + i as i32 * 14;
                c.fill(gx, y + 2, 11, 3, ' ', St::bg(Bg::Key));
                c.put(
                    gx + 3,
                    y + 3,
                    &group
                        .chars()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(" "),
                    if session.answered {
                        St::D.on(Bg::Key)
                    } else {
                        St::B.on(Bg::Key)
                    },
                );
            }
            if session.answered {
                c.put(l, y + 6, "✓", St::O.bold());
                c.put(l + 2, y + 6, "You said they match.", St::NONE);
                c.put(l, y + 8, spinner(v.tick), St::A);
                c.put(l + 2, y + 8, "Waiting for the other computer", St::NONE);
                c.put(l + 2, y + 9, &format!("{n} still has to answer. Pairing finishes when both computers say they match."), St::D);
                c.put(l + 2, y + 10, &deadline, St::D);
                button(c, l, by, " esc  Cancel ", false);
            } else {
                c.put(
                    l,
                    y + 6,
                    &format!("Does {n} show {} {} too?", &code[..3], &code[3..]),
                    St::NONE,
                );
                c.put(l, y + 7, "If the codes differ, someone may be between the two computers: choose They don't match.", St::D);
                if session.confirming {
                    c.put(l, by, spinner(v.tick), St::A);
                    c.put(
                        l + 2,
                        by,
                        "Waiting for confirmation on this computer",
                        St::NONE,
                    );
                    c.put(
                        l + 2,
                        by + 1,
                        "If this computer shows a prompt, answer it there.",
                        St::D,
                    );
                } else {
                    button(c, l, by, " y  They match ", true);
                    button(c, l + 17, by, " n  They don't match ", false);
                    button(c, l + 40, by, " esc  Cancel ", false);
                    c.put(
                        l,
                        by + 2,
                        "After y, this computer asks you to confirm.",
                        St::D,
                    );
                    c.put(
                        l,
                        by + 3,
                        "If it has no system prompt, y itself is your confirmation.",
                        St::D,
                    );
                }
                c.put(l, by + 4, &deadline, St::F);
            }
        }
        state => {
            let paired = matches!(state, Watch::Paired { .. });
            let (glyph, style, title, lines) = match state {
                Watch::Paired { name, .. } => ("✓", St::O.bold(), format!("{name} is paired."), vec![format!("The two computers are connected. Nothing is shared yet: {n} can't use anything on"), "this computer until you turn on a switch in its details.".into()]),
                Watch::Declined { peer } => ("○", St::D.bold(), format!("{} answered They don't match. Nothing was paired.", if *peer { n } else { "You" }), vec!["If the codes really differed, someone may have been between the two computers.".into(), "Make sure both computers are yours before you start again.".into()]),
                Watch::Expired { peer: true } => ("○", St::D.bold(), format!("{n} stopped pairing. Nothing was paired."), vec![format!("Pairing was cancelled on {n}, or ran out of time there.")]),
                Watch::Expired { peer: false } => ("○", St::D.bold(), "Pairing timed out. Nothing was paired.".into(), vec![format!("{n} didn't connect within 5 minutes, or no one answered the code within 2 minutes."), "Starting or cancelling a pairing elsewhere on this computer also ends this one.".into()]),
                _ => ("✕", St::E.bold(), "Pairing failed, start again".into(), vec!["Nothing was paired, and this attempt can't be picked up again. Start pairing again on".into(), "both computers.".into()]),
            };
            c.put(l, y, glyph, style);
            c.put(l + 2, y, &title, St::B);
            let mut ly = y + 2;
            if let Watch::Failed { reason } = state {
                if let Some(reason) = failure_reason(reason, n) {
                    c.put(l + 2, ly, &reason, St::NONE);
                    ly += 2;
                }
            }
            for (i, line) in lines.iter().enumerate() {
                c.put(l + 2, ly + i as i32, line, St::D);
            }
            if matches!(state, Watch::Paired { typed: true, .. }) {
                c.put(
                    l + 2,
                    ly + lines.len() as i32 + 1,
                    "This computer has no system prompt, so your y was the confirmation.",
                    St::NONE,
                );
            }
            if paired {
                button(c, l, by, " ⏎  Open details ", true);
                button(c, l + 19, by, " esc  Done ", false);
            } else {
                button(c, l, by, " ⏎  Start again ", true);
                button(c, l + 18, by, " esc  Close ", false);
            }
        }
    }
    if let Some(error) = &flow.error {
        c.put(l, by - 1, &trunc(error, 96), St::W);
    }
}

pub fn failure_reason(reason: &str, name: &str) -> Option<String> {
    Some(match reason {
        "commit_mismatch" => "The check behind the code failed. This can mean someone is between the two computers.".into(),
        "protocol" => format!("{name} didn't follow the pairing steps. Check that ck is up to date on both computers."),
        "device_changed" => format!("{name} was removed, replaced or set up again on your account while pairing."),
        "confirm_failed" => "This computer's confirmation was refused or didn't work.".into(),
        "peer_failed" => format!("{name} stopped pairing, or the connection to it dropped."),
        "interrupted" => "Pairing was interrupted on this computer.".into(),
        _ => return None,
    })
}

pub fn details(c: &mut Canvas, v: &View, flow: &Flow) {
    let Some(id) = &flow.detail else {
        return;
    };
    chrome(
        c,
        Tab::Devices,
        v.snap.visible_tabs(),
        &v.clock(),
        &v.snap.host.data.label,
    );
    let Some(d) = v
        .snap
        .devices
        .data
        .list
        .iter()
        .find(|d| d.fingerprint.as_ref() == Some(id))
        .or_else(|| {
            flow.remembered
                .as_ref()
                .filter(|(d, _)| flow.forget && d.fingerprint.as_ref() == Some(id))
                .map(|(d, _)| d)
        })
    else {
        c.put(
            2,
            3,
            "This computer is no longer in the paired list.",
            St::D,
        );
        frame_end(c, &[("esc", "back")], "from callosum", St::D);
        return;
    };
    let info = v.snap.devices.data.inventory.computers.get(id).or_else(|| {
        flow.remembered
            .as_ref()
            .filter(|(d, _)| flow.forget && d.fingerprint.as_ref() == Some(id))
            .map(|(_, info)| info)
    });
    c.put(2, 3, "←", St::D);
    let nx = c.put(4, 3, &d.name, St::B);
    c.put(
        nx + 1,
        3,
        &format!(
            "· computer{} · id {}",
            info.and_then(|i| i.platform.as_deref())
                .map(|s| format!(" · {}", platform(s)))
                .unwrap_or_default(),
            short_id(id)
        ),
        St::D,
    );
    c.right(
        c.w - 2,
        3,
        if d.status == Status::Ok {
            "● connected"
        } else {
            "○ disconnected"
        },
        if d.status == Status::Ok { St::O } else { St::D },
    );
    c.hline(2, 4, c.w - 4);
    heading(c, 2, 6, 54, "Status", "");
    let since = info.and_then(|i| i.since).map(|ms| {
        let at = crate::time::Timestamp::from_unix_millis(ms);
        format!("{} {}", v.day_word(at), v.civil(at).hm())
    });
    for (y, key, value) in [
        (8, "Connection", d.path.as_str()),
        (9, "Connected since", since.as_deref().unwrap_or("")),
        (10, "Last seen", d.seen.as_str()),
    ] {
        if value.is_empty() {
            continue;
        }
        c.put(2, y, &pad_right(key, 18), St::D);
        c.put(20, y, value, St::NONE);
    }
    if info.and_then(|i| i.pairing.as_ref()).is_some() {
        heading(c, 62, 6, c.w - 64, "Pairing", "");
    }
    match info.and_then(|i| i.pairing.as_deref()) {
        Some("confirmed_both") => {
            c.put(62, 8, "●", St::O);
            c.put(64, 8, "Paired on both computers", St::NONE);
            c.put(
                64,
                9,
                &format!("{} reported that it finished pairing too.", d.name),
                St::D,
            );
        }
        Some("granted_here") => {
            c.put(62, 8, "◌", St::D);
            c.put(64, 8, "Paired on this computer", St::NONE);
            c.put(
                64,
                9,
                &format!("{} hasn't reported that it finished pairing.", d.name),
                St::D,
            );
            c.put(
                64,
                10,
                "If they can't connect, forget it on both computers",
                St::D,
            );
            c.put(64, 11, "and pair them again.", St::D);
        }
        _ => {}
    }
    sharing(c, v, flow, id, &d.name);
    if v.snap.serves == crate::model::Serves::DESIGN {
        heading(c, 2, 27, c.w - 4, "Recent", "today");
        c.put(2, 29, "20:52", St::D);
        c.put(9, 29, "connected directly over the LAN", St::NONE);
        c.put(2, 30, "20:51", St::D);
        c.put(9, 30, "paired on this computer", St::NONE);
    }
    frame_end(
        c,
        &[
            ("⏎", "on or off"),
            ("↑↓", "move"),
            ("x", "forget"),
            ("esc", "back"),
        ],
        "from callosum",
        St::D,
    );
    if flow.forget {
        c.dim_all();
        let x = (c.w - 76) / 2;
        let y = (c.h - 18) / 2;
        let l = x + 3;
        c.boxed(x, y, 76, 11, &format!("Forget {}", d.name));
        if flow.partial {
            c.put(l, y + 2, "▲", St::W);
            c.put(
                l + 2,
                y + 2,
                &format!("{} is only partly forgotten · x tries again", d.name),
                St::B,
            );
            c.put(
                l + 2,
                y + 3,
                "Forgetting didn't finish on this computer.",
                St::D,
            );
        } else {
            c.put(
                l,
                y + 2,
                &format!(
                    "This computer stops trusting {} and turns off every switch for",
                    d.name
                ),
                St::NONE,
            );
            c.put(l, y + 3, "it. The two computers stop connecting.", St::NONE);
        }
        c.put(
            l,
            y + 5,
            &format!(
                "{} still lists this computer until you forget it there too.",
                d.name
            ),
            St::D,
        );
        if let Some(error) = &flow.error {
            c.put(l, y + 6, &trunc(error, 70), St::W);
        }
        if flow.busy {
            c.put(l, y + 8, "Forgetting…", St::D);
        } else if flow.partial {
            button(c, l, y + 8, " x  Try again ", true);
            button(c, l + 16, y + 8, " esc  Close ", false);
        } else {
            button(c, l, y + 8, &format!(" x  Forget {} ", d.name), true);
            button(c, l + 23, y + 8, " esc  Cancel ", false);
        }
    }
}

fn sharing(c: &mut Canvas, v: &View, flow: &Flow, id: &str, name: &str) {
    let shares = flow.shares(&v.snap.devices.data);
    let selected = flow.share_index(&v.snap.devices.data);
    heading(
        c,
        2,
        13,
        c.w - 4,
        "Sharing",
        &format!(
            "{} of {} on",
            shares.iter().filter(|s| s.enabled).count(),
            shares.len()
        ),
    );
    if shares.is_empty() {
        return;
    }
    c.put(4, 15, "Switch", St::D);
    c.put(12, 15, &format!("What {name} can do"), St::D);
    c.put(52, 15, "Module", St::D);
    c.hline(2, 16, c.w - 4);
    let (first, count, _) = super::agents::window(shares.len(), (c.h - 27).max(1), Some(selected));
    for (i, s) in shares.iter().enumerate().skip(first).take(count) {
        let y = 17 + (i - first) as i32;
        let sel = i == selected;
        let pending = flow.share_pending.as_ref().is_some_and(|(_, p)| {
            p.fingerprint == id && p.module == s.module && p.share == s.share && p.enabled
        });
        c.put(
            4,
            y,
            if s.enabled { "◉ on" } else { "○ off" },
            if s.enabled { St::O } else { St::D },
        );
        c.put(
            12,
            y,
            &trunc(&s.label, 38),
            if sel { St::B } else { St::NONE },
        );
        c.put(52, y, &s.module, St::D);
        if pending {
            c.put(68, y, spinner(v.tick), St::A);
            c.put(70, y, "waiting for confirmation on this computer", St::A);
        } else if let Some(error) = flow.share_problem(id, s) {
            c.put(68, y, &trunc(&error, (c.w - 69).max(0) as usize), St::W);
        } else if s.needs_review {
            c.put(68, y, "▲ needs review", St::W);
        }
        if sel {
            c.paint(1, y, c.w - 3, St::bg(Bg::Sel));
        }
    }
    let mut y = 18 + count as i32;
    if let Some(review) = shares.iter().find(|s| s.needs_review) {
        c.put(
            4,
            y,
            &format!(
                "{} added operations to \"{}\". They stay off until you turn it on again.",
                review.module, review.label
            ),
            St::D,
        );
        y += 1;
    }
    if let Some((_, pending)) = &flow.share_pending {
        if pending.fingerprint == id && pending.enabled {
            c.fill(1, y, c.w - 2, 1, ' ', St::bg(Bg::AccBg));
            c.put(2, y, spinner(v.tick), St::A.on(Bg::AccBg));
            c.put(4, y, &format!("Turning on \"{}\" for {name}. If this computer shows a prompt, answer it there.", pending.label), St::bg(Bg::AccBg));
        }
    } else if let Some((call, error, _)) = &flow.share_error {
        if call.fingerprint == id && error.code == "fed_operator_confirmation_refused" {
            c.put(
                4,
                y,
                &format!("Nothing changed: \"{}\" is still off.", call.label),
                St::D,
            );
        }
    }
    y += 2;
    c.put(4, y, "Turning a switch on asks you to confirm on this computer. Turning one off happens at once.", St::F);
    c.put(
        4,
        y + 1,
        "If this computer has no system prompt, ⏎ itself is your confirmation.",
        St::F,
    );
}
