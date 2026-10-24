//! The update wizard's data and the service behind it.
//!
//! ck-ui can't depend on the setup engine (the engine's crate depends on
//! ck-ui), so the program that embeds the app implements [`UpgradeService`]
//! with its engine and hands it to the live layer, the way it hands over a
//! [`DaemonService`](crate::live::DaemonService). Everything that crosses
//! that boundary is a plain type defined here: plan rows, refusal text, the
//! stages a run will take, and an event per stage as it starts and ends.

use crate::time::Timestamp;

/// One component an upgrade looks at, as the plan describes it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PlanRow {
    /// The component's name, such as "ck" or "daemon (ck-subc)".
    pub component: String,
    /// The version installed now. Empty when the binary can't say.
    pub installed: String,
    /// The version the upgrade leaves installed. The same as `installed`
    /// when nothing changes.
    pub new: String,
    /// What the upgrade does to it, such as "replace" or "up to date".
    pub what: String,
    /// The run replaces this component.
    pub replaces: bool,
    /// This component is the daemon, whose replacement restarts every
    /// module.
    pub daemon: bool,
}

impl PlanRow {
    /// The run replaces the daemon, so every module restarts.
    pub fn replaces_daemon(&self) -> bool {
        self.replaces && self.daemon
    }
}

/// One stage of a run, by the engine's names: the target's label (such as
/// "ck-subc") and the stage id (such as "download-and-verify").
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageRef {
    pub target: String,
    pub stage: String,
}

/// What the setup engine planned: a row per component, the refusal that
/// stops the plan (exactly as `ck upgrade` prints it), and the stages a run
/// takes, in order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpgradeCheck {
    pub rows: Vec<PlanRow>,
    pub refusal: Option<String>,
    pub stages: Vec<StageRef>,
}

impl UpgradeCheck {
    /// The check found a newer version of something it can install.
    pub fn newer(&self) -> bool {
        self.refusal.is_none() && self.rows.iter().any(|r| r.replaces)
    }

    /// The run would replace the daemon.
    pub fn replaces_daemon(&self) -> bool {
        self.rows.iter().any(PlanRow::replaces_daemon)
    }

    /// The `ck` row, which names the version "you have".
    pub fn ck(&self) -> Option<&PlanRow> {
        self.rows.iter().find(|r| r.component == "ck")
    }
}

/// How a stage went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageStatus {
    Started,
    /// It finished; the engine's account of what it did.
    Ok(String),
    /// It failed; the engine's reason, shown as it is.
    Failed(String),
}

/// A stage starting or ending.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageEvent {
    pub target: String,
    pub stage: String,
    pub status: StageStatus,
}

/// What a run reports as it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpgradeEvent {
    /// The plan the run follows, made again when the run starts. Its stages
    /// replace those the earlier check listed.
    Planned(UpgradeCheck),
    Stage(StageEvent),
}

/// How a run ended.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UpgradeOutcome {
    /// Each component the run installed and verified, with its version,
    /// such as "ck 0.20.110".
    pub installed: Vec<String>,
    /// Why the run stopped short, as the engine said it: a refusal or a
    /// failed stage. `None` when every stage succeeded.
    pub error: Option<String>,
    /// What the engine warned about along the way, such as a rollback copy
    /// it couldn't remove, as it said it.
    pub warnings: Vec<String>,
}

/// The setup engine, as the update wizard uses it. Both methods block (they
/// read the release index, run binaries and wait for restarts); the live
/// layer calls them on a blocking thread.
pub trait UpgradeService: Send + Sync + 'static {
    /// Plans an upgrade without changing anything.
    fn check(&self) -> Result<UpgradeCheck, String>;
    /// Plans again and runs the plan, reporting the plan and then each
    /// stage's start and result through `report`.
    fn run(&self, report: &mut dyn FnMut(UpgradeEvent)) -> UpgradeOutcome;
}

/// A stage as the wizard reads it: "<target> " and what the stage does.
/// A stage id the wizard doesn't know reads as the id itself.
pub fn stage_label(target: &str, stage: &str) -> String {
    let doing = match stage {
        "download-and-verify" => "downloading and verifying",
        "rollback-copy" => "keeping a rollback copy",
        "destination-replacement" => "replacing",
        "warm-execution" => "checking it runs",
        "restart-initiation" => "restarting",
        "restart-completion" => "waiting for the restart",
        "service-manager-restart" => "restarting the daemon",
        "service-manager-completion" => "waiting for the daemon",
        "post-verification" => "verifying",
        "rollback" => "rolling back",
        other => other,
    };
    format!("{target} {doing}")
}

/// Where a stage of the run on screen is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageState {
    Pending,
    Running,
    Done(String),
    Failed(String),
}

/// One stage of the run on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageView {
    pub target: String,
    pub stage: String,
    pub state: StageState,
}

/// A run the wizard started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Run {
    pub started_at: Timestamp,
    pub stages: Vec<StageView>,
    /// When the run ended, and how.
    pub outcome: Option<(Timestamp, UpgradeOutcome)>,
}

impl Run {
    /// A run about to start: every stage of `stages` pending.
    pub fn new(started_at: Timestamp, stages: &[StageRef]) -> Run {
        Run {
            started_at,
            stages: stages.iter().map(pending).collect(),
            outcome: None,
        }
    }

    /// Applies one report from the run.
    pub fn record(&mut self, event: UpgradeEvent) {
        match event {
            // The run plans before any stage starts, so every stage it
            // lists is still pending.
            UpgradeEvent::Planned(check) => {
                self.stages = check.stages.iter().map(pending).collect();
            }
            UpgradeEvent::Stage(event) => {
                let state = match event.status {
                    StageStatus::Started => StageState::Running,
                    StageStatus::Ok(detail) => StageState::Done(detail),
                    StageStatus::Failed(reason) => StageState::Failed(reason),
                };
                // The first stage of that name not yet finished: a pending
                // one that starts, or the running one that ends.
                let slot = self.stages.iter_mut().find(|s| {
                    s.target == event.target
                        && s.stage == event.stage
                        && matches!(s.state, StageState::Pending | StageState::Running)
                });
                match slot {
                    Some(slot) => slot.state = state,
                    None => self.stages.push(StageView {
                        target: event.target,
                        stage: event.stage,
                        state,
                    }),
                }
            }
        }
    }

    pub fn finished(&self) -> bool {
        self.outcome.is_some()
    }
}

fn pending(r: &StageRef) -> StageView {
    StageView {
        target: r.target.clone(),
        stage: r.stage.clone(),
        state: StageState::Pending,
    }
}

/// The update check as of now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Checked {
    pub at: Timestamp,
    pub result: Result<UpgradeCheck, String>,
}

/// Everything the Updates section shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Updates {
    /// The program running the app supplied an [`UpgradeService`].
    pub served: bool,
    /// The session's check, once it has answered.
    pub check: Option<Checked>,
    /// The run started from the wizard, if any.
    pub run: Option<Run>,
}

impl Updates {
    /// What the check planned, when it succeeded.
    pub fn plan(&self) -> Option<&UpgradeCheck> {
        self.check.as_ref().and_then(|c| c.result.as_ref().ok())
    }

    /// Whether the nav marks Updates: the check found a newer version and no
    /// run has installed it since.
    pub fn marked(&self) -> bool {
        let installed = self
            .run
            .as_ref()
            .and_then(|r| r.outcome.as_ref())
            .is_some_and(|(_, o)| o.error.is_none());
        !installed && self.plan().is_some_and(UpgradeCheck::newer)
    }

    /// A run has started and not ended.
    pub fn running(&self) -> bool {
        self.run.as_ref().is_some_and(|r| !r.finished())
    }
}
