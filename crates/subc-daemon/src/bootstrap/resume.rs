use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{self, Read},
    os::{
        fd::{AsFd, RawFd},
        unix::fs::MetadataExt,
    },
    time::{Duration, Instant, SystemTime},
};

use rustix::{
    fs::{self as rfs, FileType, Mode, OFlags},
    io::{fcntl_setfd, FdFlags},
};
use sha2::{Digest, Sha256};
use subc_os::child_spawn::{monotonic_ns, InheritedDescriptors};

use crate::{
    config_store::{StoreRoot, StoreWriterLock},
    live_children::{self, AdoptedPids},
    run_dir_lock::{RunDirLock, RUN_DIR_LOCK_FILE_NAME},
    supervise::{
        handover::{
            self, Cohort, Handover, HexBytes, LockPaths, LockRole, StreamWitness, VerifiedImage,
        },
        live_upgrade::{
            reconcile_child_with_container, reconcile_child_with_unrecorded_fence,
            InheritedStreams, ReconciledChild,
        },
        resume_hello::Preparation,
        RegisteredConfiguredModule,
    },
};

use super::{
    start_lock_path, BootstrapConfig, BootstrapError, Path, PathBuf, StartLock, Supervisor,
    SupervisorHandle,
};

/// Formats that the daemon binary can resume on this platform.
pub const HANDOVER_VERSIONS: &[u32] = handover::HANDOVER_VERSIONS;

/// A startup request whose handover has been checked before daemon startup.
pub struct ResumeBootstrap {
    config: BootstrapConfig,
    inherited: Result<Inherited, io::Error>,
}

impl ResumeBootstrap {
    /// Validate the handover using only read-only boot configuration and the
    /// descriptor snapshot taken at process entry. A different running image
    /// is allowed to boot cold, but a malformed handover refuses startup.
    pub fn validate(
        config: BootstrapConfig,
        path: &Path,
        key_fd: RawFd,
        inherited: InheritedDescriptors,
    ) -> Result<Self, BootstrapError> {
        let inherited = validate(&config, path, key_fd, inherited)?;
        Ok(Self { config, inherited })
    }

    pub async fn run(self) -> Result<(), BootstrapError> {
        let inherited = match self.inherited {
            Ok(inherited) => Some(inherited),
            Err(error) => {
                tracing::warn!(%error, "running image did not match the upgrade target; starting cold");
                eprintln!("ck-subc: running image did not match the upgrade target; starting cold: {error}");
                None
            }
        };
        super::run_with_config_inner(self.config, inherited).await
    }
}

pub(super) struct Inherited {
    pub(super) start: StartLock,
    pub(super) run: RunDirLock,
    pub(super) store: Option<(StoreRoot, StoreWriterLock)>,
    pub(super) resume: Resume,
}

pub(super) struct Resume {
    handover: Handover,
    streams: BTreeMap<RawFd, std::os::fd::OwnedFd>,
    deadline: Instant,
    #[cfg(feature = "test-support")]
    pause_install: Option<(String, PathBuf)>,
}

fn refused(error: impl std::fmt::Display) -> BootstrapError {
    BootstrapError::Resume(io::Error::other(error.to_string()))
}

fn validate(
    config: &BootstrapConfig,
    path: &Path,
    key_fd: RawFd,
    mut inherited: InheritedDescriptors,
) -> Result<Result<Inherited, io::Error>, BootstrapError> {
    let record = config
        .live_children_path
        .as_ref()
        .ok_or_else(|| refused("no live-children record configured"))?;
    let expected = record.with_file_name(handover::HANDOVER_FILE_NAME);
    if path.as_os_str() != expected.as_os_str() {
        return Err(refused(
            "handover path does not match the configured run directory",
        ));
    }
    let file = File::from(
        rfs::open(
            path,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(refused)?,
    );
    let stat = file.metadata().map_err(refused)?;
    if !stat.is_file() || stat.uid() != subc_os::effective_uid() || stat.mode() & 0o7777 != 0o600 {
        return Err(refused(
            "handover must be a regular file owned by this user with mode 0600",
        ));
    }
    let key =
        read_key(File::from(inherited.take_key(key_fd).map_err(refused)?)).map_err(refused)?;
    let mut bytes = Vec::new();
    (&file).read_to_end(&mut bytes).map_err(refused)?;
    let mut handover = handover::decode(&bytes, &key).map_err(refused)?;
    validate_cohort(&handover)?;
    let now_ms = crate::stderr_tail::unix_ms(SystemTime::now());
    let deadline = Instant::now()
        .checked_add(Duration::from_millis(
            handover.reconnect_by_ms.saturating_sub(now_ms),
        ))
        .ok_or_else(|| refused("handover reconnect deadline is out of range"))?;

    let run = record.with_file_name(RUN_DIR_LOCK_FILE_NAME);
    let publication = start_lock_path(&config.connection_file_path);
    let store = config
        .config_store_root()
        .map(StoreWriterLock::inherited_layout)
        .transpose()
        .map_err(refused)?;
    let paths = LockPaths {
        run_dir: &run,
        publication: &publication,
        store_writer: store.as_ref().map(|(_, lock)| lock.as_path()),
    };
    let expected_fds = handover
        .locks
        .iter()
        .map(|lock| lock.fd)
        .chain(
            handover
                .children
                .iter()
                .flat_map(|child| [&child.stdout, &child.stderr])
                .filter_map(|stream| match stream {
                    StreamWitness::Open { fd, .. } => Some(*fd),
                    StreamWitness::Ended {} => None,
                }),
        )
        .collect();
    let mut descriptors = inherited.into_named(&expected_fds).map_err(refused)?;
    handover::validate_inherited_fds(&handover, config, &paths, |fd| {
        descriptors.get(&fd).map(AsFd::as_fd)
    })
    .map_err(refused)?;

    if let Err(error) = verify_running_image(config, &handover.verified_image) {
        // Close the inherited descriptors without adopting their locks. The
        // cold boot removes the handover once it owns the run directory.
        return Ok(Err(error));
    }

    let current = fs::symlink_metadata(path).map_err(refused)?;
    if current.dev() != stat.dev() || current.ino() != stat.ino() {
        return Err(refused("handover file changed during validation"));
    }
    fs::remove_file(path).map_err(refused)?;
    for fd in descriptors.values() {
        fcntl_setfd(fd, FdFlags::CLOEXEC).map_err(refused)?;
    }

    let mut locks: BTreeMap<_, _> = handover
        .locks
        .drain(..)
        .map(|lock| {
            (
                lock.role,
                File::from(
                    descriptors
                        .remove(&lock.fd)
                        .expect("validated lock descriptor"),
                ),
            )
        })
        .collect();
    let start = StartLock {
        _file: locks
            .remove(&LockRole::Publication)
            .expect("validated publication lock"),
    };
    let run = RunDirLock::from_inherited(
        record.clone(),
        locks.remove(&LockRole::RunDir).expect("validated run lock"),
    );
    let store = store.map(|(root, _)| {
        (
            root,
            StoreWriterLock::from_inherited(
                locks
                    .remove(&LockRole::StoreWriter)
                    .expect("validated store lock"),
            ),
        )
    });
    Ok(Ok(Inherited {
        start,
        run,
        store,
        resume: Resume {
            handover,
            streams: descriptors,
            deadline,
            #[cfg(feature = "test-support")]
            pause_install: config
                .upgrade_binary
                .as_ref()
                .filter(|binary| binary.test_hooks)
                .and_then(|_| std::env::var("SUBC_TEST_RESUME_PAUSE_MODULE").ok())
                // No longer than `subc.sock`, so it fits the Unix socket path
                // limit wherever the daemon's own socket does.
                .map(|module| (module, record.with_file_name("rp.sock"))),
        },
    }))
}

fn read_key(mut pipe: File) -> io::Result<[u8; 32]> {
    let flags = rfs::fcntl_getfl(&pipe)?;
    if FileType::from_raw_mode(rfs::fstat(&pipe)?.st_mode) != FileType::Fifo
        || flags.intersects(OFlags::WRONLY | OFlags::RDWR)
    {
        return Err(io::Error::other(
            "handover key descriptor must be a pipe read end",
        ));
    }
    // The old image closes its writer before exec. Never block if a malformed
    // invocation presents a pipe whose writer is still open.
    rfs::fcntl_setfl(&pipe, flags | OFlags::NONBLOCK)?;
    let mut key = [0; 32];
    pipe.read_exact(&mut key)?;
    if pipe.read(&mut [0])? != 0 {
        return Err(io::Error::other("handover key must be exactly 32 bytes"));
    }
    Ok(key)
}

fn validate_cohort(handover: &Handover) -> Result<(), BootstrapError> {
    let cohort = &handover.cohort;
    let mut modules = BTreeSet::new();
    let carried: BTreeSet<_> = cohort.carried.iter().collect();
    let mut children = BTreeSet::new();
    let mut pids = BTreeSet::new();
    if !cohort
        .carried
        .iter()
        .chain(&cohort.restarting)
        .chain(&cohort.stopped)
        .all(|id| modules.insert(id))
        || !handover
            .children
            .iter()
            .all(|child| children.insert(&child.module_id) && pids.insert(child.pid))
        || carried != children
    {
        return Err(refused("handover children and cohort disagree"));
    }
    let mut streams = BTreeSet::new();
    for stream in handover
        .children
        .iter()
        .flat_map(|child| [&child.stdout, &child.stderr])
    {
        if let StreamWitness::Open { fd, .. } = stream {
            if !streams.insert(*fd) {
                return Err(refused(format!(
                    "duplicate handover stream descriptor {fd}"
                )));
            }
        }
    }
    Ok(())
}

fn verify_running_image(_config: &BootstrapConfig, expected: &VerifiedImage) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    let (path, cdhash_matches) = (PathBuf::from("/proc/self/exe"), expected.cdhash.is_none());
    #[cfg(target_os = "macos")]
    let (path, cdhash_matches) = {
        use subc_os::code_signature::{self, SignatureError};
        let cdhash_matches = match (expected.cdhash, code_signature::self_cdhash()) {
            (Some(expected), Ok(actual)) => expected.0 == actual.0,
            #[cfg(feature = "test-support")]
            (None, Err(SignatureError::NotSigned)) => _config
                .upgrade_binary
                .as_ref()
                .is_some_and(|binary| binary.test_hooks),
            (_, Err(SignatureError::NotSigned)) | (None, Ok(_)) => false,
            (_, Err(error)) => return Err(io::Error::other(error)),
        };
        (std::env::current_exe()?, cdhash_matches)
    };
    let mut digest = Sha256::new();
    io::copy(&mut File::open(path)?, &mut digest)?;
    if !cdhash_matches || HexBytes(digest.finalize().into()) != expected.sha256 {
        return Err(io::Error::other(
            "running executable digest or code signature differs from the verified target",
        ));
    }
    Ok(())
}

/// Cold cleanup is only callable with ownership of the same run directory.
pub(super) fn discard_leftover(owner: &RunDirLock) {
    let path = owner
        .live_children_record()
        .with_file_name(handover::HANDOVER_FILE_NAME);
    if let Err(error) = fs::remove_file(&path) {
        if error.kind() != io::ErrorKind::NotFound {
            tracing::warn!(path = %path.display(), %error, "could not remove a leftover daemon handover");
        }
    }
}

pub(super) struct AdoptionPlan {
    pub(super) protected: AdoptedPids,
    pub(super) monotonic_ns: u64,
    cohort: Cohort,
    children: BTreeMap<String, (ReconciledChild, InheritedStreams)>,
    unrecorded_fences: BTreeMap<String, live_children::RecordedChild>,
    attempt: HexBytes<16>,
    started_at_ms: u64,
    deadline: Instant,
    #[cfg(feature = "test-support")]
    pause_install: Option<(String, PathBuf)>,
}

impl Resume {
    pub(super) fn verified_digest(&self) -> HexBytes<32> {
        self.handover.verified_image.sha256
    }

    pub(super) fn reconcile(mut self, owner: &RunDirLock) -> Result<AdoptionPlan, BootstrapError> {
        let path = owner.live_children_record();
        let (records, reasons) = match live_children::read_record_entries(path) {
            Ok(record) => (record.0, Some(record.1)),
            Err(error) => {
                // Missing identity makes the supervisor hold, never signal, a
                // listed child. The normal sweep quarantines the unusable file.
                tracing::warn!(%error, "could not read the carried children's durable identities");
                (Vec::new(), None)
            }
        };
        // A carried process-group container is recorded in its own row under
        // the group's first module process. That pid can also be the module
        // process's own row, so container rows are looked up by group id.
        let (containers, processes): (Vec<_>, Vec<_>) = records
            .iter()
            .partition(|record| record.child.is_group_container());
        let by_pid: BTreeMap<_, _> = processes
            .into_iter()
            .map(|record| (record.child.pid, record))
            .collect();
        let containers: BTreeMap<_, _> = containers
            .into_iter()
            .map(|record| (record.child.pid, record))
            .collect();
        let unrecorded_fences =
            fence_unrecorded_containers(path, &records, reasons, &self.handover.children);
        let mut adopted = Vec::new();
        let mut held = Vec::new();
        let mut children = BTreeMap::new();
        for mut child in self.handover.children {
            let streams = [&mut child.stdout, &mut child.stderr].map(|stream| {
                match std::mem::replace(stream, StreamWitness::Ended {}) {
                    StreamWitness::Open { fd, tail } => Some((
                        self.streams
                            .remove(&fd)
                            .expect("validated stream descriptor"),
                        tail,
                    )),
                    StreamWitness::Ended {} => None,
                }
            });
            let pid = child.pid;
            let module = child.module_id.clone();
            let carried_pgid = child.carried_container.as_ref().map(|carried| carried.pgid);
            let container = carried_pgid.and_then(|pgid| containers.get(&pgid).copied());
            let process = by_pid.get(&pid).copied();
            let reconciled = if unrecorded_fences.contains_key(&module) {
                reconcile_child_with_unrecorded_fence(child, process)
            } else {
                reconcile_child_with_container(child, process, container)
            }
            .map_err(refused)?;
            match &reconciled {
                ReconciledChild::Adopt(child) => {
                    adopted.push(pid);
                    adopted.extend(child.carried_pgid());
                }
                ReconciledChild::Held(_) => held.push(pid),
                ReconciledChild::Exited => {}
            }
            // A container that is not taken over keeps its row as a held
            // fence: the sweep leaves it, the record keeps it across a
            // restart, and only the settlement of that container removes it.
            if let Some(pgid) = carried_pgid {
                if !matches!(&reconciled, ReconciledChild::Adopt(child) if child.carried_pgid().is_some())
                {
                    held.push(pgid);
                }
            }
            children.insert(module, (reconciled, streams));
        }
        Ok(AdoptionPlan {
            protected: AdoptedPids::from_classes(adopted, held),
            monotonic_ns: self.handover.monotonic_ns,
            cohort: self.handover.cohort,
            children,
            unrecorded_fences,
            attempt: self.handover.attempt_id,
            started_at_ms: self.handover.started_at_ms,
            deadline: self.deadline,
            #[cfg(feature = "test-support")]
            pause_install: self.pause_install,
        })
    }
}

/// Record a carried container that the signed handover names but the durable
/// record lacks. Missing evidence does not prove that the container is empty,
/// so its row is written from the handover's own facts before the orphan sweep
/// or a shutdown could lose them. The resume holds that row like any other
/// container row, a cold start sweeps it, and only the container's settlement
/// removes it.
pub(super) fn fence_unrecorded_containers(
    path: &Path,
    records: &[live_children::RecordedChild],
    reasons: Option<BTreeMap<u32, live_children::RetainedReason>>,
    children: &[handover::ChildWitness],
) -> BTreeMap<String, live_children::RecordedChild> {
    let recorded = |pgid: u32| {
        records
            .iter()
            .any(|record| record.child.is_group_container() && record.child.pid == pgid)
    };
    let missing: Vec<_> = children
        .iter()
        .filter_map(|child| {
            let carried = child.carried_container.as_ref()?;
            (!recorded(carried.pgid)).then(|| live_children::RecordedChild {
                child: live_children::LiveChild {
                    module_id: child.module_id.clone(),
                    pid: carried.pgid,
                    protocol: records
                        .iter()
                        .find(|record| record.child.module_id == child.module_id)
                        .map_or(subc_control::ModuleProtocol::Subc, |record| {
                            record.child.protocol
                        }),
                    start_time: Some(carried.created_start_time),
                    executable: None,
                    cgroup_name: None,
                    container: Some(live_children::ContainerIdentity {
                        pgid: Some(carried.pgid),
                        created_start_time: Some(carried.created_start_time),
                        boot_id: Some(carried.boot_id.clone()),
                        session_id: Some(carried.session_id),
                        cgroup_path: None,
                    }),
                },
                resume_token_sha256: None,
            })
        })
        .collect();
    if missing.is_empty() {
        return BTreeMap::new();
    }
    for row in &missing {
        tracing::warn!(module_id = %row.child.module_id, pgid = row.child.pid,
            "the handover names a carried container the record lacks; recording it as a fence");
    }
    // Leave an unreadable record to the orphan sweep's quarantine. Retry
    // against the running roster, not against this stale bootstrap snapshot.
    let result = reasons.map_or_else(
        || {
            Err(std::io::Error::other(
                "the existing recovery record is unreadable",
            ))
        },
        |reasons| {
            let rows: Vec<_> = records.iter().chain(&missing).cloned().collect();
            live_children::write_record_with_reasons(path, &rows, reasons)
        },
    );
    match result {
        Ok(()) => BTreeMap::new(),
        Err(error) => {
            for row in &missing {
                tracing::error!(module_id = %row.child.module_id, pgid = row.child.pid, %error,
                    "could not record the carried container fence; holding the module and retrying");
            }
            missing
                .into_iter()
                .map(|row| (row.child.module_id.clone(), row))
                .collect()
        }
    }
}

type RegisteredCohort = BTreeMap<String, (RegisteredConfiguredModule, bool)>;
pub(super) type PlannedStarts = Vec<(String, Option<RegisteredConfiguredModule>, bool)>;
type Preparations = BTreeMap<String, Preparation>;

impl AdoptionPlan {
    fn modules(&self) -> impl Iterator<Item = &String> {
        self.cohort
            .carried
            .iter()
            .chain(&self.cohort.restarting)
            .chain(&self.cohort.stopped)
    }

    pub(super) fn begin_report(&self, supervisor: &SupervisorHandle) {
        supervisor.begin_resume_report(
            self.attempt.to_string(),
            self.started_at_ms,
            self.modules().cloned(),
        );
    }

    pub(super) fn enabled(&self, module: &str, configured: bool) -> Option<bool> {
        self.modules()
            .any(|id| id == module)
            .then(|| self.cohort.allow_start(module, Some(configured)))
    }

    pub(super) fn stage_preparations(
        &self,
        handle: &SupervisorHandle,
        registered: &RegisteredCohort,
    ) -> Result<Preparations, BootstrapError> {
        self.children
            .iter()
            .filter_map(|(module, (child, _))| {
                let ReconciledChild::Adopt(child) = child else {
                    return None;
                };
                registered.get(module).filter(|(_, enabled)| *enabled)?;
                Some(
                    child
                        .begin_preparation(handle, self.deadline.into())
                        .map(|guard| (module.clone(), guard))
                        .map_err(refused),
                )
            })
            .collect()
    }

    pub(super) async fn install(
        mut self,
        supervisor: &Supervisor,
        mut registered: RegisteredCohort,
        mut preparations: Preparations,
    ) -> Result<PlannedStarts, BootstrapError> {
        let mut planned = Vec::new();
        let mut installing = tokio::task::JoinSet::new();
        let attempt = self.attempt.0;
        let deadline = self.deadline.into();
        for module in self
            .cohort
            .carried
            .into_iter()
            .chain(self.cohort.restarting)
            .chain(self.cohort.stopped)
        {
            let (registered, enabled) = match registered.remove(&module) {
                Some((registered, enabled)) => (Some(registered), enabled),
                None => (None, false),
            };
            match self.children.remove(&module) {
                Some((ReconciledChild::Adopt(child), streams)) => {
                    if let Some(registered) = registered {
                        let supervisor = supervisor.clone();
                        let preparation = preparations.remove(&module);
                        #[cfg(feature = "test-support")]
                        let pause = self
                            .pause_install
                            .as_ref()
                            .filter(|(id, _)| *id == module)
                            .map(|(_, path)| path.clone());
                        installing.spawn(async move {
                            let _preparation = preparation;
                            #[cfg(feature = "test-support")]
                            if let Some(path) = pause {
                                let _ = tokio::time::timeout_at(
                                    deadline,
                                    crate::control::upgrade_in_place::wait_at_gate(&path, 1),
                                )
                                .await;
                            }
                            supervisor
                                .start_adopted_after_upgrade(
                                    registered, *child, streams, attempt, deadline, enabled,
                                )
                                .await
                        });
                    } else {
                        supervisor.stop_unconfigured_after_upgrade(*child);
                    }
                }
                Some((ReconciledChild::Held(child), _)) => {
                    supervisor.watch_unresolved_with_fence(
                        *child,
                        registered,
                        enabled,
                        self.unrecorded_fences.remove(&module),
                    );
                }
                Some((ReconciledChild::Exited, _)) | None => {
                    planned.push((module, registered, enabled))
                }
            }
        }
        while let Some(result) = installing.join_next().await {
            result.map_err(refused)?.map_err(refused)?;
        }
        Ok(planned)
    }
}

pub(super) fn log_accept(sample: u64) {
    match monotonic_ns() {
        Ok(now) => tracing::info!(
            resume_accept_ms = now.saturating_sub(sample) / 1_000_000,
            "local listener accepting daemon upgrade reconnects"
        ),
        Err(error) => {
            tracing::warn!(%error, "could not measure when the local listener started accepting")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// A signed handover names a carried container the record lacks, and a
    /// helper survives in it. The fence must outlive this daemon: after a
    /// shutdown, a cold start still has the group identity and holds the module.
    #[test]
    fn an_unrecorded_carried_container_stays_fenced_across_a_cold_start() {
        use std::os::unix::process::CommandExt;
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let scratch = cortexkit_test_support::ScratchDir::new("resume-unrecorded-container");
        let sleep = |group: i32| {
            Owned(
                std::process::Command::new("/bin/sleep")
                    .arg("60")
                    .process_group(group)
                    .spawn()
                    .unwrap(),
            )
        };
        let pin = sleep(0);
        let pgid = pin.0.id();
        let _helper = sleep(i32::try_from(pgid).unwrap());
        let created = subc_os::process_group::inspect(pgid)
            .unwrap()
            .unwrap()
            .start_time;
        let mut handover = handover::decode(
            include_bytes!("../../tests/fixtures/handover-format-2.txt"),
            b"0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        let child = &mut handover.children[0];
        child.module_id = "carrier".into();
        child.pid = pgid;
        child.start_time = created;
        child.carried_container = Some(handover::CarriedContainerWitness {
            pgid,
            pin_pid: pgid,
            created_start_time: created,
            boot_id: subc_os::boot_id::current().unwrap(),
            session_id: subc_os::process_group::session_id(std::process::id())
                .unwrap()
                .unwrap(),
        });
        let module_row = live_children::RecordedChild {
            child: live_children::LiveChild {
                module_id: "carrier".into(),
                pid: pgid,
                protocol: subc_control::ModuleProtocol::Subc,
                start_time: Some(created),
                executable: None,
                cgroup_name: None,
                container: None,
            },
            resume_token_sha256: None,
        };
        let path = scratch.join("live-children.json");
        let blocked_path = scratch.join("blocked-record");
        std::fs::create_dir(&blocked_path).unwrap();
        let pending = fence_unrecorded_containers(
            &blocked_path,
            std::slice::from_ref(&module_row),
            Some(BTreeMap::new()),
            &handover.children[..1],
        );
        assert!(pending["carrier"].child.is_group_container());
        assert_eq!(pending["carrier"].child.pid, pgid);
        live_children::write_record_with_reasons(
            &path,
            std::slice::from_ref(&module_row),
            BTreeMap::new(),
        )
        .unwrap();

        assert!(fence_unrecorded_containers(
            &path,
            std::slice::from_ref(&module_row),
            Some(BTreeMap::new()),
            &handover.children[..1],
        )
        .is_empty());
        // The resumed daemon holds the row; then it shuts down.
        let resumed = crate::child_roster::ChildRoster::default();
        resumed.record_to_with_adopted(path.clone(), &AdoptedPids::from_classes([], [pgid]));
        assert!(resumed.previous_instance("carrier").is_some());
        drop(resumed);
        assert!(live_children::read_record_entries(&path)
            .unwrap()
            .0
            .iter()
            .any(|row| row.child.is_group_container() && row.child.pid == pgid));

        // A cold start finds the fence and holds the module until the group
        // is settled under the recorded group's own proof.
        let cold = crate::child_roster::ChildRoster::default();
        cold.record_to_with_adopted(path.clone(), &AdoptedPids::none());
        assert!(
            cold.previous_instance("carrier").is_some(),
            "a cold start must not start the module beside the container's helper"
        );
    }

    #[test]
    fn duplicate_stream_descriptor_is_refused_before_adoption() {
        let mut handover = handover::decode(
            include_bytes!("../../tests/fixtures/handover-format-2.txt"),
            b"0123456789abcdef0123456789abcdef",
        )
        .unwrap();
        validate_cohort(&handover).unwrap();
        let StreamWitness::Open { fd, .. } = handover.children[0].stdout else {
            panic!("golden handover must have an open stdout");
        };
        handover.children[1].stderr = StreamWitness::Open { fd, tail: vec![] };
        assert!(validate_cohort(&handover)
            .unwrap_err()
            .to_string()
            .contains("duplicate handover stream descriptor"));
    }

    #[test]
    fn handover_key_requires_exactly_32_bytes_and_a_closed_writer() {
        for key in [&[7; 31][..], &[7; 32], &[7; 33]] {
            let (reader, mut writer) = io::pipe().unwrap();
            writer.write_all(key).unwrap();
            drop(writer);
            let result = read_key(File::from(std::os::fd::OwnedFd::from(reader)));
            if key.len() == 32 {
                assert_eq!(result.unwrap(), [7; 32]);
            } else {
                assert!(result.is_err());
            }
        }
        let (reader, mut writer) = io::pipe().unwrap();
        writer.write_all(&[7; 32]).unwrap();
        let error = read_key(File::from(std::os::fd::OwnedFd::from(reader))).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        drop(writer);
    }
}
