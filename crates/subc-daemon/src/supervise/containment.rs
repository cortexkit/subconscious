//! A module actor owns the container; individual launch generations only borrow it.

use super::*;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::sync::Weak;

pub(crate) type Slot = Arc<Mutex<Option<Arc<CarriedContainment>>>>;

#[derive(Debug)]
pub(crate) struct CarriedContainment {
    #[cfg(unix)]
    module_id: String,
    #[cfg(unix)]
    counters: crate::observability::DaemonCounters,
    #[cfg(unix)]
    cleanup_block: Mutex<Option<crate::live_children::RetainedReason>>,
    #[cfg(all(test, unix))]
    group_signals: Mutex<Option<Arc<Mutex<Vec<rustix::process::Signal>>>>>,
    resource: Resource,
    roster: Mutex<Option<crate::child_roster::RosterGuard>>,
    terminated: std::sync::atomic::AtomicBool,
    ending: std::sync::atomic::AtomicBool,
    admitting: std::sync::atomic::AtomicBool,
    admission_changed: tokio::sync::Notify,
    cleanup_finished: tokio::sync::Notify,
    hard_killed: std::sync::atomic::AtomicBool,
    kill_notify: tokio::sync::Notify,
}

#[derive(Debug)]
enum Resource {
    #[cfg(unix)]
    Group {
        pgid: u32,
        created_start_time: Option<u64>,
        identity: LaunchIdentity,
        anchor_lost: std::sync::atomic::AtomicBool,
        pin: Mutex<Option<Child>>,
        pending_pin: std::sync::atomic::AtomicBool,
    },
    #[cfg(target_os = "linux")]
    Cgroup {
        placement: subc_cgroup::Placement,
        name: String,
        path: PathBuf,
        identity: LaunchIdentity,
    },
    #[cfg(windows)]
    Job(Arc<Mutex<subc_jobobject::JobObject>>),
}

#[cfg(unix)]
#[derive(Debug, Clone)]
pub(super) struct LaunchIdentity {
    pub(super) boot_id: String,
    pub(super) session_id: u32,
}

#[cfg(unix)]
impl LaunchIdentity {
    pub(super) fn capture() -> io::Result<Self> {
        Ok(Self {
            boot_id: subc_os::boot_id::current()?,
            // Setting a process group does not change the inherited session.
            // Capture it before fork, so even a fast-exiting first leader has
            // a session identity recorded before it can be reaped.
            session_id: subc_os::process_group::session_id(std::process::id())?
                .ok_or_else(|| io::Error::other("daemon session identity is unavailable"))?,
        })
    }
}

impl CarriedContainment {
    #[cfg(unix)]
    fn launch_identity(&self) -> &LaunchIdentity {
        match &self.resource {
            Resource::Group { identity, .. } => identity,
            #[cfg(target_os = "linux")]
            Resource::Cgroup { identity, .. } => identity,
        }
    }
    fn claim_failed_admission(&self, failure: crate::child_roster::AdmissionError) -> io::Error {
        let (source, guard) = failure.into_parts();
        *self.roster.lock().unwrap_or_else(|p| p.into_inner()) = Some(guard);
        source
    }

    #[cfg(unix)]
    fn note_observation_error(&self, pid: u32, error: &io::Error) {
        #[allow(irrefutable_let_patterns)]
        let Resource::Group {
            pgid,
            created_start_time,
            anchor_lost,
            ..
        } = &self.resource
        else {
            return;
        };
        if *pgid == pid
            && error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error())
            && !anchor_lost.swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.counters.increment_carried_containment_anchor_lost();
            error!(module_id = %self.module_id, pgid, ?created_start_time, %error,
                "carried process-group anchor was unexpectedly reaped; only verified per-PID cleanup is safe");
        }
    }

    #[cfg(unix)]
    fn recorded_group(&self) -> Option<crate::group_recovery::RecordedGroup<'_>> {
        match &self.resource {
            Resource::Group {
                pgid,
                created_start_time: Some(created_start_time),
                identity,
                ..
            } => Some(crate::group_recovery::RecordedGroup {
                pgid: *pgid,
                created_start_time: *created_start_time,
                boot_id: &identity.boot_id,
                session_id: identity.session_id,
            }),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    #[cfg(unix)]
    fn signal_pinned_group(&self, signal: rustix::process::Signal) -> bool {
        #[allow(irrefutable_let_patterns)]
        let Resource::Group {
            pgid,
            pin,
            anchor_lost,
            ..
        } = &self.resource
        else {
            return false;
        };
        let pin = pin.lock().unwrap_or_else(|p| p.into_inner());
        if !self.owns_pin(*pgid, &pin, anchor_lost) {
            return false;
        }
        #[cfg(test)]
        if let Some(signals) = self
            .group_signals
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
        {
            signals
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .push(signal);
            return true;
        }
        signal_group(*pgid, signal);
        true
    }

    #[cfg(unix)]
    fn owns_pin(
        &self,
        pgid: u32,
        pin: &Option<Child>,
        anchor_lost: &std::sync::atomic::AtomicBool,
    ) -> bool {
        let pending = matches!(&self.resource, Resource::Group { pending_pin, .. }
            if pending_pin.load(std::sync::atomic::Ordering::Acquire));
        if anchor_lost.load(std::sync::atomic::Ordering::Acquire)
            || (pin.as_ref().and_then(Child::id) != Some(pgid) && !pending)
        {
            return false;
        }
        // A retained Tokio handle is not proof: another waiter may have reaped
        // the child. Reconfirm ownership before every group-wide signal.
        if let Err(error) = subc_os::child_exit::observe(pgid) {
            self.note_observation_error(pgid, &error);
            warn!(module_id = %self.module_id, pgid, %error, "cannot verify the process-group pin; refusing a group-wide signal");
            return false;
        }
        let Some(record) = self.recorded_group() else {
            return false;
        };
        #[cfg(target_os = "macos")]
        let birth = subc_os::owned_child_start_time(pgid);
        #[cfg(not(target_os = "macos"))]
        let birth = subc_os::process_group::inspect(pgid).and_then(|identity| {
            identity.map(|identity| identity.start_time).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "process-group pin identity is missing",
                )
            })
        });
        match birth {
            Ok(actual) if actual == record.created_start_time => {}
            actual => {
                error!(module_id = %self.module_id, pgid, expected = record.created_start_time,
                    ?actual, "cannot verify the exact process-group pin birth; refusing signal or join");
                return false;
            }
        }
        true
    }

    #[cfg(unix)]
    pub(super) fn verified_join_group(&self) -> io::Result<Option<u32>> {
        #[allow(irrefutable_let_patterns)]
        if let Resource::Group {
            pgid,
            pin,
            anchor_lost,
            ..
        } = &self.resource
        {
            let pin = pin.lock().unwrap_or_else(|p| p.into_inner());
            if !self.owns_pin(*pgid, &pin, anchor_lost) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "cannot join a carried process group without its exact owned pin identity",
                ));
            }
            return Ok(Some(*pgid));
        }
        Ok(None)
    }

    #[cfg(unix)]
    async fn recover_group(&self) {
        let Some(record) = self.recorded_group() else {
            *self.cleanup_block.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(crate::live_children::RetainedReason::ContainerUnverifiable(
                    "unpinned group has no recorded creation identity".into(),
                ));
            return;
        };
        let decision =
            crate::group_recovery::sweep(record, crate::live_children::SweepBounds::default())
                .await;
        *self.cleanup_block.lock().unwrap_or_else(|p| p.into_inner()) = match decision {
            crate::live_children::SweepDecision::Unverifiable(error) => Some(
                crate::live_children::RetainedReason::ContainerUnverifiable(error),
            ),
            crate::live_children::SweepDecision::ContainerSurvivors(pids) => Some(
                crate::live_children::RetainedReason::ContainerSurvivors(pids),
            ),
            _ => None,
        };
    }

    fn release_roster(&self) {
        let guard = self.roster.lock().unwrap_or_else(|p| p.into_inner()).take();
        #[cfg(unix)]
        if let Some(reason) = self
            .cleanup_block
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take()
        {
            if let Some(guard) = guard {
                guard.retain(reason);
            }
            return;
        }
        drop(guard);
    }

    pub(crate) fn group(&self) -> Option<u32> {
        match &self.resource {
            #[cfg(unix)]
            Resource::Group { pgid, .. } => Some(*pgid),
            #[allow(unreachable_patterns)]
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn cgroup(&self) -> Option<(&subc_cgroup::Placement, &str)> {
        match &self.resource {
            Resource::Cgroup {
                placement, name, ..
            } => Some((placement, name)),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) fn cgroup_path(&self) -> Option<&Path> {
        match &self.resource {
            Resource::Cgroup { path, .. } => Some(path),
            _ => None,
        }
    }

    #[cfg(target_os = "linux")]
    async fn end_cgroup(&self, placement: &subc_cgroup::Placement, path: &Path) {
        let decision = crate::cgroup_recovery::sweep(
            Some(placement),
            path,
            crate::live_children::SweepBounds::default(),
        )
        .await;
        let reason = match decision {
            crate::live_children::SweepDecision::Unverifiable(error) => Some(
                crate::live_children::RetainedReason::ContainerUnverifiable(error),
            ),
            other => other.retained_reason(),
        };
        *self.cleanup_block.lock().unwrap_or_else(|p| p.into_inner()) = reason;
    }

    #[cfg(windows)]
    pub(super) fn job(&self) -> Arc<Mutex<subc_jobobject::JobObject>> {
        match &self.resource {
            Resource::Job(job) => Arc::clone(job),
        }
    }

    /// Shutdown escalation may kill members before the actor has recorded the
    /// leader's logical exit. Keep the group's first leader unreaped, reserving
    /// its process-group ID, until that record has been published.
    pub(crate) fn hard_kill(&self) {
        if self.terminated.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        self.hard_killed
            .store(true, std::sync::atomic::Ordering::Release);
        self.kill_notify.notify_waiters();
        match &self.resource {
            #[cfg(unix)]
            Resource::Group { .. } => {
                self.signal_pinned_group(rustix::process::Signal::KILL);
                // Signal acceptance does not establish that all live members
                // have exited, including when Drop is the only cleanup owner.
                *self.cleanup_block.lock().unwrap_or_else(|p| p.into_inner()) =
                    self.recorded_group().map_or_else(
                        || {
                            Some(crate::live_children::RetainedReason::ContainerUnverifiable(
                                "unpinned group has no recorded creation identity".into(),
                            ))
                        },
                        crate::group_recovery::recheck,
                    );
            }
            #[cfg(target_os = "linux")]
            Resource::Cgroup {
                placement, path, ..
            } => {
                *self.cleanup_block.lock().unwrap_or_else(|p| p.into_inner()) =
                    crate::cgroup_recovery::recheck(Some(placement), path);
            }
            #[cfg(windows)]
            Resource::Job(job) => {
                if let Err(error) = job.lock().unwrap_or_else(|p| p.into_inner()).terminate() {
                    warn!(%error, "carried job termination failed");
                }
            }
        }
    }

    pub(super) async fn stop_members(&self) {
        if self.terminated.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        match &self.resource {
            #[cfg(unix)]
            Resource::Group { .. } => {
                if self.signal_pinned_group(rustix::process::Signal::TERM) {
                    self.grace(crate::child_roster::TERM_TO_KILL).await;
                } else {
                    self.recover_group().await;
                }
            }
            #[cfg(target_os = "linux")]
            Resource::Cgroup {
                placement, path, ..
            } => {
                self.end_cgroup(placement, path).await;
            }
            #[cfg(windows)]
            Resource::Job(_) => {}
        }
        self.hard_kill();
    }

    pub(super) async fn terminate(&self) {
        if self.ending.swap(true, std::sync::atomic::Ordering::AcqRel) {
            while !self.terminated.load(std::sync::atomic::Ordering::Acquire) {
                let changed = self.cleanup_finished.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if !self.terminated.load(std::sync::atomic::Ordering::Acquire) {
                    changed.await;
                }
            }
            return;
        }
        while self.admitting.load(std::sync::atomic::Ordering::Acquire) {
            self.hard_kill();
            let changed = self.admission_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.admitting.load(std::sync::atomic::Ordering::Acquire) {
                changed.await;
            }
        }
        if self.terminated.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        match &self.resource {
            #[cfg(unix)]
            Resource::Group { pin, .. } => {
                if self.signal_pinned_group(rustix::process::Signal::TERM) {
                    self.grace(crate::child_roster::TERM_TO_KILL).await;
                } else {
                    self.recover_group().await;
                }
                self.hard_kill();
                let owned_pin = pin.lock().unwrap_or_else(|p| p.into_inner()).take();
                if let Some(mut child) = owned_pin {
                    // Rejection cleanup may run on a separate timer driver
                    // while the child's original I/O reactor is blocked.
                    loop {
                        match child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => sleep(Duration::from_millis(1)).await,
                            Err(error) => {
                                warn!(module_id = %self.module_id, %error, "carried process-group pin reap failed");
                                break;
                            }
                        }
                    }
                }
                // A successful group signal is not proof that every member
                // exited. Retain an unverifiable or surviving tree before a
                // caller can start a fresh container after a failed join.
                self.recover_group().await;
            }
            #[cfg(target_os = "linux")]
            Resource::Cgroup {
                placement, path, ..
            } => {
                self.end_cgroup(placement, path).await;
            }
            #[cfg(windows)]
            Resource::Job(_) => self.hard_kill(),
        }
        self.release_roster();
        self.terminated
            .store(true, std::sync::atomic::Ordering::Release);
        self.cleanup_finished.notify_waiters();
    }

    #[cfg(unix)]
    async fn grace(&self, duration: Duration) {
        let killed = self.kill_notify.notified();
        tokio::pin!(killed);
        killed.as_mut().enable();
        if self.hard_killed.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        // A second daemon stop or the roster's kill deadline ends this grace
        // immediately, leaving the shutdown budget available for reap/record.
        tokio::select! { _ = sleep(duration) => {}, _ = killed => {} }
    }
}

impl Drop for CarriedContainment {
    fn drop(&mut self) {
        // Abort and unwinding cannot await graceful cleanup. Kill before the
        // retained Child can be dropped and handed to Tokio's background reaper.
        self.hard_kill();
        self.release_roster();
    }
}

#[cfg(unix)]
fn signal_group(pgid: u32, signal: rustix::process::Signal) {
    let Some(group) = i32::try_from(pgid)
        .ok()
        .filter(|pid| *pid > 1)
        .and_then(rustix::process::Pid::from_raw)
    else {
        return;
    };
    if let Err(error) = rustix::process::kill_process_group(group, signal) {
        if !matches!(error, rustix::io::Errno::SRCH | rustix::io::Errno::PERM) {
            warn!(pgid, errno = error.raw_os_error(), %error, "carried process-group signal failed");
        }
    }
}

pub(super) fn current(slot: &Slot) -> Option<Arc<CarriedContainment>> {
    slot.lock().unwrap_or_else(|p| p.into_inner()).clone()
}

pub(super) async fn terminate(slot: &Slot) {
    let owned = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(container) = owned {
        container.terminate().await;
    }
}

pub(super) fn terminate_blocking(slot: &Slot) {
    let owned = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(container) = owned {
        #[cfg(unix)]
        if container
            .admitting
            .load(std::sync::atomic::Ordering::Acquire)
        {
            container.hard_kill();
            std::thread::spawn(move || finish_admission_cleanup(container));
            return;
        }
        // Spawn APIs are synchronous, including rejection cleanup. A separate
        // timer driver completes the same finalizer even on a current-thread
        // caller, without attempting a nested Tokio block_on.
        std::thread::scope(|scope| {
            let result = scope
                .spawn(move || {
                    match tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                    {
                        Ok(runtime) => runtime.block_on(container.terminate()),
                        Err(error) => {
                            warn!(%error, "could not create carried-container cleanup driver")
                        }
                    }
                })
                .join();
            if result.is_err() {
                warn!("carried-container cleanup thread panicked");
            }
        });
    }
}

/// The initial Unix group leader belongs to the container, not its launch.
/// All carried Unix waits observe only; the actor reaps after classifying exit.
#[derive(Debug)]
pub(super) struct ProcessChild {
    process: Option<Child>,
    container: Weak<CarriedContainment>,
    slot: Weak<Mutex<Option<Arc<CarriedContainment>>>>,
    #[cfg(unix)]
    pid: u32,
    #[cfg(unix)]
    observed: Option<ExitStatus>,
    settled: bool,
}

impl ProcessChild {
    pub(super) fn reject(&mut self, #[cfg(unix)] group: GroupTarget) {
        if let Some(child) = &mut self.process {
            super::kill_and_reap_rejected_child(
                child,
                #[cfg(unix)]
                group,
            );
        }
    }
    pub(super) fn plain(process: Child, pid: u32) -> Self {
        let _ = pid;
        Self {
            process: Some(process),
            container: Weak::new(),
            slot: Weak::new(),
            #[cfg(unix)]
            pid,
            #[cfg(unix)]
            observed: None,
            settled: false,
        }
    }

    pub(super) fn container(&self) -> Option<Arc<CarriedContainment>> {
        self.container.upgrade()
    }

    pub(super) async fn terminate_container(&self) {
        if let Some(slot) = self.slot.upgrade() {
            terminate(&slot).await;
        }
    }

    #[cfg(unix)]
    pub(super) fn id(&self) -> Option<u32> {
        if self.container().is_some() {
            Some(self.pid)
        } else {
            self.process.as_ref().and_then(Child::id)
        }
    }

    pub(super) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        #[cfg(unix)]
        if self.container().is_some() {
            if self.observed.is_none() {
                self.observed = match subc_os::child_exit::observe(self.pid) {
                    Ok(observed) => observed,
                    Err(error) => {
                        if let Some(container) = self.container() {
                            container.note_observation_error(self.pid, &error);
                        }
                        return Err(error);
                    }
                };
            }
            return Ok(self.observed);
        }
        self.process
            .as_mut()
            .ok_or_else(|| io::Error::other("process-group pin is no longer owned"))?
            .try_wait()
    }

    pub(super) async fn wait(&mut self) -> io::Result<ExitStatus> {
        #[cfg(unix)]
        if self.container().is_some() {
            loop {
                if let Some(status) = self.try_wait()? {
                    return Ok(status);
                }
                sleep(Duration::from_millis(1)).await;
            }
        }
        self.process
            .as_mut()
            .ok_or_else(|| io::Error::other("process-group pin is no longer owned"))?
            .wait()
            .await
    }

    pub(super) fn start_kill(&mut self) -> io::Result<()> {
        #[cfg(unix)]
        if let Some(container) = self.container() {
            if let Err(error) = subc_os::child_exit::observe(self.pid) {
                container.note_observation_error(self.pid, &error);
                return Err(error);
            }
        }
        if let Some(process) = &mut self.process {
            return process.start_kill();
        }
        #[cfg(unix)]
        if let Some(container) = self.container() {
            match &container.resource {
                Resource::Group { pin, .. } => {
                    if let Some(pin) = pin.lock().unwrap_or_else(|p| p.into_inner()).as_mut() {
                        return pin.start_kill();
                    }
                }
                #[cfg(target_os = "linux")]
                Resource::Cgroup { .. } => {}
            }
        }
        Err(io::Error::other("process-group pin is no longer owned"))
    }

    pub(super) async fn reap(&mut self) -> io::Result<()> {
        if let Some(process) = &mut self.process {
            process.wait().await?;
        }
        self.settled = true;
        Ok(())
    }
}

pub(super) fn adopt(
    child: &mut SupervisedChild,
    spec: &ModuleSpec,
    roster: &ChildRoster,
) -> Result<(), SuperviseError> {
    #[cfg(test)]
    if roster.take_adoption_failure_for_test() {
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other("test rejected carried adoption"),
            cgroup_path: None,
        });
    }
    let container = current(&roster.containment);
    if container.is_none() && !spec.carry_containment {
        return Ok(());
    }
    #[cfg(unix)]
    let container = container.ok_or_else(|| SuperviseError::Spawn {
        program: spec.program.clone(),
        source: io::Error::other("carried container identity was lost before final adoption"),
        cgroup_path: None,
    })?;
    #[cfg(windows)]
    let container = match container {
        Some(container) => container,
        None => adopt_suspended_job(child, spec, roster)?,
    };
    if container.ending.load(std::sync::atomic::Ordering::Acquire) {
        return Err(SuperviseError::Spawn {
            program: spec.program.clone(),
            source: io::Error::other("carried container ended during admission"),
            cgroup_path: None,
        });
    }
    #[cfg(unix)]
    #[allow(irrefutable_let_patterns)]
    if let Resource::Group {
        pgid,
        pin,
        pending_pin,
        ..
    } = &container.resource
    {
        if *pgid == child.pid {
            let mut pin = pin.lock().unwrap_or_else(|p| p.into_inner());
            if container.ending.load(std::sync::atomic::Ordering::Acquire) {
                return Err(SuperviseError::Spawn {
                    program: spec.program.clone(),
                    source: io::Error::other("carried container ended before pin transfer"),
                    cgroup_path: None,
                });
            }
            if pending_pin.swap(false, std::sync::atomic::Ordering::AcqRel) {
                *pin = child.child.process.take();
            }
        }
    }
    child.child.container = Arc::downgrade(&container);
    child.child.slot = Arc::downgrade(&roster.containment);
    #[cfg(windows)]
    {
        child.job = None;
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn prepare_before_exec(
    spec: &ModuleSpec,
    roster: &ChildRoster,
    pid: u32,
    birth: u64,
    identity: LaunchIdentity,
    #[cfg(target_os = "linux")] cgroup: Option<(subc_cgroup::Placement, String, PathBuf)>,
) -> Arc<CarriedContainment> {
    let group = || Resource::Group {
        pgid: pid,
        created_start_time: Some(birth),
        identity: identity.clone(),
        anchor_lost: std::sync::atomic::AtomicBool::new(false),
        pin: Mutex::new(None),
        // The readiness pipe proves this is our unreaped forked child, even
        // though Command::spawn cannot return its handle until after go.
        pending_pin: std::sync::atomic::AtomicBool::new(true),
    };
    #[cfg(target_os = "linux")]
    let resource = match cgroup {
        Some((placement, name, path)) => Resource::Cgroup {
            placement,
            name,
            path,
            identity,
        },
        None => group(),
    };
    #[cfg(not(target_os = "linux"))]
    let resource = group();
    let owned = Arc::new(CarriedContainment {
        module_id: spec.module_id.clone(),
        counters: roster.counters.clone(),
        cleanup_block: Mutex::new(None),
        #[cfg(test)]
        group_signals: Mutex::new(None),
        resource,
        roster: Mutex::new(None),
        terminated: std::sync::atomic::AtomicBool::new(false),
        ending: std::sync::atomic::AtomicBool::new(false),
        admitting: std::sync::atomic::AtomicBool::new(false),
        admission_changed: tokio::sync::Notify::new(),
        cleanup_finished: tokio::sync::Notify::new(),
        hard_killed: std::sync::atomic::AtomicBool::new(false),
        kill_notify: tokio::sync::Notify::new(),
    });
    *roster.containment.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&owned));
    owned
}

#[cfg(windows)]
fn adopt_suspended_job(
    child: &mut SupervisedChild,
    spec: &ModuleSpec,
    roster: &ChildRoster,
) -> Result<Arc<CarriedContainment>, SuperviseError> {
    let owned = Arc::new(CarriedContainment {
        resource: Resource::Job(
            child
                .job
                .take()
                .expect("suspended child was assigned to its job"),
        ),
        roster: Mutex::new(None),
        terminated: std::sync::atomic::AtomicBool::new(false),
        ending: std::sync::atomic::AtomicBool::new(false),
        admitting: std::sync::atomic::AtomicBool::new(false),
        admission_changed: tokio::sync::Notify::new(),
        cleanup_finished: tokio::sync::Notify::new(),
        hard_killed: std::sync::atomic::AtomicBool::new(false),
        kill_notify: tokio::sync::Notify::new(),
    });
    *roster.containment.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::clone(&owned));
    let recorded = crate::child_roster::RecordedIdentity {
        start_time: child.process_start_time,
        container: Some(crate::live_children::ContainerIdentity {
            created_start_time: child.process_start_time,
            ..Default::default()
        }),
        ..Default::default()
    };
    match roster.admit(
        spec.module_id.clone(),
        child.pid,
        child.protocol,
        child.process_start_time,
        recorded,
    ) {
        Ok(guard) => *owned.roster.lock().unwrap_or_else(|p| p.into_inner()) = Some(guard),
        Err(failure) => {
            let source = owned.claim_failed_admission(failure);
            terminate_blocking(&roster.containment);
            return Err(SuperviseError::Spawn {
                program: spec.program.clone(),
                source,
                cgroup_path: None,
            });
        }
    }
    Ok(owned)
}

#[cfg(target_os = "linux")]
pub(super) fn prepare_cgroup_path(
    placement: &subc_cgroup::Placement,
    name: &str,
) -> io::Result<PathBuf> {
    let path = placement.module_path(name)?;
    let canonical = std::fs::canonicalize(&path)?;
    placement.existing_module_path(&canonical)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "new cgroup disappeared before spawn",
        )
    })
}

#[cfg(unix)]
impl CarriedContainment {
    pub(super) fn admit_before_exec(
        self: &Arc<Self>,
        spec: &ModuleSpec,
        roster: &ChildRoster,
        pid: u32,
        birth: u64,
    ) -> io::Result<()> {
        let recorded = crate::child_roster::RecordedIdentity {
            start_time: Some(birth),
            executable: None,
            cgroup_name: {
                #[cfg(target_os = "linux")]
                {
                    self.cgroup().map(|(_, name)| name.to_owned())
                }
                #[cfg(not(target_os = "linux"))]
                {
                    None
                }
            },
            container: Some(crate::live_children::ContainerIdentity {
                pgid: self.group(),
                created_start_time: Some(birth),
                boot_id: Some(self.launch_identity().boot_id.clone()),
                session_id: Some(self.launch_identity().session_id),
                cgroup_path: {
                    #[cfg(target_os = "linux")]
                    {
                        self.cgroup_path().map(Path::to_owned)
                    }
                    #[cfg(not(target_os = "linux"))]
                    {
                        None
                    }
                },
            }),
            carried: Some(Arc::downgrade(self)),
            #[cfg(target_os = "linux")]
            cgroup_placement: self.cgroup().map(|(placement, _)| placement.clone()),
        };
        match roster.admit(
            spec.module_id.clone(),
            pid,
            spec.protocol,
            Some(birth),
            recorded,
        ) {
            Ok(guard) => {
                *self.roster.lock().unwrap_or_else(|p| p.into_inner()) = Some(guard);
                Ok(())
            }
            Err(failure) => Err(self.claim_failed_admission(failure)),
        }
    }

    pub(super) fn begin_admission(&self) {
        self.admitting
            .store(true, std::sync::atomic::Ordering::Release);
    }

    pub(super) fn end_admission(&self) {
        self.admitting
            .store(false, std::sync::atomic::Ordering::Release);
        self.admission_changed.notify_waiters();
    }

    pub(super) fn is_ending(&self) -> bool {
        self.ending.load(std::sync::atomic::Ordering::Acquire)
    }
}

pub(super) fn terminate_deferred(slot: &Slot) {
    let owned = slot.lock().unwrap_or_else(|p| p.into_inner()).take();
    if let Some(container) = owned {
        // Do not wait behind an uncancellable admission write or a child stuck
        // in placement. The container identity stays owned by this finalizer.
        container.hard_kill();
        std::thread::spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
            {
                Ok(runtime) => runtime.block_on(container.terminate()),
                Err(error) => warn!(%error, "could not start deferred carried-container cleanup"),
            }
        });
    }
}

#[cfg(unix)]
pub(super) fn finish_admission_cleanup(container: Arc<CarriedContainment>) {
    match tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
    {
        Ok(runtime) => runtime.block_on(container.terminate()),
        Err(error) => warn!(%error, "could not finish carried admission cleanup"),
    }
}

#[cfg(unix)]
#[derive(Clone)]
pub(super) enum GroupTarget {
    OwnLeader,
    Pinned(Arc<CarriedContainment>),
    NoGroup,
}

#[cfg(unix)]
impl GroupTarget {
    pub(super) fn signal_rejected(&self, child: &Child) {
        match self {
            Self::OwnLeader => {
                if let Some(pid) = child.id() {
                    signal_group(pid, rustix::process::Signal::KILL);
                }
            }
            Self::Pinned(container) => container.hard_kill(),
            Self::NoGroup => {}
        }
    }
}

impl Drop for ProcessChild {
    fn drop(&mut self) {
        if !self.settled {
            if let Some(container) = self.container() {
                container.hard_kill();
                let _ = self.start_kill();
            }
        }
    }
}

#[cfg(all(test, unix))]
mod anchor_tests {
    use super::*;

    #[tokio::test]
    async fn mismatched_pin_birth_refuses_every_group_signal_and_join() {
        let mut command = Command::new("/bin/sleep");
        command.arg("600").process_group(0).kill_on_drop(true);
        let leader = command.spawn().unwrap();
        let pgid = leader.id().unwrap();
        let actual = subc_os::process_group::inspect(pgid)
            .unwrap()
            .unwrap()
            .start_time;
        let group_signals = Arc::new(Mutex::new(Vec::new()));
        let container = CarriedContainment {
            module_id: "mismatched-pin-birth".into(),
            counters: crate::observability::DaemonCounters::default(),
            cleanup_block: Mutex::new(None),
            group_signals: Mutex::new(Some(Arc::clone(&group_signals))),
            resource: Resource::Group {
                identity: LaunchIdentity::capture().unwrap(),
                pgid,
                created_start_time: Some(actual + 1),
                anchor_lost: std::sync::atomic::AtomicBool::new(false),
                pin: Mutex::new(Some(leader)),
                pending_pin: std::sync::atomic::AtomicBool::new(false),
            },
            roster: Mutex::new(None),
            terminated: std::sync::atomic::AtomicBool::new(false),
            ending: std::sync::atomic::AtomicBool::new(false),
            admitting: std::sync::atomic::AtomicBool::new(false),
            admission_changed: tokio::sync::Notify::new(),
            cleanup_finished: tokio::sync::Notify::new(),
            hard_killed: std::sync::atomic::AtomicBool::new(false),
            kill_notify: tokio::sync::Notify::new(),
        };
        let joined = container.verified_join_group();
        let signalled = container.signal_pinned_group(rustix::process::Signal::TERM);
        assert!(joined.is_err() && !signalled && group_signals.lock().unwrap().is_empty(),
            "an owned child with a different birth must authorize neither joining nor group signalling");
        assert!(subc_os::child_exit::observe(pgid).unwrap().is_none());
    }

    #[tokio::test]
    async fn failed_container_admission_keeps_the_guard_for_unverifiable_cleanup() {
        let dir = cortexkit_test_support::ScratchDir::new("failed-container-admission");
        let path = dir.join("record.json");
        let roster = ChildRoster::default();
        roster.record_to(path.clone());
        std::fs::create_dir(path).unwrap();
        // No process owns this fixture: a missing creation identity and no
        // child handle must remain unverifiable without sending any signal.
        let container = Arc::new(CarriedContainment {
            module_id: "unverifiable-admission".into(),
            counters: crate::observability::DaemonCounters::default(),
            cleanup_block: Mutex::new(None),
            group_signals: Mutex::new(None),
            resource: Resource::Group {
                identity: LaunchIdentity::capture().unwrap(),
                pgid: u32::MAX,
                created_start_time: None,
                anchor_lost: std::sync::atomic::AtomicBool::new(false),
                pin: Mutex::new(None),
                pending_pin: std::sync::atomic::AtomicBool::new(false),
            },
            roster: Mutex::new(None),
            terminated: std::sync::atomic::AtomicBool::new(false),
            ending: std::sync::atomic::AtomicBool::new(false),
            admitting: std::sync::atomic::AtomicBool::new(false),
            admission_changed: tokio::sync::Notify::new(),
            cleanup_finished: tokio::sync::Notify::new(),
            hard_killed: std::sync::atomic::AtomicBool::new(false),
            kill_notify: tokio::sync::Notify::new(),
        });
        let failure = roster
            .admit(
                "unverifiable-admission".into(),
                u32::MAX,
                ModuleProtocol::None,
                None,
                crate::child_roster::RecordedIdentity {
                    container: Some(crate::live_children::ContainerIdentity {
                        pgid: Some(u32::MAX),
                        cgroup_path: None,
                        created_start_time: None,
                        ..crate::live_children::ContainerIdentity::test_metadata()
                    }),
                    ..Default::default()
                },
            )
            .unwrap_err();
        let _error = container.claim_failed_admission(failure);
        container.terminate().await;
        assert!(
            roster.previous_instance("unverifiable-admission").is_some(),
            "failed durable admission must retain its guard when cleanup cannot prove emptiness"
        );
    }

    #[tokio::test]
    async fn drop_retains_the_container_when_kill_acceptance_is_not_exit_proof() {
        let dir = cortexkit_test_support::ScratchDir::new("drop-container-retention");
        let record = dir.join("record.json");
        let roster = ChildRoster::default();
        roster.record_to(record.clone());
        let mut command = Command::new("/bin/sleep");
        command.arg("600").process_group(0).kill_on_drop(true);
        let leader = command.spawn().unwrap();
        let pgid = leader.id().unwrap();
        let facts = subc_os::process_group::inspect(pgid).unwrap().unwrap();
        let mut command = Command::new("/bin/sleep");
        command
            .arg("600")
            .process_group(pgid as i32)
            .kill_on_drop(true);
        let mut holder = command.spawn().unwrap();
        // Accept but intercept the group KILL so both owned children are still
        // live when the destructor must decide whether to retain ownership.
        let group_signals = Arc::new(Mutex::new(Vec::new()));
        let container = Arc::new(CarriedContainment {
            module_id: "drop-retention".into(),
            counters: crate::observability::DaemonCounters::default(),
            cleanup_block: Mutex::new(None),
            group_signals: Mutex::new(Some(Arc::clone(&group_signals))),
            resource: Resource::Group {
                identity: LaunchIdentity::capture().unwrap(),
                pgid,
                created_start_time: Some(facts.start_time),
                anchor_lost: std::sync::atomic::AtomicBool::new(false),
                pin: Mutex::new(Some(leader)),
                pending_pin: std::sync::atomic::AtomicBool::new(false),
            },
            roster: Mutex::new(None),
            terminated: std::sync::atomic::AtomicBool::new(false),
            ending: std::sync::atomic::AtomicBool::new(false),
            admitting: std::sync::atomic::AtomicBool::new(false),
            admission_changed: tokio::sync::Notify::new(),
            cleanup_finished: tokio::sync::Notify::new(),
            hard_killed: std::sync::atomic::AtomicBool::new(false),
            kill_notify: tokio::sync::Notify::new(),
        });
        let guard = roster
            .admit(
                "drop-retention".into(),
                pgid,
                ModuleProtocol::None,
                Some(facts.start_time),
                crate::child_roster::RecordedIdentity {
                    container: Some(crate::live_children::ContainerIdentity {
                        pgid: Some(pgid),
                        cgroup_path: None,
                        created_start_time: Some(facts.start_time),
                        ..crate::live_children::ContainerIdentity::test_metadata()
                    }),
                    carried: Some(Arc::downgrade(&container)),
                    ..Default::default()
                },
            )
            .unwrap();
        *container.roster.lock().unwrap() = Some(guard);
        drop(container);
        assert_eq!(
            *group_signals.lock().unwrap(),
            vec![rustix::process::Signal::KILL]
        );
        assert!(
            crate::live_children::read_record(&record)
                .unwrap()
                .iter()
                .any(|entry| entry.module_id == "drop-retention" && entry.container.is_some()),
            "accepting KILL must not erase the only record before members are proven gone"
        );
        // The verified per-PID cleanup still terminates the owned holder.
        timeout(Duration::from_secs(2), holder.wait())
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    #[allow(irrefutable_let_patterns)]
    async fn lost_first_anchor_uses_only_verified_member_signals() {
        let mut leader_command = Command::new("/bin/sleep");
        leader_command
            .arg("600")
            .process_group(0)
            .kill_on_drop(true);
        let leader = leader_command.spawn().unwrap();
        let pgid = leader.id().unwrap();
        let facts = subc_os::process_group::inspect(pgid).unwrap().unwrap();
        let mut holder_command = Command::new("/bin/sleep");
        holder_command
            .arg("600")
            .process_group(pgid as i32)
            .kill_on_drop(true);
        let mut holder = holder_command.spawn().unwrap();
        let mut sentinel_command = Command::new("/bin/sleep");
        sentinel_command
            .arg("600")
            .process_group(0)
            .kill_on_drop(true);
        let mut sentinel = sentinel_command.spawn().unwrap();
        let group_signals = Arc::new(Mutex::new(Vec::new()));
        let counters = crate::observability::DaemonCounters::default();
        let container = Arc::new(CarriedContainment {
            module_id: "lost-anchor-fixture".into(),
            counters: counters.clone(),
            cleanup_block: Mutex::new(None),
            group_signals: Mutex::new(Some(Arc::clone(&group_signals))),
            resource: Resource::Group {
                identity: LaunchIdentity::capture().unwrap(),
                pgid,
                created_start_time: Some(facts.start_time),
                anchor_lost: std::sync::atomic::AtomicBool::new(false),
                pin: Mutex::new(Some(leader)),
                pending_pin: std::sync::atomic::AtomicBool::new(false),
            },
            roster: Mutex::new(None),
            terminated: std::sync::atomic::AtomicBool::new(false),
            ending: std::sync::atomic::AtomicBool::new(false),
            admitting: std::sync::atomic::AtomicBool::new(false),
            admission_changed: tokio::sync::Notify::new(),
            cleanup_finished: tokio::sync::Notify::new(),
            hard_killed: std::sync::atomic::AtomicBool::new(false),
            kill_notify: tokio::sync::Notify::new(),
        });
        let mut observed = ProcessChild {
            process: None,
            container: Arc::downgrade(&container),
            slot: Weak::new(),
            pid: pgid,
            observed: None,
            settled: false,
        };
        // Deliberately reproduce another waiter reaping the first group leader.
        // Capturing group signals prevents even a broken finalizer from using
        // killpg after that reservation is lost; real signals target owned children.
        if let Resource::Group { pin, .. } = &container.resource {
            pin.lock().unwrap().as_mut().unwrap().start_kill().unwrap();
        }
        timeout(Duration::from_secs(2), async {
            loop {
                let reaped = if let Resource::Group { pin, .. } = &container.resource {
                    pin.lock()
                        .unwrap()
                        .as_mut()
                        .unwrap()
                        .try_wait()
                        .unwrap()
                        .is_some()
                } else {
                    unreachable!()
                };
                if reaped {
                    break;
                }
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            observed.try_wait().unwrap_err().raw_os_error(),
            Some(rustix::io::Errno::CHILD.raw_os_error())
        );
        container.hard_kill();
        container.stop_members().await;
        container.terminate().await;
        assert!(
            group_signals.lock().unwrap().is_empty(),
            "an already-reaped P1 must never authorize a group-wide signal"
        );
        assert_eq!(counters.snapshot()["carried_containment_anchor_lost"], 1);
        timeout(Duration::from_secs(2), holder.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(
            sentinel.try_wait().unwrap().is_none(),
            "a separate owned group is not part of the container"
        );
        sentinel.start_kill().unwrap();
        sentinel.wait().await.unwrap();
        observed.settled = true;
    }
}

#[cfg(all(test, target_os = "linux"))]
mod cgroup_policy_tests {
    use super::*;
    use cortexkit_test_support::ScratchDir;

    #[test]
    fn carried_cgroup_preflight_normalizes_noncanonical_and_relative_roots_before_fork() {
        let root = ScratchDir::new("carried-cgroup-path-preflight");
        let canonical = std::fs::canonicalize(&*root).unwrap();
        std::fs::write(canonical.join("cgroup.procs"), "").unwrap();
        let dotted = PathBuf::from(format!("{}/.", canonical.display()));
        let relative = std::env::current_dir().unwrap().join("target/scratch");
        std::fs::create_dir_all(&relative).unwrap();
        let child = relative.join(format!(
            "carried-cgroup-relative-{}-{}",
            std::process::id(),
            unix_ms_now()
        ));
        std::fs::create_dir(&child).unwrap();
        std::fs::write(child.join("cgroup.procs"), "").unwrap();
        let relative_name = child
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap();
        for root in [&dotted, relative_name] {
            let placement = subc_cgroup::prepare_at(root).unwrap().unwrap();
            let path = prepare_cgroup_path(&placement, "preflight").unwrap();
            assert!(path.is_absolute());
            assert_eq!(path, std::fs::canonicalize(&path).unwrap());
            assert_eq!(
                placement.existing_module_path(&path).unwrap(),
                Some(path.clone()),
                "a path accepted before fork must already satisfy recovery/adoption validation"
            );
            std::fs::remove_dir(path).unwrap();
        }
        std::fs::remove_dir(child.join("subc-modules")).unwrap();
        std::fs::remove_file(child.join("cgroup.procs")).unwrap();
        std::fs::remove_dir(child).unwrap();
    }

    #[tokio::test]
    async fn carried_cgroup_hard_kill_never_signals_a_process_group() {
        let dir = ScratchDir::new("carried-cgroup-hard-kill-policy");
        std::fs::write(dir.join("cgroup.procs"), "").unwrap();
        let placement = subc_cgroup::prepare_at(&dir).unwrap().unwrap();
        let path = placement.module_path("owned").unwrap();
        std::fs::write(path.join("cgroup.kill"), "0").unwrap();
        std::fs::write(path.join("cgroup.events"), "populated 1\n").unwrap();
        std::fs::write(path.join("cgroup.procs"), "").unwrap();
        let mut leader = Command::new("/bin/sleep");
        leader.arg("600").process_group(0).kill_on_drop(true);
        let mut leader = leader.spawn().unwrap();
        let signals = Arc::new(Mutex::new(Vec::new()));
        let container = Arc::new(CarriedContainment {
            module_id: "carried-cgroup-policy".into(),
            resource: Resource::Cgroup {
                identity: LaunchIdentity::capture().unwrap(),
                placement,
                name: "owned".into(),
                path: path.clone(),
            },
            counters: crate::observability::DaemonCounters::default(),
            cleanup_block: Mutex::new(None),
            group_signals: Mutex::new(Some(Arc::clone(&signals))),
            roster: Mutex::new(None),
            terminated: std::sync::atomic::AtomicBool::new(false),
            ending: std::sync::atomic::AtomicBool::new(false),
            admitting: std::sync::atomic::AtomicBool::new(false),
            admission_changed: tokio::sync::Notify::new(),
            cleanup_finished: tokio::sync::Notify::new(),
            hard_killed: std::sync::atomic::AtomicBool::new(false),
            kill_notify: tokio::sync::Notify::new(),
        });
        assert_eq!(container.group(), None, "a cgroup has no reserved PGID");
        assert!(!container.signal_pinned_group(rustix::process::Signal::KILL));
        container.hard_kill();
        assert_eq!(
            std::fs::read(path.join("cgroup.kill")).unwrap(),
            b"1",
            "cgroup cleanup must use the trusted kernel interface, never substitute killpg"
        );
        assert!(signals.lock().unwrap().is_empty());
        assert!(
            leader.try_wait().unwrap().is_none(),
            "a separate process group is not a cgroup member"
        );
        container.hard_kill();
        leader.start_kill().unwrap();
        leader.wait().await.unwrap();
    }
}
