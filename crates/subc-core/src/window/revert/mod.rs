mod files;
mod floors;
mod placement;
mod report;
mod store;

#[cfg(test)]
mod tests;

pub use report::{Outcome, RevertResult};

use std::path::{Path, PathBuf};

use super::{
    journal::{self, paths, Action, Liveness, Locked, Owner, Record, Snapshot, State},
    Windows,
};
use crate::{
    setup::{install_journal::Journal, runtime::RuntimePlatform},
    stack::{self, LockedStack, RosterError, Stack, StackResult, StopIntent, WindowCheck},
};
use store::Store;

impl Windows {
    /// Revert a named window, or the newest non-terminal window. This prompts
    /// for presence and waits synchronously; call off the async executor.
    pub fn revert(&self, id: Option<&str>) -> RevertResult {
        self.revert_with(
            id,
            &System {
                windows: self,
                stack: Stack::current(self.connection.as_deref()),
            },
        )
    }

    fn revert_with(&self, id: Option<&str>, env: &impl Environment) -> RevertResult {
        let mut result = RevertResult::default();
        let plan = match self.revert_candidate(id) {
            Ok(plan) => plan,
            Err(error) => {
                result.report.message = Some(error.message);
                return result;
            }
        };
        result.read_snapshot(&plan);
        if let Err(error) = self.execute(&plan, env, &mut result) {
            result.report.message = Some(error);
        }
        if let Ok(snapshot) = journal::snapshot(&self.root, &plan.id) {
            result.read_snapshot(&snapshot);
        }
        result
    }

    fn execute(
        &self,
        plan: &Snapshot,
        env: &impl Environment,
        result: &mut RevertResult,
    ) -> Result<(), String> {
        let platform = env.platform();
        env.confirm(&prompt(plan, platform))?;
        let stack = env.lock()?;
        let locked = self
            .lock(&plan.id, plan.record_count)
            .map_err(|error| error.message)?;
        let current = locked.snapshot().clone();
        let dir = locked.directory().to_owned();

        let manifest = self.root.join("installer-manifest.json");
        if Journal::exists(&manifest)? {
            return Err(format!(
                "installer journal {} exists",
                Journal::path(&manifest).display()
            ));
        }
        check_state(&current, env)?;
        let data = env.roster().map_err(|error| {
            let message = error.message.clone();
            result.report.roster_error = Some(error);
            message
        })?;
        // Every decoded window began on an unprotected install. If the bound
        // daemon now reports protection, stop before appending anything.
        env.recheck_protection()?;
        let mut placements = Vec::new();
        if current.restored.is_none() {
            placements = placement::plan(&dir, &current)?;
            for (i, entry) in current
                .steps
                .iter()
                .enumerate()
                .filter(|(_, entry)| !entry.undone)
            {
                if let Some(store) = store_step(&dir, i, &entry.step) {
                    store.preflight()?;
                }
            }
            floors::check(&data, &dir, &current)?;
        }
        env.checkpoint("before-revert-start")?;
        let owner = env.owner()?;
        // Append rereads the count under this lock, without truncating a torn
        // tail if the prompted plan no longer matches the journal.
        let mut locked = locked;
        append(&mut locked, Record::RevertStart { owner }, result)?;
        result.set_outcome(Outcome::StoppedPartWay);
        let intent = StopIntent::Restart;

        let restored = match &current.restored {
            Some(targets) => targets.clone(),
            None => {
                let stopped = stack.stop(intent);
                let failed = stopped.exit != 0;
                result.read_stack(stopped, true);
                if failed {
                    return Err(result.stack_message("could not stop the stack"));
                }
                floors::check(&data, &dir, &current)?;
                let mut placements = placements.into_iter().peekable();
                for (i, entry) in current
                    .steps
                    .iter()
                    .enumerate()
                    .rev()
                    .filter(|(_, entry)| !entry.undone)
                {
                    if let Some(undo) = placements.next_if(|undo| undo.index == i + 1) {
                        undo.apply(&dir)?;
                    } else if let Some(store) = store_step(&dir, i, &entry.step) {
                        store.undo(|path| env.integrity(path), |point| env.checkpoint(point))?;
                    } else {
                        continue;
                    }
                    env.checkpoint("before-undo-mark")?;
                    append(&mut locked, Record::Undone { index: i + 1 }, result)?;
                    env.checkpoint("after-undo-mark")?;
                }
                let targets = placement::final_targets(&current);
                placement::check_restored(&current, &targets)?;
                append(
                    &mut locked,
                    Record::Restored {
                        targets: targets.clone(),
                    },
                    result,
                )?;
                targets
            }
        };

        placement::check_restored(&current, &restored)?;
        let live = stack.daemon_live().map_err(|report| {
            result.read_stack(*report, false);
            result.stack_message("could not check the running daemon image")
        })?;
        let started = if live && platform == RuntimePlatform::Linux {
            stack.running_health()
        } else {
            if live {
                let stopped = stack.stop(intent);
                let failed = stopped.exit != 0;
                result.read_stack(stopped, true);
                if failed {
                    return Err(result.stack_message("could not stop the running daemon"));
                }
                placement::check_restored(&current, &restored)?;
            }
            stack.start(&|| self.stack_check(Some(&plan.id)))
        };
        let failed = started.exit != 0
            && (started.exit != 1
                || started.unhealthy.is_empty()
                || started.daemon_unreachable
                || started.manager_error.is_some());
        result.read_stack(started, false);
        if failed {
            return Err(result.stack_message("could not start the stack"));
        }
        env.checkpoint("after-start")?;
        for (i, entry) in current.steps.iter().enumerate().rev() {
            if !entry.undone && matches!(entry.step.action, Action::Activation { .. }) {
                append(&mut locked, Record::Undone { index: i + 1 }, result)?;
            }
        }
        append(&mut locked, Record::RevertEnd, result)?;
        result.set_outcome(Outcome::Reverted);
        Ok(())
    }
}

fn store_step<'a>(dir: &'a Path, i: usize, step: &'a journal::Step) -> Option<Store<'a>> {
    match &step.action {
        Action::Store {
            snapshot_sha256,
            floors,
            ..
        } => Some(Store {
            target: &step.target,
            directory: dir,
            index: i + 1,
            sha: snapshot_sha256,
            floors,
        }),
        _ => None,
    }
}

fn check_state(snapshot: &Snapshot, env: &impl Environment) -> Result<(), String> {
    let state = snapshot.state(|owner| env.liveness(owner));
    match state {
        State::Open | State::Interrupted | State::RevertInterrupted => {}
        State::Reverting => {
            let owner = snapshot.revert_owner.as_ref().expect("revert owner");
            return Err(format!(
                "window {} is reverting; revert owner {}",
                snapshot.id,
                describe(owner, env.liveness(owner))
            ));
        }
        _ => return Err(format!("window {} is {state}", snapshot.id)),
    }
    if let Some((i, owner)) = snapshot.unfinished() {
        let liveness = env.liveness(owner);
        if liveness != Liveness::Gone {
            return Err(format!(
                "window {} step {i} is busy: {}",
                snapshot.id,
                describe(owner, liveness)
            ));
        }
    }
    Ok(())
}

fn describe(owner: &Owner, liveness: Liveness) -> String {
    let mut description = owner.describe();
    if liveness == Liveness::Unreadable {
        description.push_str(" (identity unreadable)");
    }
    description
}

fn prompt(plan: &Snapshot, platform: RuntimePlatform) -> String {
    let note = plan.note.as_ref().map_or_else(
        || "no note".into(),
        |note| {
            // Escape invisible characters before embedding an automation-supplied note.
            format!("note \"{}\"", note.escape_default())
        },
    );
    let outstanding = plan.steps.iter().filter(|entry| !entry.undone).count();
    let intent = if platform == RuntimePlatform::Windows {
        "Modules will be told to stop and give up long-running work."
    } else {
        "Modules will be told the stack is restarting and may keep long-running work."
    };
    format!("Allow `ck window revert` to restore window {} ({note}; {outstanding} of {} steps remaining) and restart the stack? {intent}", plan.id, plan.steps.len())
}

fn append(locked: &mut Locked, record: Record, result: &mut RevertResult) -> Result<(), String> {
    if let Some(warning) = locked.append(record)? {
        result.report.warnings.push(warning);
    }
    Ok(())
}

trait Environment {
    fn platform(&self) -> RuntimePlatform;
    fn confirm(&self, text: &str) -> Result<(), String>;
    fn lock(&self) -> Result<Box<dyn StackOps + '_>, String>;
    fn owner(&self) -> Result<Owner, String>;
    fn liveness(&self, owner: &Owner) -> Liveness;
    fn recheck_protection(&self) -> Result<(), String>;
    /// Validate the strict roster and give the root that contains module data.
    fn roster(&self) -> Result<PathBuf, RosterError>;
    fn integrity(&self, path: &Path) -> Result<(), String> {
        store::integrity_check(path)
    }
    fn checkpoint(&self, _point: &str) -> Result<(), String> {
        Ok(())
    }
}

trait StackOps {
    fn stop(&self, intent: StopIntent) -> StackResult;
    fn daemon_live(&self) -> Result<bool, Box<StackResult>>;
    fn running_health(&self) -> StackResult;
    fn start(&self, window: &dyn Fn() -> WindowCheck) -> StackResult;
}

struct System<'a> {
    windows: &'a Windows,
    stack: Result<Stack, String>,
}

impl Environment for System<'_> {
    fn platform(&self) -> RuntimePlatform {
        RuntimePlatform::current()
    }
    fn confirm(&self, text: &str) -> Result<(), String> {
        stack::confirm(text)
    }
    fn lock(&self) -> Result<Box<dyn StackOps + '_>, String> {
        let stack = self.stack.as_ref().map_err(Clone::clone)?;
        if !paths::same_path(&paths::canonical(stack.root())?, self.windows.root()) {
            return Err("the stack install home does not match the window install home".into());
        }
        Ok(Box::new(stack.lock("ck window revert")?))
    }
    fn owner(&self) -> Result<Owner, String> {
        Owner::current()
    }
    fn liveness(&self, owner: &Owner) -> Liveness {
        owner.liveness()
    }
    fn recheck_protection(&self) -> Result<(), String> {
        self.windows
            .recheck_protection()
            .map_err(|error| error.message)
    }
    fn roster(&self) -> Result<PathBuf, RosterError> {
        let run = subc_daemon::daemon_config::daemon_run_dir().map_err(|error| {
            RosterError::from(subc_daemon::RecordReadError::Io(std::io::Error::other(
                error,
            )))
        })?;
        subc_daemon::read_live_children_roster(&run.join("live-children.json"))?;
        Ok(run.parent().expect("daemon data directory").to_owned())
    }
}

impl StackOps for LockedStack<'_> {
    fn stop(&self, intent: StopIntent) -> StackResult {
        self.stop(stack::STOP_TIMEOUT, intent)
    }
    fn daemon_live(&self) -> Result<bool, Box<StackResult>> {
        self.revert_daemon_live()
    }
    fn running_health(&self) -> StackResult {
        self.revert_running_health(stack::START_TIMEOUT)
    }
    fn start(&self, window: &dyn Fn() -> WindowCheck) -> StackResult {
        self.start(stack::START_TIMEOUT, window)
    }
}
