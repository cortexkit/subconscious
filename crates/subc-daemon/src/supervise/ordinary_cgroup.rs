use crate::live_children::{ContainerIdentity, RetainedReason};
use std::{
    io,
    path::{Path, PathBuf},
    time::Duration,
};
use subc_cgroup::Placement;
use tokio::time::{sleep_until, Instant};

pub(super) struct Unresolved {
    path: PathBuf,
    reason: RetainedReason,
}

impl Unresolved {
    pub(super) fn detail(&self) -> String {
        format!(
            "cgroup cleanup unresolved at {}: {}",
            self.path.display(),
            self.reason.waiting_message(0)
        )
    }

    pub(super) fn retain(self, guard: crate::child_roster::RosterGuard) {
        let record = recorded_identity(self.path);
        guard.retain_cgroup(record, self.reason);
    }
}

pub(super) fn recorded_identity(path: PathBuf) -> ContainerIdentity {
    ContainerIdentity {
        boot_id: subc_os::boot_id::current().ok(),
        session_id: subc_os::process_group::session_id(std::process::id())
            .ok()
            .flatten(),
        cgroup_path: Some(path),
        ..ContainerIdentity::default()
    }
}

pub(super) fn is_empty(path: &Path) -> bool {
    matches!(crate::cgroup_recovery::populated(path), Ok(false))
}

async fn wait_empty(path: &Path, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    let mut interval = Duration::from_millis(1);
    loop {
        if is_empty(path)
            || matches!(path.symlink_metadata(), Err(error) if error.kind() == io::ErrorKind::NotFound)
        {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        sleep_until((now + interval).min(deadline)).await;
        interval = (interval * 2).min(Duration::from_millis(100));
    }
}

pub(super) async fn cleanup(placement: &Placement, path: &Path) -> Option<Unresolved> {
    if !is_empty(path) {
        if let subc_cgroup::FreezeOutcome::Frozen(frozen) =
            subc_cgroup::freeze_path(placement, path, Duration::from_millis(100)).await
        {
            let signalled =
                crate::cgroup_recovery::signal_frozen(&frozen, subc_os::Signal::Terminate).is_ok();
            let thawed = matches!(frozen.thaw(), subc_cgroup::ThawOutcome::Thawed);
            if signalled && thawed {
                wait_empty(path, Duration::from_secs(2)).await;
            }
        }
        if !is_empty(path) {
            let outcome = subc_cgroup::kill_path(placement, path);
            if matches!(outcome, subc_cgroup::KillOutcome::Killed) {
                wait_empty(path, Duration::from_secs(10)).await;
            } else {
                tracing::debug!(path = %path.display(), ?outcome, "could not kill ordinary cgroup subtree");
            }
        }
    }
    crate::cgroup_recovery::state(Some(placement), path)
        .filter(RetainedReason::blocks_fresh_copy)
        .map(|reason| Unresolved {
            path: path.to_owned(),
            reason,
        })
}

impl super::SupervisedChild {
    pub(super) async fn cleanup_ordinary_cgroup(&mut self) {
        if self.child.container().is_none() {
            if let (Some(placement), crate::subtree_resources::Target::Cgroup(path)) =
                (&self.cgroup_placement, &self.subtree_target)
            {
                self.unresolved_cgroup = cleanup(placement, path).await;
                self.cgroup_placement = None;
            }
        }
    }
}

pub(super) async fn await_clear(roster: &crate::child_roster::ChildRoster, module_id: &str) {
    while roster.has_retained_cgroup(module_id) && roster.previous_instance(module_id).is_some() {
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}
