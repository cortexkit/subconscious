//! Coordinate carried forks without allowing module code before durable identity.

use super::*;
use crate::child_roster::{RecordedIdentity, RosterGuard};
use std::sync::{mpsc, Condvar};
use subc_os::admission_gate::{AdmissionGate, PlacementBarrier};

// Disk pressure can make fsync slow. A generous bound avoids failing an otherwise
// healthy start; only the few opted-in modules queue behind this spawn permit.
const ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);
static SPAWN_PERMIT: (Mutex<bool>, Condvar) = (Mutex::new(false), Condvar::new());

struct SpawnPermit;
impl SpawnPermit {
    fn acquire(mut waiting: impl FnMut() -> io::Result<()>) -> io::Result<Self> {
        let mut held = SPAWN_PERMIT.0.lock().unwrap_or_else(|p| p.into_inner());
        while *held {
            waiting()?;
            held = SPAWN_PERMIT.1.wait(held).unwrap_or_else(|p| p.into_inner());
        }
        *held = true;
        Ok(Self)
    }
}
impl Drop for SpawnPermit {
    fn drop(&mut self) {
        *SPAWN_PERMIT.0.lock().unwrap_or_else(|p| p.into_inner()) = false;
        SPAWN_PERMIT.1.notify_one();
    }
}

pub(super) struct Gate {
    permission: Option<AdmissionGate>,
    placement: Option<PlacementBarrier>,
    permit: Option<SpawnPermit>,
}

pub(super) struct Admitted {
    pub(super) leader: Option<RosterGuard>,
    pub(super) birth: u64,
    identified: bool,
    pid: Option<u32>,
    container: Option<Arc<containment::CarriedContainment>>,
    gate: Option<AdmissionGate>,
    spawning: Option<std::thread::JoinHandle<io::Result<Child>>>,
    permit: Option<SpawnPermit>,
    records: Option<mpsc::Receiver<Packet>>,
    packet: Option<Packet>,
    proof: ReapProof,
    roster: ChildRoster,
    module_id: String,
    timed_out: bool,
    armed: bool,
}

impl Admitted {
    pub(super) fn rejection_target(&self) -> containment::GroupTarget {
        let container = self
            .container
            .as_ref()
            .expect("reported carried child has a container");
        container.hard_kill();
        self.group_target()
    }

    pub(super) fn group_target(&self) -> containment::GroupTarget {
        let container = self
            .container
            .as_ref()
            .expect("reported carried child has a container");
        if container.group().is_some() {
            containment::GroupTarget::Pinned(Arc::clone(container))
        } else {
            containment::GroupTarget::NoGroup
        }
    }
    pub(super) fn disarm(mut self) {
        assert!(
            self.spawning.is_none(),
            "adoption follows completion of the owned spawn"
        );
        self.armed = false;
        self.container
            .as_ref()
            .expect("adopted container")
            .end_admission();
        self.roster.finish_pending_admission(&self.module_id);
    }
}

impl Drop for Admitted {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(gate) = &mut self.gate {
            gate.cancel();
        }
        let birth = self.identified.then_some(self.birth);
        if let Some(pid) = self.pid {
            kill_owned_pid(pid, birth);
        }
        if let Some(container) = &self.container {
            container.hard_kill();
        }
        let permit = self.permit.take();
        // A reported PID proves fork has already happened. Before that report,
        // keep both the writer and permit until the blocking spawn returns.
        let permit = if self.pid.is_some() {
            drop(permit);
            None
        } else {
            permit
        };
        let spawning = self.spawning.take();
        let gate = self.gate.take();
        let records = self.records.take();
        let packet = self.packet.take().unwrap_or(Packet {
            leader: self.leader.take(),
            failure: None,
        });
        let proof = Arc::clone(&self.proof);
        let roster = self.roster.clone();
        let module_id = self.module_id.clone();
        let container = self.container.take();
        let pid = self.pid;
        let late = self.timed_out;
        std::thread::spawn(move || {
            #[cfg(test)]
            let _ = roster.admission_event_for_test(&module_id, "before_reap", pid);
            let reaped = match spawning {
                Some(spawning) => reap_spawn(spawning, pid, birth),
                None => {
                    // After spawn returns, the Child handle is stored in the
                    // container's pin field or owned by the caller's rejection
                    // or orphan reaper. Recording has
                    // finished: let terminal cleanup reap an adopted pin before
                    // waiting for the direct-child relationship to disappear.
                    if let Some(container) = &container {
                        container.end_admission();
                        containment::finish_admission_cleanup(Arc::clone(container));
                    }
                    wait_owned_exit(pid, birth)
                }
            };
            drop(gate);
            drop(permit);
            *proof.0.lock().unwrap_or_else(|p| p.into_inner()) = Some(reaped);
            proof.1.notify_all();
            let packet = records
                .and_then(|receiver| receiver.recv().ok())
                .unwrap_or(packet);
            cleanup_packet(packet, late, &proof, &roster, &module_id, container);
        });
    }
}

#[derive(Default)]
struct Packet {
    leader: Option<RosterGuard>,
    failure: Option<io::Error>,
}

type ReapProof = Arc<(Mutex<Option<bool>>, Condvar)>;

impl Gate {
    pub(super) fn install(
        command: &mut Command,
        _spec: &ModuleSpec,
        _roster: &ChildRoster,
    ) -> io::Result<Self> {
        #[cfg(test)]
        checkpoint(_roster, &_spec.module_id, "before_permit", None)?;
        let acquire = || {
            SpawnPermit::acquire(|| {
                #[cfg(test)]
                checkpoint(_roster, &_spec.module_id, "permit_waiting", None)?;
                Ok(())
            })
        };
        let permit = if tokio::runtime::Handle::try_current().is_ok_and(|runtime| {
            runtime.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread
        }) {
            tokio::task::block_in_place(acquire)?
        } else {
            acquire()?
        };
        let (permission, placement) = AdmissionGate::install(command.as_std_mut())?;
        let gate = Self {
            permission: Some(permission),
            placement: Some(placement),
            permit: Some(permit),
        };
        #[cfg(test)]
        checkpoint(_roster, &_spec.module_id, "after_gate_install", None)?;
        #[cfg(test)]
        if let Some(pause) = _roster.take_placement_pause_for_test(&_spec.module_id) {
            pause.install(command.as_std_mut());
        }
        Ok(gate)
    }

    pub(super) fn install_placement(&mut self, command: &mut Command) {
        self.placement
            .take()
            .expect("placement callback installed once")
            .install(command.as_std_mut());
    }

    pub(super) fn spawn(
        self,
        command: Command,
        spec: &ModuleSpec,
        roster: &ChildRoster,
        identity: Option<containment::LaunchIdentity>,
        #[cfg(target_os = "linux")] cgroup: Option<(subc_cgroup::Placement, String, PathBuf)>,
    ) -> io::Result<(Child, Admitted)> {
        let work = || {
            self.spawn_blocking(
                command,
                spec,
                roster,
                identity,
                #[cfg(target_os = "linux")]
                cgroup,
            )
        };
        if tokio::runtime::Handle::current().runtime_flavor()
            == tokio::runtime::RuntimeFlavor::MultiThread
        {
            tokio::task::block_in_place(work)
        } else {
            work()
        }
    }

    fn spawn_blocking(
        mut self,
        mut command: Command,
        spec: &ModuleSpec,
        roster: &ChildRoster,
        identity: Option<containment::LaunchIdentity>,
        #[cfg(target_os = "linux")] cgroup: Option<(subc_cgroup::Placement, String, PathBuf)>,
    ) -> io::Result<(Child, Admitted)> {
        #[cfg(test)]
        let timeout = roster
            .admission_timeout_for_test()
            .unwrap_or(ADMISSION_TIMEOUT);
        #[cfg(not(test))]
        let timeout = ADMISSION_TIMEOUT;
        let deadline = std::time::Instant::now() + timeout;
        roster.begin_pending_admission(&spec.module_id, 0);
        let mut attempt = Admitted {
            leader: None,
            birth: 0,
            identified: false,
            pid: None,
            container: None,
            gate: self.permission.take(),
            spawning: None,
            permit: self.permit.take(),
            records: None,
            packet: None,
            proof: Arc::new((Mutex::new(None), Condvar::new())),
            roster: roster.clone(),
            module_id: spec.module_id.clone(),
            timed_out: false,
            armed: true,
        };
        let runtime = tokio::runtime::Handle::current();
        attempt.spawning = Some(std::thread::spawn(move || {
            let _runtime = runtime.enter();
            command.spawn()
        }));
        #[cfg(test)]
        checkpoint(roster, &spec.module_id, "before_pid_report", None)?;
        let pid = match attempt
            .gate
            .as_mut()
            .expect("one admission gate")
            .pid_within(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            Ok(pid) => pid,
            Err(error) => {
                attempt.timed_out = error.kind() == io::ErrorKind::TimedOut;
                return Err(if attempt.timed_out {
                    timed_out()
                } else {
                    error
                });
            }
        };
        attempt.pid = Some(pid);
        roster.begin_pending_admission(&spec.module_id, pid);
        let facts = subc_os::process_group::inspect(pid)?
            .ok_or_else(|| io::Error::other("gated child has no verifiable creation identity"))?;
        attempt.birth = facts.start_time;
        attempt.identified = true;
        #[cfg(test)]
        checkpoint(roster, &spec.module_id, "after_pid", Some(pid))?;
        let birth = facts.start_time;
        let fresh = containment::current(&roster.containment).is_none();
        let container = containment::current(&roster.containment).unwrap_or_else(|| {
            containment::prepare_before_exec(
                spec,
                roster,
                pid,
                birth,
                identity.expect("fresh carried fork captured boot and session"),
                #[cfg(target_os = "linux")]
                cgroup.clone(),
            )
        });
        container.begin_admission();
        attempt.container = Some(Arc::clone(&container));
        let (sender, receiver) = mpsc::channel();
        attempt.records = Some(receiver);
        let worker_roster = roster.clone();
        let worker_spec = spec.clone();
        let worker_container = Arc::clone(&container);
        let worker_proof = Arc::clone(&attempt.proof);
        std::thread::spawn(move || {
            let mut packet = Packet::default();
            if fresh {
                packet.failure = worker_container
                    .admit_before_exec(&worker_spec, &worker_roster, pid, birth)
                    .err();
            }
            if packet.failure.is_none() {
                let recorded = RecordedIdentity {
                    start_time: Some(birth),
                    #[cfg(target_os = "linux")]
                    cgroup_name: cgroup.as_ref().map(|(_, name, _)| name.clone()),
                    #[cfg(target_os = "linux")]
                    cgroup_placement: cgroup.as_ref().map(|(placement, _, _)| placement.clone()),
                    ..RecordedIdentity::default()
                };
                match worker_roster.admit(
                    worker_spec.module_id.clone(),
                    pid,
                    worker_spec.protocol,
                    Some(birth),
                    recorded,
                ) {
                    Ok(guard) => packet.leader = Some(guard),
                    Err(error) => {
                        let (source, guard) = error.into_parts();
                        packet.leader = Some(guard);
                        packet.failure = Some(source);
                    }
                }
            }
            if let Err(returned) = sender.send(packet) {
                cleanup_packet(
                    returned.0,
                    true,
                    &worker_proof,
                    &worker_roster,
                    &worker_spec.module_id,
                    Some(worker_container),
                );
            }
        });
        if let Err(error) = attempt
            .gate
            .as_mut()
            .expect("one admission gate")
            .ready_within(deadline.saturating_duration_since(std::time::Instant::now()))
        {
            attempt.timed_out = error.kind() == io::ErrorKind::TimedOut;
            return Err(if attempt.timed_out {
                timed_out()
            } else {
                error
            });
        }
        #[cfg(test)]
        checkpoint(roster, &spec.module_id, "after_placement", Some(pid))?;
        let packet = remaining(deadline).and_then(|bound| {
            attempt
                .records
                .as_ref()
                .expect("admission writer")
                .recv_timeout(bound)
                .map_err(|_| timed_out())
        });
        let packet = match packet {
            Ok(packet) => packet,
            Err(error) => {
                attempt.timed_out = error.kind() == io::ErrorKind::TimedOut;
                return Err(error);
            }
        };
        attempt.records.take();
        attempt.packet = Some(packet);
        if let Some(error) = attempt
            .packet
            .as_mut()
            .and_then(|packet| packet.failure.take())
        {
            return Err(error);
        }
        if std::time::Instant::now() >= deadline {
            attempt.timed_out = true;
            return Err(timed_out());
        }
        if roster.is_closed() || container.is_ending() {
            return Err(io::Error::other(
                "carried admission cancelled during daemon shutdown",
            ));
        }
        #[cfg(test)]
        checkpoint(roster, &spec.module_id, "after_durable", Some(pid))?;
        if std::time::Instant::now() >= deadline {
            attempt.timed_out = true;
            return Err(timed_out());
        }
        attempt
            .gate
            .as_mut()
            .expect("one admission gate")
            .release()?;
        drop(attempt.permit.take());
        let spawned = attempt
            .spawning
            .take()
            .expect("owned spawn worker")
            .join()
            .map_err(|_| io::Error::other("carried spawn worker panicked"))?;
        let child = spawned?;
        drop(attempt.gate.take());
        if child.id() != Some(pid) {
            let mut child = child;
            let _ = child.start_kill();
            while matches!(child.try_wait(), Ok(None)) {
                std::thread::yield_now();
            }
            return Err(io::Error::other(
                "carried spawn returned a different child PID",
            ));
        }
        attempt.leader = attempt.packet.take().and_then(|packet| packet.leader);
        #[cfg(test)]
        checkpoint(roster, &spec.module_id, "after_go", Some(pid))?;
        Ok((child, attempt))
    }
}

#[cfg(test)]
fn checkpoint(
    roster: &ChildRoster,
    module: &str,
    stage: &'static str,
    pid: Option<u32>,
) -> io::Result<()> {
    if roster.admission_event_for_test(module, stage, pid) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "test rejected carried admission at {stage}"
        )))
    }
}

fn timed_out() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "carried admission timed out")
}

fn remaining(deadline: std::time::Instant) -> io::Result<Duration> {
    let left = deadline.saturating_duration_since(std::time::Instant::now());
    if left.is_zero() {
        Err(timed_out())
    } else {
        Ok(left)
    }
}

fn reap_spawn(
    spawning: std::thread::JoinHandle<io::Result<Child>>,
    pid: Option<u32>,
    birth: Option<u64>,
) -> bool {
    if let Some(pid) = pid {
        kill_owned_pid(pid, birth);
    }
    match spawning.join() {
        Ok(Ok(mut child)) => loop {
            match child.try_wait() {
                Ok(Some(_)) => break true,
                Ok(None) => {
                    let _ = child.start_kill();
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(_) => break false,
            }
        },
        Ok(Err(_)) => true, // std reaps a failed pre-exec/exec attempt itself.
        Err(_) => false,
    }
}

fn cleanup_packet(
    mut packet: Packet,
    late: bool,
    proof: &ReapProof,
    roster: &ChildRoster,
    module_id: &str,
    container: Option<Arc<containment::CarriedContainment>>,
) {
    let (state, changed) = &**proof;
    let mut state = state.lock().unwrap_or_else(|p| p.into_inner());
    while state.is_none() {
        state = changed.wait(state).unwrap_or_else(|p| p.into_inner());
    }
    if *state != Some(true) {
        return;
    }
    drop(state);
    if let Some(container) = container {
        container.end_admission();
        containment::finish_admission_cleanup(container);
    }
    let failure = packet.failure.take();
    let removed = if let Some(guard) = packet.leader.take() {
        guard.release_durably()
    } else {
        roster.flush_admissions()
    };
    if let Some(error) = removed.err().or(if late { failure } else { None }) {
        roster.quarantine_failed_admission(module_id, &error);
    } else {
        roster.finish_pending_admission(module_id);
    }
}

fn kill_owned_pid(pid: u32, birth: Option<u64>) {
    let Some(expected) = birth else {
        return;
    };
    let Ok(Some(process)) = subc_os::Process::open(pid) else {
        return;
    };
    if subc_os::child_exit::observe(pid).is_ok()
        && subc_os::process_group::inspect(pid)
            .ok()
            .flatten()
            .is_some_and(|facts| facts.start_time == expected && !facts.exited)
    {
        let _ = process.signal(subc_os::Signal::Kill);
    }
}

fn wait_owned_exit(pid: Option<u32>, birth: Option<u64>) -> bool {
    let Some(pid) = pid else {
        return true;
    };
    loop {
        match subc_os::child_exit::observe(pid) {
            Err(error) if error.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) => {
                return true
            }
            Ok(Some(_)) => std::thread::sleep(Duration::from_millis(1)),
            Ok(None) => {
                kill_owned_pid(pid, birth);
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod tests;
