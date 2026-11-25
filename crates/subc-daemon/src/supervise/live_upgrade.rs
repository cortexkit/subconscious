//! Supervisor operations for handing child ownership to a replacement image.
//!
//! Lock order: acquire `operation_lock` before inspecting pending adoptions;
//! never acquire `operation_lock` from inside the pending-adoptions lock.

#[cfg(unix)]
pub(crate) mod output {
    use std::{
        fs::File,
        io::{self, Read, Write},
        net::Shutdown,
        os::{fd::OwnedFd, unix::net::UnixStream},
        sync::{Arc, Condvar, Mutex},
    };

    use rustix::{
        event::{poll, PollFd, PollFlags},
        io::fcntl_dupfd_cloexec,
    };
    use tokio::sync::Notify;

    #[derive(Debug)]
    pub(crate) struct OpenPipe {
        pub(crate) fd: OwnedFd,
        pub(crate) tail: Vec<u8>,
    }

    pub(crate) type ParkResult = Result<Arc<OpenPipe>, String>;

    #[derive(Debug, Default)]
    struct State {
        stopped: bool,
        next_request: u64,
        request: Option<u64>,
        ended: [bool; 2],
        drained: [Option<ParkResult>; 2],
    }

    #[derive(Debug)]
    pub(crate) struct Control {
        signal: UnixStream,
        wake: UnixStream,
        state: Mutex<State>,
        resume: Condvar,
        changed: Notify,
    }

    impl Control {
        pub(crate) fn new() -> io::Result<Arc<Self>> {
            let (signal, wake) = UnixStream::pair()?;
            Ok(Arc::new(Self {
                signal,
                wake,
                state: Mutex::default(),
                resume: Condvar::new(),
                changed: Notify::new(),
            }))
        }

        pub(crate) fn stop(&self) {
            let mut state = self.state.lock().unwrap();
            state.stopped = true;
            let _ = self.signal.shutdown(Shutdown::Write);
            self.resume.notify_all();
            self.changed.notify_waiters();
        }

        pub(crate) fn park(self: &Arc<Self>) -> io::Result<Park> {
            let mut state = self.state.lock().unwrap();
            if state.stopped || state.request.is_some() {
                return Err(io::Error::other("capture already stopped or parked"));
            }
            (&self.signal).write_all(&[1])?;
            state.next_request += 1;
            let request = state.next_request;
            state.request = Some(request);
            Ok(Park {
                control: self.clone(),
                request,
            })
        }

        pub(crate) fn read(
            &self,
            file: &mut File,
            buf: &mut [u8],
            tail: &[u8],
            mut queue_barrier: impl FnMut(u64, ParkResult),
        ) -> io::Result<usize> {
            loop {
                let mut fds = [
                    PollFd::new(&*file, PollFlags::IN),
                    PollFd::new(&self.wake, PollFlags::IN),
                ];
                poll(&mut fds, None)?;
                if fds[1].revents().is_empty() {
                    // This thread is the only reader. Readiness means this read
                    // cannot block waiting on the child.
                    return file.read(buf);
                }
                let mut state = self.state.lock().unwrap();
                while !state.stopped {
                    let Some(request) = state.request else { break };
                    let parked = fcntl_dupfd_cloexec(&*file, 3)
                        .map(|fd| {
                            Arc::new(OpenPipe {
                                fd,
                                tail: tail.to_vec(),
                            })
                        })
                        .map_err(|error| error.to_string());
                    queue_barrier(request, parked);
                    state = self
                        .resume
                        .wait_while(state, |state| {
                            !state.stopped && state.request == Some(request)
                        })
                        .unwrap();
                }
                if state.stopped {
                    return Err(io::Error::other(
                        "late child output lost: capture pipe did not close",
                    ));
                }
            }
        }

        /// Called only after earlier lines from this stream reached the log.
        pub(crate) fn drained(&self, stream: usize, request: u64, result: ParkResult) {
            let mut state = self.state.lock().unwrap();
            if state.request == Some(request) {
                state.drained[stream] = Some(result);
                self.changed.notify_waiters();
            }
        }

        /// An absent stream, or an EOF whose final line reached the log.
        pub(crate) fn ended(&self, stream: usize) {
            let mut state = self.state.lock().unwrap();
            state.ended[stream] = true;
            self.changed.notify_waiters();
        }
    }

    /// Keeps both readers suspended at a read boundary. Dropping the lease
    /// resumes the original readers without replaying their buffered tails.
    #[derive(Debug)]
    pub(crate) struct Park {
        control: Arc<Control>,
        request: u64,
    }

    impl Park {
        pub(crate) async fn streams(&self) -> io::Result<[Option<Arc<OpenPipe>>; 2]> {
            loop {
                let changed = self.control.changed.notified();
                {
                    let state = self.control.state.lock().unwrap();
                    if state.stopped || state.request != Some(self.request) {
                        return Err(io::Error::other("capture stopped while parking"));
                    }
                    let stream = |index: usize| {
                        if state.ended[index] {
                            Ok(Some(None))
                        } else {
                            state.drained[index]
                                .as_ref()
                                .map(|result| result.clone().map(Some).map_err(io::Error::other))
                                .transpose()
                        }
                    };
                    if let (Some(stdout), Some(stderr)) = (stream(0)?, stream(1)?) {
                        return Ok([stdout, stderr]);
                    }
                }
                changed.await;
            }
        }
    }

    impl Drop for Park {
        fn drop(&mut self) {
            let mut state = self.control.state.lock().unwrap();
            if state.request != Some(self.request) {
                return;
            }
            if !state.stopped {
                // Only this lease consumes the single wake byte. Readers poll
                // it but never read it; both see the same request.
                let _ = (&self.control.wake).read_exact(&mut [0]);
            }
            state.request = None;
            state.drained = Default::default();
            self.control.resume.notify_all();
        }
    }
}

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};
use tokio::sync::{watch, OwnedMutexGuard};

use super::SupervisorHandle;
use subc_protocol::ErrorBody;

#[allow(clippy::too_many_arguments)]
pub(super) async fn paused_command(
    command: super::SupervisorCommand,
    _key: &Arc<()>,
    _spec: &super::ModuleSpec,
    _runtime: &super::SupervisorRuntimeConfig,
    _registry: &super::Registry,
    _liveness: &super::SupervisorProcessLiveness,
    _snapshot: &super::SharedSnapshot,
    _child: &mut Option<super::SupervisedChild>,
    deferred: &mut std::collections::VecDeque<super::SupervisorCommand>,
) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if let super::SupervisorCommand::Upgrade { key, request } = command {
        if Arc::ptr_eq(&key, _key) {
            unix::actor_request(
                request, _spec, _runtime, _registry, _liveness, _snapshot, _child,
            )
            .await;
        } else {
            request.refuse_inactive();
        }
        return;
    }
    refuse_and_defer(command, deferred);
}

pub(super) fn refuse_and_defer(
    command: super::SupervisorCommand,
    deferred: &mut std::collections::VecDeque<super::SupervisorCommand>,
) {
    use super::{SuperviseError::UpgradeInProgress, SupervisorCommand::*};
    match command {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        Upgrade { request, .. } => request.refuse_inactive(),
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        command @ HeldAdoptionResolved { .. } => deferred.push_back(command),
        command @ (ConfigPolicyProbe { .. }
        | ConfigApplyRestart { .. }
        | ConfigDeliveryFailed { .. }) => deferred.push_back(command),
        ApprovedConfiguration { reply, .. }
        | ConfigExitHold { reply }
        | Drain { reply, .. }
        | Retire { reply, .. }
        | Restart { reply, .. }
        | Reload { reply, .. }
        | UpdateConfiguration { reply, .. } => {
            let _ = reply.send(Err(UpgradeInProgress));
        }
        SetEnabled { reply, .. } => {
            let _ = reply.send(Err(UpgradeInProgress));
        }
        Swap { reply, .. } => {
            let _ = reply.send(Err(UpgradeInProgress));
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(super) fn defer_exit_publication_command(
    command: super::SupervisorCommand,
    deferred: &mut std::collections::VecDeque<super::SupervisorCommand>,
) -> Option<super::SupervisorCommand> {
    if matches!(
        command,
        super::SupervisorCommand::Upgrade { .. }
            | super::SupervisorCommand::HeldAdoptionResolved { .. }
    ) {
        deferred.push_back(command);
        None
    } else {
        Some(command)
    }
}

#[derive(Debug, Clone)]
pub(super) struct UpgradeState(Arc<UpgradeStateInner>);

#[derive(Debug)]
struct UpgradeStateInner {
    attempt: watch::Sender<Option<Arc<()>>>,
    background: Mutex<usize>,
    dispositions: Mutex<Option<UpgradeReport>>,
    #[cfg(test)]
    disposition_changed: tokio::sync::Notify,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    replacements: Mutex<BTreeMap<String, ReplacementTicket>>,
}

impl Default for UpgradeState {
    fn default() -> Self {
        Self(Arc::new(UpgradeStateInner {
            attempt: watch::channel(None).0,
            background: Mutex::default(),
            dispositions: Mutex::new(None),
            #[cfg(test)]
            disposition_changed: tokio::sync::Notify::new(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            replacements: Mutex::default(),
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Pending,
    Replacing,
    Resumed,
    Restarted,
    Stopped,
    Blocked,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Default)]
pub(super) struct ReplacementTicket(Arc<Mutex<Option<ReplacementLaunch>>>);

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug)]
struct ReplacementLaunch {
    spawn: std::sync::Weak<super::RegistrationSpawn>,
    generation: u64,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl ReplacementTicket {
    pub(super) fn bind(&self, launch: &Arc<super::RegistrationSpawn>) {
        let mut bound = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if bound.is_none() {
            *bound = Some(ReplacementLaunch {
                spawn: Arc::downgrade(launch),
                generation: launch.authority.spawn_number(),
            });
        }
    }

    fn matches(&self, launch: &Arc<super::RegistrationSpawn>) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|expected| {
                expected.spawn.ptr_eq(&Arc::downgrade(launch))
                    && expected.generation == launch.authority.spawn_number()
            })
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Replacing {
    state: UpgradeState,
    module: String,
    ticket: ReplacementTicket,
    outcome: Disposition,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for Replacing {
    fn drop(&mut self) {
        self.state.update(&self.module, self.outcome);
        self.state
            .0
            .replacements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.module);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Replacing {
    fn restarted(&mut self) {
        self.outcome = Disposition::Restarted;
    }
}

#[derive(Debug)]
struct UpgradeReport {
    attempt_id: String,
    started_at_ms: u64,
    modules: BTreeMap<String, Disposition>,
}

impl UpgradeState {
    pub(super) fn background_work(&self) -> Option<BackgroundWork> {
        let mut count = self.0.background.lock().unwrap_or_else(|p| p.into_inner());
        if self.is_active() {
            return None;
        }
        *count += 1;
        Some(BackgroundWork(self.clone()))
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn begin_replacement(&self, module: &str) -> Replacing {
        let ticket = ReplacementTicket::default();
        self.0
            .replacements
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(module.to_owned(), ticket.clone());
        self.update(module, Disposition::Replacing);
        Replacing {
            state: self.clone(),
            module: module.to_owned(),
            ticket,
            // A spawn error is a stopped replacement, not evidence that a new
            // process started. Only a successful launch changes this outcome.
            outcome: Disposition::Stopped,
        }
    }
    pub(super) fn subscribe(&self) -> watch::Receiver<Option<Arc<()>>> {
        self.0.attempt.subscribe()
    }

    fn is_active(&self) -> bool {
        self.0.attempt.borrow().is_some()
    }

    fn disposition(&self, module: &str) -> Option<Disposition> {
        self.0
            .dispositions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()?
            .modules
            .get(module)
            .copied()
    }

    #[allow(dead_code)] // The resume bootstrap owns the report's initial cohort.
    pub(super) fn update(&self, module: &str, disposition: Disposition) {
        if let Some(report) = self
            .0
            .dispositions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_mut()
        {
            report.modules.insert(module.to_owned(), disposition);
        }
        #[cfg(test)]
        self.0.disposition_changed.notify_waiters();
    }
}

pub(super) struct BackgroundWork(UpgradeState);

impl Drop for BackgroundWork {
    fn drop(&mut self) {
        *self
            .0
             .0
            .background
            .lock()
            .unwrap_or_else(|p| p.into_inner()) -= 1;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LifecycleRefusal {
    UpgradeBusy,
    AwaitingAdoption,
}

impl LifecycleRefusal {
    pub(crate) fn body(self) -> ErrorBody {
        let (code, reason, message) = match self {
            Self::UpgradeBusy => ("upgrade_busy", "upgrade", "daemon upgrade is in progress"),
            Self::AwaitingAdoption => (
                "module_reloading",
                "daemon_upgrade",
                "module is awaiting daemon upgrade recovery",
            ),
        };
        ErrorBody {
            code: code.into(),
            message: message.into(),
            detail: Some(serde_json::json!({"reason": reason})),
        }
    }
}

impl SupervisorHandle {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn upgrade_active(&self) -> bool {
        self.upgrade_state.is_active()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn restart_intent_active(&self) -> bool {
        self.restart_intent
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .expires
            .is_some_and(|expiry| expiry > subc_uptime::suspend_inclusive_now())
    }
    pub(crate) fn adoption_blocks(&self, module: &str) -> bool {
        self.pending_adoptions.contains(module)
            || matches!(
                self.upgrade_state.disposition(module),
                Some(Disposition::Blocked | Disposition::Pending | Disposition::Replacing)
            )
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn adoption_hello_allowed(
        &self,
        module: &str,
        channel: Option<&Arc<super::RegistrationSpawn>>,
        nonce: Option<&str>,
    ) -> bool {
        if !self.adoption_blocks(module) {
            return true;
        }
        if self.upgrade_state.disposition(module) == Some(Disposition::Replacing) {
            let ticket = self
                .upgrade_state
                .0
                .replacements
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(module)
                .cloned();
            let launch = channel
                .cloned()
                .or_else(|| nonce.and_then(|nonce| self.nonce_hello_spawn(module, nonce)));
            return ticket
                .zip(launch)
                .is_some_and(|(ticket, launch)| ticket.matches(&launch));
        }
        false
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn adoption_hello_allowed(
        &self,
        module: &str,
        _: Option<&Arc<super::RegistrationSpawn>>,
        _: Option<&str>,
    ) -> bool {
        !self.adoption_blocks(module)
    }

    pub(crate) fn lifecycle_refusal(&self, module: Option<&str>) -> Option<LifecycleRefusal> {
        if self.upgrade_state.is_active() {
            return Some(LifecycleRefusal::UpgradeBusy);
        }
        let adopting = match module {
            Some(module) => self.adoption_blocks(module),
            None => !self.pending_adoptions.is_empty() || self.unresolved_dispositions(),
        };
        adopting.then_some(LifecycleRefusal::AwaitingAdoption)
    }

    fn unresolved_dispositions(&self) -> bool {
        self.upgrade_state
            .0
            .dispositions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .is_some_and(|report| {
                report.modules.values().any(|disposition| {
                    matches!(
                        disposition,
                        Disposition::Blocked | Disposition::Pending | Disposition::Replacing
                    )
                })
            })
    }

    /// Preserve normal operation serialization, but do not queue behind an
    /// upgrade or admit an operation that would touch a pending/held child.
    pub(crate) async fn lifecycle_operation(
        &self,
        module: Option<&str>,
    ) -> Result<OwnedMutexGuard<()>, LifecycleRefusal> {
        let mut attempts = self.upgrade_state.subscribe();
        let lock = self.operation_lock().lock_owned();
        tokio::pin!(lock);
        loop {
            if let Some(refusal) = self.lifecycle_refusal(module) {
                return Err(refusal);
            }
            tokio::select! {
                biased;
                _ = attempts.changed() => {},
                guard = &mut lock => return match self.lifecycle_refusal(module) {
                    Some(refusal) => Err(refusal),
                    None => Ok(guard),
                },
            }
        }
    }

    /// The resume bootstrap initializes the whole cohort before accepting traffic.
    #[allow(dead_code)]
    pub(crate) fn begin_resume_report(
        &self,
        attempt_id: String,
        started_at_ms: u64,
        modules: impl IntoIterator<Item = String>,
    ) {
        *self
            .upgrade_state
            .0
            .dispositions
            .lock()
            .unwrap_or_else(|p| p.into_inner()) = Some(UpgradeReport {
            attempt_id,
            started_at_ms,
            modules: modules
                .into_iter()
                .map(|module| (module, Disposition::Pending))
                .collect(),
        });
    }

    /// The describe operation reads this report from the replacement image.
    #[allow(dead_code)]
    pub(crate) fn resume_report(&self) -> Option<serde_json::Value> {
        let report = self
            .upgrade_state
            .0
            .dispositions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let report = report.as_ref()?;
        let pending = report
            .modules
            .values()
            .any(|d| matches!(d, Disposition::Pending | Disposition::Replacing));
        let modules: Vec<_> = report
            .modules
            .iter()
            .map(|(id, disposition)| {
                let disposition = if *disposition == Disposition::Replacing {
                    Disposition::Pending
                } else {
                    *disposition
                };
                serde_json::json!({"id": id, "disposition": disposition})
            })
            .collect();
        Some(serde_json::json!({
            "attempt_id": report.attempt_id,
            "started_at_ms": report.started_at_ms,
            "outcome": if pending { "pending" } else { "upgraded" },
            "modules": modules,
        }))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl super::Supervisor {
    pub(crate) fn retained_children(&self) -> crate::child_roster::RetainedView {
        self.child_roster.retained_view()
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    // The upgrade control operation and the resume bootstrap call this module.
    #![allow(dead_code)]
    use super::{output, Disposition};
    use crate::{
        child_output, live_children,
        supervise::{spawn::RegisteredConfiguredModule, *},
    };
    use handover::{
        ChildWitness, ContainmentWitness, FileIdentity, HexBytes, ProgramKey,
        RegistrationSpawnKind, StreamWitness,
    };
    use rustix::process::{Pid, Signal, WaitOptions};
    #[cfg(any(target_os = "linux", test))]
    use std::path::Path;
    use std::{
        collections::BTreeMap,
        io,
        os::fd::{AsRawFd, OwnedFd},
        task::{Context, Poll, Waker},
    };
    use tokio::{
        io::{unix::AsyncFd, Interest},
        sync::OwnedMutexGuard,
        task::JoinSet,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum UpgradeBusy {
        ModuleLifecycle,
        Upgrade,
    }

    /// The old-image upgrade operation owns this token from admission through
    /// exec or unwind. Queued actor commands carry its identity, not its guard.
    pub(crate) struct UpgradeAttempt {
        supervisor: SupervisorHandle,
        key: Arc<()>,
        _guard: OwnedMutexGuard<()>,
    }

    impl Drop for UpgradeAttempt {
        fn drop(&mut self) {
            self.supervisor.upgrade_state.0.attempt.send_replace(None);
        }
    }

    impl SupervisorHandle {
        pub(crate) fn begin_upgrade(&self) -> Result<UpgradeAttempt, UpgradeBusy> {
            let guard = self.operation_lock().try_lock_owned().map_err(|_| {
                if self.upgrade_state.is_active() {
                    UpgradeBusy::Upgrade
                } else {
                    UpgradeBusy::ModuleLifecycle
                }
            })?;
            if !self.pending_adoptions.is_empty() || self.unresolved_dispositions() {
                return Err(UpgradeBusy::Upgrade);
            }
            let background = self
                .upgrade_state
                .0
                .background
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            if *background != 0 {
                return Err(UpgradeBusy::ModuleLifecycle);
            }
            if !self
                .swaps
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty()
            {
                return Err(UpgradeBusy::ModuleLifecycle);
            }
            let key = Arc::new(());
            self.upgrade_state.0.attempt.send_replace(Some(key.clone()));
            drop(background);
            Ok(UpgradeAttempt {
                supervisor: self.clone(),
                key,
                _guard: guard,
            })
        }
    }

    #[derive(Debug)]
    pub(in crate::supervise) enum ActorRequest {
        Select(oneshot::Sender<io::Result<Option<Selection>>>),
        Stop(oneshot::Sender<io::Result<()>>),
        Park(oneshot::Sender<io::Result<output::Park>>),
        Restart(oneshot::Sender<io::Result<bool>>),
        Exited(oneshot::Sender<io::Result<bool>>),
        RestartAnnounced {
            deadline: Instant,
            reply: oneshot::Sender<io::Result<AnnouncedRestart>>,
        },
    }

    #[derive(Debug)]
    pub(crate) struct AnnouncedRestart {
        pub(crate) release_failed: Option<io::Error>,
        pub(crate) replacement: io::Result<()>,
    }

    impl ActorRequest {
        pub(in crate::supervise) fn refuse_running_swap(self, module: &str) {
            self.refuse(format!("upgrade busy: module '{module}' is swapping"));
        }

        pub(in crate::supervise) fn refuse_inactive(self) {
            self.refuse("upgrade attempt no longer owns the supervisor".into());
        }

        fn refuse(self, detail: String) {
            let busy = || io::Error::new(io::ErrorKind::WouldBlock, detail);
            match self {
                Self::Select(reply) => {
                    let _ = reply.send(Err(busy()));
                }
                Self::Stop(reply) => {
                    let _ = reply.send(Err(busy()));
                }
                Self::RestartAnnounced { reply, .. } => {
                    let _ = reply.send(Err(busy()));
                }
                Self::Restart(reply) => {
                    let _ = reply.send(Err(busy()));
                }
                Self::Park(reply) => {
                    let _ = reply.send(Err(busy()));
                }
                Self::Exited(reply) => {
                    let _ = reply.send(Err(busy()));
                }
            }
        }
    }

    #[derive(Debug)]
    pub(in crate::supervise) enum Selection {
        Carried(Box<ChildWitness>),
        Restarting,
        Stopped,
    }

    #[derive(Default)]
    pub(crate) struct SelectedCohort {
        pub(crate) carried: BTreeMap<String, ChildWitness>,
        pub(crate) restarting: Vec<String>,
        pub(crate) stopped: Vec<String>,
        pub(crate) config_enabled: BTreeMap<String, bool>,
    }

    pub(crate) struct ParkedChild {
        pub(crate) witness: ChildWitness,
        // Keep descriptors and the suspension lease alive until exec, or drop
        // them to resume the original readers during unwind.
        _streams: [Option<Arc<output::OpenPipe>>; 2],
        _park: output::Park,
    }

    impl UpgradeAttempt {
        fn fan_out<T: Send + 'static>(
            &self,
            ids: impl IntoIterator<Item = String>,
            request: impl Fn(oneshot::Sender<T>) -> ActorRequest,
        ) -> JoinSet<(String, io::Result<T>)> {
            let mut tasks = JoinSet::new();
            for id in ids {
                let module = self.supervisor.get(&id);
                let key = self.key.clone();
                let (reply, received) = oneshot::channel();
                let request = request(reply);
                tasks.spawn(async move {
                    let result = async {
                        let Some(module) = module else {
                            return Err(io::Error::other("module left supervision"));
                        };
                        module
                            .inner
                            .commands
                            .send(SupervisorCommand::Upgrade { key, request })
                            .await
                            .map_err(|_| io::Error::other("module supervisor is closed"))?;
                        received.await.map_err(|_| {
                            io::Error::other("module supervisor did not answer upgrade")
                        })
                    }
                    .await;
                    (id, result)
                });
            }
            tasks
        }

        /// The upgrade operation snapshots the actors before any notices or stops.
        pub(crate) async fn select(&self) -> io::Result<SelectedCohort> {
            let modules = self
                .supervisor
                .modules
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut selected = SelectedCohort::default();
            let mut ids = Vec::new();
            for module in modules {
                let id = module.module_id().to_owned();
                let stopped = module.inner.commands.is_closed()
                    && lock_snapshot(&module.inner.snapshot).is_ok_and(|state| {
                        !state.process_alive
                            && !state.respawn_pending
                            && matches!(
                                state.state,
                                ModuleState::Stopped | ModuleState::Disabled | ModuleState::Failed
                            )
                    });
                if stopped {
                    selected.config_enabled.insert(
                        id.clone(),
                        module.configured_enabled().map_err(io::Error::other)?,
                    );
                    selected.stopped.push(id);
                } else {
                    ids.push(id);
                }
            }
            let mut tasks = self.fan_out(ids, ActorRequest::Select);
            while let Some(result) = tasks.join_next().await {
                let (id, selection) = result?;
                let selection = selection??;
                if selection.is_some() {
                    let module = self
                        .supervisor
                        .get(&id)
                        .ok_or_else(|| io::Error::other("selected module left supervision"))?;
                    selected.config_enabled.insert(
                        id.clone(),
                        module.configured_enabled().map_err(io::Error::other)?,
                    );
                }
                match selection {
                    Some(Selection::Carried(witness)) => {
                        selected.carried.insert(id, *witness);
                    }
                    Some(Selection::Restarting) => selected.restarting.push(id),
                    Some(Selection::Stopped) => selected.stopped.push(id),
                    None => {}
                }
            }
            selected.restarting.sort();
            selected.stopped.sort();
            Ok(selected)
        }

        async fn operate(
            &self,
            ids: Vec<String>,
            request: impl Fn(oneshot::Sender<io::Result<()>>) -> ActorRequest,
        ) -> BTreeMap<String, io::Result<()>> {
            self.fan_out(ids, request)
                .join_all()
                .await
                .into_iter()
                .map(|(id, result)| (id, result.and_then(|result| result)))
                .collect()
        }

        /// The old image stops the whole restarting cohort concurrently.
        pub(crate) async fn stop_restarting(
            &self,
            ids: Vec<String>,
        ) -> BTreeMap<String, io::Result<()>> {
            self.operate(ids, ActorRequest::Stop).await
        }

        /// The old image restarts stopped modules on unwind without counting
        /// these starts toward their crash-restart limit.
        pub(crate) async fn restart_stopped(
            &self,
            ids: Vec<String>,
        ) -> BTreeMap<String, io::Result<bool>> {
            self.fan_out(ids, ActorRequest::Restart)
                .join_all()
                .await
                .into_iter()
                .map(|(id, result)| (id, result.and_then(|result| result)))
                .collect()
        }

        /// During unwind, wait for announced modules to exit, using their
        /// original deadline if any do not react to the closed connection.
        pub(crate) async fn restart_announced(
            &self,
            ids: Vec<String>,
            deadline: Instant,
        ) -> BTreeMap<String, io::Result<AnnouncedRestart>> {
            self.fan_out(ids, |reply| ActorRequest::RestartAnnounced {
                deadline,
                reply,
            })
            .join_all()
            .await
            .into_iter()
            .map(|(id, result)| (id, result.and_then(|result| result)))
            .collect()
        }

        /// Immediately before writing the handover, reap children which exited
        /// after selection. The writer adds these ids to `restarting`.
        pub(crate) async fn remove_exited(
            &self,
            parked: &mut Vec<ParkedChild>,
        ) -> io::Result<Vec<String>> {
            let mut requests = self.fan_out(
                parked
                    .iter()
                    .map(|child| child.witness.module_id.clone())
                    .collect::<Vec<_>>(),
                ActorRequest::Exited,
            );
            let mut exited = Vec::new();
            while let Some(result) = requests.join_next().await {
                let (id, result) = result?;
                if result?? {
                    exited.push(id);
                }
            }
            exited.sort();
            parked.retain(|child| exited.binary_search(&child.witness.module_id).is_err());
            for result in self.stop_restarting(exited.clone()).await.into_values() {
                result?;
            }
            Ok(exited)
        }

        /// The old image gets a single two-second budget for every reader and writer barrier.
        /// Dropping the returned leases, including on error, unconditionally resumes capture.
        pub(crate) async fn park(
            &self,
            mut carried: BTreeMap<String, ChildWitness>,
        ) -> io::Result<Vec<ParkedChild>> {
            tokio::time::timeout(Duration::from_secs(2), async {
                let mut requests = self.fan_out(
                    carried.keys().cloned().collect::<Vec<_>>(),
                    ActorRequest::Park,
                );
                let mut drains = JoinSet::<io::Result<ParkedChild>>::new();
                while let Some(result) = requests.join_next().await {
                    let (id, park) = result?;
                    let park = park??;
                    let mut witness = carried.remove(&id).expect("one park request per actor");
                    drains.spawn(async move {
                        let streams = park.streams().await?;
                        let stream = |index: usize| {
                            streams[index]
                                .as_ref()
                                .map_or(StreamWitness::Ended {}, |open| StreamWitness::Open {
                                    fd: open.fd.as_raw_fd(),
                                    tail: open.tail.clone(),
                                })
                        };
                        witness.stdout = stream(0);
                        witness.stderr = stream(1);
                        Ok(ParkedChild {
                            witness,
                            _streams: streams,
                            _park: park,
                        })
                    });
                }
                let mut parked = Vec::new();
                while let Some(result) = drains.join_next().await {
                    parked.push(result??);
                }
                parked.sort_by(|a, b| a.witness.module_id.cmp(&b.witness.module_id));
                Ok(parked)
            })
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "child output did not park within two seconds",
                )
            })?
        }
    }

    fn select_child(child: &SupervisedChild, registry: &Registry) -> Option<ChildWitness> {
        let registration = child.registration_spawn.as_ref()?;
        // The nonce path closes its unused registration channel. Its durable
        // token and current registry slot still identify a registered primary.
        if child.capture_stop.is_none()
            || registry
                .get_module(&registration.module_id)
                .ok()
                .flatten()
                .is_none()
        {
            return None;
        }
        let witnesses = registration.resume_witnesses.lock().ok()?;
        let witnesses = witnesses.as_ref().filter(|witnesses| witnesses.opted_in)?;
        let start_time = child.process_start_time?;
        let pid = child.pid;
        #[cfg(target_os = "linux")]
        let _pidfd = open_watch(pid).ok()?;
        #[cfg(target_os = "linux")]
        let cgroup = match &child.subtree_target {
            crate::subtree_resources::Target::Cgroup(path) => {
                if let Some(verified) = child.child.verified_adoptee() {
                    match &verified.containment {
                        Containment::Cgroup(target) => target.witness(pid),
                        _ => None,
                    }
                } else {
                    CgroupTarget::capture(path)
                        .ok()
                        .and_then(|target| target.witness(pid))
                }
            }
            _ => None,
        };
        #[cfg(target_os = "macos")]
        let cgroup = None;
        let containment = cgroup.or_else(|| {
            child
                .child
                .plain_group
                .as_ref()
                .filter(|_| subc_os::process_group::pgid(pid).ok().flatten() == Some(pid))
                .map(|_| ContainmentWitness::ProcessGroup {
                    leader_pid: pid,
                    leader_start_time: start_time,
                })
        })?;
        Some(ChildWitness {
            module_id: registration.module_id.clone(),
            pid,
            start_time,
            spawn_generation: registration.authority.spawn_number(),
            containment,
            resume_token_sha256: HexBytes(witnesses.resume_token_sha256),
            launch_nonce_sha256: witnesses.launch_nonce_sha256.map(HexBytes),
            launch_spec_digest: HexBytes(child.launch_spec_digest.0),
            spawned_file_identity: child.spawned_file_identity.as_ref().map(|identity| {
                FileIdentity {
                    device: identity.device,
                    inode: identity.inode,
                }
            }),
            program_key: ProgramKey {
                incarnation: registration.binding_key.incarnation.clone(),
                channel_launch_seq: registration.binding_key.channel_launch_seq,
            },
            admission_base_digest: child
                .admission_base
                .as_ref()
                .and_then(|digest| HexBytes::from_hex(digest.as_str().as_bytes())),
            store_gated: child.live_program_binding.is_some(),
            // Selection runs only on the actor's current child, after any
            // promoted candidate has replaced the incumbent and the swap ended.
            registration_spawn_kind: RegistrationSpawnKind::Plain,
            provider_declarations: witnesses.provider_declarations.clone(),
            spawned_at_ms: child.spawned_at_ms,
            stdout: StreamWitness::Ended {},
            stderr: StreamWitness::Ended {},
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn actor_request(
        request: ActorRequest,
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        registry: &Registry,
        liveness: &SupervisorProcessLiveness,
        snapshot: &SharedSnapshot,
        child: &mut Option<SupervisedChild>,
    ) {
        match request {
            ActorRequest::Select(reply) => {
                let selected = child
                    .as_mut()
                    .map(|child| match child.child.try_wait() {
                        Ok(None) => select_child(child, registry)
                            .map_or(Selection::Restarting, |witness| {
                                Selection::Carried(Box::new(witness))
                            }),
                        _ => Selection::Restarting,
                    })
                    .or_else(|| {
                        lock_snapshot(snapshot).ok().map(|state| {
                            if state.enabled && state.respawn_pending {
                                Selection::Restarting
                            } else {
                                Selection::Stopped
                            }
                        })
                    });
                let _ = reply.send(Ok(selected));
            }
            ActorRequest::Park(reply) => {
                let result = child
                    .as_ref()
                    .and_then(|child| child.capture_stop.as_ref())
                    .ok_or_else(|| io::Error::other("carried child has no capture control"))
                    .and_then(child_output::StopReaders::park);
                let _ = reply.send(result);
            }
            ActorRequest::Exited(reply) => {
                let result = child.as_mut().map_or(Ok(true), |child| {
                    child.child.try_wait().map(|status| status.is_some())
                });
                let _ = reply.send(result);
            }
            ActorRequest::Stop(reply) => {
                let result = async {
                    let notice = begin_forwarding_drain_if_configured(
                        spec,
                        runtime,
                        registry,
                        snapshot,
                        None,
                        RouteCloseReason::Disable,
                    )
                    .await?;
                    drain::drain_for_upgrade(spec, runtime, registry, snapshot, child, notice).await
                }
                .await;
                if child.is_none() {
                    containment::terminate(&runtime.child_roster.containment).await;
                    liveness.untrack_if_current(&spec.module_id, snapshot);
                }
                let _ = reply.send(result.map_err(io::Error::other));
            }
            ActorRequest::Restart(reply) => {
                let result = if child.is_none()
                    && lock_snapshot(snapshot).is_ok_and(|state| state.enabled)
                {
                    liveness.track(spec.module_id.clone(), snapshot.clone());
                    spawn_and_mark_running_async(spec, runtime, snapshot)
                        .await
                        .map(|next| {
                            *child = Some(next);
                            true
                        })
                        .map_err(|error| {
                            liveness.untrack_if_current(&spec.module_id, snapshot);
                            io::Error::other(error)
                        })
                } else {
                    Ok(false)
                };
                let _ = reply.send(result);
            }
            ActorRequest::RestartAnnounced { deadline, reply } => {
                let result: io::Result<AnnouncedRestart> = async {
                    if let Some(active) = child.as_mut() {
                        match tokio::time::timeout_at(deadline, active.wait()).await {
                            Ok(status) => {
                                status?;
                            }
                            Err(_) => {
                                let recorded = active
                                    .roster_guard
                                    .as_ref()
                                    .and_then(|guard| guard.recorded_identity())
                                    .ok_or_else(|| {
                                        io::Error::other("announced child has no recorded identity")
                                    })?;
                                if let Some(verified) = active.child.verified_adoptee() {
                                    verified.signal(Signal::KILL)?;
                                } else {
                                    let containment = if active.child.plain_group.is_some() {
                                        Containment::Group
                                    } else { Containment::LeaderOnly };
                                    VerifiedChild { recorded: recorded.child, containment, reaped: Mutex::default(), #[cfg(test)] signal_attempts: Default::default(), #[cfg(test)] kill_identity_sample: Mutex::default(), #[cfg(test)] signal_refusal: Mutex::default() }.signal(Signal::KILL)?;
                                }
                                 timeout(drain::UPGRADE_KILL_GRACE, active.wait()).await
                                     .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "announced child did not exit within the kill grace; child remains owned"))??;
                            }
                        }
                    }
                    let release_failed = drain::drain_for_upgrade(spec, runtime, registry, snapshot, child, StopNotice::SentOverConnection)
                        .await.err().map(io::Error::other);
                    if child.is_some() {
                        return Err(release_failed.unwrap_or_else(|| io::Error::other("announced child is still owned")));
                    }
                    let replacement = spawn_and_mark_running_async(spec, runtime, snapshot)
                        .await.map(|next| *child = Some(next)).map_err(io::Error::other);
                    Ok(AnnouncedRestart { release_failed, replacement })
                }
                .await;
                if result
                    .as_ref()
                    .is_err_and(|error| error.kind() != io::ErrorKind::TimedOut)
                    && child
                        .as_ref()
                        .is_some_and(|child| child.child.verified_adoptee().is_some())
                {
                    *child = None;
                }
                let _ = reply.send(result);
            }
        }
    }

    async fn spawn_replacement(
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        snapshot: &SharedSnapshot,
    ) -> Result<SupervisedChild, SuperviseError> {
        let mut replacing = runtime
            .supervisor_handle
            .as_ref()
            .map(|handle| handle.upgrade_state.begin_replacement(&spec.module_id));
        let mut runtime = runtime.clone();
        runtime.replacement_ticket = replacing.as_ref().map(|replacing| replacing.ticket.clone());
        let result = spawn_and_mark_running_async(spec, &runtime, snapshot).await;
        if result.is_ok() {
            if let Some(replacing) = &mut replacing {
                replacing.restarted();
            }
        }
        result
    }

    pub(in crate::supervise) async fn held_resolved(
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        snapshot: &SharedSnapshot,
        liveness: &SupervisorProcessLiveness,
        child: &mut Option<SupervisedChild>,
        restart: bool,
    ) {
        let Some(handle) = &runtime.supervisor_handle else {
            return;
        };
        let id = &spec.module_id;
        if handle.upgrade_state.disposition(id) != Some(Disposition::Blocked) || child.is_some() {
            return;
        }
        let enabled = lock_snapshot(snapshot).is_ok_and(|state| state.enabled);
        let failure = runtime.failure_context(true);
        if restart
            && enabled
            && !runtime.child_roster.is_closed()
            && release_dead_registration(
                &runtime.registry,
                runtime.forwarding.as_deref(),
                snapshot,
                id,
                &failure,
            )
            .await
            .is_ok()
        {
            liveness.track(id.clone(), snapshot.clone());
            match spawn_replacement(spec, runtime, snapshot).await {
                Ok(replacement) => *child = Some(replacement),
                Err(error) => {
                    warn!(module_id = %id, %error, "planned restart after Held child failed");
                    fail_snapshot(snapshot, Some(id), None, &failure);
                    liveness.untrack_if_current(id, snapshot);
                }
            }
        } else {
            let _ = update_snapshot(snapshot, Some(id), |state| {
                state.state = if state.enabled {
                    ModuleState::Stopped
                } else {
                    ModuleState::Disabled
                };
                clear_current_process_facts(state);
            });
            handle.upgrade_state.update(id, Disposition::Stopped);
        }
    }

    fn open_watch(pid: u32) -> io::Result<OwnedFd> {
        #[cfg(target_os = "linux")]
        {
            use rustix::process::{pidfd_open, Pid, PidfdFlags};
            Ok(pidfd_open(
                Pid::from_raw(i32::try_from(pid).map_err(io::Error::other)?)
                    .ok_or_else(|| io::Error::other("invalid child pid"))?,
                PidfdFlags::empty(),
            )?)
        }
        #[cfg(target_os = "macos")]
        subc_os::child_exit::watch_owned_child(pid)
    }

    fn async_watch(pid: u32) -> io::Result<Arc<AsyncFd<OwnedFd>>> {
        // A macOS kernel event queue can itself be watched for reads, not writes.
        AsyncFd::with_interest(open_watch(pid)?, Interest::READABLE).map(Arc::new)
    }

    #[derive(Debug)]
    enum Containment {
        Group,
        #[cfg(target_os = "linux")]
        Cgroup(CgroupTarget),
        LeaderOnly,
    }

    #[cfg(target_os = "linux")]
    #[derive(Debug)]
    struct CgroupTarget {
        path: PathBuf,
        identity: FileIdentity,
        directory: Option<std::fs::File>,
    }

    #[cfg(target_os = "linux")]
    impl CgroupTarget {
        fn open(path: &Path) -> io::Result<std::fs::File> {
            use rustix::fs::{open, Mode, OFlags};
            Ok(open(
                path,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?
            .into())
        }

        fn identity(directory: &std::fs::File) -> io::Result<FileIdentity> {
            use std::os::unix::fs::MetadataExt;
            let stat = directory.metadata()?;
            Ok(FileIdentity {
                device: stat.dev(),
                inode: stat.ino(),
            })
        }

        fn capture(path: &Path) -> io::Result<Self> {
            let directory = Self::open(path)?;
            let identity = Self::identity(&directory)?;
            Ok(Self {
                path: path.to_owned(),
                identity,
                directory: Some(directory),
            })
        }

        fn from_witness(path: PathBuf, identity: FileIdentity) -> Self {
            let directory = Self::open(&path)
                .ok()
                .filter(|dir| Self::identity(dir).ok() == Some(identity.clone()));
            Self {
                path,
                identity,
                directory,
            }
        }

        fn witness(&self, pid: u32) -> Option<ContainmentWitness> {
            let directory = Self::open(&self.path).ok()?;
            if Self::identity(&directory).ok()? != self.identity || !cgroup_matches(pid, &self.path)
            {
                return None;
            }
            Some(ContainmentWitness::Cgroup {
                path: self.path.to_str()?.into(),
                device: self.identity.device,
                inode: self.identity.inode,
            })
        }

        fn kill_through(directory: &std::fs::File) -> io::Result<()> {
            use rustix::fs::{openat, Mode, OFlags};
            use std::io::Write;
            let fd = openat(
                directory,
                "cgroup.kill",
                OFlags::WRONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            std::fs::File::from(fd).write_all(b"1")
        }

        fn kill_with_leader(&self, still_member: impl FnOnce(&Path) -> bool) -> io::Result<()> {
            if self.directory.is_none() {
                return Err(io::Error::other("adopted cgroup was not pinned"));
            }
            let directory = Self::open(&self.path)?;
            if Self::identity(&directory)? != self.identity || !still_member(&self.path) {
                return Err(io::Error::other(
                    "adopted cgroup identity or membership changed",
                ));
            }
            Self::kill_through(&directory)
        }

        fn empty(directory: &std::fs::File) -> io::Result<bool> {
            use rustix::fs::{openat, Mode, OFlags};
            use std::io::Read;
            let fd = match openat(
                directory,
                "cgroup.events",
                OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                // ENOENT through the pinned directory is how a removed cgroup
                // reports that it is empty. Failure to look up its path again
                // is not equivalent and never settles an unpinned target.
                Err(rustix::io::Errno::NOENT) => return Ok(true),
                Err(error) => return Err(error.into()),
            };
            let mut events = String::new();
            std::fs::File::from(fd)
                .take(4096)
                .read_to_string(&mut events)?;
            events
                .lines()
                .find_map(|line| match line {
                    "populated 0" => Some(true),
                    "populated 1" => Some(false),
                    _ => None,
                })
                .ok_or_else(|| io::Error::other("adopted cgroup has no population state"))
        }

        fn settle(&self, may_signal: bool) -> io::Result<()> {
            let directory = self
                .directory
                .as_ref()
                .ok_or_else(|| io::Error::other("adopted cgroup was not pinned"))?;
            if Self::empty(directory)? {
                return Ok(());
            }
            if may_signal {
                Self::kill_through(directory)?;
            }
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "adopted cgroup remains populated",
            ))
        }
    }

    /// An identity that may be signalled only while it is still our unreaped
    /// child and all of the roster's identity witnesses still match.
    #[derive(Debug)]
    pub(crate) struct VerifiedChild {
        recorded: live_children::LiveChild,
        containment: Containment,
        reaped: Mutex<ReapState>,
        #[cfg(test)]
        signal_attempts: std::sync::atomic::AtomicUsize,
        #[cfg(test)]
        kill_identity_sample: Mutex<Option<KillIdentitySample>>,
        #[cfg(test)]
        signal_refusal: Mutex<Option<String>>,
    }

    #[cfg(test)]
    #[derive(Debug)]
    struct KillIdentitySample {
        observed_alive: oneshot::Sender<()>,
        sample_identity: std::sync::mpsc::Receiver<()>,
    }

    #[derive(Debug, Default)]
    struct ReapState {
        reaped: bool,
        settled: bool,
    }

    impl VerifiedChild {
        fn check_identity(&self, process: &subc_os::Process) -> io::Result<()> {
            use live_children::IdentityVerdict;
            let observed = process.observe().ok_or_else(|| {
                io::Error::other("child identity facts unreadable: process observation failed")
            })?;
            let verdict =
                live_children::identity_verdict(&self.recorded, self.recorded.pid, &observed);
            let reason = match verdict {
                IdentityVerdict::Matches => return Ok(()),
                IdentityVerdict::PidDiffers => "child pid differs from the live roster".into(),
                IdentityVerdict::StartTimeUnrecorded => format!(
                    "child identity facts unreadable: no start time was recorded; observed {}",
                    observed.start_time
                ),
                IdentityVerdict::StartTimeDiffers => format!(
                    "child start time differs from the live roster: recorded {:?}, observed {}",
                    self.recorded.start_time, observed.start_time
                ),
                IdentityVerdict::ExecutableUnrecorded => format!(
                    "child identity facts unreadable: no executable was recorded; observed start time {}",
                    observed.start_time
                ),
                IdentityVerdict::ExecutableUnreadable => format!(
                    "child identity facts unreadable: executable observation failed; recorded {:?}, observed start time {}",
                    self.recorded.executable, observed.start_time
                ),
                IdentityVerdict::ExecutableDiffers => format!(
                    "child executable differs from the live roster: recorded {:?}, observed {:?}",
                    self.recorded.executable, observed.executable
                ),
                #[cfg(target_os = "linux")]
                IdentityVerdict::MatchesWithExecutablePermissionDenied => format!(
                    "child identity facts unreadable: executable permission was denied; recorded {:?}, observed start time {}",
                    self.recorded.executable, observed.start_time
                ),
                #[cfg(target_os = "linux")]
                IdentityVerdict::ProcessHidden => format!(
                    "child identity facts unreadable: procfs entry is hidden; observed start time {}",
                    observed.start_time
                ),
            };
            Err(io::Error::other(reason))
        }

        pub(crate) fn signal(&self, signal: Signal) -> io::Result<()> {
            let result = self.signal_checked(signal);
            #[cfg(test)]
            {
                *self
                    .signal_refusal
                    .lock()
                    .unwrap_or_else(|p| p.into_inner()) =
                    result.as_ref().err().map(|error| error.to_string());
            }
            result
        }

        #[cfg(test)]
        fn signal_refusal(&self) -> Option<String> {
            self.signal_refusal
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
        }

        fn signal_checked(&self, signal: Signal) -> io::Result<()> {
            #[cfg(test)]
            self.signal_attempts
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let reaped = self.reaped.lock().unwrap_or_else(|p| p.into_inner());
            if reaped.reaped || subc_os::child_exit::observe(self.recorded.pid)?.is_some() {
                return Ok(());
            }
            #[cfg(test)]
            if signal == Signal::KILL {
                let sample = self.kill_identity_sample.lock().unwrap().take();
                if let Some(sample) = sample {
                    tokio::task::block_in_place(|| {
                        sample.observed_alive.send(()).unwrap();
                        sample
                            .sample_identity
                            .recv_timeout(Duration::from_secs(5))
                            .unwrap();
                    });
                }
            }
            // The identity sample and the signal are separate operations; the
            // process could exec in between. The sample cannot cover that gap.
            let process = match subc_os::Process::open(self.recorded.pid) {
                Ok(Some(process)) => match self.check_identity(&process) {
                    Ok(()) => process,
                    Err(error) => return self.exit_or_refusal(error),
                },
                Ok(None) => {
                    return self.exit_or_refusal(io::Error::other(
                        "child identity facts unreadable: process could not be opened",
                    ))
                }
                Err(error) => return self.exit_or_refusal(error),
            };
            match &self.containment {
                Containment::Group => {
                    let pid = raw_pid(self.recorded.pid)?;
                    match subc_os::process_group::pgid(self.recorded.pid) {
                        Ok(Some(leader)) if leader == self.recorded.pid => {}
                        Ok(actual) => {
                            return self.exit_or_refusal(io::Error::other(format!(
                                "child process group differs from its recorded leadership: recorded {}, observed {:?}",
                                self.recorded.pid, actual
                            )))
                        }
                        Err(error) => return self.exit_or_refusal(error),
                    }
                    match rustix::process::kill_process_group(pid, signal) {
                        Ok(()) | Err(rustix::io::Errno::SRCH) => return Ok(()),
                        Err(error) => return Err(error.into()),
                    }
                }
                #[cfg(target_os = "linux")]
                Containment::Cgroup(target)
                    if signal == Signal::KILL
                        && target
                            .kill_with_leader(|path| cgroup_matches(self.recorded.pid, path))
                            .is_ok() =>
                {
                    return Ok(())
                }
                _ => {}
            }
            let signal = match signal {
                Signal::KILL => subc_os::Signal::Kill,
                Signal::TERM => subc_os::Signal::Terminate,
                _ => return Err(io::Error::other("unsupported adopted child signal")),
            };
            process.signal(signal).map(|_| ())
        }

        // The caller still holds the reap lock. An owned child can exit after
        // the first observation and lose its executable or process-group info.
        // Only a confirmed exit makes the signal unnecessary; an unreadable
        // or mismatched live process remains a refusal.
        fn exit_or_refusal(&self, refusal: io::Error) -> io::Result<()> {
            if matches!(subc_os::child_exit::observe(self.recorded.pid), Ok(Some(_))) {
                Ok(())
            } else {
                Err(refusal)
            }
        }

        fn reap(&self) -> io::Result<()> {
            let mut reaped = self.reaped.lock().unwrap_or_else(|p| p.into_inner());
            if reaped.settled {
                return Ok(());
            }
            if !reaped.reaped {
                if matches!(self.containment, Containment::Group) {
                    plain_group::settle_pinned_exited_group(self.recorded.pid, true)?;
                }
                reap_owned(self.recorded.pid)?;
                reaped.reaped = true;
            } else if matches!(self.containment, Containment::Group) {
                return Err(io::Error::other(
                    "process-group leader is no longer held; group emptiness cannot be proven",
                ));
            }
            #[cfg(target_os = "linux")]
            if let Containment::Cgroup(target) = &self.containment {
                target.settle(true)?;
            }
            reaped.settled = true;
            Ok(())
        }

        fn settled(&self) -> bool {
            self.reaped
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .settled
        }
    }

    fn raw_pid(pid: u32) -> io::Result<Pid> {
        i32::try_from(pid)
            .ok()
            .filter(|pid| *pid > 1)
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::other("invalid handover child pid"))
    }

    fn reap_owned(pid: u32) -> io::Result<()> {
        let pid = raw_pid(pid)?;
        loop {
            match rustix::process::waitpid(Some(pid), WaitOptions::NOHANG) {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => return Err(io::Error::other("child has not exited")),
                Err(rustix::io::Errno::INTR) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    #[cfg(target_os = "linux")]
    fn cgroup_matches(pid: u32, path: &Path) -> bool {
        let Ok(relative) = path.strip_prefix("/sys/fs/cgroup") else {
            return false;
        };
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return false;
        }
        std::fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|entry| {
            entry
                .lines()
                .filter_map(|line| line.strip_prefix("0::/"))
                .any(|member| Path::new(member) == relative)
        })
    }

    #[derive(Debug)]
    pub(in crate::supervise) struct AdoptedProcess {
        watcher: Arc<AsyncFd<OwnedFd>>,
        verified: Arc<VerifiedChild>,
        observed: Option<ExitStatus>,
        pub(in crate::supervise) resume: Option<AwaitingResume>,
        #[cfg(test)]
        wait_test: TestWait,
    }

    #[cfg(test)]
    #[derive(Debug, Default)]
    enum TestWait {
        #[default]
        Normal,
        Error,
        Stuck,
        Held(watch::Receiver<bool>),
    }

    impl AdoptedProcess {
        pub(in crate::supervise) fn verified(&self) -> &Arc<VerifiedChild> {
            &self.verified
        }

        pub(in crate::supervise) fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
            if self.observed.is_none() {
                let mut cx = Context::from_waker(Waker::noop());
                if let Poll::Ready(ready) = self.watcher.poll_read_ready(&mut cx) {
                    let mut ready = ready?;
                    self.observed = subc_os::child_exit::observe(self.verified.recorded.pid)?;
                    if self.observed.is_none() {
                        ready.clear_ready();
                    }
                }
            }
            Ok(self.observed)
        }

        pub(in crate::supervise) async fn wait(&mut self) -> io::Result<ExitStatus> {
            #[cfg(test)]
            match &mut self.wait_test {
                TestWait::Error => return Err(io::Error::other("forced watcher error")),
                TestWait::Stuck => return std::future::pending().await,
                TestWait::Held(release) => {
                    release
                        .wait_for(|released| *released)
                        .await
                        .expect("test owns the release");
                }
                TestWait::Normal => {}
            }
            loop {
                if let Some(status) = self.observed {
                    return Ok(status);
                }
                let mut ready = self.watcher.readable().await?;
                self.observed = subc_os::child_exit::observe(self.verified.recorded.pid)?;
                if self.observed.is_none() {
                    ready.clear_ready();
                }
            }
        }

        pub(in crate::supervise) fn kill(
            &self,
            group: Option<&plain_group::PlainGroup>,
        ) -> io::Result<()> {
            group.map_or_else(
                || self.verified.signal(Signal::KILL),
                |group| group.signal(Signal::KILL),
            )
        }

        pub(in crate::supervise) fn reap(&self) -> io::Result<()> {
            self.verified.reap()
        }
    }

    /// Classify before the resume bootstrap runs the orphan sweep. A listed
    /// pid is either protected from the sweep or already reaped by this image.
    pub(crate) enum ReconciledChild {
        Adopt(Box<PreparedAdoption>),
        Held(Box<UnresolvedChild>),
        Exited,
    }

    pub(crate) struct PreparedAdoption {
        witness: ChildWitness,
        recorded: live_children::RecordedChild,
        process: AdoptedProcess,
    }

    pub(crate) struct UnresolvedChild {
        witness: ChildWitness,
        exit: HeldExit,
        ending: bool,
    }

    struct HeldExit {
        pid: u32,
        start_time: u64,
        watcher: Option<Arc<AsyncFd<OwnedFd>>>,
        verified: Option<Arc<VerifiedChild>>,
        unverified_group: Option<u32>,
        #[cfg(target_os = "linux")]
        unverified_cgroup: Option<CgroupTarget>,
    }

    fn is_no_child(error: &io::Error) -> bool {
        error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error())
    }

    fn proven_gone(pid: u32, start_time: u64) -> bool {
        raw_pid(pid).is_ok_and(|raw| {
            rustix::process::test_kill_process(raw) == Err(rustix::io::Errno::SRCH)
        }) || crate::provenance::process_start_time(pid).is_some_and(|actual| actual != start_time)
    }

    pub(crate) fn reconcile_child(
        witness: ChildWitness,
        record: Option<&live_children::RecordedChild>,
    ) -> io::Result<ReconciledChild> {
        let mut watcher = async_watch(witness.pid).ok();
        let owned = subc_os::child_exit::observe(witness.pid);
        let roster = record.filter(|record| {
            record.child.module_id == witness.module_id
                && record.child.pid == witness.pid
                && record.child.start_time == Some(witness.start_time)
        });
        let token = witness.resume_token_sha256.to_string();
        let authenticated_launch =
            roster.is_some_and(|record| record.resume_token_sha256.as_deref() == Some(&token));
        let gone =
            owned.as_ref().is_err_and(is_no_child) && proven_gone(witness.pid, witness.start_time);
        let exited_owned = matches!(owned, Ok(Some(_)));
        let mut containment = match &witness.containment {
            ContainmentWitness::ProcessGroup {
                leader_pid,
                leader_start_time,
            } if *leader_pid == witness.pid
                && *leader_start_time == witness.start_time
                && (subc_os::process_group::pgid(witness.pid).ok().flatten()
                    == Some(witness.pid)
                    || (exited_owned || gone)
                        && authenticated_launch
                        && crate::provenance::process_start_time(witness.pid)
                            .is_none_or(|start| start == witness.start_time)) =>
            {
                Some(Containment::Group)
            }
            #[cfg(target_os = "linux")]
            ContainmentWitness::Cgroup {
                path,
                device,
                inode,
            } => Some(Containment::Cgroup(CgroupTarget::from_witness(
                PathBuf::from(path),
                FileIdentity {
                    device: *device,
                    inode: *inode,
                },
            ))),
            _ => None,
        };
        let is_contained = match &containment {
            Some(Containment::Group) => true,
            #[cfg(target_os = "linux")]
            Some(Containment::Cgroup(target)) => {
                target.directory.is_some() && cgroup_matches(witness.pid, &target.path)
            }
            _ => false,
        };
        let matches = roster.is_some_and(|record| {
            subc_os::Process::open(witness.pid)
                .ok()
                .flatten()
                .and_then(|process| process.observe())
                .is_some_and(|observed| {
                    live_children::identity_verdict(&record.child, witness.pid, &observed)
                        == live_children::IdentityVerdict::Matches
                })
        });
        // A zombie no longer exposes its executable. Its unreaped pid, start
        // time and authenticated launch still identify the group pin we inherited.
        let verified = roster
            .filter(|_| {
                (matches && matches!(owned, Ok(None)))
                    || (authenticated_launch && (exited_owned || gone))
            })
            .map(|record| {
                Arc::new(VerifiedChild {
                    recorded: record.child.clone(),
                    containment: if is_contained || authenticated_launch && (exited_owned || gone) {
                        containment.take().unwrap_or(Containment::LeaderOnly)
                    } else {
                        Containment::LeaderOnly
                    },
                    reaped: Mutex::new(ReapState {
                        reaped: gone,
                        settled: false,
                    }),
                    #[cfg(test)]
                    signal_attempts: Default::default(),
                    #[cfg(test)]
                    kill_identity_sample: Mutex::default(),
                    #[cfg(test)]
                    signal_refusal: Mutex::default(),
                })
            });
        if let Some(verified) = &verified {
            if exited_owned || gone {
                if verified.reap().is_ok() {
                    return Ok(ReconciledChild::Exited);
                }
            } else if matches!(owned, Ok(None))
                && matches
                && is_contained
                && authenticated_launch
                && witness.spawn_generation != 0
            {
                if let Some(watcher) = watcher.take() {
                    return Ok(ReconciledChild::Adopt(Box::new(PreparedAdoption {
                        witness,
                        recorded: roster.expect("verified roster").clone(),
                        process: AdoptedProcess {
                            watcher,
                            verified: verified.clone(),
                            observed: None,
                            resume: None,
                            #[cfg(test)]
                            wait_test: TestWait::Normal,
                        },
                    })));
                }
            }
        }
        let ending = matches!(owned, Ok(None))
            && verified
                .as_ref()
                .is_some_and(|verified| verified.signal(Signal::KILL).is_ok());
        #[cfg(target_os = "linux")]
        let unverified_cgroup = match containment {
            Some(Containment::Cgroup(target)) => Some(target),
            _ => None,
        };
        let unverified_group = if verified.is_some() {
            None
        } else {
            match &witness.containment {
                ContainmentWitness::ProcessGroup {
                    leader_pid,
                    leader_start_time,
                } if *leader_pid == witness.pid && *leader_start_time == witness.start_time => {
                    Some(*leader_pid)
                }
                _ => None,
            }
        };
        let mut exit = HeldExit {
            pid: witness.pid,
            start_time: witness.start_time,
            watcher,
            verified,
            unverified_group,
            #[cfg(target_os = "linux")]
            unverified_cgroup,
        };
        if (exited_owned || gone) && exit.try_resolution().is_ok_and(|resolved| resolved) {
            return Ok(ReconciledChild::Exited);
        }
        Ok(ReconciledChild::Held(Box::new(UnresolvedChild {
            witness,
            exit,
            ending,
        })))
    }

    #[derive(Debug)]
    pub(in crate::supervise) struct AwaitingResume {
        attempt: [u8; 16],
        deadline: Instant,
        accepted: oneshot::Receiver<Option<crate::config_gate::runtime::DeliverySpawn>>,
    }

    /// The roster guard owns a watcher even if the normal actor loses its child
    /// on a failed drain, wait, or containment cleanup.
    pub(crate) struct AdoptionRecovery {
        exit: HeldExit,
        attempt: [u8; 16],
        handle: SupervisorHandle,
        roster: ChildRoster,
        module: String,
        restart: bool,
    }

    impl std::fmt::Debug for AdoptionRecovery {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("AdoptionRecovery")
                .field("module", &self.module)
                .field("pid", &self.exit.pid)
                .finish()
        }
    }

    impl AdoptionRecovery {
        pub(crate) fn no_restart(&mut self) {
            self.restart = false;
        }
        pub(crate) fn settled(&self) -> bool {
            self.exit
                .verified
                .as_ref()
                .is_some_and(|child| child.settled())
        }
        pub(crate) fn block(&self) {
            self.handle
                .upgrade_state
                .update(&self.module, Disposition::Blocked);
            self.handle
                .pending_adoptions
                .remove_pending(&self.module, self.attempt);
        }
        pub(crate) fn observe(self) {
            let Ok(runtime) = tokio::runtime::Handle::try_current() else {
                return;
            };
            runtime.spawn(async move {
                let pid = self.exit.pid;
                let start_time = self.exit.start_time;
                let result = tokio::select! {
                    result = self.exit.observe_until_resolved() => result,
                    _ = self.roster.closed() => return,
                };
                let result = result.and_then(|()| {
                    self.roster
                        .remove_reaped_adoption(&self.module, pid, start_time)
                        .map(|_| ())
                });
                if let Err(error) = result {
                    warn!(module_id = %self.module, %error, "adopted child remains held");
                    return;
                }
                if let Some(module) = self.handle.get(&self.module) {
                    if module
                        .inner
                        .commands
                        .send(SupervisorCommand::HeldAdoptionResolved {
                            restart: self.restart,
                        })
                        .await
                        .is_ok()
                    {
                        return;
                    }
                }
                self.handle
                    .upgrade_state
                    .update(&self.module, Disposition::Stopped);
            });
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(in crate::supervise) async fn drive_pending_child(
        spec: &ModuleSpec,
        runtime: &SupervisorRuntimeConfig,
        registry: &Registry,
        liveness: &SupervisorProcessLiveness,
        snapshot: &SharedSnapshot,
        child: &mut Option<SupervisedChild>,
        commands: &mut mpsc::Receiver<SupervisorCommand>,
        deferred: &mut VecDeque<SupervisorCommand>,
    ) -> bool {
        let Some(active) = child.as_mut() else {
            return false;
        };
        let containment::ProcessChild::Adopted(_, process) = &mut active.child else {
            return false;
        };
        let Some(mut resume) = process.resume.take() else {
            return false;
        };
        let handle = runtime
            .supervisor_handle
            .as_ref()
            .expect("adopted child needs its supervisor");
        let id = &spec.module_id;
        let mut deadline_fired = false;
        let mut accepted_open = true;
        let status = loop {
            tokio::select! {
                biased;
                accepted = &mut resume.accepted, if accepted_open => {
                    if let Ok(delivery) = accepted {
                        active.registration_deadline = None;
                        active.config_delivery = delivery;
                        handle.upgrade_state.update(id, Disposition::Resumed);
                        return true;
                    }
                    accepted_open = false;
                }
                status = active.wait() => break status,
                _ = sleep_until(resume.deadline), if !deadline_fired => {
                    deadline_fired = true;
                    if handle.pending_adoptions.take_expired(id, resume.attempt).is_some() {
                        active.registration_deadline = None;
                        if let Err(error) = active.start_kill() {
                            warn!(module_id = %id, %error, "expired adopted child could not be safely signalled");
                            *child = None; // The roster guard transfers ownership to Held.
                            return true;
                        }
                    }
                }
                command = commands.recv(), if !commands.is_closed() => {
                    if let Some(command) = command {
                        super::refuse_and_defer(command, deferred);
                    }
                }
            }
        };
        handle.pending_adoptions.remove_pending(id, resume.attempt);
        if let Ok(status) = status {
            let report = classify_reaped_child_exit(snapshot, active, &status);
            active.drain_stderr(id).await;
            if active.finish_exit(false).await.is_ok() {
                record_terminal_with_detail(
                    id,
                    &runtime.terminal_ring,
                    &runtime.spawn_events,
                    &report,
                    TerminalDisposition::Restarting,
                    Some("daemon_upgrade".into()),
                );
                *child = None;
                let failure = runtime.failure_context(true);
                let _ = release_dead_registration(
                    registry,
                    runtime.forwarding.as_deref(),
                    snapshot,
                    id,
                    &failure,
                )
                .await;
                if !runtime.child_roster.is_closed() {
                    match spawn_replacement(spec, runtime, snapshot).await {
                        Ok(replacement) => {
                            *child = Some(replacement);
                            handle.upgrade_state.update(id, Disposition::Restarted);
                        }
                        Err(error) => {
                            warn!(module_id = %id, %error, "planned adoption restart could not launch");
                            fail_snapshot(snapshot, Some(id), None, &failure);
                            liveness.untrack_if_current(id, snapshot);
                            handle.upgrade_state.update(id, Disposition::Stopped);
                        }
                    }
                } else {
                    handle.upgrade_state.update(id, Disposition::Stopped);
                }
                return true;
            }
        }
        // A watcher or reap error transfers both the durable row and observer.
        *child = None;
        true
    }

    pub(crate) type InheritedStreams = [Option<(OwnedFd, Vec<u8>)>; 2];

    impl PreparedAdoption {
        pub(crate) fn begin_preparation(
            &self,
            supervisor: &SupervisorHandle,
            deadline: Instant,
        ) -> Result<resume_hello::Preparation, &'static str> {
            supervisor
                .pending_adoptions
                .begin_preparation(&self.witness, deadline)
        }

        /// The resume bootstrap uses this pid to protect the durable row from sweep.
        pub(crate) fn pid(&self) -> u32 {
            self.witness.pid
        }

        /// The resume bootstrap calls this for a disabled child or a refused binding.
        pub(crate) fn stop(self) -> UnresolvedChild {
            let ending = self.process.verified.signal(Signal::KILL).is_ok();
            UnresolvedChild {
                exit: HeldExit::for_adopted(&self.process),
                ending,
                witness: self.witness,
            }
        }
    }

    impl UnresolvedChild {
        pub(crate) fn pid(&self) -> u32 {
            self.witness.pid
        }

        async fn reap(self) -> io::Result<()> {
            self.exit.observe_until_resolved().await
        }
    }

    impl HeldExit {
        fn for_adopted(process: &AdoptedProcess) -> Self {
            Self {
                pid: process.verified.recorded.pid,
                start_time: process
                    .verified
                    .recorded
                    .start_time
                    .expect("adoptee has a birth time"),
                watcher: Some(process.watcher.clone()),
                verified: Some(process.verified.clone()),
                unverified_group: None,
                #[cfg(target_os = "linux")]
                unverified_cgroup: None,
            }
        }

        // `Ok(false)` needs another exit notification. An error after exit
        // (for example, a populated cgroup) keeps the row fenced as well.
        fn try_resolution(&mut self) -> io::Result<bool> {
            let already_reaped = self.verified.as_ref().is_some_and(|verified| {
                verified
                    .reaped
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .reaped
            });
            if !already_reaped {
                match subc_os::child_exit::observe(self.pid) {
                    Ok(Some(_)) => {
                        if self.verified.is_none() {
                            if let Some(leader) = self.unverified_group {
                                plain_group::settle_pinned_exited_group(leader, false)?;
                            }
                            reap_owned(self.pid)?;
                        }
                    }
                    Err(error) if is_no_child(&error) && proven_gone(self.pid, self.start_time) => {
                        if self.unverified_group.is_some() {
                            return Err(io::Error::other(
                                "process-group leader is no longer held; group emptiness cannot be proven",
                            ));
                        }
                        if let Some(verified) = &self.verified {
                            if matches!(verified.containment, Containment::Group) {
                                return Err(io::Error::other(
                                    "process-group leader is no longer held; group emptiness cannot be proven",
                                ));
                            }
                            verified
                                .reaped
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .reaped = true;
                        }
                    }
                    Ok(None) | Err(_) => return Ok(false),
                }
            }
            if let Some(verified) = &self.verified {
                verified.reap()?;
            }
            #[cfg(target_os = "linux")]
            if let Some(target) = &self.unverified_cgroup {
                target.settle(false)?;
            }
            Ok(true)
        }

        async fn observe_until_resolved(mut self) -> io::Result<()> {
            use tokio::signal::unix::{signal, SignalKind};
            let mut child_events = signal(SignalKind::child())?;
            let mut last_cleanup_error = None;
            loop {
                let needs_cleanup = match self.try_resolution() {
                    Ok(true) => return Ok(()),
                    Ok(false) => false,
                    Err(error) => {
                        let detail = error.to_string();
                        if last_cleanup_error.as_deref() != Some(detail.as_str()) {
                            warn!(pid = self.pid, %error, "held child containment is not proven empty");
                        }
                        last_cleanup_error = Some(detail);
                        true
                    }
                };
                tokio::select! {
                    event = child_events.recv() => { if event.is_none() { return Err(io::Error::other("child exit signal closed")); } },
                    _ = tokio::time::sleep(Duration::from_millis(100)), if needs_cleanup => {},
                    _ = async { match &self.watcher { Some(watcher) => { let _ = watcher.readable().await; }, None => std::future::pending().await } } => {
                        // Exit readiness is permanent; subsequent observations
                        // use SIGCHLD so an unobservable pid cannot spin.
                        self.watcher = None;
                    }
                }
            }
        }
    }

    impl Supervisor {
        /// Start one resolved cohort member as a planned startup. The resume
        /// bootstrap supplies the decision from both configurations, not cold autostart.
        pub(crate) async fn start_planned_after_upgrade(
            &self,
            module: &str,
            registered: Option<RegisteredConfiguredModule>,
            allow_start: bool,
        ) -> Result<(), SuperviseError> {
            if let Some(mut registered) = registered {
                update_snapshot(&registered.module.inner.snapshot, Some(module), |state| {
                    state.enabled = allow_start;
                    if !allow_start {
                        state.state = ModuleState::Disabled;
                    }
                    state.waiting_for_previous_pid = None;
                    state.waiting_for_previous_detail = None;
                })?;
                let mut replacing = self
                    .supervisor_handle
                    .as_ref()
                    .filter(|_| allow_start)
                    .map(|handle| handle.upgrade_state.begin_replacement(module));
                registered.runtime.replacement_ticket =
                    replacing.as_ref().map(|replacing| replacing.ticket.clone());
                self.start_registered_configured_async(registered, None, true)
                    .await?;
                if let Some(replacing) = &mut replacing {
                    replacing.restarted();
                }
            }
            if let Some(handle) = &self.supervisor_handle {
                handle.upgrade_state.update(
                    module,
                    if allow_start {
                        Disposition::Restarted
                    } else {
                        Disposition::Stopped
                    },
                );
            }
            Ok(())
        }

        /// Observe a Held child without signalling it. A bootstrap rejection
        /// whose verified kill was already sent uses the same one-reap path.
        pub(crate) fn watch_unresolved_after_upgrade(
            &self,
            unresolved: UnresolvedChild,
            registered: Option<RegisteredConfiguredModule>,
            allow_start: bool,
        ) -> tokio::task::JoinHandle<()> {
            let supervisor = self.clone();
            let module = unresolved.witness.module_id.clone();
            if let Some(handle) = &self.supervisor_handle {
                handle.upgrade_state.update(
                    &module,
                    if unresolved.ending {
                        Disposition::Pending
                    } else {
                        Disposition::Blocked
                    },
                );
            }
            let pid = unresolved.witness.pid;
            let start = unresolved.witness.start_time;
            tokio::spawn(async move {
                let result = async {
                    unresolved.reap().await?;
                    supervisor
                        .child_roster
                        .remove_reaped_adoption(&module, pid, start)?;
                    if !supervisor.child_roster.is_closed() {
                        supervisor
                            .start_planned_after_upgrade(&module, registered, allow_start)
                            .await
                            .map_err(io::Error::other)?;
                    }
                    Ok::<_, io::Error>(())
                }
                .await;
                if let Err(error) = result {
                    tracing::warn!(module_id = %module, pid, %error, "could not resolve held child");
                }
            })
        }

        /// Install a verified child and its pending HELLO before releasing that
        /// child's preparation guard. Consume only validated, owned pipe fds.
        pub(crate) async fn start_adopted_after_upgrade(
            &self,
            registered: RegisteredConfiguredModule,
            prepared: PreparedAdoption,
            streams: InheritedStreams,
            attempt: [u8; 16],
            deadline: Instant,
            allow_start: bool,
        ) -> io::Result<()> {
            if !allow_start {
                self.watch_unresolved_after_upgrade(prepared.stop(), Some(registered), false);
                return Ok(());
            }
            let RegisteredConfiguredModule {
                spec,
                runtime,
                module,
                commands,
            } = registered;
            let PreparedAdoption {
                witness,
                recorded,
                process,
            } = prepared;
            let handle = self
                .supervisor_handle
                .as_ref()
                .ok_or_else(|| io::Error::other("adoption requires a supervisor handle"))?;
            let verified = process.verified.clone();
            let module_id = &spec.module_id;
            let pid = witness.pid;
            let start = witness.start_time;
            let owner = runtime
                .config_delivery
                .as_ref()
                .map(|delivery| delivery.0.clone());
            let admission = tokio::time::timeout_at(deadline, async {
                let admission = match &owner {
                    Some(owner) => {
                        let spec = spec.clone();
                        Some(
                            owner
                                .on_blocking(move |owner| owner.prepare_spawn(&spec))
                                .await
                                .map_err(io::Error::other)?,
                        )
                    }
                    None => None,
                };
                let base = admission
                    .as_ref()
                    .and_then(|admission| admission.approved_base())
                    .cloned();
                if admission
                    .as_ref()
                    .is_some_and(|admission| admission.gated())
                    != witness.store_gated
                    || base.as_ref().map(|digest| digest.as_str())
                        != witness
                            .admission_base_digest
                            .as_ref()
                            .map(ToString::to_string)
                            .as_deref()
                {
                    return Err(io::Error::other(
                        "carried program binding does not match the active configuration",
                    ));
                }
                Ok((admission, base))
            })
            .await
            .unwrap_or_else(|_| {
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "carried program preparation expired",
                ))
            });
            let (mut admission, admission_base) = match admission {
                Ok(admission) => admission,
                Err(error) => {
                    tracing::info!(%module_id, %error, "carried program needs a fresh admission");
                    let prepared = PreparedAdoption {
                        witness,
                        recorded,
                        process,
                    };
                    self.watch_unresolved_after_upgrade(
                        prepared.stop(),
                        Some(RegisteredConfiguredModule {
                            spec,
                            runtime,
                            module,
                            commands,
                        }),
                        true,
                    );
                    return Ok(());
                }
            };
            let group = if matches!(verified.containment, Containment::Group) {
                plain_group::PlainGroup::from_adopted(pid, &verified).map(Arc::new)
            } else {
                None
            };
            let key = program_binding::ProgramBindingKey::adopted_from_handover(
                witness.program_key.incarnation.clone(),
                witness.program_key.channel_launch_seq,
            );
            let publication = if witness.store_gated {
                Some(
                    handle
                        .publish_adopted_program_binding(module_id, key.clone())
                        .ok_or_else(|| {
                            io::Error::other("could not publish adopted program binding")
                        })?,
                )
            } else {
                None
            };
            let role = match witness.registration_spawn_kind {
                RegistrationSpawnKind::Plain => SpawnRole::Plain,
                RegistrationSpawnKind::SwapCandidate => SpawnRole::SwapCandidate,
            };
            let registration = RegistrationSpawn::adopted_from_handover(
                handle,
                module_id,
                role,
                publication.clone(),
                key,
                witness.spawn_generation,
            );
            let registration_declaration = admission
                .as_ref()
                .and_then(|admission| admission.registration_declaration(module_id));
            let launch = admission.as_mut().and_then(|admission| {
                if let Some(launch) = &admission.launch {
                    if let Ok(origin) = crate::program_origin::LaunchWitness::capture(&launch.spec)
                    {
                        admission.initialize_origin(&registration.authority, &origin);
                    }
                }
                admission.launch.take()
            });
            let exec = Arc::new(crate::activity::ExecOutcome::new());
            exec.finish(crate::activity::ExecState::CommittedExec);
            #[cfg(target_os = "linux")]
            let subtree_target = match &verified.containment {
                Containment::Cgroup(target) => {
                    crate::subtree_resources::Target::Cgroup(target.path.clone())
                }
                _ => crate::subtree_resources::Target::Unavailable("no_cgroup"),
            };
            #[cfg(target_os = "macos")]
            let subtree_target =
                crate::subtree_resources::Target::Unavailable("unsupported_platform");
            let (accepted, accepted_rx) = oneshot::channel();
            let mut process = process;
            process.resume = Some(AwaitingResume {
                attempt,
                deadline,
                accepted: accepted_rx,
            });
            let mut child = SupervisedChild {
                registration_spawn: Some(registration.clone()),
                registration_channel: None,
                live_program_binding: None,
                registered_config_connection: None,
                admission_base: admission_base.clone(),
                registration_declaration,
                _program_binding: publication,
                subtree_target,
                child: containment::ProcessChild::adopted(pid, process, group.clone()),
                exec: crate::activity::ExecOwner(exec),
                #[cfg(target_os = "macos")]
                privacy_exec: None,
                #[cfg(target_os = "macos")]
                report_ready: Arc::new(OnceLock::from(())),
                spawn_failure: None,
                admission_kill_sent: false,
                protocol: recorded.child.protocol,
                #[cfg(target_os = "linux")]
                module_id: recorded.child.cgroup_name.clone().unwrap_or_default(),
                #[cfg(target_os = "linux")]
                cgroup_placement: None,
                #[cfg(target_os = "linux")]
                unresolved_cgroup: None,
                stdout_pump: None,
                stderr_pump: None,
                capture_stop: None,
                late_output_lost: false,
                stderr_ring: runtime.stderr_ring.clone(),
                spawned_at: Instant::now(),
                config_delivery: None,
                registration_deadline: Some(deadline),
                registration_watch: watch_registration(&self.registry, &spec),
                spawned_at_ms: witness.spawned_at_ms,
                spawned_from: spec.program.clone(),
                launch_spec_digest: LaunchSpecDigest(witness.launch_spec_digest.0),
                spawned_file_identity: witness.spawned_file_identity.as_ref().map(|identity| {
                    SpawnedFileIdentity {
                        device: identity.device,
                        inode: identity.inode,
                    }
                }),
                process_start_time: Some(start),
                process_identity: Some(ProcessIdentity {
                    pid,
                    start_time: start,
                }),
                pid,
                roster_guard: None,
            };
            if witness.store_gated {
                child.live_program_binding =
                    launch
                        .zip(owner)
                        .zip(admission_base)
                        .and_then(|((launch, owner), base)| {
                            content_binding::LiveProgramBinding::new(launch, &child, owner, base)
                        });
                let binding = child._program_binding.as_ref().expect("gated publication");
                let verified_binding = child.live_program_binding.as_ref().is_some_and(|live| {
                    if live.verdict().is_err() {
                        return false;
                    }
                    binding.attach(live);
                    binding.verify().is_ok()
                });
                if !verified_binding {
                    let ending = verified.signal(Signal::KILL).is_ok();
                    self.watch_unresolved_after_upgrade(
                        UnresolvedChild {
                            exit: match &child.child {
                                containment::ProcessChild::Adopted(_, process) => {
                                    HeldExit::for_adopted(process)
                                }
                                _ => unreachable!("built an adopted child"),
                            },
                            witness,
                            ending,
                        },
                        Some(RegisteredConfiguredModule {
                            spec,
                            runtime,
                            module,
                            commands,
                        }),
                        true,
                    );
                    return Ok(());
                }
            }
            let mut guard = runtime.child_roster.admit_adopted(
                &recorded,
                crate::child_roster::RecordedIdentity {
                    plain_group: group.as_ref().map(Arc::downgrade),
                    ..Default::default()
                },
            )?;
            let containment::ProcessChild::Adopted(_, process) = &child.child else {
                unreachable!("built an adopted child")
            };
            guard.bind_adopted_signal(
                &verified,
                AdoptionRecovery {
                    exit: HeldExit::for_adopted(process),
                    attempt,
                    handle: handle.clone(),
                    roster: runtime.child_roster.clone(),
                    module: module_id.clone(),
                    restart: true,
                },
            );
            registration
                .resume_token_record
                .set(
                    guard
                        .resume_token_record()
                        .ok_or_else(|| io::Error::other("adopted child has no token record"))?,
                )
                .map_err(|_| io::Error::other("adopted token already bound"))?;
            child.roster_guard = Some(guard);
            let pumps = child_output::resume(
                streams,
                super::super::spawn::capture_sink(&spec, runtime.capture_logs_dir.as_deref()),
                &runtime.stderr_ring,
                module_id,
                false,
            );
            child.stdout_pump = pumps.stdout;
            child.stderr_pump = pumps.stderr;
            child.capture_stop = pumps.stop;
            let snapshot = &module.inner.snapshot;
            set_running(snapshot, &child, module_id, &runtime.spawn_events)
                .map_err(io::Error::other)?;
            self.process_liveness
                .track(module_id.clone(), snapshot.clone());
            registration.ready();
            handle
                .pending_adoptions
                .insert_pending(resume_hello::PendingAdoption {
                    spawn: registration,
                    pid,
                    start_time: start,
                    resume_token_sha256: witness.resume_token_sha256.0,
                    attempt_id: attempt,
                    deadline,
                    provider_declarations: witness.provider_declarations,
                    launch_nonce_sha256: witness.launch_nonce_sha256.map(|hash| hash.0),
                    delivery: admission.map(|admission| resume_hello::PendingDelivery {
                        admission,
                        register: runtime.register_timeout,
                        health: runtime.health.deadline,
                    }),
                    accepted,
                })
                .map_err(io::Error::other)?;
            self.start_monitor(spec, runtime, &module, Some(child), commands);
            Ok(())
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;
        use cortexkit_test_support::ScratchDir;
        use std::io::Write;
        use tokio::time::timeout;

        struct AdoptionTestChild(std::process::Child);

        impl Drop for AdoptionTestChild {
            fn drop(&mut self) {
                match subc_os::child_exit::observe(self.0.id()) {
                    Ok(None) => {
                        let _ = self.0.kill();
                    }
                    Ok(Some(_)) => {}
                    Err(_) => return, // The adopted monitor already reaped it.
                }
                let _ = self.0.wait();
            }
        }

        fn spec(id: &str, dir: &ScratchDir) -> ModuleSpec {
            ModuleSpec {
                script: None,
                carry_containment: false,
                module_id: id.into(),
                program: "/bin/sleep".into(),
                args: vec!["60".into()],
                env: ["XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"]
                    .into_iter()
                    .map(|key| (key.into(), dir.path().display().to_string()))
                    .collect(),
                reserved: false,
                reserved_prefixes: vec![],
                protocol: ModuleProtocol::None,
                overlap: Default::default(),
            }
        }

        fn recorded(pid: u32, id: &str) -> live_children::RecordedChild {
            let observed = subc_os::Process::open(pid)
                .unwrap()
                .unwrap()
                .observe()
                .unwrap();
            live_children::RecordedChild {
                child: live_children::LiveChild {
                    module_id: id.into(),
                    pid,
                    protocol: ModuleProtocol::Subc,
                    start_time: Some(observed.start_time),
                    executable: observed.executable.map(Into::into),
                    cgroup_name: None,
                    container: None,
                },
                resume_token_sha256: Some(HexBytes([9; 32]).to_string()),
            }
        }

        fn witness(record: &live_children::RecordedChild) -> ChildWitness {
            let pid = record.child.pid;
            let start_time = record.child.start_time.unwrap();
            ChildWitness {
                module_id: record.child.module_id.clone(),
                pid,
                start_time,
                spawn_generation: 7,
                containment: ContainmentWitness::ProcessGroup {
                    leader_pid: pid,
                    leader_start_time: start_time,
                },
                resume_token_sha256: HexBytes([9; 32]),
                launch_nonce_sha256: None,
                launch_spec_digest: HexBytes([3; 32]),
                spawned_file_identity: None,
                program_key: ProgramKey {
                    incarnation: "old-image".into(),
                    channel_launch_seq: 4,
                },
                admission_base_digest: None,
                store_gated: false,
                registration_spawn_kind: RegistrationSpawnKind::Plain,
                provider_declarations: None,
                spawned_at_ms: 140,
                stdout: StreamWitness::Ended {},
                stderr: StreamWitness::Ended {},
            }
        }

        #[tokio::test]
        async fn upgrade_refuses_restart_and_reconcile_and_releases_on_drop() {
            let handle = SupervisorHandle::new();
            let attempt = handle.begin_upgrade().unwrap();
            for module in [Some("sample"), None] {
                let result = timeout(Duration::from_secs(1), handle.lifecycle_operation(module))
                    .await
                    .unwrap();
                assert_eq!(
                    result.unwrap_err(),
                    super::super::LifecycleRefusal::UpgradeBusy
                );
            }
            assert!(matches!(handle.begin_upgrade(), Err(UpgradeBusy::Upgrade)));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                let _attempt = attempt;
                panic!("attempt aborted before exec");
            }));
            assert!(result.is_err());
            let guard = handle.lifecycle_operation(Some("sample")).await.unwrap();
            assert!(matches!(
                handle.begin_upgrade(),
                Err(UpgradeBusy::ModuleLifecycle)
            ));
            drop(guard);
            assert!(handle.begin_upgrade().is_ok());
        }

        #[tokio::test]
        async fn stopping_the_cohort_submits_each_stop_before_waiting() {
            let scratch = ScratchDir::new("upgrade-stop-fanout");
            let handle = SupervisorHandle::new();
            let supervisor =
                Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                    .with_handle(handle.clone());
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let ids = vec!["first".to_owned(), "second".to_owned()];
            let mut tasks = JoinSet::new();
            for id in &ids {
                let mut module = supervisor
                    .register_configured_with_health(
                        spec(id, &scratch),
                        true,
                        HealthConfig::default(),
                        None,
                        RestartPolicy::default(),
                        Duration::from_secs(1),
                    )
                    .unwrap();
                let barrier = barrier.clone();
                tasks.spawn(async move {
                    let Some(SupervisorCommand::Upgrade {
                        request: ActorRequest::Stop(reply),
                        ..
                    }) = module.commands.recv().await
                    else {
                        panic!("expected attempt-owned stop");
                    };
                    barrier.wait().await;
                    reply.send(Ok(())).unwrap();
                });
            }
            let attempt = handle.begin_upgrade().unwrap();
            let stopped = timeout(Duration::from_secs(2), attempt.stop_restarting(ids.clone()))
                .await
                .unwrap();
            assert_eq!(stopped.keys().cloned().collect::<Vec<_>>(), ids);
            assert!(stopped.into_values().all(|result| result.is_ok()));
            tasks.join_all().await;
        }

        #[tokio::test]
        async fn lingering_registration_after_reap_does_not_strand_partial_stop_cohort() {
            let scratch = ScratchDir::new("upgrade-lingering-registration");
            let registry = Arc::new(Registry::default());
            let handle = SupervisorHandle::new();
            let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
                .with_handle(handle.clone());
            let ids = vec!["lingering".to_owned(), "ordinary".to_owned()];
            let mut incumbent = BTreeMap::new();
            for id in &ids {
                let mut spec = spec(id, &scratch);
                if id == "lingering" {
                    spec.protocol = ModuleProtocol::Subc;
                }
                let registered = supervisor
                    .register_configured_with_health(
                        spec,
                        true,
                        HealthConfig::default(),
                        Some(0),
                        RestartPolicy::default(),
                        Duration::from_secs(10),
                    )
                    .unwrap();
                let module = supervisor
                    .start_registered_configured_async(registered, None, true)
                    .await
                    .unwrap();
                incumbent.insert(id.clone(), module.status().unwrap().pid.unwrap());
            }
            // Keep a real, authenticated HELLO connection open independently
            // of the supervised child's lifetime.
            let (router, auth, info) = wire_router(&registry, &handle);
            let (socket, mut peer) = tokio::net::UnixStream::pair().unwrap();
            let connection = tokio::spawn(crate::server::handle_connection_with_origin(
                socket,
                router,
                auth,
                crate::server::LocalOrigin::Tcp,
            ));
            subc_transport::authenticate_client(&mut peer, &info, Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(
                offer_hello(&mut peer, "lingering", None)
                    .await
                    .unwrap()
                    .header
                    .ty,
                subc_protocol::FrameType::HelloAck
            );

            let attempt = handle.begin_upgrade().unwrap();
            let stopped = attempt.stop_restarting(ids.clone()).await;
            assert!(stopped["ordinary"].is_ok());
            let failure = stopped["lingering"].as_ref().unwrap_err();
            assert!(
                failure
                    .get_ref()
                    .and_then(|error| error.downcast_ref::<SuperviseError>())
                    .is_some_and(|error| matches!(
                        error,
                        SuperviseError::RegistrationStillActive { .. }
                    )),
                "{failure}"
            );
            assert!(registry.get_module("lingering").unwrap().is_some());
            drop(peer);
            connection.await.unwrap().unwrap();
            assert!(registry.get_module("lingering").unwrap().is_none());

            let restarted = attempt.restart_stopped(ids.clone()).await;
            for id in &ids {
                assert!(
                    matches!(restarted[id], Ok(true)),
                    "{id} must be replaced: {restarted:?}"
                );
                assert_ne!(
                    handle.get(id).unwrap().status().unwrap().pid.unwrap(),
                    incumbent[id]
                );
            }
            drop(attempt);
            for id in ids {
                handle.get(&id).unwrap().set_enabled(false).await.unwrap();
            }
        }

        #[tokio::test]
        async fn announced_reaped_child_is_replaced_despite_lingering_registration() {
            let scratch = ScratchDir::new("announced-lingering-registration");
            let registry = Arc::new(Registry::default());
            let handle = SupervisorHandle::new();
            let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
                .with_handle(handle.clone());
            let id = "announced";
            let mut spec = spec(id, &scratch);
            spec.protocol = ModuleProtocol::Subc;
            let registered = supervisor
                .register_configured_with_health(
                    spec,
                    true,
                    HealthConfig::default(),
                    Some(0),
                    RestartPolicy::default(),
                    Duration::from_secs(10),
                )
                .unwrap();
            let module = supervisor
                .start_registered_configured_async(registered, None, true)
                .await
                .unwrap();
            let old = module.status().unwrap().pid.unwrap();
            let (router, auth, info) = wire_router(&registry, &handle);
            let (socket, mut peer) = tokio::net::UnixStream::pair().unwrap();
            let server = tokio::spawn(crate::server::handle_connection_with_origin(
                socket,
                router,
                auth,
                crate::server::LocalOrigin::Tcp,
            ));
            subc_transport::authenticate_client(&mut peer, &info, Duration::from_secs(2))
                .await
                .unwrap();
            assert_eq!(
                offer_hello(&mut peer, id, None).await.unwrap().header.ty,
                subc_protocol::FrameType::HelloAck
            );
            let attempt = handle.begin_upgrade().unwrap();
            rustix::process::kill_process(Pid::from_raw(old as i32).unwrap(), Signal::TERM)
                .unwrap();
            let restarted = attempt
                .restart_announced(vec![id.into()], Instant::now() + Duration::from_secs(5))
                .await;
            assert!(
                restarted[id].is_ok(),
                "release error must not skip spawning: {restarted:?}"
            );
            assert_ne!(module.status().unwrap().pid.unwrap(), old);
            let outcome = restarted[id].as_ref().unwrap();
            assert!(outcome.release_failed.is_some() && outcome.replacement.is_ok());
            drop(peer);
            server.await.unwrap().unwrap();
            drop(attempt);
            module.set_enabled(false).await.unwrap();
        }

        #[tokio::test]
        async fn launch_allocations_do_not_reuse_a_carried_generation() {
            let handle = SupervisorHandle::new();
            let first = RegistrationSpawn::new(&handle, "sample", SpawnRole::Plain, None);
            let key = program_binding::ProgramBindingKey::adopted_from_handover("old".into(), 25);
            let adopted = RegistrationSpawn::adopted_from_handover(
                &handle,
                "sample",
                SpawnRole::Plain,
                None,
                key,
                19,
            );
            let next = RegistrationSpawn::new(&handle, "sample", SpawnRole::Plain, None);
            assert_eq!(first.authority.spawn_number(), 1);
            assert_eq!(adopted.authority.spawn_number(), 19);
            assert_eq!(next.authority.spawn_number(), 20);
            assert_ne!(first.binding_key, next.binding_key);
        }

        async fn running_child() -> (ScratchDir, SupervisedChild) {
            let scratch = ScratchDir::new("upgrade-real-child");
            let supervisor =
                Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default());
            let child = spawn_child_async(&spec("sample", &scratch), &supervisor.runtime_config())
                .await
                .unwrap();
            (scratch, child)
        }

        fn false_identity(child: &SupervisedChild) -> containment::ProcessChild {
            let pid = child.pid;
            let mut record = recorded(pid, "sample").child;
            record.start_time = record.start_time.map(|start| start + 1);
            let verified = Arc::new(VerifiedChild {
                recorded: record,
                containment: Containment::LeaderOnly,
                reaped: Mutex::default(),
                signal_attempts: Default::default(),
                kill_identity_sample: Mutex::default(),
                signal_refusal: Mutex::default(),
            });
            containment::ProcessChild::adopted(
                pid,
                AdoptedProcess {
                    watcher: async_watch(pid).unwrap(),
                    verified,
                    observed: None,
                    resume: None,
                    wait_test: TestWait::Normal,
                },
                None,
            )
        }

        #[tokio::test]
        async fn supervisor_kill_routes_adopted_children_through_identity_check() {
            let (_scratch, mut child) = running_child().await;
            let invalid = false_identity(&child);
            let ordinary = std::mem::replace(&mut child.child, invalid);
            assert!(child
                .start_kill()
                .unwrap_err()
                .to_string()
                .contains("start time differs"));
            assert!(subc_os::child_exit::observe(child.pid).unwrap().is_none());
            child.child = ordinary;
            child.start_kill().unwrap();
            assert!(!timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success());
            child.finish_exit(false).await.unwrap();
        }

        #[tokio::test]
        async fn content_refusal_routes_adopted_children_through_identity_check() {
            let (scratch, mut child) = running_child().await;
            let (_fixture, owner, _supervisor, _handler, _) =
                crate::config_gate::runtime::RuntimeOwner::unactivated_supervisor_fixture_for_test(
                );
            let spec = spec("sample", &scratch);
            let invalid = false_identity(&child);
            let verification = invalid.verified_adoptee().unwrap().clone();
            let ordinary = std::mem::replace(&mut child.child, invalid);
            let launch = crate::config_gate::program::capture(&spec, &[]).unwrap();
            let binding = content_binding::LiveProgramBinding::new(
                launch,
                &child,
                owner.clone(),
                subc_config::raw_digest(b"base"),
            )
            .unwrap();
            binding.kill_for_test();
            assert_eq!(
                verification
                    .signal_attempts
                    .load(std::sync::atomic::Ordering::Relaxed),
                1,
                "content refusal must use the checked signal path"
            );
            assert!(subc_os::child_exit::observe(child.pid).unwrap().is_none());
            child.child = ordinary;
            let launch = crate::config_gate::program::capture(&spec, &[]).unwrap();
            let binding = content_binding::LiveProgramBinding::new(
                launch,
                &child,
                owner,
                subc_config::raw_digest(b"base"),
            )
            .unwrap();
            binding.kill_for_test();
            assert!(!timeout(Duration::from_secs(5), child.wait())
                .await
                .unwrap()
                .unwrap()
                .success());
            child.finish_exit(false).await.unwrap();
        }

        #[tokio::test]
        async fn adopted_watcher_observes_exit_and_reap_is_idempotent() {
            let (_scratch, mut child) = running_child().await;
            let recorded = recorded(child.pid, "sample");
            let ReconciledChild::Adopt(prepared) =
                reconcile_child(witness(&recorded), Some(&recorded)).unwrap()
            else {
                panic!("owned child should be verified");
            };
            let mut process = prepared.process;
            assert!(process.try_wait().unwrap().is_none());
            process.verified.signal(Signal::KILL).unwrap();
            assert!(!timeout(Duration::from_secs(5), process.wait())
                .await
                .unwrap()
                .unwrap()
                .success());
            assert!(subc_os::child_exit::observe(child.pid).unwrap().is_some());
            process.reap().unwrap();
            process.reap().unwrap();
            assert!(subc_os::child_exit::observe(child.pid).is_err());
            child.release_roster();
        }

        #[tokio::test]
        async fn held_child_is_left_unsignalled_and_resolved_only_after_its_exit() {
            let (_scratch, mut child) = running_child().await;
            let mut recorded = recorded(child.pid, "sample");
            recorded.child.executable = None;
            let ReconciledChild::Held(held) =
                reconcile_child(witness(&recorded), Some(&recorded)).unwrap()
            else {
                panic!("unverified child should be held");
            };
            assert!(!held.ending);
            assert!(subc_os::child_exit::observe(child.pid).unwrap().is_none());
            let child_watch = tokio::spawn(held.reap());
            child.start_kill().unwrap();
            timeout(Duration::from_secs(5), child_watch)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(subc_os::child_exit::observe(child.pid).is_err());
            child.release_roster();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
        async fn held_group_scan_waits_without_signalling_or_blocking_runtime() {
            use std::{os::unix::process::CommandExt, process::Stdio};
            let scratch = ScratchDir::new("held-group-empty-proof");
            let env = spec("sample", &scratch).env;
            let mut old = AdoptionTestChild(
                std::process::Command::new("/bin/sh")
                    .args(["-c", "read line"])
                    .envs(env.clone())
                    .process_group(0)
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut helper = std::process::Command::new("/bin/sleep")
                .arg("60")
                .envs(env)
                .process_group(i32::try_from(old.0.id()).unwrap())
                .spawn()
                .unwrap();
            let mut record = recorded(old.0.id(), "sample");
            record.child.executable = None;
            let ReconciledChild::Held(held) =
                reconcile_child(witness(&record), Some(&record)).unwrap()
            else {
                panic!("unverified group should be held");
            };
            assert!(!held.ending);
            drop(old.0.stdin.take());
            let pid = old.0.id();
            wait_until(|| subc_os::child_exit::observe(pid).unwrap().is_some()).await;
            let resolution = tokio::spawn(held.reap());
            let (served, served_rx) = oneshot::channel();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = served.send(());
            });
            timeout(Duration::from_secs(1), served_rx)
                .await
                .expect("one Tokio worker stayed responsive while the group scan waited")
                .unwrap();
            assert!(!resolution.is_finished());
            assert!(subc_os::child_exit::observe(pid).unwrap().is_some());
            assert!(subc_os::process_group::inspect(helper.id())
                .unwrap()
                .is_some_and(|member| !member.exited));
            helper.kill().unwrap();
            let helper_reaper = tokio::task::spawn_blocking(move || helper.wait());
            timeout(Duration::from_secs(5), resolution)
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            helper_reaper.await.unwrap().unwrap();
            assert!(subc_os::child_exit::observe(pid).is_err());
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn cgroup_kill_uses_the_fstat_checked_directory_not_its_replacement() {
            let scratch = ScratchDir::new("adopted-cgroup-pin");
            let path = scratch.join("group");
            let moved = scratch.join("moved");
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("cgroup.kill"), b"0").unwrap();
            std::fs::write(path.join("cgroup.events"), b"populated 1\n").unwrap();
            let target = CgroupTarget::capture(&path).unwrap();
            target
                .kill_with_leader(|_| {
                    std::fs::rename(&path, &moved).unwrap();
                    std::fs::create_dir(&path).unwrap();
                    std::fs::write(path.join("cgroup.kill"), b"0").unwrap();
                    true
                })
                .unwrap();
            assert_eq!(std::fs::read(moved.join("cgroup.kill")).unwrap(), b"1");
            assert_eq!(std::fs::read(path.join("cgroup.kill")).unwrap(), b"0");
            assert!(target.kill_with_leader(|_| true).is_err());
            assert_eq!(
                target.settle(true).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(std::fs::read(path.join("cgroup.kill")).unwrap(), b"0");
            std::fs::write(moved.join("cgroup.events"), b"populated 0\n").unwrap();
            target.settle(true).unwrap();
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn cgroup_settles_only_removal_observed_through_its_pinned_descriptor() {
            let scratch = ScratchDir::new("adopted-cgroup-removal");
            let path = scratch.join("group");
            std::fs::create_dir(&path).unwrap();
            std::fs::write(path.join("cgroup.events"), b"populated 1\n").unwrap();
            let pinned = CgroupTarget::capture(&path).unwrap();
            let identity = pinned.identity.clone();
            std::fs::remove_file(path.join("cgroup.events")).unwrap();
            std::fs::remove_dir(&path).unwrap();
            pinned.settle(false).unwrap();

            let missing = CgroupTarget::from_witness(path, identity);
            assert!(missing.directory.is_none());
            assert!(missing.settle(false).is_err());
        }

        #[tokio::test]
        async fn exited_adopted_leader_pins_the_group_through_final_signal() {
            use std::{
                io::{BufRead, BufReader},
                os::unix::process::CommandExt,
                process::Stdio,
            };
            let scratch = ScratchDir::new("adopted-group-pin");
            let env = spec("sample", &scratch).env;
            let mut old = AdoptionTestChild(
                std::process::Command::new("/bin/sh")
                    .args(["-c", "sleep 60 & echo $!; read line"])
                    .envs(env)
                    .process_group(0)
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut helper = String::new();
            BufReader::new(old.0.stdout.take().unwrap())
                .read_line(&mut helper)
                .unwrap();
            let helper: u32 = helper.trim().parse().unwrap();
            let record = recorded(old.0.id(), "sample");
            let ReconciledChild::Adopt(child) =
                reconcile_child(witness(&record), Some(&record)).unwrap()
            else {
                panic!("group was not adopted");
            };
            let mut process = child.process;
            drop(old.0.stdin.take());
            timeout(Duration::from_secs(5), process.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(subc_os::process_group::inspect(helper)
                .unwrap()
                .is_some_and(|member| !member.exited));
            process.reap().unwrap();
            assert!(
                subc_os::process_group::inspect(helper).unwrap().is_none(),
                "reap returned before the adopted group helper was reaped"
            );
        }

        #[tokio::test]
        async fn gone_group_leader_stays_held_without_container_proof() {
            let (_scratch, mut child) = running_child().await;
            let record = recorded(child.pid, "sample");
            child.start_kill().unwrap();
            child.wait().await.unwrap();
            child.finish_exit(false).await.unwrap();
            assert!(matches!(
                reconcile_child(witness(&record), Some(&record)).unwrap(),
                ReconciledChild::Held(_)
            ));
            let mut reused = recorded(std::process::id(), "sample");
            reused.child.start_time = reused.child.start_time.map(|start| start + 1);
            assert!(matches!(
                reconcile_child(witness(&reused), Some(&reused)).unwrap(),
                ReconciledChild::Exited
            ));
        }

        async fn wait_until(mut condition: impl FnMut() -> bool) {
            timeout(Duration::from_secs(5), async {
                while !condition() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("upgrade condition did not hold");
        }

        async fn install_test_adoptee(
            wait_test: TestWait,
            invalid_image: bool,
            sample: Option<KillIdentitySample>,
        ) -> (ScratchDir, AdoptionTestChild, SupervisorHandle, Supervisor) {
            use std::{io::Read, os::unix::process::CommandExt, process::Stdio};
            let scratch = ScratchDir::new("upgrade-held-child");
            let id = "sample";
            let spec = spec(id, &scratch);
            let mut command = if sample.is_some() {
                let mut command = std::process::Command::new("/bin/sh");
                // Stay alive through TERM and notify the test after installing
                // the trap. Closing stdin then gives the test control of exit.
                command
                    .args(["-c", "trap '' TERM; printf r; read line"])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped());
                command
            } else {
                let mut command = std::process::Command::new(&spec.program);
                command.args(&spec.args);
                command
            };
            let mut old = AdoptionTestChild(
                command
                    .envs(spec.env.iter().cloned())
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
            if sample.is_some() {
                let mut ready = [0];
                old.0.stdout.take().unwrap().read_exact(&mut ready).unwrap();
                assert_eq!(ready, *b"r");
            }
            let record = recorded(old.0.id(), id);
            let ReconciledChild::Adopt(mut prepared) =
                reconcile_child(witness(&record), Some(&record)).unwrap()
            else {
                panic!("child was not adopted");
            };
            prepared.process.wait_test = wait_test;
            *prepared
                .process
                .verified
                .kill_identity_sample
                .lock()
                .unwrap() = sample;
            if invalid_image {
                Arc::get_mut(&mut prepared.process.verified)
                    .unwrap()
                    .recorded
                    .executable = None;
            }
            let path = scratch.join("live-children.json");
            live_children::write_record_with_reasons(&path, &[record], BTreeMap::new()).unwrap();
            let handle = SupervisorHandle::new();
            handle.begin_resume_report("attempt".into(), 1, [id.to_owned()]);
            let supervisor =
                Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                    .with_handle(handle.clone());
            supervisor.child_roster.record_to_with_adopted(
                path,
                &live_children::AdoptedPids::from_classes([old.0.id()], []),
            );
            let registered = supervisor
                .register_configured_with_health(
                    spec,
                    true,
                    HealthConfig::default(),
                    Some(10),
                    RestartPolicy::default(),
                    Duration::from_secs(1),
                )
                .unwrap();
            supervisor
                .start_adopted_after_upgrade(
                    registered,
                    *prepared,
                    [None, None],
                    [1; 16],
                    Instant::now() + Duration::from_secs(60),
                    true,
                )
                .await
                .unwrap();
            (scratch, old, handle, supervisor)
        }

        #[tokio::test]
        async fn refused_adoptee_stop_fences_start_until_held_child_exits() {
            let (_scratch, mut old, handle, supervisor) =
                install_test_adoptee(TestWait::Normal, true, None).await;
            let id = "sample";
            let pid = old.0.id();
            handle
                .pending_adoptions
                .remove_pending(id, [1; 16])
                .unwrap()
                .accepted
                .send(None)
                .unwrap();
            wait_until(|| handle.upgrade_state.disposition(id) == Some(Disposition::Resumed)).await;
            let module = handle.get(id).unwrap();
            let error = module.stop().await.unwrap_err();
            assert!(matches!(error, SuperviseError::Kill { .. }));
            assert!(
                handle.lifecycle_operation(Some(id)).await.is_err(),
                "a following start must be refused"
            );
            assert_eq!(supervisor.child_roster.previous_instance_pid(id), Some(pid));
            let generation = module.status().unwrap().spawn_generation;
            let _ = module.set_enabled(true).await;
            assert_eq!(module.status().unwrap().spawn_generation, generation);
            assert!(subc_os::child_exit::observe(pid).unwrap().is_none());
            old.0.kill().unwrap();
            wait_until(|| !handle.adoption_blocks(id)).await;
            assert_eq!(
                handle.upgrade_state.disposition(id),
                Some(Disposition::Stopped)
            );
            assert!(subc_os::child_exit::observe(pid).is_err());
        }

        #[tokio::test]
        async fn pending_watch_error_transfers_to_a_held_observer() {
            let (_scratch, mut old, handle, supervisor) =
                install_test_adoptee(TestWait::Error, false, None).await;
            let id = "sample";
            let pid = old.0.id();
            wait_until(|| handle.upgrade_state.disposition(id) == Some(Disposition::Blocked)).await;
            assert_eq!(supervisor.child_roster.previous_instance_pid(id), Some(pid));
            old.0.kill().unwrap();
            let module = handle.get(id).unwrap();
            wait_until(|| module.status().unwrap().pid.is_some_and(|new| new != pid)).await;
            assert_eq!(
                handle.upgrade_state.disposition(id),
                Some(Disposition::Restarted)
            );
            assert!(subc_os::child_exit::observe(pid).is_err());
            module.stop().await.unwrap();
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn upgrade_returns_stuck_without_dropping_the_child_after_kill_grace() {
            let (observed_alive, checked) = oneshot::channel();
            let (proceed, sample_identity) = std::sync::mpsc::channel();
            let (_scratch, mut old, handle, supervisor) = install_test_adoptee(
                TestWait::Stuck,
                false,
                Some(KillIdentitySample {
                    observed_alive,
                    sample_identity,
                }),
            )
            .await;
            let pid = old.0.id();
            let exit = async_watch(pid).unwrap();
            let id = "sample";
            handle
                .pending_adoptions
                .remove_pending(id, [1; 16])
                .unwrap()
                .accepted
                .send(None)
                .unwrap();
            wait_until(|| handle.upgrade_state.disposition(id) == Some(Disposition::Resumed)).await;
            let attempt = handle.begin_upgrade().unwrap();
            let stop = tokio::spawn(async move {
                let result = attempt.stop_restarting(vec![id.into()]).await;
                (attempt, result)
            });
            timeout(Duration::from_secs(5), checked)
                .await
                .unwrap()
                .unwrap();
            drop(old.0.stdin.take());
            let _ready = timeout(Duration::from_secs(5), exit.readable())
                .await
                .unwrap()
                .unwrap();
            assert!(subc_os::child_exit::observe(pid).unwrap().is_some());
            proceed.send(()).unwrap();
            let (attempt, result) =
                timeout(drain::UPGRADE_KILL_GRACE + Duration::from_secs(1), stop)
                    .await
                    .unwrap()
                    .unwrap();
            let error = result[id]
                .as_ref()
                .expect_err("a stuck child must prevent exec");
            assert!(
                error.to_string().contains("child remains owned"),
                "expected the owned-child kill-grace timeout, got: {error} ({error:?})"
            );
            assert_eq!(supervisor.child_roster.live_count_for_test(), 1);
            assert!(handle.upgrade_state.disposition(id) == Some(Disposition::Resumed));
            drop(attempt);
        }

        #[tokio::test]
        async fn wake_pair_emfile_keeps_ordinary_pipes_capturing_without_park() {
            let scratch = ScratchDir::new("capture-emfile");
            let path = scratch.join("capture.log");
            let sink = ChildOutputSink::open(&path, cortexkit_log::Retention::default()).unwrap();
            let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
            let (stdout, mut out) = std::io::pipe().unwrap();
            let (stderr, mut err) = std::io::pipe().unwrap();
            child_output::fail_next_pair_for_test();
            let pumps = child_output::resume(
                [
                    Some((stdout.into(), Vec::new())),
                    Some((stderr.into(), Vec::new())),
                ],
                sink,
                &ring,
                "sample",
                false,
            );
            assert!(pumps.stop.is_none());
            out.write_all(b"ordinary stdout\n").unwrap();
            err.write_all(b"ordinary stderr\n").unwrap();
            drop((out, err));
            wait_for_log(&path, "ordinary stdout").await;
            wait_for_log(&path, "ordinary stderr").await;
        }

        #[tokio::test]
        async fn unchanged_configured_true_module_stays_stopped_in_handover() {
            let scratch = ScratchDir::new("upgrade-stopped");
            let handle = SupervisorHandle::new();
            let supervisor =
                Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                    .with_handle(handle.clone());
            let module = supervisor
                .supervise_configured(spec("sample", &scratch), true)
                .unwrap();
            module.set_enabled(false).await.unwrap();
            let closed = supervisor
                .supervise_configured(spec("closed", &scratch), true)
                .unwrap();
            closed.stop().await.unwrap();
            wait_until(|| closed.inner.commands.is_closed()).await;
            let attempt = handle.begin_upgrade().unwrap();
            let selected = attempt.select().await.unwrap();
            assert_eq!(selected.stopped, ["closed", "sample"]);
            assert_eq!(selected.config_enabled.get("sample"), Some(&true));
            let cohort = handover::Cohort {
                carried: selected.carried.into_keys().collect(),
                restarting: selected.restarting,
                stopped: selected.stopped,
                config_enabled: selected.config_enabled,
            };
            assert!(!cohort.allow_start("sample", Some(true)));
            assert!(!cohort.allow_start("closed", Some(true)));
        }

        #[tokio::test]
        async fn upgrade_barrier_serializes_background_dispatch_in_both_directions() {
            let handle = SupervisorHandle::new();
            let already_dispatched = handle.upgrade_state.background_work().unwrap();
            assert!(matches!(
                handle.begin_upgrade(),
                Err(UpgradeBusy::ModuleLifecycle)
            ));
            drop(already_dispatched);
            let attempt = handle.begin_upgrade().unwrap();
            assert!(handle.upgrade_state.background_work().is_none());
            drop(attempt);
            assert!(handle.upgrade_state.background_work().is_some());
        }

        #[tokio::test]
        async fn queued_registration_config_work_holds_the_upgrade_barrier() {
            let handle = SupervisorHandle::new();
            let (started, started_rx) = oneshot::channel();
            let (release, release_rx) = oneshot::channel();
            let (finished, finished_rx) = oneshot::channel();
            assert!(crate::supervise::config_delivery::dispatch_registered_work(
                &handle.upgrade_state,
                async move {
                    let _ = started.send(());
                    let _ = release_rx.await;
                    let _ = finished.send(());
                }
            ));
            started_rx.await.unwrap();
            assert!(matches!(
                handle.begin_upgrade(),
                Err(UpgradeBusy::ModuleLifecycle)
            ));
            release.send(()).unwrap();
            finished_rx.await.unwrap();
            assert!(handle.begin_upgrade().is_ok());
        }

        #[tokio::test]
        async fn select_waiting_behind_exit_publication_keeps_its_reply() {
            let scratch = ScratchDir::new("upgrade-exit-publication");
            let handle = SupervisorHandle::new();
            let registry = Arc::new(Registry::default());
            let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
                .with_handle(handle.clone());
            let attempt = handle.begin_upgrade().unwrap();
            let (reply, received) = oneshot::channel();
            let mut deferred = VecDeque::new();
            let request = SupervisorCommand::Upgrade {
                key: attempt.key.clone(),
                request: ActorRequest::Select(reply),
            };
            assert!(super::super::defer_exit_publication_command(request, &mut deferred).is_none());
            super::super::paused_command(
                deferred.pop_front().unwrap(),
                &attempt.key,
                &spec("sample", &scratch),
                &supervisor.runtime_config(),
                &registry,
                &supervisor.process_liveness,
                &Arc::new(Mutex::new(SupervisorSnapshot::disabled())),
                &mut None,
                &mut deferred,
            )
            .await;
            assert!(matches!(
                received.await.unwrap().unwrap(),
                Some(Selection::Stopped)
            ));
        }

        fn wire_router(
            registry: &Arc<Registry>,
            handle: &SupervisorHandle,
        ) -> (
            Arc<crate::Router>,
            crate::server::ServerAuth,
            subc_transport::ConnectionInfo,
        ) {
            let router = Arc::new(crate::Router::with_control_handler(Arc::new(
                crate::ControlHandler::new(registry.clone()).with_supervisor(handle.clone()),
            )));
            let info = subc_transport::ConnectionInfo {
                schema: subc_transport::SCHEMA_VERSION,
                wire_version: Some(subc_protocol::PROTOCOL_VERSION),
                endpoints: vec![],
                key: subc_transport::generate_key().unwrap(),
                daemon_id: subc_transport::generate_daemon_id().unwrap(),
                pid: std::process::id(),
                daemon_ver: "test".into(),
            };
            let auth = crate::server::ServerAuth::new(info.key.clone(), info.daemon_id, "test");
            (router, auth, info)
        }

        async fn wire_registration(
            router: &Arc<crate::Router>,
            auth: &crate::server::ServerAuth,
            info: &subc_transport::ConnectionInfo,
            launch: Arc<RegistrationSpawn>,
        ) -> tokio::net::UnixStream {
            launch.set_wire_deadline_for_test(Instant::now() + Duration::from_secs(5));
            let (server, mut peer) = tokio::net::UnixStream::pair().unwrap();
            let router = router.clone();
            let auth = auth.clone();
            tokio::spawn(async move {
                let _ = crate::server::handle_connection_with_origin(
                    server,
                    router,
                    auth,
                    crate::server::LocalOrigin::RegistrationChannel(launch),
                )
                .await;
            });
            subc_transport::authenticate_client(&mut peer, info, Duration::from_secs(5))
                .await
                .unwrap();
            peer
        }

        async fn offer_hello(
            peer: &mut tokio::net::UnixStream,
            id: &str,
            token: Option<&str>,
        ) -> Option<Frame> {
            let manifest = subc_protocol::manifest::ModuleManifest::builder(id, "1.0.0").build();
            let mut hello = serde_json::json!({"protocol_ver": subc_protocol::PROTOCOL_VERSION,
                "manifest": manifest, "module_capabilities": ["live-upgrade/v1"]});
            if let Some(token) = token {
                hello["resume_token"] = serde_json::json!(token);
            }
            let frame = Frame::build(
                subc_protocol::FrameType::Hello,
                control_flags(),
                0,
                0,
                19,
                serde_json::to_vec(&hello).unwrap(),
            )
            .unwrap();
            subc_transport::write_frame(peer, &frame).await.unwrap();
            timeout(Duration::from_secs(5), subc_transport::read_frame(peer))
                .await
                .unwrap()
                .unwrap()
        }

        async fn wire_hello(
            peer: &mut tokio::net::UnixStream,
            id: &str,
            token: Option<&str>,
        ) -> Frame {
            offer_hello(peer, id, token)
                .await
                .expect("the authorized HELLO must receive an answer")
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn only_the_ticketed_replacement_can_hello_before_spawn_returns() {
            use subc_protocol::{ErrorBody, FrameType};
            let scratch = ScratchDir::new("upgrade-fast-hello");
            let id = "sample";
            let registry = Arc::new(Registry::default());
            let handle = SupervisorHandle::new();
            handle.begin_resume_report("attempt".into(), 1, [id.to_owned()]);
            handle.capture_registration_channels_for_test();
            let (router, auth, info) = wire_router(&registry, &handle);
            let mut supervisor =
                Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
                    .with_handle(handle.clone());
            let (arrived, before_return) = oneshot::channel();
            let (release, unblock) = std::sync::mpsc::channel();
            let gate = Mutex::new(Some((arrived, unblock)));
            supervisor.test_after_first_spawn =
                spawn::AfterFirstSpawnHook(Some(Arc::new(move |_| {
                    let (arrived, unblock) = gate.lock().unwrap().take().unwrap();
                    arrived.send(()).unwrap();
                    unblock.recv_timeout(Duration::from_secs(10)).unwrap();
                })));
            let mut spec = spec(id, &scratch);
            spec.protocol = ModuleProtocol::Subc;
            let registered = supervisor
                .register_configured_with_health(
                    spec,
                    false,
                    HealthConfig::default(),
                    Some(0),
                    RestartPolicy::default(),
                    Duration::from_secs(3),
                )
                .unwrap();
            let launched = tokio::spawn(async move {
                supervisor
                    .start_planned_after_upgrade(id, Some(registered), true)
                    .await
            });
            timeout(Duration::from_secs(5), before_return)
                .await
                .unwrap()
                .unwrap();
            assert!(!launched.is_finished());
            assert_eq!(
                handle.upgrade_state.disposition(id),
                Some(Disposition::Replacing)
            );
            let other = RegistrationSpawn::new(&handle, id, SpawnRole::Plain, None);
            other.ready();
            let mut offered = wire_registration(&router, &auth, &info, other).await;
            // A refused inherited channel can retire before its ERROR reaches
            // the writer. It must not publish a registration in either case.
            if let Some(refusal) = offer_hello(&mut offered, id, None).await {
                assert_eq!(refusal.header.ty, FrameType::Error);
                assert_eq!(
                    serde_json::from_slice::<ErrorBody>(&refusal.body)
                        .unwrap()
                        .code,
                    "reserved_module"
                );
            }
            assert!(registry.get_module(id).unwrap().is_none());
            let launch = handle
                .registration_for_test(id)
                .expect("spawn prepared its registered launch");
            // The fixture pauses before starting the actor, which normally
            // confirms a macOS launch image and opens its registration latch.
            launch.ready();
            let mut replacement = wire_registration(&router, &auth, &info, launch).await;
            assert_eq!(
                wire_hello(&mut replacement, id, None).await.header.ty,
                FrameType::HelloAck
            );
            release.send(()).unwrap();
            launched.await.unwrap().unwrap();
            handle.get(id).unwrap().stop().await.unwrap();
        }

        #[tokio::test]
        #[ignore = "subprocess for the promoted-child resume test"]
        async fn promoted_resume_wire_child() {
            let dir = std::env::current_dir().unwrap();
            if !dir.join("promoted-resume-fixture").is_file() {
                return;
            }
            std::fs::write(dir.join("started"), []).unwrap();
            wait_until(|| dir.join("resume").is_file()).await;
            let (info, token): (subc_transport::ConnectionInfo, String) =
                serde_json::from_slice(&std::fs::read(dir.join("resume")).unwrap()).unwrap();
            let mut peer = tokio::net::UnixStream::connect(dir.join("s"))
                .await
                .unwrap();
            subc_transport::authenticate_client(&mut peer, &info, Duration::from_secs(5))
                .await
                .unwrap();
            assert_eq!(
                wire_hello(&mut peer, "sample", Some(&token))
                    .await
                    .header
                    .ty,
                subc_protocol::FrameType::HelloAck
            );
            std::fs::write(dir.join("ack"), b"ok").unwrap();
            wait_until(|| dir.join("finish").is_file()).await;
        }

        #[tokio::test]
        async fn a_promoted_candidate_is_carried_as_primary_and_resumes_after_retirement() {
            use crate::{local_listener::BoundLocalListener, server::serve_local_listener};
            use subc_protocol::{FrameType, ModuleHelloAckBody};
            let scratch = ScratchDir::new("pu");
            let id = "sample";
            let old_handle = SupervisorHandle::new();
            let old_registry = Arc::new(Registry::default());
            let (router, auth, info) = wire_router(&old_registry, &old_handle);
            let old_supervisor =
                Supervisor::new_for_test(old_registry.clone(), RestartPolicy::default())
                    .with_handle(old_handle.clone());
            let mut helper = spec(id, &scratch);
            std::fs::write(scratch.join("promoted-resume-fixture"), []).unwrap();
            helper.program = "/bin/sh".into();
            // Only the child enters the scratch folder. The shell execs the
            // helper in place, retaining the candidate's pid and process group.
            helper.args = vec![
                "-c".into(),
                r#"cd "$1" && exec "$2" --ignored --exact "$3""#.into(),
                "promoted-resume-fixture".into(),
                scratch.path().display().to_string(),
                std::env::current_exe().unwrap().display().to_string(),
                "supervise::live_upgrade::unix::tests::promoted_resume_wire_child".into(),
            ];
            let mut child = spawn_child_async(&helper, &old_supervisor.runtime_config())
                .await
                .unwrap();
            #[cfg(target_os = "macos")]
            child.confirm_privacy_exec().await;
            wait_until(|| scratch.join("started").is_file()).await;
            let authenticated = recorded(child.pid, id);
            let incumbent = RegistrationSpawn::new(&old_handle, id, SpawnRole::Plain, None);
            incumbent.ready();
            let mut incumbent_peer =
                wire_registration(&router, &auth, &info, incumbent.clone()).await;
            assert_eq!(
                wire_hello(&mut incumbent_peer, id, None).await.header.ty,
                FrameType::HelloAck
            );
            old_handle.open_swap(id, String::new());
            let candidate = RegistrationSpawn::new(&old_handle, id, SpawnRole::SwapCandidate, None);
            let old_path = scratch.join("old-roster");
            let old_roster = ChildRoster::default();
            old_roster.record_to(old_path.clone());
            let token_guard = old_roster
                .admit(
                    id.into(),
                    child.pid,
                    ModuleProtocol::Subc,
                    authenticated.child.start_time,
                    crate::child_roster::RecordedIdentity {
                        start_time: authenticated.child.start_time,
                        executable: authenticated.child.executable,
                        ..Default::default()
                    },
                )
                .unwrap();
            candidate
                .resume_token_record
                .set(token_guard.resume_token_record().unwrap())
                .unwrap();
            candidate.ready();
            let mut candidate_peer =
                wire_registration(&router, &auth, &info, candidate.clone()).await;
            let ack = wire_hello(&mut candidate_peer, id, None).await;
            assert_eq!(ack.header.ty, FrameType::HelloAck);
            let token = serde_json::from_slice::<ModuleHelloAckBody>(&ack.body)
                .unwrap()
                .resume_token
                .unwrap();
            old_registry.promote_candidate(id).unwrap().unwrap();
            old_handle.promote_swap_nonce(id, false);
            incumbent.retire();
            drop(incumbent_peer);
            old_handle.close_swap(id);
            child.registration_spawn = Some(candidate);
            let carried = select_child(&child, &old_registry)
                .expect("a promoted, running child remains carryable");
            assert_eq!(
                carried.registration_spawn_kind,
                RegistrationSpawnKind::Plain
            );
            let row = live_children::read_record_entries(&old_path)
                .unwrap()
                .0
                .remove(0);
            drop(candidate_peer);

            let ReconciledChild::Adopt(prepared) = reconcile_child(carried, Some(&row)).unwrap()
            else {
                panic!("promoted child should be adopted");
            };
            let roster_path = scratch.join("roster");
            live_children::write_record_with_reasons(&roster_path, &[row], BTreeMap::new())
                .unwrap();
            let registry = Arc::new(Registry::default());
            let handle = SupervisorHandle::new();
            handle.begin_resume_report("attempt".into(), 1, [id.into()]);
            let supervisor = Supervisor::new_for_test(registry.clone(), RestartPolicy::default())
                .with_handle(handle.clone());
            supervisor.child_roster.record_to_with_adopted(
                roster_path,
                &live_children::AdoptedPids::from_classes([child.pid], []),
            );
            helper.protocol = ModuleProtocol::Subc;
            let registered = supervisor
                .register_configured_with_health(
                    helper,
                    true,
                    HealthConfig::default(),
                    Some(0),
                    RestartPolicy::default(),
                    Duration::from_secs(1),
                )
                .unwrap();
            supervisor
                .start_adopted_after_upgrade(
                    registered,
                    *prepared,
                    [None, None],
                    [1; 16],
                    Instant::now() + Duration::from_secs(5),
                    true,
                )
                .await
                .unwrap();
            let (router, auth, info) = wire_router(&registry, &handle);
            let path = scratch.join("s");
            let listener = BoundLocalListener::Unix {
                listener: std::os::unix::net::UnixListener::bind(&path).unwrap(),
                path,
            };
            let listener = tokio::spawn(serve_local_listener(listener, router, auth));
            let staged = scratch.join("resume-staged");
            std::fs::write(&staged, serde_json::to_vec(&(info, token)).unwrap()).unwrap();
            std::fs::rename(staged, scratch.join("resume")).unwrap();
            wait_until(|| scratch.join("ack").is_file()).await;
            assert!(registry.get_module(id).unwrap().is_some());
            assert_eq!(
                handle.get(id).unwrap().status().unwrap().pid,
                Some(child.pid)
            );
            std::fs::write(scratch.join("finish"), []).unwrap();
            listener.abort();
        }

        #[tokio::test]
        async fn an_adopted_child_misses_its_carried_deadline_and_restarts_without_a_charge() {
            use std::os::unix::process::CommandExt;

            let _trace = tracing::subscriber::set_default(
                tracing_subscriber::fmt()
                    .with_test_writer()
                    .with_max_level(tracing::Level::DEBUG)
                    .finish(),
            );
            let scratch = ScratchDir::new("upgrade-missed-deadline");
            let id = "sample";
            let spec = spec(id, &scratch);
            // A standard child owns no async reaper. The adopted monitor is
            // the only waiter and must claim the exit before starting another.
            let mut old = std::process::Command::new(&spec.program);
            let old = AdoptionTestChild(
                old.args(&spec.args)
                    .envs(spec.env.iter().cloned())
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
            let old_pid = old.0.id();
            let recorded = recorded(old_pid, id);
            let ReconciledChild::Adopt(prepared) =
                reconcile_child(witness(&recorded), Some(&recorded)).unwrap()
            else {
                panic!("expected verified adoption");
            };
            let verified = prepared.process.verified.clone();

            let path = scratch.join("live-children.json");
            live_children::write_record_with_reasons(&path, &[recorded], BTreeMap::new()).unwrap();
            let handle = SupervisorHandle::new();
            handle.begin_resume_report("attempt".into(), 1, [id.to_owned()]);
            let restart = RestartPolicy::new(0, Duration::from_secs(60));
            let supervisor = Supervisor::new_for_test(Arc::new(Registry::default()), restart)
                .with_handle(handle.clone());
            supervisor.child_roster.record_to_with_adopted(
                path,
                &live_children::AdoptedPids::from_classes([old_pid], []),
            );
            let registered = supervisor
                .register_configured_with_health(
                    spec,
                    false,
                    HealthConfig::default(),
                    None,
                    restart,
                    Duration::from_secs(60),
                )
                .unwrap();
            let deadline = Instant::now() + Duration::from_secs(1);
            supervisor
                .start_adopted_after_upgrade(
                    registered,
                    *prepared,
                    [None, None],
                    [1; 16],
                    deadline,
                    true,
                )
                .await
                .unwrap();
            assert_eq!(handle.get(id).unwrap().status().unwrap().pid, Some(old_pid));
            assert!(handle.adoption_blocks(id));
            assert_eq!(
                handle
                    .lifecycle_operation(Some(id))
                    .await
                    .unwrap_err()
                    .body()
                    .detail
                    .unwrap()["reason"],
                "daemon_upgrade"
            );
            assert!(matches!(handle.begin_upgrade(), Err(UpgradeBusy::Upgrade)));
            let module = handle.get(id).unwrap();
            timeout(Duration::from_secs(5), async {
                // macOS reports the replacement pid only after the launch
                // helper confirms it has executed the module image.
                while handle.adoption_blocks(id)
                    || module
                        .status()
                        .unwrap()
                        .pid
                        .is_none_or(|pid| pid == old_pid)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "replacement not reportable: {:?}; prior child: {:?}; upgrade: {:?}; stop refusal: {:?}",
                    module.status(),
                    subc_os::child_exit::observe(old_pid),
                    handle.resume_report(),
                    verified.signal_refusal()
                )
            });
            assert!(Instant::now() >= deadline);
            assert!(subc_os::child_exit::observe(old_pid).is_err());
            let status = module.status().unwrap();
            assert!(status.pid.is_some_and(|pid| pid != old_pid));
            assert_eq!(status.restart_count, 0);
            assert_eq!(
                handle.resume_report().unwrap()["modules"][0]["disposition"],
                "restarted"
            );
            module.stop().await.unwrap();
        }

        #[tokio::test(start_paused = true)]
        async fn failed_pending_replacement_is_reported_as_stopped_not_restarted() {
            use std::os::unix::process::CommandExt;
            let scratch = ScratchDir::new("upgrade-failed-replacement");
            let id = "sample";
            let launch = spec(id, &scratch);
            let old = AdoptionTestChild(
                std::process::Command::new(&launch.program)
                    .args(&launch.args)
                    .envs(launch.env.iter().cloned())
                    .process_group(0)
                    .spawn()
                    .unwrap(),
            );
            let old_pid = old.0.id();
            let record = recorded(old_pid, id);
            let ReconciledChild::Adopt(mut prepared) =
                reconcile_child(witness(&record), Some(&record)).unwrap()
            else {
                panic!("expected verified adoption");
            };
            let verified = prepared.process.verified.clone();
            let (release, held) = watch::channel(false);
            prepared.process.wait_test = TestWait::Held(held);
            let path = scratch.join("live-children.json");
            live_children::write_record_with_reasons(&path, &[record], BTreeMap::new()).unwrap();
            let handle = SupervisorHandle::new();
            handle.begin_resume_report("attempt".into(), 1, [id.to_owned()]);
            let supervisor =
                Supervisor::new_for_test(Arc::new(Registry::default()), RestartPolicy::default())
                    .with_handle(handle.clone());
            supervisor.child_roster.record_to_with_adopted(
                path,
                &live_children::AdoptedPids::from_classes([old_pid], []),
            );
            let mut replacement = spec(id, &scratch);
            replacement.program = scratch.join("missing-module");
            let registered = supervisor
                .register_configured_with_health(
                    replacement,
                    false,
                    HealthConfig::default(),
                    None,
                    RestartPolicy::default(),
                    Duration::from_secs(60),
                )
                .unwrap();
            supervisor
                .start_adopted_after_upgrade(
                    registered,
                    *prepared,
                    [None, None],
                    [1; 16],
                    Instant::now() + Duration::from_millis(50),
                    true,
                )
                .await
                .unwrap();
            let module = handle.get(id).unwrap();
            let reported_stopped = async {
                loop {
                    let changed = handle.upgrade_state.0.disposition_changed.notified();
                    if handle.upgrade_state.disposition(id) == Some(Disposition::Stopped) {
                        return;
                    }
                    changed.await;
                }
            };
            tokio::pin!(reported_stopped);
            // Expiring the reconnect window sends SIGKILL, but the supervisor
            // cannot start the replacement until it observes and reaps the
            // exit. Hold the observation past the old five-second test budget.
            assert!(timeout(Duration::from_secs(5), &mut reported_stopped)
                .await
                .is_err());
            assert_eq!(
                verified
                    .signal_attempts
                    .load(std::sync::atomic::Ordering::Relaxed),
                1
            );
            assert_eq!(verified.signal_refusal(), None);
            assert_eq!(
                handle.upgrade_state.disposition(id),
                Some(Disposition::Pending)
            );

            // Reaping and launching use real OS events; stop advancing the
            // virtual clock and let the report notification drive the check.
            tokio::time::resume();
            release.send_replace(true);
            timeout(Duration::from_secs(30), reported_stopped)
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "replacement never settled: {:?}; child: {:?}; signal refusal: {:?}",
                        handle.resume_report(),
                        subc_os::child_exit::observe(old_pid),
                        verified.signal_refusal()
                    );
                });
            assert!(subc_os::child_exit::observe(old_pid).is_err());
            assert!(module.status().unwrap().pid.is_none());
            assert_eq!(module.status().unwrap().state, ModuleState::Failed);
            assert!(!handle.adoption_blocks(id));
            assert_eq!(
                handle.resume_report().unwrap()["modules"][0]["disposition"],
                "stopped"
            );
        }

        async fn wait_for_log(path: &Path, line: &str) {
            timeout(Duration::from_secs(5), async {
                loop {
                    if std::fs::read_to_string(path).is_ok_and(|log| log.contains(line)) {
                        return;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }

        #[tokio::test]
        async fn park_preserves_an_open_tail_and_never_replays_an_ended_stream() {
            for unwind in [true, false] {
                let scratch = ScratchDir::new("upgrade-capture");
                let path = scratch.join("capture.log");
                let sink =
                    ChildOutputSink::open(&path, cortexkit_log::Retention::default()).unwrap();
                let ring = Arc::new(Mutex::new(StderrRing::new(StderrTailConfig::default())));
                let (stdout, mut stdout_writer) = std::io::pipe().unwrap();
                let (stderr, mut stderr_writer) = std::io::pipe().unwrap();
                let pumps = child_output::resume(
                    [
                        Some((stdout.into(), Vec::new())),
                        Some((stderr.into(), Vec::new())),
                    ],
                    sink.clone(),
                    &ring,
                    "sample",
                    true,
                );
                stdout_writer.write_all(b"queued on stdout\nleft-").unwrap();
                stderr_writer
                    .write_all(b"queued on stderr\nclosed-suffix")
                    .unwrap();
                drop(stderr_writer);
                wait_for_log(&path, "queued on stdout").await;
                wait_for_log(&path, "closed-suffix").await;
                let park = pumps.stop.as_ref().unwrap().park().unwrap();
                let [stdout, stderr] = timeout(Duration::from_secs(2), park.streams())
                    .await
                    .unwrap()
                    .unwrap();
                assert!(stderr.is_none());
                let stdout = stdout.unwrap();
                assert_eq!(stdout.tail, b"left-");
                stdout_writer.write_all(b"right\n").unwrap();
                let resumed = if unwind {
                    drop(park);
                    pumps
                } else {
                    let fd = rustix::io::fcntl_dupfd_cloexec(&stdout.fd, 3).unwrap();
                    drop(pumps);
                    drop(park);
                    child_output::resume(
                        [Some((fd, stdout.tail.clone())), None],
                        sink,
                        &ring,
                        "sample",
                        true,
                    )
                };
                drop(stdout_writer);
                timeout(Duration::from_secs(2), resumed.stdout.unwrap())
                    .await
                    .unwrap()
                    .unwrap();
                let log = std::fs::read_to_string(path).unwrap();
                for line in [
                    "left-right",
                    "queued on stdout",
                    "queued on stderr",
                    "closed-suffix",
                ] {
                    assert_eq!(log.matches(line).count(), 1, "{line}: {log}");
                }
            }
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[allow(unused_imports)] // Bootstrap and the upgrade control operation use these entry points.
pub(crate) use unix::*;
