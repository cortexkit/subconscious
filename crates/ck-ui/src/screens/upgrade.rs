//! Settings → Updates: what the update check planned, the confirmed run's
//! stages as they go, and how the run ended.
//!
//! The section is a column of lines from row 3 down. When the window is too
//! short for all of them, the blank gaps go first, then the run's stages and
//! the plan's rows show a window ending "+N more".

use super::View;
use crate::draw::{frame_end, trunc, width, Canvas};
use crate::model::{Daemon, Serves, Status};
use crate::theme::{Bg, Fg, St};
use crate::upgrade::{
    stage_label, PlanRow, RollbackAnswer, RollbackQuestion, Run, StageState, StageView,
    UpgradeCheck,
};

/// The sentence that says what replacing the daemon does. It shows, and the
/// confirmation names it, only when the plan replaces the daemon.
pub const DAEMON_RESTART: &str = "Upgrading restarts the daemon, which restarts every module.";

/// The title when the check was refused before anything was checked.
pub const REFUSED: &str = "Updates can't be checked";

/// What the confirmation says the run will do: each component it replaces,
/// and the daemon restart when the plan replaces the daemon.
pub fn confirmation(check: &UpgradeCheck) -> Vec<String> {
    let replaced: Vec<String> = check
        .rows
        .iter()
        .filter(|r| r.replaces)
        .map(|r| {
            if r.installed.is_empty() {
                format!("{} → {}", r.component, r.new)
            } else {
                format!("{} {} → {}", r.component, r.installed, r.new)
            }
        })
        .collect();
    let mut body = vec![format!("Replaces {}.", replaced.join(", "))];
    if check.replaces_daemon() {
        body.push(DAEMON_RESTART.to_string());
    }
    body
}

/// "12 minutes", "1 hour": an age in its largest whole unit.
pub fn age_words(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    let (n, unit) = match s {
        0..=59 => (s, "second"),
        60..=3_599 => (s / 60, "minute"),
        3_600..=86_399 => (s / 3_600, "hour"),
        _ => (s / 86_400, "day"),
    };
    format!("{n} {unit}{}", if n == 1 { "" } else { "s" })
}

/// Splits `text` into lines of at most `w` columns without dropping or
/// changing a character, so a refusal or failure reads exactly as the engine
/// wrote it. A line breaks after a space where one fits.
pub fn wrap_verbatim(text: &str, w: usize) -> Vec<String> {
    let w = w.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        let mut rest: Vec<char> = line.chars().collect();
        loop {
            let mut used = 0;
            let mut cut = 0;
            let mut space = None;
            while cut < rest.len() {
                let cw = width(&rest[cut].to_string()).max(1);
                if used + cw > w {
                    break;
                }
                used += cw;
                if rest[cut] == ' ' {
                    space = Some(cut + 1);
                }
                cut += 1;
            }
            if cut < rest.len() {
                cut = space.filter(|s| *s > 0).unwrap_or(cut.max(1));
            }
            out.push(rest[..cut].iter().collect());
            rest.drain(..cut);
            if rest.is_empty() {
                break;
            }
        }
    }
    out
}

/// One line of the section.
enum Line<'a> {
    /// A blank line that goes first when the window is short.
    Gap,
    Text(String, St),
    /// The title, with the installed ck version on the right.
    Title(String, Option<String>),
    Header,
    Rule,
    Row(&'a PlanRow),
    Buttons,
    Stage(&'a StageView),
    /// "+N more" under a list cut short.
    More(usize),
}

/// Draws the Updates section from column `x`, and the footer.
pub fn draw(c: &mut Canvas, v: &View, x: i32) {
    let snap = v.snap;
    let design = snap.serves == Serves::DESIGN;
    let updates = &snap.upgrade;
    let dw = c.w - x - 3;
    let mut lines: Vec<Line> = Vec::new();
    // The lists that may show a window: the run's stages (keeping at least
    // three), then the plan's rows (keeping at least one).
    let mut lists = Vec::new();
    let running = updates.running();

    let checked = updates.check.as_ref();
    match checked.map(|c| &c.result) {
        _ if !updates.served => {
            lines.push(Line::Title("Updates".into(), None));
            lines.push(Line::Gap);
            lines.push(Line::Text(
                "This program can't check for updates.".into(),
                St::D,
            ));
        }
        None => {
            lines.push(Line::Title("Updates".into(), None));
            lines.push(Line::Gap);
            lines.push(Line::Text("Checking for updates…".into(), St::D));
        }
        Some(Err(error)) => {
            lines.push(Line::Title("Updates".into(), None));
            lines.push(Line::Gap);
            lines.push(Line::Text("Couldn't check for updates.".into(), St::E));
            for l in wrap_verbatim(error, dw as usize) {
                lines.push(Line::Text(l, St::D));
            }
        }
        // A refused check looked at nothing: no versions, no component
        // table and no "checked" age, only the refusal as the engine
        // words it.
        Some(Ok(UpgradeCheck {
            refusal: Some(refusal),
            ..
        })) => {
            lines.push(Line::Title(REFUSED.into(), None));
            lines.push(Line::Gap);
            for l in wrap_verbatim(refusal, dw as usize) {
                lines.push(Line::Text(l, St::W));
            }
        }
        Some(Ok(plan)) => {
            let ck = plan.ck();
            // Only a plan that checked every component and found nothing to
            // do reads as up to date.
            let title = match ck.filter(|r| r.replaces) {
                Some(r) => format!("CortexKit {} is available", r.new),
                None if plan.newer() => "Updates are available".to_string(),
                None => "CortexKit is up to date".to_string(),
            };
            let have = ck
                .filter(|r| !r.installed.is_empty())
                .map(|r| format!("you have {}", r.installed));
            lines.push(Line::Title(title, have));
            if design {
                lines.push(Line::Text(
                    "Released 2026-10-21 · signed by CortexKit · ⏎ release notes".into(),
                    St::D,
                ));
            } else {
                lines.push(Line::Gap);
            }
            lines.push(Line::Gap);
            lines.push(Line::Header);
            lines.push(Line::Rule);
            let start = lines.len();
            lines.extend(plan.rows.iter().map(Line::Row));
            let rows = start..lines.len();
            lines.push(Line::Gap);
            if plan.newer() {
                if plan.replaces_daemon() {
                    let sentence = if design {
                        format!("{DAEMON_RESTART} It takes about 40 s.")
                    } else {
                        DAEMON_RESTART.to_string()
                    };
                    lines.push(Line::Text(sentence, St::NONE));
                }
                lines.push(Line::Text(
                    if design {
                        "Running agents pause and pick up where they left off. Nothing is downgraded."
                    } else {
                        "Nothing is downgraded."
                    }
                    .into(),
                    St::D,
                ));
            }
            match &updates.run {
                Some(run) => {
                    lines.push(Line::Gap);
                    let start = lines.len();
                    lines.extend(run.stages.iter().map(Line::Stage));
                    let current = run
                        .stages
                        .iter()
                        .position(|s| !matches!(s.state, StageState::Done(_)))
                        .unwrap_or(run.stages.len().saturating_sub(1));
                    lists.push((start..lines.len(), current, 3));
                    outcome_lines(&mut lines, v, run, dw as usize);
                }
                None => {
                    if plan.newer() {
                        lines.push(Line::Gap);
                        lines.push(Line::Gap);
                        lines.push(Line::Buttons);
                    }
                }
            }
            lists.push((rows, 0, 1));
        }
    }
    let refused = updates.plan().is_some_and(|p| p.refusal.is_some());
    if updates.run.is_none() && !refused {
        if let Some(checked) = checked {
            lines.push(Line::Gap);
            lines.push(Line::Gap);
            let mut text = format!("Checked {} ago", age_words(v.age(checked.at)));
            if design {
                text.push_str(" from releases.cortexkit.io · u simulates an upgrade here");
            }
            lines.push(Line::Text(text, St::F));
        }
    }

    let room = (c.h - 5).max(0) as usize;
    fit(&mut lines, room, lists);
    for (i, line) in lines.iter().enumerate() {
        draw_line(c, v, x, 3 + i as i32, dw, line);
    }

    let mut hints: Vec<(&str, &str)> = Vec::new();
    let offers = updates.plan().is_some_and(UpgradeCheck::newer) && updates.run.is_none();
    if offers {
        hints.push(("⏎", "upgrade"));
    }
    if !running {
        hints.push(("l", "later"));
    }
    // The release notes key and source belong to a check that reached the
    // release index; a refused one didn't.
    let design = design && !refused;
    if design {
        hints.push(("n", "release notes"));
    }
    let status = if design {
        "from releases.cortexkit.io"
    } else if running {
        "Upgrading…"
    } else {
        ""
    };
    frame_end(c, &hints, status, St::D);
}

/// The question a running upgrade asks when a component it replaced fails:
/// the failure as the engine gave it, "⏎ Roll back to <old>" (focused)
/// and "esc Keep <new>".
pub fn rollback_question(c: &mut Canvas, question: &RollbackQuestion) {
    let lines = wrap_verbatim(&question.sentence(), super::overlays::choice_width(c.w));
    super::overlays::choice(
        c,
        &format!("Roll back {}?", question.component),
        &lines,
        &format!("Roll back to {}", question.old),
        &format!("Keep {}", question.new),
    );
}

/// The lines after a run's stages: a failed stage's reason, then what the
/// run installed and how many modules are healthy.
fn outcome_lines<'a>(lines: &mut Vec<Line<'a>>, v: &View, run: &'a Run, w: usize) {
    let failed = run.stages.iter().find_map(|s| match &s.state {
        StageState::Failed(reason) => Some(reason),
        _ => None,
    });
    if let Some(reason) = failed {
        for l in wrap_verbatim(reason, w.saturating_sub(2)) {
            lines.push(Line::Text(format!("  {l}"), St::E));
        }
    }
    rollback_lines(lines, run, w);
    let Some((at, outcome)) = &run.outcome else {
        return;
    };
    lines.push(Line::Gap);
    // A refusal made when the run planned again belongs to no stage.
    if let (Some(error), None) = (&outcome.error, failed) {
        for l in wrap_verbatim(error, w) {
            lines.push(Line::Text(l, St::E));
        }
    }
    let secs = at.secs_since(run.started_at).round().max(0.0);
    let installed = outcome.installed.join(" · ");
    let what = match (&outcome.error, installed.is_empty()) {
        (None, false) => format!("✓ Upgraded {installed} in {secs:.0} s."),
        (None, true) => format!("✓ Nothing needed upgrading ({secs:.0} s)."),
        (Some(_), false) => format!("✗ Upgrade stopped. Installed {installed}."),
        (Some(_), true) => "✗ Upgrade stopped. Nothing was installed.".to_string(),
    };
    let snap = v.snap;
    let health = match &snap.daemon.data {
        Daemon::Running(_) if snap.modules.loaded => {
            let total = snap.modules.data.len();
            let healthy = snap
                .modules
                .data
                .iter()
                .filter(|m| m.status == Status::Ok)
                .count();
            format!("{healthy} of {total} modules healthy.")
        }
        _ => "Waiting for the daemon to report its modules.".to_string(),
    };
    let st = if outcome.error.is_none() {
        St::B
    } else {
        St::E
    };
    for l in wrap_verbatim(&format!("{what} {health}"), w) {
        lines.push(Line::Text(l, st));
    }
    for warning in &outcome.warnings {
        for l in wrap_verbatim(warning, w) {
            lines.push(Line::Text(l, St::W));
        }
    }
}

/// What became of the run's rollback question once answered: rolled back,
/// kept, or kept because nobody could answer. When a rollback stage ran,
/// its own account (what it restored, or why it couldn't) follows, exactly
/// as the engine gave it.
fn rollback_lines(lines: &mut Vec<Line>, run: &Run, w: usize) {
    let Some(rollback) = &run.rollback else {
        return;
    };
    let Some(answer) = rollback.answer else {
        return;
    };
    let q = &rollback.question;
    let stage = run
        .stages
        .iter()
        .rev()
        .find(|s| s.stage == "rollback" && s.target == q.component)
        .map(|s| &s.state);
    let (said, st) = match (answer, stage) {
        (RollbackAnswer::RollBack, Some(StageState::Done(_))) => {
            (format!("{}: rolled back to {}.", q.component, q.old), St::B)
        }
        (RollbackAnswer::RollBack, Some(StageState::Failed(_))) => (
            format!("{}: couldn't roll back to {}.", q.component, q.old),
            St::E,
        ),
        (RollbackAnswer::RollBack, _) => (
            format!("{}: rolling back to {}…", q.component, q.old),
            St::D,
        ),
        (RollbackAnswer::Keep, _) => (format!("{}: kept {}.", q.component, q.new), St::B),
        (RollbackAnswer::NoAnswer, _) => (
            format!("{}: no answer; kept {}.", q.component, q.new),
            St::W,
        ),
    };
    for l in wrap_verbatim(&said, w) {
        lines.push(Line::Text(l, st));
    }
    let evidence = match stage {
        Some(StageState::Done(detail)) => Some((detail, St::D)),
        Some(StageState::Failed(reason)) => Some((reason, St::E)),
        _ => None,
    };
    if let Some((text, st)) = evidence {
        for l in wrap_verbatim(text, w.saturating_sub(2)) {
            lines.push(Line::Text(format!("  {l}"), st));
        }
    }
}

/// Fits `lines` into `room` rows: blank gaps go first, from the bottom; then
/// each list in `lists` in turn (its range in `lines`, the item to keep in
/// view, and the fewest items it keeps) shows a window around that item,
/// ending "+N more" for the items it hides; anything still over is cut at
/// the bottom. Lists are given last first, so shrinking one never moves the
/// next one's range.
fn fit(lines: &mut Vec<Line>, room: usize, mut lists: Vec<(std::ops::Range<usize>, usize, usize)>) {
    while lines.len() > room {
        let Some(gap) = lines.iter().rposition(|l| matches!(l, Line::Gap)) else {
            break;
        };
        lines.remove(gap);
        for (list, ..) in &mut lists {
            if gap < list.start {
                *list = list.start - 1..list.end - 1;
            }
        }
    }
    for (list, current, least) in lists {
        let over = lines.len().saturating_sub(room);
        if over == 0 || list.len() <= least {
            continue;
        }
        // The window and its "+N more" line take `keep + 1` rows.
        let keep = list.len().saturating_sub(over + 1).max(least);
        let start = current
            .saturating_sub(keep.saturating_sub(1))
            .min(list.len() - keep);
        let hidden = list.len() - keep;
        let mut window: Vec<Line> = lines.drain(list.clone()).skip(start).take(keep).collect();
        // Items above the window (finished stages) count in the same
        // "+N more" as those below it, so nothing is hidden silently.
        window.push(Line::More(hidden));
        let tail = lines.split_off(list.start);
        lines.extend(window);
        lines.extend(tail);
    }
    lines.truncate(room);
}

fn draw_line(c: &mut Canvas, v: &View, x: i32, y: i32, dw: i32, line: &Line) {
    match line {
        Line::Gap => {}
        Line::Text(text, st) => {
            c.put(x, y, &trunc(text, dw.max(0) as usize), *st);
        }
        Line::Title(title, have) => {
            let right = have.as_deref().unwrap_or("");
            let room = (dw - width(right) as i32 - 2).max(0) as usize;
            c.put(x, y, &trunc(title, room), St::B);
            if !right.is_empty() {
                c.right(x + dw, y, right, St::D);
            }
        }
        Line::Header => {
            for (dx, t) in [
                (0, "Component"),
                (22, "Installed"),
                (35, "New"),
                (48, "What happens"),
            ] {
                c.put(x + dx, y, t, St::D);
            }
        }
        Line::Rule => c.hline(x, y, dw),
        Line::Row(r) => {
            let st = |on: St| if r.replaces { on } else { St::D };
            c.put(x, y, &trunc(&r.component, 21), st(St::B));
            c.put(x + 22, y, &trunc(&r.installed, 12), St::D);
            c.put(x + 35, y, &trunc(&r.new, 12), st(St::A.bold()));
            c.put(
                x + 48,
                y,
                &trunc(&r.what, (dw - 48).max(0) as usize),
                if r.replaces { St::NONE } else { St::F },
            );
        }
        Line::Buttons => {
            c.put(
                x,
                y,
                " ⏎  Upgrade now ",
                St::fg(Fg::OnAcc).on(Bg::Acc).bold(),
            );
            c.put(x + 19, y, " l  Later ", St::bg(Bg::Key));
        }
        Line::Stage(s) => {
            const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
            let (glyph, gst, lst) = match &s.state {
                StageState::Pending => ("○", St::F, St::F),
                StageState::Running => (SPIN[(v.tick % 10) as usize], St::A, St::B),
                StageState::Done(_) => ("✓", St::O, St::D),
                StageState::Failed(_) => ("✗", St::E, St::B),
            };
            c.put(x, y, glyph, gst);
            c.put(
                x + 2,
                y,
                &trunc(&stage_label(&s.target, &s.stage), (dw - 2).max(0) as usize),
                lst,
            );
        }
        Line::More(n) => {
            c.put(x, y, &format!("+{n} more"), St::F);
        }
    }
}
