//! Settings → Updates against a scripted setup engine: the once-per-session
//! check, what the section shows live, the confirmation, the run's stages,
//! and the wizard through the daemon restart it causes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ck_ui::app::{App, Request, Tab};
use ck_ui::live::schedule::Showing;
use ck_ui::live::{self, LiveOptions, NoService};
use ck_ui::model::{Daemon, DaemonRunning, Panel, Serves, Status};
use ck_ui::screens::upgrade::{confirmation, wrap_verbatim, DAEMON_RESTART};
use ck_ui::time::Timestamp;
use ck_ui::upgrade::{
    stage_label, Checked, PlanRow, Run, StageEvent, StageRef, StageState, StageStatus, Updates,
    UpgradeCheck, UpgradeEvent, UpgradeOutcome, UpgradeService,
};
use ck_ui::{Snapshot, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{buffer::Buffer, layout::Rect};

fn press(app: &mut App, snap: &Snapshot, code: KeyCode) -> bool {
    app.handle_key(KeyEvent::new(code, KeyModifiers::NONE), snap)
}

fn render_at(app: &App, snap: &Snapshot, w: u16, h: u16, now: Timestamp) -> Vec<String> {
    let mut buffer = Buffer::empty(Rect::new(0, 0, w, h));
    app.render(&mut buffer, snap, now);
    (0..h)
        .map(|y| (0..w).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

fn render(app: &App, snap: &Snapshot) -> Vec<String> {
    render_at(app, snap, 120, 36, Snapshot::fixture_now())
}

fn row(component: &str, installed: &str, new: &str, daemon: bool) -> PlanRow {
    let replaces = installed != new;
    PlanRow {
        component: component.into(),
        installed: installed.into(),
        new: new.into(),
        what: if replaces { "replace" } else { "up to date" }.into(),
        replaces,
        daemon,
    }
}

fn stages(target: &str, ids: &[&str]) -> Vec<StageRef> {
    ids.iter()
        .map(|id| StageRef {
            target: target.into(),
            stage: (*id).into(),
        })
        .collect()
}

const REPLACE: [&str; 4] = [
    "download-and-verify",
    "rollback-copy",
    "destination-replacement",
    "warm-execution",
];

/// A plan that replaces the daemon and ck.
fn daemon_plan() -> UpgradeCheck {
    let mut all = stages("ck-subc", &REPLACE);
    all.extend(stages(
        "ck-subc",
        &[
            "service-manager-restart",
            "service-manager-completion",
            "post-verification",
        ],
    ));
    all.extend(stages("ck", &REPLACE));
    all.extend(stages("ck", &["post-verification"]));
    UpgradeCheck {
        rows: vec![
            row("ck", "0.20.108", "0.20.110", false),
            row("daemon (ck-subc)", "0.36.17", "0.36.18", true),
            row("ck-subc-mcp", "0.1.40", "0.1.40", false),
        ],
        refusal: None,
        stages: all,
    }
}

/// A plan that replaces only ck; the daemon is current.
fn ck_only_plan() -> UpgradeCheck {
    let mut all = stages("ck", &REPLACE);
    all.extend(stages("ck", &["post-verification"]));
    UpgradeCheck {
        rows: vec![
            row("ck", "0.20.108", "0.20.110", false),
            row("daemon (ck-subc)", "0.36.17", "0.36.17", true),
        ],
        refusal: None,
        stages: all,
    }
}

/// The fixture as the live app serves it, on Settings → Updates, with
/// `updates` as the live layer reported them.
fn updates_session(updates: Updates) -> (App, Snapshot) {
    let mut snap = Snapshot::fixture();
    snap.serves = Serves::LIVE;
    snap.upgrade = updates;
    let mut app = App::new(Theme::test());
    app.tab = Tab::Settings;
    app.settings.section = 2;
    (app, snap)
}

fn checked(result: Result<UpgradeCheck, String>) -> Updates {
    Updates {
        served: true,
        check: Some(Checked {
            at: Snapshot::fixture_now().add_secs(-12.0 * 60.0),
            result,
        }),
        run: None,
    }
}

fn screen(rows: &[String]) -> String {
    rows.join("\n")
}

/// The update mark beside Updates in Settings' section list.
fn marked(rows: &[String]) -> bool {
    rows[7].chars().nth(19) == Some('●')
}

#[test]
fn each_stage_reads_with_its_label() {
    for (id, label) in [
        ("download-and-verify", "ck-subc downloading and verifying"),
        ("rollback-copy", "ck-subc keeping a rollback copy"),
        ("destination-replacement", "ck-subc replacing"),
        ("warm-execution", "ck-subc checking it runs"),
        ("restart-initiation", "ck-subc restarting"),
        ("restart-completion", "ck-subc waiting for the restart"),
        ("service-manager-restart", "ck-subc restarting the daemon"),
        (
            "service-manager-completion",
            "ck-subc waiting for the daemon",
        ),
        ("post-verification", "ck-subc verifying"),
        ("rollback", "ck-subc rolling back"),
    ] {
        assert_eq!(stage_label("ck-subc", id), label);
    }
}

#[test]
fn the_updates_mark_appears_only_after_the_check_finds_a_newer_version() {
    let unchecked = Updates {
        served: true,
        ..Updates::default()
    };
    let (app, snap) = updates_session(unchecked);
    let rows = render(&app, &snap);
    assert!(!marked(&rows), "no mark before the check answers");
    assert!(screen(&rows).contains("Checking for updates…"));

    let mut current = ck_only_plan();
    current.rows[0].new = "0.20.108".into();
    current.rows[0].replaces = false;
    let (app, snap) = updates_session(checked(Ok(current)));
    assert!(!marked(&render(&app, &snap)), "no mark when all is current");

    let (app, snap) = updates_session(checked(Err("index unreachable".into())));
    assert!(
        !marked(&render(&app, &snap)),
        "no mark when the check failed"
    );

    let (app, snap) = updates_session(checked(Ok(daemon_plan())));
    assert!(marked(&render(&app, &snap)), "a newer version is marked");
}

#[test]
fn live_shows_the_installed_version_and_the_check_age_and_hides_unsourced_rows() {
    let (app, snap) = updates_session(checked(Ok(daemon_plan())));
    let rows = render(&app, &snap);
    let text = screen(&rows);
    assert!(
        rows[3].contains("CortexKit 0.20.110 is available"),
        "{text}"
    );
    assert!(rows[3].contains("you have 0.20.108"), "{text}");
    assert!(text.contains("Checked 12 minutes ago"), "{text}");
    for unsourced in [
        "Released",
        "signed by",
        "release notes",
        "It takes about",
        "Running agents pause",
        "releases.cortexkit.io",
        "simulates",
    ] {
        assert!(!text.contains(unsourced), "{unsourced} shows live:\n{text}");
    }
    assert!(
        !rows[35].contains(" n "),
        "the release notes key shows live: {}",
        rows[35]
    );
    assert!(rows[35].contains("upgrade") && rows[35].contains("later"));
}

#[test]
fn l_returns_to_settings_and_sends_nothing() {
    let (mut app, snap) = updates_session(checked(Ok(daemon_plan())));
    assert!(press(&mut app, &snap, KeyCode::Char('l')));
    assert_eq!(app.tab, Tab::Settings);
    assert_eq!(app.settings.section, 0);
    assert!(app.requests.is_empty());
    assert!(!app.upgrade_confirm);
}

#[test]
fn a_plan_that_replaces_only_ck_names_no_daemon_restart() {
    let (mut app, snap) = updates_session(checked(Ok(ck_only_plan())));
    let text = screen(&render(&app, &snap));
    assert!(!text.contains(DAEMON_RESTART), "{text}");
    assert!(press(&mut app, &snap, KeyCode::Enter));
    assert!(app.upgrade_confirm);
    let text = screen(&render(&app, &snap));
    assert!(text.contains("Upgrade CortexKit?"), "{text}");
    assert!(text.contains("Replaces ck 0.20.108 → 0.20.110."), "{text}");
    assert!(!text.contains("restart"), "{text}");
    for line in confirmation(&ck_only_plan()) {
        assert!(!line.contains("daemon"), "{line}");
    }
    assert!(
        app.requests.is_empty(),
        "nothing runs before the confirmation"
    );
    assert!(press(&mut app, &snap, KeyCode::Esc));
    assert!(!app.upgrade_confirm);
    assert!(app.requests.is_empty(), "esc runs nothing");
}

#[test]
fn a_plan_that_replaces_the_daemon_says_so_and_its_confirmation_names_the_restart() {
    let (mut app, snap) = updates_session(checked(Ok(daemon_plan())));
    let text = screen(&render(&app, &snap));
    assert!(text.contains(DAEMON_RESTART), "{text}");
    press(&mut app, &snap, KeyCode::Enter);
    let text = screen(&render(&app, &snap));
    assert!(text.contains("restarts the daemon"), "{text}");
    assert!(confirmation(&daemon_plan()).contains(&DAEMON_RESTART.to_string()));
    assert!(press(&mut app, &snap, KeyCode::Enter));
    assert_eq!(app.requests, vec![Request::Upgrade]);
}

#[test]
fn a_refusal_shows_verbatim_and_offers_no_upgrade() {
    let refusal = "ck upgrade manages only what ck setup installed. ck setup did not install these, so ck upgrade won't replace them:\n  ~/.local/share/cortexkit/bin/ck-auth\nMove them aside and run ck setup again. Nothing was changed.";
    let plan = UpgradeCheck {
        rows: Vec::new(),
        refusal: Some(refusal.into()),
        stages: Vec::new(),
    };
    let (mut app, snap) = updates_session(checked(Ok(plan)));
    let rows = render(&app, &snap);
    let lines = wrap_verbatim(refusal, 92);
    assert_eq!(lines.concat(), refusal.replace('\n', ""));
    let text = screen(&rows);
    for line in &lines {
        assert!(text.contains(line.as_str()), "{line:?} missing:\n{text}");
    }
    assert!(!marked(&rows));
    assert!(!press(&mut app, &snap, KeyCode::Enter));
    assert!(!app.upgrade_confirm);
}

/// A scripted setup engine: counts its checks, and runs by reporting the
/// plan and the given events once the test opens its gate.
struct Scripted {
    checks: AtomicUsize,
    plan: UpgradeCheck,
    events: Vec<UpgradeEvent>,
    outcome: UpgradeOutcome,
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
}

impl UpgradeService for Scripted {
    fn check(&self) -> Result<UpgradeCheck, String> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Ok(self.plan.clone())
    }

    fn run(&self, report: &mut dyn FnMut(UpgradeEvent)) -> UpgradeOutcome {
        if let Some(gate) = self.gate.lock().unwrap().take() {
            let _ = gate.recv();
        }
        report(UpgradeEvent::Planned(self.plan.clone()));
        for event in &self.events {
            report(event.clone());
        }
        self.outcome.clone()
    }
}

fn stage(target: &str, id: &str, status: StageStatus) -> UpgradeEvent {
    UpgradeEvent::Stage(StageEvent {
        target: target.into(),
        stage: id.into(),
        status,
    })
}

fn start(service: Arc<Scripted>) -> live::Live {
    let mut options = LiveOptions::new(
        Some(std::env::temp_dir().join("ck-ui-upgrade-test-no-daemon.json")),
        Arc::new(NoService),
    );
    options.upgrade = Some(service);
    live::start(options)
}

async fn until(
    rx: &mut tokio::sync::watch::Receiver<Snapshot>,
    what: impl Fn(&Snapshot) -> bool,
) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            {
                let snap = rx.borrow_and_update();
                if what(&snap) {
                    return snap.clone();
                }
            }
            rx.changed().await.unwrap();
        }
    })
    .await
    .expect("the live layer reported in time")
}

fn updates_showing() -> Showing {
    Showing {
        updates: true,
        ..Showing::tab(Tab::Settings)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_check_runs_once_per_session_when_updates_first_shows() {
    let service = Arc::new(Scripted {
        checks: AtomicUsize::new(0),
        plan: daemon_plan(),
        events: Vec::new(),
        outcome: UpgradeOutcome::default(),
        gate: Mutex::new(None),
    });
    let mut live = start(Arc::clone(&service));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        service.checks.load(Ordering::SeqCst),
        0,
        "not before Updates"
    );
    assert!(live.snapshots.borrow().upgrade.check.is_none());

    live.requests
        .send(Request::Show(updates_showing()))
        .unwrap();
    let snap = until(&mut live.snapshots, |s| s.upgrade.check.is_some()).await;
    assert_eq!(snap.upgrade.plan(), Some(&daemon_plan()));
    for showing in [
        Showing::tab(Tab::Home),
        updates_showing(),
        Showing::tab(Tab::Home),
    ] {
        live.requests.send(Request::Show(showing)).unwrap();
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(service.checks.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_planned_stage_is_pending_before_the_run_and_a_failure_reads_verbatim() {
    let reason = "ck-subc: new process has stale version 0.36.17 (expected 0.36.18)";
    let (open, gate) = std::sync::mpsc::channel();
    let service = Arc::new(Scripted {
        checks: AtomicUsize::new(0),
        plan: daemon_plan(),
        events: vec![
            stage("ck-subc", "download-and-verify", StageStatus::Started),
            stage(
                "ck-subc",
                "download-and-verify",
                StageStatus::Ok("archive verified".into()),
            ),
            stage("ck-subc", "rollback-copy", StageStatus::Started),
            stage(
                "ck-subc",
                "rollback-copy",
                StageStatus::Failed(reason.into()),
            ),
        ],
        outcome: UpgradeOutcome {
            installed: Vec::new(),
            error: Some(format!("refusal: ck-subc rollback-copy failed: {reason}")),
        },
        gate: Mutex::new(Some(gate)),
    });
    let mut live = start(Arc::clone(&service));
    live.requests
        .send(Request::Show(updates_showing()))
        .unwrap();
    until(&mut live.snapshots, |s| s.upgrade.check.is_some()).await;
    live.requests.send(Request::Upgrade).unwrap();
    let snap = until(&mut live.snapshots, |s| s.upgrade.run.is_some()).await;

    // Before any stage starts, every stage the plan lists is pending.
    let run = snap.upgrade.run.clone().unwrap();
    let planned = daemon_plan().stages;
    assert_eq!(run.stages.len(), planned.len());
    assert!(run.stages.iter().all(|s| s.state == StageState::Pending));
    let mut app = App::new(Theme::test());
    app.tab = Tab::Settings;
    app.settings.section = 2;
    let mut shown = snap.clone();
    shown.daemon = Panel::new(
        Snapshot::fixture_now(),
        Daemon::Running(DaemonRunning {
            version: "0.36.17".into(),
            started_at: None,
            clients: None,
        }),
    );
    let text = screen(&render(&app, &shown));
    for s in &planned {
        let line = format!("○ {}", stage_label(&s.target, &s.stage));
        assert!(text.contains(&line), "{line} missing:\n{text}");
    }

    open.send(()).unwrap();
    let snap = until(&mut live.snapshots, |s| {
        s.upgrade.run.as_ref().is_some_and(Run::finished)
    })
    .await;
    let mut shown = snap.clone();
    shown.daemon = Panel::new(
        Snapshot::fixture_now(),
        Daemon::Running(DaemonRunning {
            version: "0.36.17".into(),
            started_at: None,
            clients: None,
        }),
    );
    shown.modules = Snapshot::fixture().modules;
    let text = screen(&render(&app, &shown));
    assert!(
        text.contains("✓ ck-subc downloading and verifying"),
        "{text}"
    );
    assert!(text.contains("✗ ck-subc keeping a rollback copy"), "{text}");
    assert!(text.contains(&format!("  {reason}")), "{text}");
    assert!(
        text.contains("○ ck downloading and verifying"),
        "stages after a failure stay pending:\n{text}"
    );
    assert!(
        text.contains("✗ Upgrade stopped. Nothing was installed."),
        "{text}"
    );
    assert!(!text.contains("Checked"), "{text}");
}

/// The fixture's modules: how many there are and how many are healthy.
fn module_counts() -> (usize, usize) {
    let modules = Snapshot::fixture().modules.data;
    let healthy = modules.iter().filter(|m| m.status == Status::Ok).count();
    (healthy, modules.len())
}

#[test]
fn the_wizard_stays_through_the_daemon_restart_and_ends_with_what_happened() {
    let plan = daemon_plan();
    let started = Snapshot::fixture_now().add_secs(-30.0);
    let mut run = Run::new(started, &plan.stages);
    for s in &plan.stages[..4] {
        run.record(stage(&s.target, &s.stage, StageStatus::Started));
        run.record(stage(&s.target, &s.stage, StageStatus::Ok("done".into())));
    }
    run.record(stage(
        "ck-subc",
        "service-manager-restart",
        StageStatus::Started,
    ));
    let mut updates = checked(Ok(plan.clone()));
    updates.run = Some(run.clone());

    // The daemon goes away mid-run: the wizard keeps the screen, and ⏎
    // does not start a daemon.
    let mut down = Snapshot::fixture_daemon_down();
    down.serves = Serves::LIVE;
    down.upgrade = updates.clone();
    let mut app = App::new(Theme::test());
    app.tab = Tab::Settings;
    app.settings.section = 2;
    let text = screen(&render(&app, &down));
    assert!(text.contains("ck-subc restarting the daemon"), "{text}");
    assert!(!text.contains("isn't running"), "{text}");
    assert!(!press(&mut app, &down, KeyCode::Enter));
    assert!(
        app.requests.is_empty(),
        "no StartDaemon: {:?}",
        app.requests
    );

    // The daemon is back and the run ended: versions installed, and the
    // modules healthy out of all of them.
    for s in &plan.stages[4..] {
        run.record(stage(&s.target, &s.stage, StageStatus::Started));
        run.record(stage(&s.target, &s.stage, StageStatus::Ok("done".into())));
    }
    run.outcome = Some((
        Snapshot::fixture_now().add_secs(-1.0),
        UpgradeOutcome {
            installed: vec!["ck-subc 0.36.18".into(), "ck 0.20.110".into()],
            error: None,
        },
    ));
    let mut back = Snapshot::fixture();
    back.serves = Serves::LIVE;
    back.upgrade = Updates {
        run: Some(run.clone()),
        ..updates.clone()
    };
    let text = screen(&render(&app, &back));
    let (healthy, total) = module_counts();
    let end = format!(
        "✓ Upgraded ck-subc 0.36.18 · ck 0.20.110 in 29 s. {healthy} of {total} modules healthy."
    );
    assert!(text.contains(&end), "{end} missing:\n{text}");
    assert!(
        !marked(&render(&app, &back)),
        "an installed update isn't marked"
    );

    // Just after the run, the live layer may not have reconnected yet: the
    // wizard still holds the screen and waits for the modules.
    let mut reconnecting = down.clone();
    reconnecting.upgrade = back.upgrade.clone();
    let text = screen(&render(&app, &reconnecting));
    assert!(text.contains("Waiting for the daemon to report its"), "{text}");
    // Long after, a daemon that is still down gets its own screen back.
    let later = Snapshot::fixture_now().add_secs(60.0);
    let text = screen(&render_at(&app, &reconnecting, 120, 36, later));
    assert!(!text.contains("Upgraded"), "{text}");
}

#[test]
fn the_section_draws_at_every_size_and_says_how_many_stages_are_hidden() {
    let mut plan = daemon_plan();
    for i in 0..10 {
        plan.rows
            .push(row(&format!("ck-extra-{i}"), "1.0.0", "1.0.1", false));
        plan.stages
            .extend(stages(&format!("ck-extra-{i}"), &REPLACE));
    }
    let mut run = Run::new(Snapshot::fixture_now(), &plan.stages);
    run.record(stage(
        &plan.stages[0].target,
        &plan.stages[0].stage,
        StageStatus::Started,
    ));
    let mut updates = checked(Ok(plan.clone()));
    updates.run = Some(run);
    let (app, snap) = updates_session(updates);
    for (w, h) in [
        (80, 24),
        (100, 30),
        (120, 36),
        (140, 40),
        (160, 48),
        (200, 60),
    ] {
        let rows = render_at(&app, &snap, w, h, Snapshot::fixture_now());
        let text = screen(&rows);
        let shown = plan
            .stages
            .iter()
            .filter(|s| {
                let label = format!(" {}", stage_label(&s.target, &s.stage));
                rows.iter().any(|r| r.trim_end().ends_with(&label))
            })
            .count();
        let hidden = plan.stages.len() - shown;
        assert!(hidden > 0, "{w}x{h} shows every stage:\n{text}");
        assert!(
            text.contains(&format!("+{hidden} more")),
            "{w}x{h} hides {hidden} stages without saying so:\n{text}"
        );
        assert!(
            text.contains(&stage_label(&plan.stages[0].target, &plan.stages[0].stage)),
            "{w}x{h} lost the running stage:\n{text}"
        );
    }
}
