//! Settings → Updates against a scripted setup engine: the once-per-session
//! check, what the section shows live, the confirmation, the run's stages,
//! and the wizard through the daemon restart it causes.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ck_ui::app::{App, Request, Tab, UPGRADE_RUNNING};
use ck_ui::live::schedule::Showing;
use ck_ui::live::{self, LiveOptions, NoService};
use ck_ui::model::{Daemon, DaemonRunning, Panel, Serves, Status};
use ck_ui::screens::overlays::choice_width;
use ck_ui::screens::upgrade::{confirmation, wrap_verbatim, DAEMON_RESTART};
use ck_ui::time::Timestamp;
use ck_ui::upgrade::{
    stage_label, Checked, PlanRow, Rollback, RollbackAnswer, RollbackChoice, RollbackQuestion, Run,
    StageEvent, StageRef, StageState, StageStatus, Updates, UpgradeCheck, UpgradeEvent,
    UpgradeOutcome, UpgradeService,
};
use ck_ui::{Snapshot, Theme};
use crossterm::event::{
    Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
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

/// The check ck setup's ownership rule refused, as `ck upgrade --check`
/// refuses on a computer where ck setup installed none of the programs it
/// found. Nothing was checked, so nothing may read as up to date.
#[test]
fn an_ownership_refusal_says_updates_cant_be_checked_and_draws_no_table() {
    let refusal = "ck upgrade manages only what ck setup installed. ck setup did not install these, so ck upgrade won't replace them:\n  ~/.local/share/cortexkit/bin/ck-subc\nMove them aside and run ck setup again. Nothing was changed.";
    let plan = UpgradeCheck {
        rows: Vec::new(),
        refusal: Some(refusal.into()),
        stages: Vec::new(),
    };
    let (app, snap) = updates_session(checked(Ok(plan)));
    let rows = render(&app, &snap);
    let text = screen(&rows);
    assert!(rows[3].contains("Updates can't be checked"), "{text}");
    for claim in [
        "up to date",
        "is available",
        "Component",
        "Installed",
        "What happens",
        "Checked",
    ] {
        assert!(
            !text.contains(claim),
            "a refused check shows {claim:?}:\n{text}"
        );
    }
    assert!(
        !rows[2..33].iter().any(|r| r.contains(&"─".repeat(40))),
        "a refused check draws the table's rule:\n{text}"
    );
    for line in wrap_verbatim(refusal, 92) {
        assert!(text.contains(line.as_str()), "{line:?} missing:\n{text}");
    }
    assert!(!marked(&rows));
    assert!(
        rows[35].contains("later") && !rows[35].contains("upgrade"),
        "{}",
        rows[35]
    );
}

/// A scripted setup engine: counts its checks, and runs by reporting the
/// plan and the given events once the test opens its gate. With a
/// `question`, it then asks it and keeps the answer; answered "roll back",
/// it reports a rollback stage that restored [`ROLLBACK_EVIDENCE`].
struct Scripted {
    checks: AtomicUsize,
    plan: UpgradeCheck,
    events: Vec<UpgradeEvent>,
    outcome: UpgradeOutcome,
    gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    question: Option<RollbackQuestion>,
    answers: Mutex<Vec<Option<RollbackChoice>>>,
}

impl Scripted {
    fn new(plan: UpgradeCheck, events: Vec<UpgradeEvent>, outcome: UpgradeOutcome) -> Scripted {
        Scripted {
            checks: AtomicUsize::new(0),
            plan,
            events,
            outcome,
            gate: Mutex::new(None),
            question: None,
            answers: Mutex::new(Vec::new()),
        }
    }
}

impl UpgradeService for Scripted {
    fn check(&self) -> Result<UpgradeCheck, String> {
        self.checks.fetch_add(1, Ordering::SeqCst);
        Ok(self.plan.clone())
    }

    fn run(
        &self,
        report: &mut dyn FnMut(UpgradeEvent),
        ask: &mut dyn FnMut(RollbackQuestion) -> Option<RollbackChoice>,
    ) -> UpgradeOutcome {
        if let Some(gate) = self.gate.lock().unwrap().take() {
            let _ = gate.recv();
        }
        report(UpgradeEvent::Planned(self.plan.clone()));
        for event in &self.events {
            report(event.clone());
        }
        if let Some(question) = &self.question {
            let answer = ask(question.clone());
            self.answers.lock().unwrap().push(answer);
            if answer == Some(RollbackChoice::RollBack) {
                report(stage("ck-subc", "rollback", StageStatus::Started));
                report(stage(
                    "ck-subc",
                    "rollback",
                    StageStatus::Ok(ROLLBACK_EVIDENCE.into()),
                ));
            }
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
    let service = Arc::new(Scripted::new(
        daemon_plan(),
        Vec::new(),
        UpgradeOutcome::default(),
    ));
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
        gate: Mutex::new(Some(gate)),
        ..Scripted::new(
            daemon_plan(),
            vec![
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
            UpgradeOutcome {
                installed: Vec::new(),
                error: Some(format!("refusal: ck-subc rollback-copy failed: {reason}")),
                warnings: Vec::new(),
            },
        )
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

/// A warning the engine keeps for the app: a previous image it couldn't
/// delete while that image's process still ran.
const HELD_IMAGE: &str =
    "warning: could not delete previous image ~/bin/.ck-aft.replaced-1-1: in use";

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
            warnings: vec![HELD_IMAGE.into()],
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
        text.contains(HELD_IMAGE),
        "the engine's warning shows as it gave it:\n{text}"
    );
    assert!(
        !marked(&render(&app, &back)),
        "an installed update isn't marked"
    );

    // Just after the run, the live layer may not have reconnected yet: the
    // wizard still holds the screen and waits for the modules.
    let mut reconnecting = down.clone();
    reconnecting.upgrade = back.upgrade.clone();
    let text = screen(&render(&app, &reconnecting));
    assert!(
        text.contains("Waiting for the daemon to report its"),
        "{text}"
    );
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

/// What the scripted rollback stage reports it restored.
const ROLLBACK_EVIDENCE: &str =
    "restored ~/.local/share/cortexkit/bin/ck-subc from ~/.local/share/cortexkit/bin/.ck-subc.rollback-1";

/// Why the daemon's check after its restart failed, as the engine words it.
const STALE: &str = "ck-subc: new process has stale version 0.36.17 (expected 0.36.18)";

/// A run of [`daemon_plan`] whose daemon passes every stage up to its check
/// after the restart, fails that check with [`STALE`], and asks whether to
/// roll back.
fn failing_check() -> Scripted {
    let mut events = Vec::new();
    for s in daemon_plan()
        .stages
        .iter()
        .filter(|s| s.target == "ck-subc")
    {
        events.push(stage(&s.target, &s.stage, StageStatus::Started));
        events.push(stage(
            &s.target,
            &s.stage,
            if s.stage == "post-verification" {
                StageStatus::Failed(STALE.into())
            } else {
                StageStatus::Ok("done".into())
            },
        ));
    }
    Scripted {
        question: Some(RollbackQuestion {
            component: "ck-subc".into(),
            old: "0.36.17".into(),
            new: "0.36.18".into(),
            reason: STALE.into(),
        }),
        ..Scripted::new(
            daemon_plan(),
            events,
            UpgradeOutcome {
                installed: Vec::new(),
                error: Some(format!(
                    "refusal: ck-subc post-verification failed: {STALE}"
                )),
                warnings: Vec::new(),
            },
        )
    }
}

/// Starts a live session on Settings → Updates and confirms the upgrade.
async fn confirmed(service: &Arc<Scripted>) -> live::Live {
    let mut live = start(Arc::clone(service));
    live.requests
        .send(Request::Show(updates_showing()))
        .unwrap();
    until(&mut live.snapshots, |s| s.upgrade.check.is_some()).await;
    live.requests.send(Request::Upgrade).unwrap();
    live
}

fn updates_app() -> App {
    let mut app = App::new(Theme::test());
    app.tab = Tab::Settings;
    app.settings.section = 2;
    app
}

/// `snap` with the daemon running and the fixture's modules, so the
/// Updates section draws instead of the daemon-down screen.
fn daemon_up(snap: &Snapshot) -> Snapshot {
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
    shown
}

fn send_requests(app: &mut App, live: &live::Live) {
    for request in app.requests.drain(..) {
        live.requests.send(request).unwrap();
    }
}

/// The daemon fails its check after the restart. The wizard asks the
/// question with the reason as the engine gave it, and the run waits for
/// the person however long they take. ⏎ rolls back and esc keeps the new
/// version, and the section says which happened, with the rollback's own
/// account shown as it was given.
#[tokio::test(flavor = "multi_thread")]
async fn a_failed_check_asks_whether_to_roll_back_and_waits_for_the_person() {
    for (key, choice) in [
        (KeyCode::Enter, RollbackChoice::RollBack),
        (KeyCode::Esc, RollbackChoice::Keep),
    ] {
        let service = Arc::new(failing_check());
        let mut live = confirmed(&service).await;
        let snap = until(&mut live.snapshots, |s| s.upgrade.question().is_some()).await;
        let mut app = updates_app();
        let text = screen(&render(&app, &daemon_up(&snap)));
        let question = service.question.clone().unwrap();
        assert_eq!(
            question.sentence(),
            format!("ck-subc 0.36.18 failed its check: {STALE}.")
        );
        for line in wrap_verbatim(&question.sentence(), choice_width(120)) {
            assert!(text.contains(line.trim_end()), "{line:?} missing:\n{text}");
        }
        assert!(text.contains("⏎  Roll back to 0.36.17"), "{text}");
        assert!(text.contains("esc  Keep 0.36.18"), "{text}");

        // Nobody has answered, so the run is still waiting, with no time
        // limit on the person.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            service.answers.lock().unwrap().is_empty(),
            "the run was answered without the person: {:?}",
            service.answers.lock().unwrap()
        );
        assert!(live.snapshots.borrow().upgrade.running());

        assert!(press(&mut app, &snap, key));
        assert_eq!(app.requests, vec![Request::Rollback(choice)]);
        send_requests(&mut app, &live);
        let snap = until(&mut live.snapshots, |s| {
            s.upgrade.run.as_ref().is_some_and(Run::finished)
        })
        .await;
        assert_eq!(*service.answers.lock().unwrap(), vec![Some(choice)]);
        let run = snap.upgrade.run.clone().unwrap();
        let text = screen(&render(&app, &daemon_up(&snap)));
        assert!(!text.contains("failed its check"), "{text}");
        match choice {
            RollbackChoice::RollBack => {
                assert_eq!(run.rollback.unwrap().answer, Some(RollbackAnswer::RollBack));
                assert!(text.contains("✓ ck-subc rolling back"), "{text}");
                assert!(text.contains("ck-subc: rolled back to 0.36.17."), "{text}");
                for line in wrap_verbatim(ROLLBACK_EVIDENCE, 92 - 2) {
                    assert!(text.contains(line.trim_end()), "{line:?} missing:\n{text}");
                }
            }
            RollbackChoice::Keep => {
                assert_eq!(run.rollback.unwrap().answer, Some(RollbackAnswer::Keep));
                assert!(text.contains("ck-subc: kept 0.36.18."), "{text}");
                assert!(!text.contains("rolling back"), "{text}");
            }
        }
    }
}

/// While the run goes, the keys that would quit or leave the wizard are
/// refused with a reason, and the run finishes. Once it has, `q` quits.
#[tokio::test(flavor = "multi_thread")]
async fn q_and_ctrl_c_mid_run_are_refused_and_the_engine_finishes() {
    let (open, gate) = std::sync::mpsc::channel();
    let service = Arc::new(Scripted {
        gate: Mutex::new(Some(gate)),
        ..Scripted::new(
            daemon_plan(),
            Vec::new(),
            UpgradeOutcome {
                installed: vec!["ck-subc 0.36.18".into(), "ck 0.20.110".into()],
                error: None,
                warnings: Vec::new(),
            },
        )
    });
    let mut live = confirmed(&service).await;
    let snap = until(&mut live.snapshots, |s| s.upgrade.running()).await;
    let mut app = updates_app();
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    for key in [
        KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE),
        ctrl('c'),
        KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE),
        ctrl('k'),
        KeyEvent::new(KeyCode::Char('1'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('7'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
        KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE),
    ] {
        app.refused = None;
        assert!(app.handle_key(key, &snap), "{key:?} drew nothing");
        assert!(!app.quit, "{key:?} quit mid-run");
        assert_eq!(app.refused, Some(UPGRADE_RUNNING), "{key:?}");
        assert_eq!(
            (app.tab, app.settings.section),
            (Tab::Settings, 2),
            "{key:?}"
        );
        assert!(app.overlay.is_none(), "{key:?} opened {:?}", app.overlay);
        assert!(app.requests.is_empty(), "{key:?} sent {:?}", app.requests);
    }
    let rows = render(&app, &daemon_up(&snap));
    assert!(rows[35].contains(UPGRADE_RUNNING), "{}", rows[35]);

    open.send(()).unwrap();
    let snap = until(&mut live.snapshots, |s| {
        s.upgrade.run.as_ref().is_some_and(Run::finished)
    })
    .await;
    assert!(!app.quit);
    assert!(press(&mut app, &daemon_up(&snap), KeyCode::Char('q')));
    assert!(app.quit, "q quits once the run has finished");
}

/// The daemon is down when the question opens (the run restarted it). The
/// daemon-down screen takes no keys, so ⏎ answers the question and starts
/// no daemon.
#[tokio::test(flavor = "multi_thread")]
async fn with_the_daemon_down_enter_answers_the_question_and_starts_no_daemon() {
    let service = Arc::new(failing_check());
    let mut live = confirmed(&service).await;
    let snap = until(&mut live.snapshots, |s| s.upgrade.question().is_some()).await;
    let mut down = snap.clone();
    down.daemon = Snapshot::fixture_daemon_down().daemon;
    assert!(matches!(down.daemon.data, Daemon::Down(_)));
    let mut app = updates_app();
    let text = screen(&render(&app, &down));
    assert!(text.contains("⏎  Roll back to 0.36.17"), "{text}");
    assert!(press(&mut app, &down, KeyCode::Enter));
    assert_eq!(
        app.requests,
        vec![Request::Rollback(RollbackChoice::RollBack)],
        "⏎ must answer the question, not start the daemon"
    );
    send_requests(&mut app, &live);
    until(&mut live.snapshots, |s| {
        s.upgrade.run.as_ref().is_some_and(Run::finished)
    })
    .await;
    assert_eq!(
        *service.answers.lock().unwrap(),
        vec![Some(RollbackChoice::RollBack)]
    );
}

/// The app goes away with the question open: the answer's channel is
/// dropped, and the run keeps the new version and says nobody answered.
#[tokio::test(flavor = "multi_thread")]
async fn a_dropped_answer_channel_keeps_the_new_version() {
    let service = Arc::new(failing_check());
    let live = confirmed(&service).await;
    let live::Live {
        mut snapshots,
        requests,
        ..
    } = live;
    until(&mut snapshots, |s| s.upgrade.question().is_some()).await;
    drop(requests);
    let snap = until(&mut snapshots, |s| {
        s.upgrade.run.as_ref().is_some_and(Run::finished)
    })
    .await;
    assert_eq!(*service.answers.lock().unwrap(), vec![None]);
    let run = snap.upgrade.run.clone().unwrap();
    assert_eq!(run.rollback.unwrap().answer, Some(RollbackAnswer::NoAnswer));
    let text = screen(&render(&updates_app(), &daemon_up(&snap)));
    assert!(text.contains("no answer; kept 0.36.18"), "{text}");
    assert!(!text.contains("rolling back"), "{text}");
}

/// The column where `word` starts on row `y` of a drawn frame, matched cell
/// by cell, so wide characters elsewhere on the row don't shift it.
fn cell_of(app: &App, snap: &Snapshot, y: u16, word: &str) -> Option<u16> {
    let mut buffer = Buffer::empty(Rect::new(0, 0, 120, 36));
    app.render(&mut buffer, snap, Snapshot::fixture_now());
    let cells: Vec<String> = word.chars().map(String::from).collect();
    let w = buffer.area.width;
    (0..w).find(|&x| {
        cells.iter().enumerate().all(|(i, ch)| {
            let cx = x + i as u16;
            cx < w && buffer[(cx, y)].symbol() == ch
        })
    })
}

/// While a run goes, the mouse can't leave the wizard either: a click on
/// another tab, on the footer, or on the question's buttons changes
/// nothing, sends nothing and asks for no redraw, whether or not the
/// rollback question is open.
#[test]
fn clicks_change_nothing_while_an_upgrade_runs() {
    let plan = daemon_plan();
    let mut going = Run::new(Snapshot::fixture_now().add_secs(-5.0), &plan.stages);
    going.record(stage(
        "ck-subc",
        "download-and-verify",
        StageStatus::Started,
    ));
    let question = RollbackQuestion {
        component: "ck-subc".into(),
        old: "0.36.17".into(),
        new: "0.36.18".into(),
        reason: STALE.into(),
    };
    for open in [false, true] {
        let mut run = going.clone();
        if open {
            run.rollback = Some(Rollback {
                question: question.clone(),
                answer: None,
            });
        }
        let mut updates = checked(Ok(plan.clone()));
        updates.run = Some(run);
        let (mut app, snap) = updates_session(updates);
        assert!(snap.upgrade.running());
        assert_eq!(snap.upgrade.question().is_some(), open);
        let before = render(&app, &snap);
        let footer = before.len() as u16 - 1;

        let mut targets = Vec::new();
        for tab in ["Home", "Modules", "Activity", "Devices"] {
            let x = cell_of(&app, &snap, 0, tab)
                .unwrap_or_else(|| panic!("no {tab} tab in the bar: {}", before[0]));
            targets.push((tab, x, 0));
        }
        let x = cell_of(&app, &snap, footer, "Upgrading")
            .unwrap_or_else(|| panic!("no footer text: {}", before[footer as usize]));
        targets.push(("the footer", x, footer));
        if open {
            let y = (0..footer)
                .find(|&y| cell_of(&app, &snap, y, "Roll back to").is_some())
                .expect("the question's buttons are drawn");
            for button in ["Roll back to", "Keep 0.36.18"] {
                let x = cell_of(&app, &snap, y, button).expect(button);
                targets.push((button, x, y));
            }
        }

        for (what, x, y) in targets {
            let click = Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            });
            assert!(
                !app.handle_event(&click, &snap, (120, 36)),
                "a click on {what} at ({x}, {y}) asked for a redraw (question open: {open})"
            );
            assert_eq!(
                (app.tab, app.settings.section),
                (Tab::Settings, 2),
                "a click on {what} left Settings → Updates (question open: {open})"
            );
            assert!(
                app.requests.is_empty(),
                "a click on {what} sent {:?}",
                app.requests
            );
            assert!(app.overlay.is_none(), "a click on {what} opened an overlay");
            assert_eq!(snap.upgrade.question().is_some(), open);
            assert_eq!(
                render(&app, &snap),
                before,
                "a click on {what} changed the frame (question open: {open})"
            );
        }
    }
}
