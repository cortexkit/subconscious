//! Agents: who is working, on what, and on which machine.
//!
//! The list joins three sources on each agent's exact id, identity on the
//! left: entorhinal's identity (name, project, role, workspace),
//! prefrontal-core's state (working or idle, what the agent is doing, the
//! questions it waits on) and entorhinal's places (the machine holding it).
//! Each column ages and dims by its own source, so a fresh identity reply
//! never makes an old state or place look current; the heading names any
//! source that is old or failing. A state source that has never answered,
//! such as one that refuses ck, leaves every state "unknown" and the
//! heading shows its error code; one that fails later keeps its last good
//! reading, which ages and dims like any other.
//!
//! Below the list, Workspaces counts the listed agents by workspace, with
//! the fleet's totals from entorhinal's `enumerate`, and Machines counts
//! them by the machine that holds them. While every agent fits, the panels
//! follow the list; otherwise the list runs down to them and its last row
//! says how many agents it leaves out. The list keeps at least
//! [`MIN_LIST`] rows, and the panels are left out whole before it gets
//! fewer. From 150 columns each agent also shows its role and how many of
//! its questions wait for an answer.
//!
//! `↑↓` move through the list and `/` filters it by name. An agent's detail
//! and the projects view have no screen: `⏎` opens nothing (it only ends
//! typing in the filter), `p` does nothing, and no hint offers either.

use crate::draw::{chrome, frame_end, heading_st, pad_right, trunc, width, Canvas, Clip};
use crate::model::{Agent, Agents, Snapshot, Tab};
use crate::theme::{Bg, St};

use super::home::agent_glyph;
use super::{count, View, UNKNOWN};

/// What the person has selected and typed on the Agents screen.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AgentsState {
    /// The selected row among those shown. `None` until the person moves:
    /// then no row is highlighted and the list starts at its top.
    pub sel: Option<usize>,
    /// The `/` filter's text; empty for none.
    pub filter: String,
}

/// The agents the list shows: those whose name holds the filter, ignoring
/// case. None until the first identity reading arrives.
pub fn shown<'a>(snap: &'a Snapshot, state: &AgentsState) -> Vec<&'a Agent> {
    if !snap.agents.loaded {
        return Vec::new();
    }
    let query = state.filter.to_lowercase();
    snap.agents
        .data
        .list
        .iter()
        .filter(|a| query.is_empty() || a.name.to_lowercase().contains(&query))
        .collect()
}

/// The row the list's first agent is drawn on.
pub const LIST_TOP: i32 = 7;
/// The fewest rows the list keeps; the panels below it are left out
/// before it gets fewer.
pub const MIN_LIST: i32 = 3;
/// Blank rows between the list and the panels below it.
const GAP: i32 = 2;
/// The rows each panel's list takes: at least the design's two, and at most
/// six, after which its last row says how many it leaves out.
const PANEL_MIN: i32 = 2;
const PANEL_MAX: i32 = 6;

/// Where each column starts at a window width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    /// From 150 columns: the role and waiting-questions columns.
    pub wide: bool,
    pub role_x: i32,
    pub doing_x: i32,
    pub state_x: i32,
    /// The waiting-questions column, whose numbers end at `asks_x + 4`.
    pub asks_x: i32,
    pub machine_x: i32,
    /// The column after the last one any text may use.
    pub right: i32,
}

pub fn layout(w: i32) -> Layout {
    let wide = w >= 150;
    let right = w - 2;
    // The machine column is 24 wide from 120 columns and narrows below,
    // so the doing column keeps some room at 80.
    let machine_x = right - if wide { 24 } else { (w - 96).clamp(12, 24) };
    let asks_x = machine_x - 6;
    let state_x = if wide { asks_x } else { machine_x } - 14;
    let role_x = 36;
    Layout {
        wide,
        role_x,
        doing_x: if wide { role_x + 14 } else { role_x },
        state_x,
        asks_x,
        machine_x,
        right,
    }
}

/// How the rows between the heading and the footer are shared.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Place {
    /// Rows the list takes from [`LIST_TOP`], its "+N more" line included.
    pub list: i32,
    /// The Workspaces and Machines headings' row, when the panels show.
    pub panels: Option<i32>,
    /// Rows each panel's list takes below its heading and rule.
    pub body: i32,
}

/// Shares a window `h` rows high between `n` agents and panels whose lists
/// are `body` rows long. While every agent fits, the panels follow the list
/// after [`GAP`] blank rows. Otherwise the panels' last row sits just above
/// the footer rule and the list runs down to the gap above them; when that
/// would leave the list fewer than [`MIN_LIST`] rows, the panels are left
/// out.
pub fn place(h: i32, n: usize, body: i32) -> Place {
    let last = h - 3;
    let avail = last - LIST_TOP + 1;
    let panel_h = 2 + body;
    let need = n.max(1) as i32;
    if need + GAP + panel_h <= avail {
        Place {
            list: need,
            panels: Some(LIST_TOP + need + GAP),
            body,
        }
    } else if avail - GAP - panel_h >= MIN_LIST {
        Place {
            list: avail - GAP - panel_h,
            panels: Some(last - panel_h + 1),
            body,
        }
    } else {
        Place {
            list: avail.max(0),
            panels: None,
            body,
        }
    }
}

/// Rows each panel's list takes for these agents.
pub fn panel_body(agents: &Agents) -> i32 {
    let rows = agents.workspace_list.len().max(agents.machine_list.len()) as i32;
    rows.clamp(PANEL_MIN, PANEL_MAX)
}

/// Which of `n` rows show in `rows` rows with row `sel` in view: the first
/// shown, how many, and how many are left out. When they don't all fit, the
/// last row is kept for the "+N more" line.
pub fn window(n: usize, rows: i32, sel: Option<usize>) -> (usize, usize, usize) {
    let rows = rows.max(0) as usize;
    if n <= rows {
        return (0, n, 0);
    }
    let k = rows.saturating_sub(1);
    let sel = sel.unwrap_or(0).min(n - 1);
    let first = if sel < k { 0 } else { sel + 1 - k };
    (first, k, n - k)
}

pub fn draw(c: &mut Canvas, v: &View, state: &AgentsState, typing: bool) {
    let snap = v.snap;
    chrome(
        c,
        Tab::Agents,
        snap.visible_tabs(),
        &v.clock(),
        &snap.host.data.label,
    );
    let agents = &snap.agents.data;
    let sources = &agents.sources;
    let l = layout(c.w);
    let rows = shown(snap, state);

    // The heading: how many agents and how many work, the filter, and on
    // the right each source that is old or failing.
    c.put(2, 3, "Agents", St::B);
    let summary = match (snap.agents.loaded, sources.state.loaded) {
        (false, _) => UNKNOWN.to_string(),
        (true, true) => format!("{} · {} working", agents.list.len(), agents.working),
        // Without a state reading nobody is known to be working.
        (true, false) => agents.list.len().to_string(),
    };
    let summary_end = c.put(9, 3, &summary, St::D);
    let (notes, notes_st) = source_notes(v, snap);
    let room = (c.w - 2 - summary_end - 2).max(0) as usize;
    let notes_start = c.right(c.w - 2, 3, &trunc(&notes, room), notes_st);
    if !state.filter.is_empty() || typing {
        let prev = c.clip_to(Clip {
            x0: 0,
            y0: 0,
            x1: notes_start - 1,
            y1: c.h,
        });
        let sx = summary_end + 2;
        c.put(sx, 3, "/", St::A);
        let end = c.put(sx + 2, 3, &state.filter, St::B.underline());
        if typing {
            c.put(end, 3, "▏", St::A);
        }
        let n = rows.len();
        let matches = if n == 1 {
            "1 match".to_string()
        } else {
            format!("{n} matches")
        };
        c.put(end + 1, 3, &matches, St::D);
        c.set_clip(prev);
    }

    // The column heads and the list.
    let mut heads = vec![(4, "Agent"), (18, "Project")];
    if l.wide {
        heads.push((l.role_x, "Role"));
    }
    heads.push((l.doing_x, "Doing"));
    heads.push((l.state_x, "State"));
    if l.wide {
        heads.push((l.asks_x, "Asks"));
    }
    heads.push((l.machine_x, "Machine"));
    for (x, head) in heads {
        c.put(x, 5, head, St::D);
    }
    c.hline(2, 6, c.w - 4);

    let at = place(c.h, rows.len(), panel_body(agents));
    let (first, k, hidden) = window(rows.len(), at.list, state.sel);
    let prev = c.clip_to(Clip {
        x0: 0,
        y0: 0,
        x1: c.w,
        y1: LIST_TOP + at.list,
    });
    for (i, a) in rows.iter().enumerate().skip(first).take(k) {
        let y = LIST_TOP + (i - first) as i32;
        agent_row(c, v, &l, y, a);
        if state.sel == Some(i) {
            c.paint(1, y, c.w - 2, St::bg(Bg::Sel));
        }
    }
    if hidden > 0 {
        c.put(2, LIST_TOP + k as i32, &format!("+{hidden} more"), St::D);
    }
    if rows.is_empty() {
        let note = if !snap.agents.loaded {
            if sources.identity.error.is_some() {
                UNKNOWN
            } else {
                "Reading entorhinal…"
            }
        } else if agents.list.is_empty() {
            "No live agents"
        } else {
            "No agent matches"
        };
        c.put(2, LIST_TOP, &trunc(note, (c.w - 4).max(0) as usize), St::D);
    }
    c.set_clip(prev);

    // Each column dims by its own source once that source's reading is
    // only a last good one.
    let h = at.list;
    if v.meta(&sources.identity, "").dim {
        c.dim_rect(4, LIST_TOP, l.doing_x - 4, h);
    }
    if v.meta(&sources.state, "").dim {
        c.dim_rect(2, LIST_TOP, 1, h);
        c.dim_rect(l.doing_x, LIST_TOP, l.machine_x - l.doing_x, h);
    }
    if v.meta(&sources.places, "").dim {
        c.dim_rect(l.machine_x, LIST_TOP, l.right - l.machine_x, h);
    }

    if let Some(y) = at.panels {
        panels(c, v, y, at.body);
    }

    let mut hints = vec![("↑↓", "move")];
    if snap.serves.agent_detail {
        hints.push(("⏎", "open agent"));
    }
    if snap.serves.agent_projects {
        hints.push(("p", "projects"));
    }
    if snap.serves.list_filter.contains(Tab::Agents) {
        hints.push(("/", "filter"));
    }
    frame_end(c, &hints, "from entorhinal", St::D);
}

/// The heading's note on the list's sources: each one whose reading is old
/// or failing, by what it gives the list ("state 45s ago", "machines
/// last good reading · 6m ago"), or that identity is still being read. A
/// state source that never answered reads "state unknown" with its error,
/// such as the code prefrontal-core refuses ck with.
fn source_notes(v: &View, snap: &Snapshot) -> (String, St) {
    let sources = &snap.agents.data.sources;
    let mut parts = Vec::new();
    for (label, panel) in [
        ("agents", &sources.identity),
        ("state", &sources.state),
        ("machines", &sources.places),
    ] {
        let meta = v.meta(panel, "");
        if meta.text.is_empty() {
            continue;
        }
        match (&panel.error, panel.loaded) {
            (Some(err), false) if label == "state" => parts.push(format!("state unknown · {err}")),
            _ => parts.push(format!("{label} {}", meta.text)),
        }
    }
    if parts.is_empty() && !snap.agents.loaded {
        return ("reading entorhinal…".to_string(), St::D);
    }
    (parts.join(" · "), St::W)
}

/// One agent at screen row `y`.
fn agent_row(c: &mut Canvas, v: &View, l: &Layout, y: i32, a: &Agent) {
    let (glyph, glyph_st) = agent_glyph(a);
    c.put(2, y, glyph, glyph_st);
    let working = a.working();
    c.put(
        4,
        y,
        &pad_right(&a.name, 13),
        if working { St::B } else { St::NONE },
    );
    let project = if a.project.is_empty() {
        UNKNOWN
    } else {
        &a.project
    };
    c.put(18, y, &pad_right(project, 17), St::D);
    if l.wide {
        c.put(
            l.role_x,
            y,
            &trunc(a.role.as_deref().unwrap_or(UNKNOWN), 12),
            St::D,
        );
    }
    let doing_w = (l.state_x - 2 - l.doing_x).max(0) as usize;
    match &a.doing {
        Some(d) => c.put(l.doing_x, y, &trunc(d, doing_w), St::NONE),
        None => c.put(l.doing_x, y, "—", St::F),
    };
    let state_st = if working { St::NONE } else { St::D };
    c.put(l.state_x, y, &trunc(&a.state, 12), state_st);
    if l.wide {
        // A narrow number column: a dim dash when the count isn't reported.
        match a.pending_asks {
            Some(n) => c.right(l.asks_x + 4, y, &n.to_string(), St::NONE),
            None => c.right(l.asks_x + 4, y, "–", St::D),
        };
    }
    // This machine and an unknown place read quietly; another machine
    // stands out.
    let quiet = a.machine == v.snap.host.data.label || a.machine == UNKNOWN;
    let machine_w = (l.right - l.machine_x).max(0) as usize;
    c.put(
        l.machine_x,
        y,
        &trunc(&a.machine, machine_w),
        if quiet { St::D } else { St::NONE },
    );
}

/// The Workspaces and Machines panels side by side from row `y`, each
/// listing `body` rows.
fn panels(c: &mut Canvas, v: &View, y: i32, body: i32) {
    let agents = &v.snap.agents.data;
    let sources = &agents.sources;
    let half = (c.w - 8) / 2;
    let left = 2;
    let right = left + half + 4;

    // Workspaces: the fleet's workspace count from `enumerate` and, where
    // no row can say how many projects its workspace has, the fleet's
    // project count beside it.
    let rows_lack_projects = agents.workspace_list.iter().any(|w| w.projects.is_none());
    let total = match (agents.workspaces, agents.projects) {
        (Some(w), Some(p)) if rows_lack_projects => {
            format!("{w} · {}", count(p as usize, "project", "projects"))
        }
        (Some(w), _) => w.to_string(),
        (None, _) => UNKNOWN.to_string(),
    };
    let meta = v.meta(&sources.counts, &total);
    heading_st(c, left, y, half, "Workspaces", &meta.text, meta.st);
    if meta.dim {
        c.dim_rect(left, y, half, 1);
    }
    let list: Vec<(String, String)> = agents
        .workspace_list
        .iter()
        .map(|w| {
            let agents = count(w.agents as usize, "agent", "agents");
            let text = match w.projects {
                Some(p) => format!("{} · {agents}", count(p as usize, "project", "projects")),
                None => agents,
            };
            (w.name.clone(), text)
        })
        .collect();
    panel_rows(c, left, y + 2, half, body, &list, &[]);
    if v.meta(&sources.identity, "").dim {
        c.dim_rect(left, y + 2, half, body);
    }

    // Machines: one row per machine holding any listed agent.
    let normal = if sources.places.loaded {
        agents.machine_list.len().to_string()
    } else {
        UNKNOWN.to_string()
    };
    let meta = v.meta(&sources.places, &normal);
    heading_st(c, right, y, half, "Machines", &meta.text, meta.st);
    // A narrow panel drops the "live here" rather than cut the count.
    let room = (half - 18).max(0) as usize;
    let list: Vec<(String, String)> = agents
        .machine_list
        .iter()
        .map(|m| {
            let text = if m.agents == 1 {
                "1 agent lives here".to_string()
            } else {
                format!("{} agents live here", m.agents)
            };
            let text = if width(&text) <= room {
                text
            } else {
                count(m.agents as usize, "agent", "agents")
            };
            (m.name.clone(), text)
        })
        .collect();
    let online: Vec<Option<bool>> = agents.machine_list.iter().map(|m| m.online).collect();
    panel_rows(c, right, y + 2, half, body, &list, &online);
    if meta.dim {
        c.dim_rect(right, y + 2, half, body);
    }
}

/// A panel's rows from row `y`: a bold name and its dim count, `w` columns
/// wide, at most `body` of them, the last saying how many are left out when
/// they don't all fit. A row whose `online` entry is true gets a dot before
/// its name; a panel whose rows say nothing about being online has no dots
/// and starts its names at `x`.
fn panel_rows(
    c: &mut Canvas,
    x: i32,
    y: i32,
    w: i32,
    body: i32,
    rows: &[(String, String)],
    online: &[Option<bool>],
) {
    let (_, k, hidden) = window(rows.len(), body, None);
    let any_dot = online.iter().any(Option::is_some);
    let name_x = if any_dot { x + 2 } else { x };
    let text_x = x + 18;
    for (i, (name, text)) in rows.iter().take(k).enumerate() {
        let ry = y + i as i32;
        if online.get(i).copied().flatten() == Some(true) {
            c.put(x, ry, "●", St::O);
        }
        c.put(
            name_x,
            ry,
            &trunc(name, (text_x - 1 - name_x).max(0) as usize),
            St::B,
        );
        c.put(
            text_x,
            ry,
            &trunc(text, (x + w - text_x).max(0) as usize),
            St::D,
        );
    }
    if hidden > 0 {
        c.put(x, y + k as i32, &format!("+{hidden} more"), St::D);
    }
}
