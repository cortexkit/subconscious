//! The Agents screen: the join of identity, state and place, each column
//! aging by its own source, fleet polls that carry their tokens, a refused
//! state source, what the live app serves and hides, and the screen at
//! every window size.

use ck_ui::app::{App, Tab};
use ck_ui::live::map;
use ck_ui::live::schedule::Source;
use ck_ui::live::{assemble, State};
use ck_ui::model::{
    Agent, AgentSources, Agents, Fault, Machine, Panel, Serves, Snapshot, Workspace,
};
use ck_ui::screens::agents::{layout, LIST_TOP};
use ck_ui::time::Timestamp;
use ck_ui::Theme;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use serde_json::{json, Value};

fn now() -> Timestamp {
    Snapshot::fixture_now()
}

fn render(app: &App, snap: &Snapshot, w: u16, h: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buf, snap, now());
    buf
}

fn row(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

fn text(buf: &Buffer, x: u16, y: u16, n: u16) -> String {
    (x..(x + n).min(buf.area.width))
        .map(|x| buf[(x, y)].symbol())
        .collect()
}

fn screen(buf: &Buffer) -> String {
    (0..buf.area.height).map(|y| row(buf, y) + "\n").collect()
}

fn dimmed(buf: &Buffer, x: u16, y: u16) -> bool {
    buf[(x, y)].modifier.contains(Modifier::DIM)
}

fn press_at(app: &mut App, snap: &Snapshot, code: KeyCode, size: (u16, u16)) -> bool {
    let key = Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
    app.handle_event(&key, snap, size)
}

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) -> bool {
    press_at(app, snap, code, (120, 36))
}

fn agents_app() -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Agents;
    app
}

/// The fixture as the live app serves it, with these agents.
fn live_with(agents: Agents) -> Snapshot {
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    s.agents = Panel::new(now(), agents);
    s
}

const MACHINES: [&str; 3] = ["This Mac", "Mac 2", "Mac 3"];

/// Agent `i` of a crowded fleet: every third works, each has a role, and
/// every other one reports how many of its questions wait.
fn agent(i: usize) -> Agent {
    let working = i.is_multiple_of(3);
    Agent {
        name: format!("AG{i:02}"),
        project: format!("proj-{:02}", i % 7),
        state: if working { "working" } else { "idle 4m" }.into(),
        machine: MACHINES[i % 3].into(),
        doing: working.then(|| format!("task {i}")),
        role: Some(format!("role-{}", i % 5)),
        pending_asks: i.is_multiple_of(2).then_some((i % 4) as u32),
    }
}

/// 80 agents in `workspaces` workspaces, on three machines, every source
/// read just now.
fn crowd(workspaces: usize) -> Snapshot {
    let list: Vec<Agent> = (1..=80).map(agent).collect();
    let machine_list = MACHINES
        .iter()
        .map(|m| Machine {
            name: m.to_string(),
            agents: list.iter().filter(|a| a.machine == *m).count() as u32,
            online: None,
        })
        .collect();
    let workspace_list = (1..=workspaces)
        .map(|i| Workspace {
            name: format!("ws-{i}"),
            agents: 80 / workspaces as u32,
            projects: None,
        })
        .collect();
    live_with(Agents {
        working: list.iter().filter(|a| a.working()).count() as u32,
        list,
        machines: Some(3),
        workspaces: Some(workspaces as u32),
        projects: Some(12),
        workspace_list,
        machine_list,
        sources: AgentSources::read_at(now()),
    })
}

/// The "+N more" counts in `text`.
fn notes(text: &str) -> Vec<usize> {
    let words: Vec<&str> = text.split_whitespace().collect();
    words
        .iter()
        .enumerate()
        .filter(|(i, _)| words.get(i + 1) == Some(&"more"))
        .filter_map(|(_, w)| w.strip_prefix('+')?.parse().ok())
        .collect()
}

/// The footer's hints that show are a prefix of `hints`, each drawn whole.
fn assert_whole_hints(buf: &Buffer, w: u16, h: u16, hints: &[&str]) {
    let footer = row(buf, h - 1);
    let shown: Vec<bool> = hints.iter().map(|hint| footer.contains(hint)).collect();
    let n = shown.iter().take_while(|s| **s).count();
    assert!(n >= 1, "{w}×{h}: {footer}");
    assert!(shown[n..].iter().all(|s| !s), "{w}×{h}: {footer}");
}

// ---- sizes ------------------------------------------------------------------------

#[test]
fn every_size_says_how_many_agents_it_leaves_out_and_keeps_the_panels_below() {
    let snap = crowd(3);
    for (w, h) in [(80, 24), (100, 30), (140, 40), (160, 48), (200, 60)] {
        let buf = render(&agents_app(), &snap, w, h);
        let top = LIST_TOP as u16;
        // The agents listed, one per row from the list's top.
        let listed: Vec<u16> = (top..h).filter(|&y| text(&buf, 4, y, 2) == "AG").collect();
        let n = listed.len();
        assert_eq!(listed, (top..top + n as u16).collect::<Vec<_>>(), "{w}×{h}");
        // The row after them says how many are left out.
        let note_y = top + n as u16;
        assert_eq!(
            notes(&text(&buf, 2, note_y, 20)),
            [80 - n],
            "{w}×{h}: {}",
            row(&buf, note_y)
        );
        assert!(n + 1 >= 3, "{w}×{h}: the list keeps at least 3 rows");

        // The panels follow two blank rows, and their last row sits just
        // above the footer rule: the list fills every row down to them.
        let l = layout(i32::from(w));
        let half = (w - 8) / 2;
        let ws_y = (top..h)
            .find(|&y| text(&buf, 2, y, 10) == "Workspaces")
            .unwrap_or_else(|| panic!("{w}×{h}: no Workspaces panel\n{}", screen(&buf)));
        assert_eq!(ws_y, note_y + 3, "{w}×{h}");
        for y in note_y + 1..ws_y {
            assert!(row(&buf, y).trim().is_empty(), "{w}×{h}: row {y}");
        }
        assert_eq!(text(&buf, 2 + half + 4, ws_y, 8), "Machines", "{w}×{h}");
        // Three workspaces and three machines: a heading, its rule and three
        // rows each.
        assert_eq!(ws_y + 4, h - 3, "{w}×{h}");
        assert!(row(&buf, h - 3).contains("ws-3"), "{w}×{h}");
        assert!(row(&buf, h - 3).contains("Mac 3"), "{w}×{h}");
        assert!(
            row(&buf, h - 2).starts_with("───"),
            "{w}×{h}: the footer rule"
        );

        // From 150 columns: each agent's role and its waiting questions.
        let heads = row(&buf, 5);
        let first = top; // AG01: role-1, no count reported.
        let second = top + 1; // AG02: role-2, two questions waiting.
        if w >= 150 {
            assert!(l.wide);
            assert!(
                heads.contains("Role") && heads.contains("Asks"),
                "{w}×{h}: {heads}"
            );
            let asks = |y| text(&buf, l.asks_x as u16, y, 4).trim().to_string();
            assert!(row(&buf, first).contains("role-1"), "{w}×{h}");
            assert_eq!(asks(first), "–", "{w}×{h}");
            assert_eq!(asks(second), "2", "{w}×{h}");
        } else {
            assert!(!l.wide);
            assert!(
                !heads.contains("Role") && !heads.contains("Asks"),
                "{w}×{h}: {heads}"
            );
            assert!(!screen(&buf).contains("role-"), "{w}×{h}");
        }

        assert_whole_hints(&buf, w, h, &["↑↓  move", "/  filter"]);
        let footer = row(&buf, h - 1);
        assert!(
            !footer.contains("open agent") && !footer.contains("projects"),
            "{w}×{h}: {footer}"
        );
    }
}

#[test]
fn a_panel_with_more_rows_than_fit_says_how_many_it_leaves_out() {
    // Nine workspaces: the panel lists six rows, the last "+4 more".
    let snap = crowd(9);
    let buf = render(&agents_app(), &snap, 120, 36);
    let ws_y = (0..36)
        .find(|&y| text(&buf, 2, y, 10) == "Workspaces")
        .unwrap();
    assert_eq!(ws_y + 1 + 6, 33, "the panel's last row is above the rule");
    let panel: Vec<String> = (ws_y + 2..=ws_y + 7)
        .map(|y| text(&buf, 2, y, 56).trim_end().to_string())
        .collect();
    assert_eq!(panel[0], format!("ws-1{}8 agents", " ".repeat(14)));
    assert_eq!(panel[4], format!("ws-5{}8 agents", " ".repeat(14)));
    assert_eq!(panel[5], "+4 more");
    // The heading counts the fleet's workspaces and, since no row can say
    // how many projects its workspace has, the fleet's projects.
    assert!(
        text(&buf, 2, ws_y, 56).ends_with("9 · 12 projects"),
        "{}",
        row(&buf, ws_y)
    );
}

#[test]
fn moving_down_reaches_the_last_agent_at_80x24() {
    let snap = crowd(3);
    let mut app = agents_app();
    for _ in 0..100 {
        press_at(&mut app, &snap, KeyCode::Down, (80, 24));
    }
    assert_eq!(app.agents.sel, Some(79));
    let buf = render(&app, &snap, 80, 24);
    let top = LIST_TOP as u16;
    let listed: Vec<u16> = (top..24).filter(|&y| text(&buf, 4, y, 2) == "AG").collect();
    let last = *listed.last().unwrap();
    assert_eq!(text(&buf, 4, last, 4), "AG80");
    assert_eq!(buf[(4, last)].bg, buf[(60, last)].bg);
    assert_ne!(
        buf[(4, last)].bg,
        buf[(4, top)].bg,
        "the selection is drawn"
    );
    // Every agent above the view is counted on the note row.
    assert_eq!(
        notes(&row(&buf, last + 1)),
        [80 - listed.len()],
        "{}",
        screen(&buf)
    );
    assert!(!press_at(&mut app, &snap, KeyCode::Down, (80, 24)));
}

#[test]
fn every_size_draws_without_panicking() {
    let snap = crowd(9);
    let mut empty = snap.clone();
    empty.agents.data.list.clear();
    empty.agents.data.workspace_list.clear();
    empty.agents.data.machine_list.clear();
    let mut waiting = snap.clone();
    waiting.agents = Panel::waiting(empty.agents.data.clone(), Some(15.0));
    let mut apps = vec![agents_app()];
    let mut deep = agents_app();
    deep.agents.sel = Some(500);
    apps.push(deep);
    let mut filtered = agents_app();
    filtered.agents.filter = "AG1".into();
    filtered.field = Some(ck_ui::app::TextField { text: "AG1".into() });
    apps.push(filtered);
    for w in 60..=220u16 {
        for h in [10, 23, 24, 25, 30, 36, 48, 60] {
            for app in &apps {
                for s in [&snap, &empty, &waiting] {
                    render(app, s, w, h);
                }
            }
        }
    }
}

// ---- each column ages by its own source ---------------------------------------------

/// The fixture as served live, its agent sources on their live intervals,
/// each read `ages` seconds ago (identity, state, places).
fn aged(ages: [f64; 3]) -> Snapshot {
    let mut s = Snapshot::fixture();
    s.serves = Serves::LIVE;
    let src = &mut s.agents.data.sources;
    for (panel, (age, every)) in [&mut src.identity, &mut src.state, &mut src.places]
        .into_iter()
        .zip(ages.into_iter().zip([15.0, 15.0, 60.0]))
    {
        panel.refresh_secs = Some(every);
        panel.received_at = now().add_secs(-age);
    }
    s
}

// At 120 columns: the name starts at column 4, the state at 80 and the
// machine at 94; ARGUS is on row 7.
const NAME_X: u16 = 4;
const STATE_X: u16 = 80;
const MACHINE_X: u16 = 94;
const FIRST: u16 = 7;

#[test]
fn each_column_dims_by_its_own_source() {
    // A fresh identity reply with a state reading 100 s old: the state
    // column dims and the heading says so; the names and machines don't.
    let buf = render(&agents_app(), &aged([1.0, 100.0, 1.0]), 120, 36);
    assert!(dimmed(&buf, STATE_X, FIRST), "{}", row(&buf, FIRST));
    assert!(dimmed(&buf, 2, FIRST), "the working dot comes from state");
    assert!(!dimmed(&buf, NAME_X, FIRST));
    assert!(!dimmed(&buf, MACHINE_X, FIRST));
    assert!(
        row(&buf, 3).contains("state last good reading · 1m"),
        "{}",
        row(&buf, 3)
    );

    // Old places alone dim the machine column.
    let buf = render(&agents_app(), &aged([1.0, 1.0, 400.0]), 120, 36);
    assert!(dimmed(&buf, MACHINE_X, FIRST));
    assert!(!dimmed(&buf, STATE_X, FIRST) && !dimmed(&buf, NAME_X, FIRST));
    assert!(row(&buf, 3).contains("machines last good reading"));

    // Old identity alone dims the names and projects.
    let buf = render(&agents_app(), &aged([100.0, 1.0, 1.0]), 120, 36);
    assert!(dimmed(&buf, NAME_X, FIRST) && dimmed(&buf, 18, FIRST));
    assert!(!dimmed(&buf, STATE_X, FIRST) && !dimmed(&buf, MACHINE_X, FIRST));

    // Past twice its interval a source shows its age in amber, undimmed.
    let buf = render(&agents_app(), &aged([1.0, 40.0, 1.0]), 120, 36);
    assert!(row(&buf, 3).contains("state 40s ago"), "{}", row(&buf, 3));
    assert!(!dimmed(&buf, STATE_X, FIRST));

    // All fresh: nothing dims and the heading names no source.
    let buf = render(&agents_app(), &aged([1.0, 1.0, 1.0]), 120, 36);
    assert!((0..36).all(|y| (0..120).all(|x| !dimmed(&buf, x, y))));
    assert_eq!(row(&buf, 3).trim_end(), "  Agents 12 · 5 working");
}

// ---- the live join ----------------------------------------------------------------

const NOW_MS: i64 = 1_800_000_000_000;

fn at(secs: f64) -> Timestamp {
    Timestamp::from_unix_millis(NOW_MS).add_secs(secs)
}

fn identity_reply(token: &str) -> Value {
    json!({ "result": {
        "agents": [
            { "agentId": "a1", "displayName": "ARGUS", "role": "builder", "tag": "", "labels": [],
              "project": { "id": "p1", "name": "web-app" }, "workspace": { "id": "w1", "name": "Main" } },
            { "agentId": "a2", "displayName": "IRIS", "role": "", "tag": "", "labels": [],
              "project": { "id": "p2", "name": "api-server" }, "workspace": { "id": "w1", "name": "Main" } },
            { "agentId": "a3", "displayName": "NOX", "tag": "", "labels": [],
              "project": { "id": "p2", "name": "api-server" }, "workspace": { "id": "w2", "name": "Side" } }
        ],
        "token": token, "generation": 1, "incarnation": "i", "logPosition": 4, "unchanged": false
    }})
}

fn residence_reply(token: &str) -> Value {
    json!({
        "token": token, "unchanged": false,
        "agents": [
            { "agentId": "a1", "residence": null, "activity": "busy",
              "board": { "statusText": "Checkout page" }, "pendingAskCount": 2 },
            { "agentId": "a2", "residence": null, "activity": "idle", "activityAtMs": NOW_MS - 240_000 },
            { "agentId": "a3", "residence": null, "activity": "unknown" }
        ]
    })
}

fn places_reply() -> Value {
    json!({ "result": { "agents": [
        { "agent_id": "a1", "status": "held_here", "holder_name": "alex-mac", "known": true, "stale": false },
        { "agent_id": "a2", "status": "held_elsewhere", "holder_name": "Mac 2", "known": true, "stale": false },
        { "agent_id": "a3", "status": "unclaimed", "holder_name": null, "known": true, "stale": false }
    ]}})
}

#[test]
fn identity_gives_role_and_workspace_and_state_gives_waiting_questions() {
    let ids = map::agent_identities(&identity_reply("t")).unwrap();
    let got: Vec<(Option<&str>, Option<&str>)> = ids
        .iter()
        .map(|i| (i.role.as_deref(), i.workspace.as_deref()))
        .collect();
    // An empty or missing role is no role.
    assert_eq!(
        got,
        [
            (Some("builder"), Some("Main")),
            (None, Some("Main")),
            (None, Some("Side"))
        ]
    );
    let states = map::agent_residences(&residence_reply("r")).unwrap();
    assert_eq!(states["a1"].pending_asks, Some(2));
    assert_eq!(states["a2"].pending_asks, None);
}

#[test]
fn workspaces_and_machines_count_the_joined_agents() {
    let ids = map::agent_identities(&identity_reply("t")).unwrap();
    let states = map::agent_residences(&residence_reply("r")).unwrap();
    let places = map::agent_places(&places_reply(), "This Mac").unwrap();
    let a = map::agents(
        &ids,
        Some(&states),
        Some(&places),
        (Some(2), Some(9)),
        at(0.0),
    );
    /// Name, state, machine, role and waiting questions.
    type Row<'a> = (&'a str, &'a str, &'a str, Option<&'a str>, Option<u32>);
    let rows: Vec<Row> = a
        .list
        .iter()
        .map(|x| {
            (
                x.name.as_str(),
                x.state.as_str(),
                x.machine.as_str(),
                x.role.as_deref(),
                x.pending_asks,
            )
        })
        .collect();
    assert_eq!(
        rows,
        [
            ("ARGUS", "working", "This Mac", Some("builder"), Some(2)),
            ("IRIS", "idle 4m", "Mac 2", None, None),
            ("NOX", "unknown", "unclaimed", None, None),
        ]
    );
    // One row per workspace name, most agents first; no source counts a
    // workspace's projects.
    assert_eq!(
        a.workspace_list,
        [
            Workspace {
                name: "Main".into(),
                agents: 2,
                projects: None
            },
            Workspace {
                name: "Side".into(),
                agents: 1,
                projects: None
            },
        ]
    );
    // One row per machine holding an agent; an unclaimed agent is on none,
    // and no source says whether a machine is online.
    assert_eq!(
        a.machine_list,
        [
            Machine {
                name: "Mac 2".into(),
                agents: 1,
                online: None
            },
            Machine {
                name: "This Mac".into(),
                agents: 1,
                online: None
            },
        ]
    );
    assert_eq!(a.machines, Some(2));
    // Without places there are no machine rows, rather than invented ones.
    let a = map::agents(&ids, Some(&states), None, (None, None), at(0.0));
    assert!(a.machine_list.is_empty());
    assert_eq!(a.machines, None);
}

#[test]
fn fleet_polls_carry_their_token_and_an_unchanged_reply_keeps_the_rows_and_resets_their_age() {
    let mut s = State::empty("This Mac");
    // The first polls carry no token.
    assert_eq!(s.poll_params(Source::Identity), json!({}));
    assert_eq!(s.poll_params(Source::Residence), json!({}));

    assert!(s.record_module_reply(Source::Identity, Ok(identity_reply("t1")), at(0.0)));
    assert!(s.record_module_reply(Source::Residence, Ok(residence_reply("r1")), at(0.0)));
    assert_eq!(s.poll_params(Source::Identity), json!({ "token": "t1" }));
    assert_eq!(s.poll_params(Source::Residence), json!({ "token": "r1" }));
    // Sources that take no token never get one.
    assert_eq!(s.poll_params(Source::Usage), json!({}));
    assert_eq!(s.poll_params(Source::Counts), json!({}));

    // Sixty seconds on, both answer unchanged, with no rows.
    let unchanged_identity = json!({ "result": {
        "agents": [], "token": "t1", "generation": 1, "incarnation": "i",
        "logPosition": 4, "unchanged": true
    }});
    let unchanged_state = json!({ "token": "r2", "unchanged": true, "agents": [] });
    assert!(s.record_module_reply(Source::Identity, Ok(unchanged_identity), at(60.0)));
    assert!(s.record_module_reply(Source::Residence, Ok(unchanged_state), at(60.0)));
    assert_eq!(s.identities.value.as_ref().unwrap().len(), 3);
    assert_eq!(s.residences.value.as_ref().unwrap().len(), 3);
    assert_eq!((s.identities.at, s.residences.at), (at(60.0), at(60.0)));
    // A newer token in an unchanged reply is the one sent next.
    assert_eq!(s.poll_params(Source::Residence), json!({ "token": "r2" }));
    let snap = assemble(&s, at(60.0));
    assert_eq!(snap.agents.data.list.len(), 3);
    assert_eq!(snap.agents.data.sources.state.received_at, at(60.0));

    // A refusal keeps the rows as a last good reading, and the token.
    let refused = json!({ "error": { "code": "principal_not_permitted" } });
    assert!(!s.record_module_reply(Source::Residence, Ok(refused), at(75.0)));
    assert_eq!(s.residences.value.as_ref().unwrap().len(), 3);
    assert_eq!(s.residences.at, at(60.0));
    assert_eq!(s.poll_params(Source::Residence), json!({ "token": "r2" }));

    // An unchanged reply with no rows on hand can't be shown: it records
    // nothing and drops the token, so the next poll asks for every row.
    let mut fresh = State::empty("This Mac");
    fresh.identity_token = Some(json!("old"));
    let unchanged = json!({ "result": { "agents": [], "token": "old", "unchanged": true }});
    assert!(!fresh.record_module_reply(Source::Identity, Ok(unchanged), at(0.0)));
    assert!(fresh.identities.value.is_none());
    assert_eq!(fresh.poll_params(Source::Identity), json!({}));
}

/// A live state with every agent source answered at `at(0)`, prefrontal-
/// core's refused when `refused` is set, assembled with the fixture's
/// roster so the Agents tab shows.
fn live_state(refused: Option<&str>) -> Snapshot {
    let mut s = State::empty("This Mac");
    s.record_module_reply(Source::Identity, Ok(identity_reply("t1")), at(0.0));
    let state = match refused {
        Some(code) => Err(Fault::code(code)),
        None => Ok(residence_reply("r1")),
    };
    s.record_module_reply(Source::Residence, state, at(0.0));
    s.record_module_reply(
        Source::Counts,
        Ok(json!({ "result": { "workspaces": 2, "projects": 9, "generation": 1, "incarnation": "i" }})),
        at(0.0),
    );
    s.places.set(
        map::agent_places(&places_reply(), "This Mac").unwrap(),
        at(0.0),
    );
    let mut snap = assemble(&s, at(1.0));
    snap.modules = Snapshot::fixture().modules;
    snap
}

fn render_live(snap: &Snapshot) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, 120, 36));
    agents_app().render(&mut buf, snap, at(1.0));
    buf
}

#[test]
fn a_refused_state_source_reads_unknown_with_its_code() {
    let buf = render_live(&live_state(Some("principal_not_permitted")));
    let heading = row(&buf, 3);
    // Nobody is known to be working: the heading counts the agents only,
    // and names the refusal.
    assert!(heading.starts_with("  Agents 3  "), "{heading}");
    assert!(!heading.contains("working"), "{heading}");
    assert!(
        heading
            .trim_end()
            .ends_with("state unknown · principal_not_permitted"),
        "{heading}"
    );
    for y in FIRST..FIRST + 3 {
        assert_eq!(text(&buf, STATE_X, y, 7), "unknown", "{}", row(&buf, y));
        assert!(!row(&buf, y).contains("idle"), "{}", row(&buf, y));
    }
}

#[test]
fn the_live_screen_hides_what_no_source_reports() {
    let snap = live_state(None);
    let buf = render_live(&snap);
    let screen = screen(&buf);
    assert!(
        row(&buf, 3).starts_with("  Agents 3 · 1 working"),
        "{screen}"
    );
    assert!(
        row(&buf, FIRST).contains("ARGUS")
            && row(&buf, FIRST).contains("Checkout page")
            && row(&buf, FIRST).contains("This Mac"),
        "{screen}"
    );
    let ws_y = (0..36)
        .find(|&y| text(&buf, 2, y, 10) == "Workspaces")
        .unwrap();
    // Workspaces: the fleet's totals from enumerate, and each workspace's
    // agents with no project count.
    assert!(
        text(&buf, 2, ws_y, 56).ends_with("2 · 9 projects"),
        "{screen}"
    );
    assert_eq!(
        text(&buf, 2, ws_y + 2, 56).trim_end(),
        format!("Main{}2 agents", " ".repeat(14))
    );
    assert_eq!(
        text(&buf, 2, ws_y + 3, 56).trim_end(),
        format!("Side{}1 agent", " ".repeat(14))
    );
    // Machines: no online dot, since nothing reports one.
    assert_eq!(
        text(&buf, 62, ws_y, 56).trim_end(),
        format!("Machines{}2", " ".repeat(47))
    );
    assert_eq!(
        text(&buf, 62, ws_y + 2, 56).trim_end(),
        format!("Mac 2{}1 agent lives here", " ".repeat(13))
    );
    for y in ws_y + 2..ws_y + 4 {
        assert!(!text(&buf, 62, y, 56).contains('●'), "{screen}");
    }
    assert!(!screen.contains("projects ·"), "{screen}");
    // No hint for the views that have no screen.
    let footer = row(&buf, 35);
    for gone in ["open agent", "projects", "⏎"] {
        assert!(!footer.contains(gone), "{footer}");
    }
    assert!(footer.contains("/  filter"), "{footer}");
}

// ---- keys -------------------------------------------------------------------------

#[test]
fn slash_filters_by_name_and_enter_and_p_do_nothing_live() {
    let snap = live_with(Snapshot::fixture().agents.data);
    let mut app = App::new(Theme::test());
    assert!(press(&mut app, &snap, KeyCode::Char('5')));
    assert_eq!(app.tab, Tab::Agents);
    assert!(press(&mut app, &snap, KeyCode::Down));
    assert_eq!(app.agents.sel, Some(0));

    // ⏎ and p have no screen to open: nothing changes and nothing is sent.
    let before = app.clone();
    for code in [KeyCode::Enter, KeyCode::Char('p')] {
        assert!(!press(&mut app, &snap, code), "{code:?}");
        assert_eq!(
            (app.tab, &app.agents, app.overlay, app.requests.len()),
            (before.tab, &before.agents, before.overlay, 0),
            "{code:?}"
        );
    }

    // `/` opens the filter; typing narrows the list, even with keys that
    // mean something elsewhere.
    assert!(press(&mut app, &snap, KeyCode::Char('/')));
    for ch in "Ri".chars() {
        assert!(press(&mut app, &snap, KeyCode::Char(ch)));
    }
    assert_eq!(app.agents.filter, "Ri");
    assert_eq!(app.agents.sel, None, "a new filter starts at the top");
    let buf = render(&app, &snap, 120, 36);
    let names: Vec<String> = (FIRST..FIRST + 3)
        .map(|y| text(&buf, NAME_X, y, 13).trim_end().to_string())
        .collect();
    assert_eq!(names, ["IRIS", "ORION", ""], "{}", screen(&buf));
    assert!(row(&buf, 3).contains("/ Ri▏2 matches"), "{}", row(&buf, 3));
    // The heading still counts every agent.
    assert!(row(&buf, 3).contains("Agents 12 · 5 working"));

    // ⏎ keeps the filter and closes the field; esc then clears it.
    assert!(press(&mut app, &snap, KeyCode::Enter));
    assert!(app.field.is_none());
    assert_eq!(app.agents.filter, "Ri");
    assert!(press(&mut app, &snap, KeyCode::Esc));
    assert!(app.agents.filter.is_empty());

    // A name that matches nothing says so.
    app.agents.filter = "zzz".into();
    let buf = render(&app, &snap, 120, 36);
    assert!(row(&buf, FIRST).contains("No agent matches"));
}

#[test]
fn the_design_draws_the_keys_the_live_app_does_not_serve() {
    let mut design = Snapshot::fixture();
    design.serves = Serves::DESIGN;
    let buf = render(&agents_app(), &design, 120, 36);
    let footer = row(&buf, 35);
    assert!(footer.contains("⏎  open agent") && footer.contains("p  projects"));
    let live = live_with(Snapshot::fixture().agents.data);
    let buf = render(&agents_app(), &live, 120, 36);
    let footer = row(&buf, 35);
    assert!(!footer.contains("open agent") && !footer.contains("projects"));
}
