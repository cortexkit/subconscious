//! Old-image daemon upgrade control. The replacement image validates the
//! handover before it adopts any children or serves any connections.
//!
//! Accept loops pause without closing their sockets or unlinking the local
//! pathname. The kernel queues connections while paused: unwind serves them
//! on the original sockets, while exec closes the sockets and resets them.
//! Resume startup binds TCP afresh and replaces the stale local socket path,
//! just as startup after a crash does. No listener descriptor is inherited.

use super::*;
use subc_control::upgrade_in_place::*;

pub(crate) type SettingsPermit = Arc<tokio::sync::OwnedRwLockReadGuard<()>>;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub(crate) enum Admission {
    #[default]
    Open,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Fenced,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    ListenersPaused,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    ClientsFrozen,
}

#[derive(Debug, Default)]
pub(crate) struct AdmissionGate {
    phase: std::sync::atomic::AtomicU8,
    slow: tokio::sync::watch::Sender<Admission>,
    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    subscriptions: std::sync::atomic::AtomicUsize,
}

impl AdmissionGate {
    pub(crate) fn phase(&self) -> Admission {
        let phase = self.phase.load(std::sync::atomic::Ordering::Relaxed);
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        match phase {
            1 => return Admission::Fenced,
            2 => return Admission::ListenersPaused,
            3 => return Admission::ClientsFrozen,
            _ => {}
        }
        let _ = phase;
        Admission::Open
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::watch::Receiver<Admission> {
        #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
        self.subscriptions
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.slow.subscribe()
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn subscriptions_for_test(&self) -> usize {
        self.subscriptions
            .load(std::sync::atomic::Ordering::Relaxed)
    }
    pub(crate) fn borrow(&self) -> tokio::sync::watch::Ref<'_, Admission> {
        self.slow.borrow()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn set(&self, phase: Admission) {
        self.slow.send_modify(|current| {
            self.phase
                .store(phase as u8, std::sync::atomic::Ordering::Relaxed);
            *current = phase;
        });
    }
}

impl Admission {
    pub(crate) fn fenced(self) -> bool {
        self != Self::Open
    }
    pub(crate) fn listeners_paused(self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return self >= Self::ListenersPaused;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        false
    }

    pub(crate) fn clients_frozen(self) -> bool {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        return self == Self::ClientsFrozen;
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        false
    }
}

pub(crate) async fn wait_for_fence(admission: &mut tokio::sync::watch::Receiver<Admission>) {
    let _ = admission.wait_for(|phase| phase.fenced()).await;
}

#[derive(Debug, Default)]
pub(crate) struct RefusalFlushes {
    batch: Mutex<Option<Arc<tokio::sync::watch::Sender<FlushCount>>>>,
    pub(crate) tasks: crate::forwarding::CutoverTasks,
    #[cfg(all(
        feature = "test-support",
        any(target_os = "linux", target_os = "macos")
    ))]
    fixture_flush_gate: Mutex<Option<Arc<PathBuf>>>,
}

/// A cutover diagnostic, not a delivery guarantee. Only daemon-generated
/// fence refusals for data REQUESTs and route.open are tracked. Other
/// refusals are not counted. A client that misses a refusal sees the
/// connection close and treats the request as outcome unknown, not as safe
/// to retry.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Default)]
struct ReceiptDrain {
    refusals_tracked: usize,
    refusals_unconfirmed: usize,
    /// Every tracked producer, reader and flush settled by the deadline.
    /// Known losses can settle in time but are still unconfirmed.
    settled_in_time: bool,
    unacknowledged_connections: usize,
    unacknowledged_tasks: usize,
}

#[derive(Default, Clone, Copy, Debug)]
struct FlushCount {
    tracked: usize,
    pending: usize,
    lost: usize,
    deadline: Option<tokio::time::Instant>,
}

struct RefusalFlush(Arc<tokio::sync::watch::Sender<FlushCount>>, bool);

impl Drop for RefusalFlush {
    fn drop(&mut self) {
        self.0.send_modify(|count| {
            count.pending -= 1;
            let late = count
                .deadline
                .is_some_and(|deadline| tokio::time::Instant::now() > deadline);
            count.lost += usize::from(!self.1 || late);
        });
    }
}

/// Created where the daemon rejects work at the upgrade fence, never from a
/// relayed reply. Dropping it before queueing counts the refusal as unconfirmed.
pub(crate) struct FenceRefusal<'a> {
    frame: Frame,
    ticket: RefusalFlush,
    accounting: &'a RefusalFlushes,
}

impl FenceRefusal<'_> {
    pub(crate) fn send(self, sink: &crate::FrameSink) {
        let Self {
            frame,
            mut ticket,
            accounting,
        } = self;
        let (ack, received) = tokio::sync::oneshot::channel();
        let queued = sink.try_send_with_flush(
            frame,
            ack,
            #[cfg(all(
                feature = "test-support",
                any(target_os = "linux", target_os = "macos")
            ))]
            accounting.fixture_flush_gate.lock().unwrap().clone(),
        );
        let _ = accounting;
        if queued.is_ok() {
            tokio::spawn(async move {
                ticket.1 = received.await.is_ok();
                drop(ticket);
            });
        }
    }
}

impl RefusalFlushes {
    pub(crate) fn track_fence_refusal(&self, frame: Frame) -> Result<FenceRefusal<'_>, Frame> {
        let active = self.batch.lock().unwrap_or_else(|p| p.into_inner());
        let Some(batch) = active.as_ref() else {
            return Err(frame);
        };
        batch.send_modify(|count| {
            count.tracked += 1;
            count.pending += 1;
        });
        Ok(FenceRefusal {
            frame,
            ticket: RefusalFlush(batch.clone(), false),
            accounting: self,
        })
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn begin(&self) {
        *self.batch.lock().unwrap_or_else(|p| p.into_inner()) = Some(Arc::default());
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn take(&self) -> Option<Arc<tokio::sync::watch::Sender<FlushCount>>> {
        self.batch.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    async fn drain_after_freeze(
        &self,
        budget: Duration,
        mut binds: tokio::sync::watch::Receiver<usize>,
        readers: &crate::forwarding::CutoverCount,
    ) -> ReceiptDrain {
        let Some(batch) = self.batch.lock().unwrap_or_else(|p| p.into_inner()).clone() else {
            return ReceiptDrain::default();
        };
        let mut readers = readers.subscribe();
        let mut changes = batch.subscribe();
        let deadline = tokio::time::Instant::now() + budget;
        batch.send_modify(|count| count.deadline = Some(deadline));
        // Tokio polls settlement before its timer. A ready drain can still
        // be late when both became ready between polls.
        let settled_in_budget = tokio::time::timeout_at(deadline, async {
            // Wait for readers before tasks: a reader may still have one
            // frame to dispatch, and that dispatch can spawn a task.
            let _ = readers.wait_for(|count| *count == 0).await;
            self.tasks.wait_until_idle().await;
            let _ = binds.wait_for(|active| *active == 0).await;
            let _ = changes.wait_for(|count| count.pending == 0).await;
            tokio::time::Instant::now()
        })
        .await
        .is_ok_and(|settled_at| settled_at <= deadline);
        let count = batch.borrow();
        let active_binds = *binds.borrow();
        let unacknowledged_connections = *readers.borrow();
        let unacknowledged_tasks = self.tasks.active();
        ReceiptDrain {
            refusals_tracked: count.tracked,
            refusals_unconfirmed: count.pending + count.lost,
            settled_in_time: settled_in_budget
                && count.pending == 0
                && unacknowledged_connections == 0
                && unacknowledged_tasks == 0
                && active_binds == 0,
            unacknowledged_connections,
            unacknowledged_tasks,
        }
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(crate) async fn receipt_for_test(
        &self,
        readers: &crate::forwarding::CutoverCount,
    ) -> (usize, usize, bool) {
        let receipt = self
            .drain_after_freeze(
                Duration::from_secs(2),
                tokio::sync::watch::channel(0).1,
                readers,
            )
            .await;
        (
            receipt.refusals_tracked,
            receipt.refusals_unconfirmed,
            receipt.settled_in_time,
        )
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    pub(super) async fn drain_with_binds(
        &self,
        budget: Duration,
        binds: tokio::sync::watch::Receiver<usize>,
    ) -> (usize, usize, bool) {
        let receipt = self
            .drain_after_freeze(budget, binds, &Default::default())
            .await;
        (
            receipt.refusals_tracked,
            receipt.refusals_unconfirmed,
            receipt.settled_in_time,
        )
    }

    #[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
    async fn drain(&self, budget: Duration) -> usize {
        self.drain_with_binds(budget, tokio::sync::watch::channel(0).1)
            .await
            .1
    }
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[tokio::test(start_paused = true)]
async fn receipt_tallies_dropped_and_stalled_refusals_without_carrying_them_to_retry() {
    let flushes = Arc::new(RefusalFlushes::default());
    let (sink, mut writer) = crate::server::connection_egress();
    flushes.begin();
    let send = |corr| {
        let flushes = flushes.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let refusal = RouterError::DaemonUpgrade {
                channel: 1,
                epoch: 7,
                corr,
            }
            .to_error_frame()
            .unwrap();
            flushes.track_fence_refusal(refusal).unwrap().send(&sink);
        })
    };

    let delivered = send(1);
    writer
        .recv()
        .await
        .unwrap()
        .flushed
        .unwrap()
        .send(())
        .unwrap();
    delivered.await.unwrap();
    let disconnected = send(2);
    drop(writer.recv().await.unwrap());
    disconnected.await.unwrap();
    let exact = flushes
        .drain_after_freeze(
            Duration::from_secs(2),
            tokio::sync::watch::channel(0).1,
            &Default::default(),
        )
        .await;
    assert!(
        exact.settled_in_time,
        "a known loss has settled and is counted exactly"
    );
    assert_eq!((exact.refusals_tracked, exact.refusals_unconfirmed), (2, 1));
    let stalled = send(3);
    let queued = writer.recv().await.unwrap();

    assert_eq!(flushes.drain(Duration::from_secs(2)).await, 2);
    flushes.begin();
    drop(queued);
    stalled.await.unwrap();
    assert_eq!(flushes.drain(Duration::from_secs(2)).await, 0);
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[tokio::test(start_paused = true)]
async fn refusal_producer_enqueues_the_batch_before_drain_waits_for_flush() {
    let flushes = Arc::new(RefusalFlushes::default());
    flushes.begin();
    let (sink, mut writer) = crate::server::connection_egress();
    let producer = tokio::spawn({
        let flushes = flushes.clone();
        async move {
            for corr in [1, 2] {
                let refusal = RouterError::DaemonUpgrade {
                    channel: 1,
                    epoch: 2,
                    corr,
                }
                .to_error_frame()
                .unwrap();
                flushes.track_fence_refusal(refusal).unwrap().send(&sink);
            }
        }
    });
    let first = writer.recv().await.unwrap();
    let second = tokio::time::timeout(Duration::from_secs(1), writer.recv())
        .await
        .expect("second refusal was serialized behind the first ACK")
        .unwrap();
    producer.await.unwrap();
    assert_eq!((first.header.corr, second.header.corr), (1, 2));

    let drain = flushes.drain(Duration::from_secs(2));
    tokio::pin!(drain);
    std::future::poll_fn(|cx| {
        assert!(
            std::future::Future::poll(drain.as_mut(), cx).is_pending(),
            "drain must wait for socket ACKs"
        );
        std::task::Poll::Ready(())
    })
    .await;
    first.flushed.unwrap().send(()).unwrap();
    second.flushed.unwrap().send(()).unwrap();
    assert_eq!(drain.await, 0);
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[tokio::test(start_paused = true)]
async fn refusal_settling_after_the_deadline_is_incomplete_even_when_polled_first() {
    let flushes = RefusalFlushes::default();
    let readers = crate::forwarding::CutoverCount::default();
    let (sink, mut writer) = crate::server::connection_egress();
    flushes.begin();
    let refusal = RouterError::DaemonUpgrade {
        channel: 1,
        epoch: 7,
        corr: 55,
    }
    .to_error_frame()
    .unwrap();
    flushes.track_fence_refusal(refusal).unwrap().send(&sink);
    let ack = writer.recv().await.unwrap().flushed.unwrap();
    let deadline = Duration::from_secs(2);
    let drain = flushes.drain_after_freeze(deadline, tokio::sync::watch::channel(0).1, &readers);
    tokio::pin!(drain);
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(drain.as_mut(), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    tokio::time::advance(deadline + Duration::from_millis(1)).await;
    ack.send(()).unwrap();
    let mut flushes_settled = flushes.batch.lock().unwrap().as_ref().unwrap().subscribe();
    flushes_settled
        .wait_for(|count| count.pending == 0)
        .await
        .unwrap();
    let receipt = drain.await;
    assert_eq!(
        (receipt.refusals_tracked, receipt.refusals_unconfirmed),
        (1, 1)
    );
    assert!(
        !receipt.settled_in_time,
        "the only writer acknowledged after the budget had expired"
    );
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
#[tokio::test(start_paused = true)]
async fn cutover_reports_a_reader_and_task_that_have_not_reached_the_barrier() {
    let forwarding = ForwardingTable::default();
    let id = ConnectionId::new(8);
    let mut reader = forwarding.register_connection_close(id);
    let (phase, _live_reader) = forwarding.start_upgrade_reader(id);
    assert_eq!(phase, Admission::Open);
    let flushes = &forwarding.upgrade_refusals;
    let already_spawned_task =
        crate::forwarding::CutoverTask::new(flushes, |flushes| &flushes.tasks);
    flushes.begin();
    let readers = forwarding.freeze_upgrade_readers();
    // A reader blocked dispatching a decoded frame polls for a real close,
    // but must not acknowledge the freeze while it still owns that frame.
    std::future::poll_fn(|cx| {
        assert!(std::future::Future::poll(std::pin::Pin::new(&mut reader), cx).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    let drain = flushes
        .drain_after_freeze(
            Duration::from_secs(2),
            tokio::sync::watch::channel(0).1,
            &readers,
        )
        .await;
    assert!(!drain.settled_in_time);
    assert_eq!(drain.unacknowledged_connections, 1);
    assert_eq!(drain.unacknowledged_tasks, 1);
    reader.acknowledge_reader();
    drop(already_spawned_task);
    let settled = flushes
        .drain_after_freeze(
            Duration::from_secs(2),
            tokio::sync::watch::channel(0).1,
            &readers,
        )
        .await;
    assert!(settled.settled_in_time);
}

impl ControlHandler {
    pub(super) async fn daemon_restart_intent(
        &self,
        frame: Frame,
        intent: serde_json::Value,
    ) -> Result<Vec<Frame>, RouterError> {
        if intent.as_str() != Some("restart") {
            return Ok(vec![control_error_frame(
                &frame,
                "invalid_params",
                "intent must be 'restart'",
            )?]);
        }
        let guard = match self.upgrade_settings_guard() {
            Ok(guard) => Arc::new(guard),
            Err(body) => return Ok(vec![control_error_body_frame(&frame, body)?]),
        };
        if let Some(owner) = &self.config_runtime {
            let job_guard = guard.clone();
            match owner
                .on_blocking(move |owner| {
                    let _guard = job_guard;
                    owner.assign_daemon_restart()
                })
                .await
            {
                Ok(Some(generation)) => info!(
                    generation,
                    "daemon restart will warm up approved generation"
                ),
                Ok(None) => {}
                Err(error) => {
                    warn!(%error, "daemon restart intent not recorded; the next start boots the last active generation")
                }
            }
        }
        let response = ClientControlResponse::DaemonShutdownIntent {
            intent: "restart".to_owned(),
            expires_at_ms: self.supervisor.announce_daemon_restart(),
        };
        Ok(vec![control_response_body_frame(
            &frame,
            &response,
            "ClientControlResponse::DaemonShutdownIntent",
        )?])
    }

    pub(super) fn upgrade_settings_guard(
        &self,
    ) -> Result<Option<tokio::sync::OwnedRwLockReadGuard<()>>, ErrorBody> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(runtime) = &self.upgrade {
            return runtime.settings_operation().map(Some);
        }
        Ok(None)
    }

    pub(super) fn upgrade_in_place(
        &self,
        ctx: &RouteCtx,
        frame: Frame,
        executable_sha256: String,
    ) -> Result<Vec<Frame>, RouterError> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        if let Some(runtime) = &self.upgrade {
            return runtime.start(self, ctx, frame, executable_sha256);
        }
        let _ = (ctx, executable_sha256);
        Ok(vec![control_error_frame(
            &frame,
            UPGRADE_IN_PLACE_UNSUPPORTED,
            "this daemon cannot hand over its local endpoint, child roster and locks",
        )?])
    }
}

#[cfg(all(
    feature = "test-support",
    any(target_os = "linux", target_os = "macos")
))]
pub(crate) use unix::fixture_tests::wait_at_gate;
#[cfg(all(
    feature = "test-support",
    any(target_os = "linux", target_os = "macos")
))]
pub(crate) use unix::Hooks;
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) use unix::{witness, BootImage, Runtime, UpgradeStartup};

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod unix {
    use super::*;
    use crate::control::config_gate::now_ms;
    use crate::{
        bootstrap::StartLock,
        child_roster::RetainedView,
        forwarding::CloseReason,
        supervise::{
            handover::{self, Handover, HexBytes, LockRole, LockWitness, VerifiedImage},
            live_upgrade::{ParkedChild, UpgradeAttempt, UpgradeBusy},
        },
    };
    use sha2::{Digest, Sha256};
    #[cfg(target_os = "linux")]
    use std::os::fd::AsFd;
    use std::{
        collections::BTreeSet,
        ffi::{CString, OsStr, OsString},
        fs::{self, File, OpenOptions},
        io::{self, PipeReader, PipeWriter, Seek, SeekFrom, Write},
        os::{
            fd::AsRawFd,
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, OpenOptionsExt},
            },
        },
        path::{Path, PathBuf},
        process::{Output, Stdio},
    };
    use subc_os::child_spawn::{self, ExecImage, ExecImageError, ExecTarget};
    use tokio::{
        io::AsyncReadExt,
        sync::{OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock},
    };

    const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
    const PROBE_LIMIT: u64 = 64 * 1024;
    const REPLY_TIMEOUT: Duration = Duration::from_secs(2);
    const BOOT_IDENTITY_WAIT: Duration = Duration::from_secs(2);
    const RECONNECT_WINDOW: Duration = Duration::from_secs(30);

    pub(crate) struct UpgradeStartup {
        pub(crate) boot: BootImage,
        pub(crate) required: &'static [LockRole],
        pub(crate) hooks: Hooks,
    }

    #[derive(Default)]
    pub(crate) struct Hooks {
        #[cfg(target_os = "macos")]
        enabled: bool,
        #[cfg(target_os = "macos")]
        team: Option<String>,
        exec_path: Option<PathBuf>,
        write_failure: std::sync::atomic::AtomicBool,
        #[cfg(feature = "test-support")]
        corrupt_hmac: bool,
        #[cfg(feature = "test-support")]
        receipt_gate: Option<PathBuf>,
        notice_failure: Option<String>,
        #[cfg(feature = "test-support")]
        freeze_gate: Option<PathBuf>,
        #[cfg(feature = "test-support")]
        refusal_writer_gate: Option<Arc<PathBuf>>,
        #[cfg(feature = "test-support")]
        pub(crate) pause_local_listener: bool,
        #[cfg(feature = "test-support")]
        boot_hash_gate: Option<PathBuf>,
    }

    #[cfg(feature = "test-support")]
    #[path = "fixture_tests.rs"]
    pub(crate) mod fixture_tests;

    pub(crate) struct Runtime {
        boot: BootImage,
        required: &'static [LockRole],
        run_dir: crate::run_dir_lock::RunDirLock,
        store_writer: Option<LockWitness>,
        retained: RetainedView,
        publication: std::sync::OnceLock<StartLock>,
        settings: Arc<RwLock<()>>,
        shutdown: tokio::sync::watch::Sender<bool>,
        running: tokio::sync::watch::Sender<Option<Arc<()>>>,
        last_failure: std::sync::Mutex<Option<serde_json::Value>>,
        hooks: Hooks,
    }

    impl Runtime {
        #[cfg(test)]
        pub(crate) fn for_settings_cutover_test(
            boot: BootImage,
            roles: &'static [LockRole],
            run: crate::run_dir_lock::RunDirLock,
            retained: RetainedView,
        ) -> Self {
            Self::new(
                boot,
                roles,
                run,
                None,
                retained,
                Hooks {
                    write_failure: true.into(),
                    #[cfg(target_os = "macos")]
                    enabled: true,
                    ..Default::default()
                },
            )
        }

        pub(crate) fn new(
            boot: BootImage,
            config_roles: &'static [LockRole],
            run_dir: crate::run_dir_lock::RunDirLock,
            store_writer: Option<LockWitness>,
            retained: RetainedView,
            hooks: Hooks,
        ) -> Self {
            Self {
                boot,
                required: config_roles,
                run_dir,
                store_writer,
                retained,
                publication: Default::default(),
                settings: Default::default(),
                shutdown: Default::default(),
                running: Default::default(),
                last_failure: Default::default(),
                hooks,
            }
        }

        pub(crate) fn retain_publication(&self, lock: StartLock) {
            let _ = self.publication.set(lock);
        }

        pub(crate) async fn shutdown(&self) {
            self.shutdown.send_replace(true);
            // Let the attempt finish unwinding before normal shutdown closes
            // the listeners and takes over the supervisor actors.
            let _ = self.running.subscribe().wait_for(Option::is_none).await;
        }

        #[cfg(test)]
        pub(crate) async fn wait_for_idle(&self) {
            let _ = self.running.subscribe().wait_for(Option::is_none).await;
        }

        #[cfg(all(
            test,
            feature = "test-support",
            any(target_os = "linux", target_os = "macos")
        ))]
        pub(crate) async fn wait_for_settings_idle(&self) {
            drop(self.settings.write().await);
        }

        pub(super) fn settings_operation(&self) -> Result<OwnedRwLockReadGuard<()>, ErrorBody> {
            self.settings
                .clone()
                .try_read_owned()
                .map_err(|_| busy("upgrade"))
        }

        fn lock_witnesses(&self) -> io::Result<Vec<LockWitness>> {
            self.required
                .iter()
                .map(|&role| match role {
                    LockRole::RunDir => witness(role, self.run_dir.file()),
                    LockRole::Publication => witness(
                        role,
                        self.publication
                            .get()
                            .ok_or_else(|| io::Error::other("publication lock not held"))?
                            .file(),
                    ),
                    LockRole::StoreWriter => self
                        .store_writer
                        .clone()
                        .ok_or_else(|| io::Error::other("store writer lock not held")),
                })
                .collect()
        }

        pub(in crate::control) fn describe(
            &self,
            counters: &mut serde_json::Map<String, serde_json::Value>,
        ) {
            if let Some(digest) = self.boot.digest() {
                counters.insert(
                    "daemon_identity".into(),
                    serde_json::json!({
                        "pid": std::process::id(),
                        "executable": self.boot.path,
                        "executable_sha256": digest,
                        "core_version": self.boot.core_version,
                        "build_git_sha": self.boot.build_git_sha,
                    }),
                );
            }
            if let Some(report) = self
                .last_failure
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .clone()
            {
                counters.insert("last_upgrade".into(), report);
            }
        }

        pub(in crate::control) async fn wait_for_identity(&self) -> bool {
            self.boot.wait_for_digest().await.is_some()
        }

        pub(super) fn start(
            self: &Arc<Self>,
            handler: &ControlHandler,
            ctx: &RouteCtx,
            frame: Frame,
            expected: String,
        ) -> Result<Vec<Frame>, RouterError> {
            let refuse = |body| control_error_body_frame(&frame, body).map(|reply| vec![reply]);
            if self.boot.digest().is_none() {
                return refuse(ErrorBody::new(
                    UPGRADE_IN_PLACE_UNSUPPORTED,
                    "running executable identity is unavailable",
                ));
            }
            let Ok(locks) = self.lock_witnesses() else {
                return refuse(ErrorBody::new(
                    UPGRADE_IN_PLACE_UNSUPPORTED,
                    "required handover lock is not held",
                ));
            };
            let Some(checksum) = HexBytes::from_hex(expected.as_bytes()) else {
                return refuse(ErrorBody::new(
                    UPGRADE_EXECUTABLE_MISMATCH,
                    "invalid candidate sha256",
                ));
            };
            let shutdown = self.shutdown.borrow();
            if *shutdown || handler.daemon_shutdown.is_set() {
                return refuse(busy("shutdown"));
            }
            let Ok(settings) = self.settings.clone().try_write_owned() else {
                return refuse(busy(if handler.supervisor.upgrade_active() {
                    "upgrade"
                } else {
                    "settings"
                }));
            };
            if handler.supervisor.restart_intent_active() {
                return refuse(busy("shutdown"));
            }
            if let Some(owner) = &handler.config_runtime {
                if let Some(reason) = owner.upgrade_blocker() {
                    return refuse(busy(reason));
                }
            }
            let attempt = match handler.supervisor.begin_upgrade() {
                Ok(attempt) => attempt,
                Err(UpgradeBusy::Upgrade) => return refuse(busy("upgrade")),
                Err(UpgradeBusy::ModuleLifecycle) => return refuse(busy("module_lifecycle")),
            };
            if self.retained.blocks_upgrade() {
                return refuse(busy("retained_children"));
            }
            let runtime = self.clone();
            let forwarding = handler.forwarding.clone();
            let egress = ctx.egress.clone();
            let identity = Arc::new(());
            self.running.send_replace(Some(identity.clone()));
            tokio::spawn(async move {
                struct Done<'a>(&'a tokio::sync::watch::Sender<Option<Arc<()>>>, Arc<()>);
                impl Drop for Done<'_> {
                    fn drop(&mut self) {
                        self.0.send_if_modified(|running| {
                            if running
                                .as_ref()
                                .is_some_and(|key| Arc::ptr_eq(key, &self.1))
                            {
                                *running = None;
                                true
                            } else {
                                false
                            }
                        });
                    }
                }
                let _done = Done(&runtime.running, identity);
                runtime
                    .run(
                        attempt, settings, forwarding, egress, frame, locks, checksum,
                    )
                    .await;
            });
            drop(shutdown);
            Ok(Vec::new())
        }
    }

    pub(crate) fn witness(role: LockRole, file: &File) -> io::Result<LockWitness> {
        let stat = file.metadata()?;
        Ok(LockWitness {
            role,
            fd: file.as_raw_fd(),
            device: stat.dev(),
            inode: stat.ino(),
        })
    }

    fn busy(reason: &str) -> ErrorBody {
        ErrorBody::new(UPGRADE_BUSY, "daemon upgrade is busy")
            .with_detail(serde_json::json!({"reason": reason}))
    }

    #[derive(Default)]
    struct Progress {
        // Stop commands may consume the child before returning an error.
        stop_attempted: Vec<String>,
        stop_failed: BTreeSet<String>,
        // On unwind, replace children whose connections were closed after any
        // attempt to send the notice, even if only one of the two frames got out.
        notice_attempted: BTreeSet<String>,
        // Report only notices for which both the notice and GOODBYE flushed.
        announced: BTreeSet<String>,
        parked: Vec<ParkedChild>,
        deadline: Option<Instant>,
        // An attempted receipt may be on the wire even without a flush ACK.
        receipt_attempted: bool,
    }

    impl Runtime {
        #[allow(clippy::too_many_arguments)]
        async fn run(
            &self,
            attempt: UpgradeAttempt,
            _settings: OwnedRwLockWriteGuard<()>,
            forwarding: Arc<ForwardingTable>,
            egress: crate::FrameSink,
            request: Frame,
            locks: Vec<LockWitness>,
            checksum: HexBytes<32>,
        ) {
            let candidate = match self.boot.verify(checksum, &self.hooks).await {
                Ok(candidate) => candidate,
                Err(body) => {
                    drop(attempt);
                    drop(_settings);
                    if let Ok(reply) = control_error_body_frame(&request, body) {
                        let _ = tokio::time::timeout(REPLY_TIMEOUT, egress.send(reply)).await;
                    }
                    return;
                }
            };
            if *self.shutdown.borrow() {
                return;
            }
            let id = match random().map(HexBytes) {
                Ok(id) => id,
                Err(error) => {
                    drop(attempt);
                    drop(_settings);
                    if let Ok(reply) =
                        control_error_frame(&request, UPGRADE_FAILED, error.to_string())
                    {
                        let _ = tokio::time::timeout(REPLY_TIMEOUT, egress.send(reply)).await;
                    }
                    return;
                }
            };
            let started_at_ms = now_ms();
            let mut progress = Progress::default();
            forwarding.upgrade_refusals.begin();
            #[cfg(feature = "test-support")]
            {
                *forwarding
                    .upgrade_refusals
                    .fixture_flush_gate
                    .lock()
                    .unwrap() = self.hooks.refusal_writer_gate.clone();
            }
            forwarding.set_upgrade_admission(Admission::Fenced);
            let (stage, error) = self
                .perform(
                    &attempt,
                    &forwarding,
                    &egress,
                    &request,
                    candidate,
                    locks,
                    id,
                    started_at_ms,
                    &mut progress,
                )
                .await;
            warn!(attempt_id = ?id, stage = stage.name(), %error, "daemon upgrade unwinding");
            drop(std::mem::take(&mut progress.parked));
            let shutdown = *self.shutdown.borrow();
            forwarding.upgrade_refusals.take();
            forwarding.set_upgrade_admission(Admission::Open);
            let mut restarted = Vec::new();
            let mut restart_failed = BTreeSet::new();
            if !shutdown || progress.deadline.is_none() {
                let restart_stopped = attempt.restart_stopped(progress.stop_attempted);
                let restart_announced = async {
                    match progress.deadline {
                        Some(deadline) => {
                            attempt
                                .restart_announced(
                                    progress.notice_attempted.iter().cloned().collect(),
                                    deadline,
                                )
                                .await
                        }
                        None => Default::default(),
                    }
                };
                let (stopped, announced) = tokio::join!(restart_stopped, restart_announced);
                let announced = announced.into_iter().map(|(id, outcome)| {
                    let replacement = outcome.and_then(|outcome| {
                        if let Some(error) = outcome.release_failed {
                            warn!(module_id = id, %error, "registration release failed before replacing announced child");
                            progress.stop_failed.insert(id.clone());
                        }
                        outcome.replacement.map(|()| true)
                    });
                    (id, replacement)
                });
                for (id, result) in stopped.into_iter().chain(announced) {
                    match result {
                        Ok(true) => restarted.push(id),
                        Ok(false) => {}
                        Err(error) => {
                            warn!(module_id = id, %error, "upgrade unwind could not restart module");
                            restart_failed.insert(id);
                        }
                    }
                }
            }
            restarted.sort();
            restarted.dedup();
            *self.last_failure.lock().unwrap_or_else(|p| p.into_inner()) =
                Some(serde_json::json!({
                    "attempt_id": id, "started_at_ms": started_at_ms, "outcome": "failed",
                    "stage": stage.name(), "restarted": restarted,
                    "stop_failed": progress.stop_failed, "restart_failed": restart_failed,
                }));
            drop(attempt);
            drop(_settings);
            if !progress.receipt_attempted {
                let body = ErrorBody::new(UPGRADE_FAILED, "daemon upgrade failed").with_detail(serde_json::json!({
                    "stage": stage.name(), "attempt_id": id, "announced": progress.announced, "restarted": restarted,
                    "stop_failed": progress.stop_failed, "restart_failed": restart_failed,
                }));
                if let Ok(reply) = control_error_body_frame(&request, body) {
                    let _ = tokio::time::timeout(REPLY_TIMEOUT, egress.send(reply)).await;
                }
            }
        }

        #[allow(clippy::too_many_arguments)]
        async fn perform(
            &self,
            attempt: &UpgradeAttempt,
            forwarding: &ForwardingTable,
            egress: &crate::FrameSink,
            request: &Frame,
            candidate: Candidate,
            locks: Vec<LockWitness>,
            id: HexBytes<16>,
            started_at_ms: u64,
            progress: &mut Progress,
        ) -> (Stage, io::Error) {
            match self.prepare(attempt, forwarding, progress).await {
                Ok((cohort, reconnect_by_ms)) => {
                    let path = self
                        .run_dir
                        .live_children_record()
                        .with_file_name(handover::HANDOVER_FILE_NAME);
                    let prepared =
                        match PreparedExec::new(&self.boot, &path, self.hooks.exec_path.as_deref())
                        {
                            Ok(prepared) => prepared,
                            Err(error) => return (Stage::HandoverWrite, error),
                        };
                    // Stop reading client input but leave all writers open.
                    // Requests arriving after this cut are not read or refused;
                    // their peers see the connection close, as on any exec race.
                    #[cfg(feature = "test-support")]
                    if let Some(path) = &self.hooks.freeze_gate {
                        if let Err(error) = fixture_tests::wait_at_gate(path, 1).await {
                            return (Stage::HandoverWrite, error);
                        }
                    }
                    let pending_readers = forwarding.freeze_upgrade_readers();
                    #[cfg(feature = "test-support")]
                    if let Some(path) = &self.hooks.freeze_gate {
                        if let Err(error) = fixture_tests::wait_at_gate(path, 2).await {
                            return (Stage::HandoverWrite, error);
                        }
                    }
                    // The receipt covers refusals for every request read before
                    // the freeze, including those still waiting for route credit.
                    let active_binds = forwarding
                        .route_bind_concurrency()
                        .track_upgrade_producers();
                    let drain = forwarding
                        .upgrade_refusals
                        .drain_after_freeze(REPLY_TIMEOUT, active_binds, &pending_readers)
                        .await;
                    let reply = match control_response_body_frame(
                        request,
                        &serde_json::json!({
                            "outcome": "exec_started", "attempt_id": id, "carried": cohort.carried, "restarting": cohort.restarting,
                            "refusals_tracked": drain.refusals_tracked,
                            "refusals_unconfirmed": drain.refusals_unconfirmed,
                            "settled_in_time": drain.settled_in_time,
                            "unacknowledged_connections": drain.unacknowledged_connections,
                            "unacknowledged_tasks": drain.unacknowledged_tasks,
                            "expected": {"core_version": candidate.build.core_version, "build_git_sha": candidate.build.build_git_sha},
                        }),
                        "daemon upgrade receipt",
                    ) {
                        Ok(frame) => frame,
                        Err(error) => return (Stage::HandoverWrite, io::Error::other(error)),
                    };
                    progress.receipt_attempted = true;
                    // The fixture finishes its pathname change and disconnect
                    // before the real receipt timer starts.
                    #[cfg(feature = "test-support")]
                    if let Some(gate) = &self.hooks.receipt_gate {
                        if let Err(error) = fixture_tests::wait_at_gate(gate, 1).await {
                            return (Stage::HandoverWrite, error);
                        }
                    }
                    let mut shutdown = self.shutdown.subscribe();
                    tokio::select! {
                        biased;
                        _ = shutdown.wait_for(|set| *set) => return (Stage::HandoverWrite, interrupted()),
                        _ = tokio::time::timeout(REPLY_TIMEOUT, async {
                            #[cfg(feature = "test-support")]
                            if self.hooks.receipt_gate.is_some() {
                                std::future::pending::<()>().await;
                            }
                            let _ = egress.send_flushed(reply).await;
                        }) => {}
                    }
                    forwarding.close_all_connections(&CloseReason::new(
                        "daemon_upgrade",
                        "daemon image is changing",
                    ));
                    // A child can exit while this receipt was queued or flushing.
                    let mut cohort = cohort;
                    match attempt.remove_exited(&mut progress.parked).await {
                        Ok(exited) => remove_exited(&mut cohort, exited),
                        Err(error) => return (Stage::HandoverWrite, error),
                    }
                    let document = Handover {
                        format: handover::FORMAT,
                        old_image_sha256: self
                            .boot
                            .digest()
                            .expect("upgrade admitted with a boot digest"),
                        verified_image: candidate.identity.clone(),
                        attempt_id: id,
                        started_at_ms,
                        reconnect_by_ms,
                        monotonic_ns: 0,
                        cohort,
                        locks,
                        children: progress
                            .parked
                            .iter()
                            .map(|child| child.witness.clone())
                            .collect(),
                    };
                    let shutdown = self.shutdown.borrow();
                    if *shutdown {
                        return (Stage::HandoverWrite, interrupted());
                    }
                    let run = || {
                        prepared.write_and_exec(
                            &self.boot,
                            &path,
                            document,
                            &candidate.file,
                            &self.hooks,
                        )
                    };
                    if tokio::runtime::Handle::current().runtime_flavor()
                        == tokio::runtime::RuntimeFlavor::MultiThread
                    {
                        tokio::task::block_in_place(run)
                    } else {
                        run()
                    }
                }
                Err(failure) => failure,
            }
        }

        async fn prepare(
            &self,
            attempt: &UpgradeAttempt,
            forwarding: &ForwardingTable,
            progress: &mut Progress,
        ) -> Result<(handover::Cohort, u64), (Stage, io::Error)> {
            let selected = attempt
                .select()
                .await
                .map_err(|e| (Stage::PlannedStop, e))?;
            if *self.shutdown.borrow() {
                return Err((Stage::PlannedStop, interrupted()));
            }
            let mut failure = None;
            progress.stop_attempted.clone_from(&selected.restarting);
            for (id, result) in attempt.stop_restarting(selected.restarting.clone()).await {
                if let Err(error) = result {
                    progress.stop_failed.insert(id);
                    failure = Some(error);
                }
            }
            if let Some(error) = failure {
                return Err((Stage::PlannedStop, error));
            }
            if *self.shutdown.borrow() {
                return Err((Stage::PlannedStop, interrupted()));
            }
            forwarding.set_upgrade_admission(Admission::ListenersPaused);
            if *self.shutdown.borrow() {
                return Err((Stage::ListenerClose, interrupted()));
            }

            let reconnect_by_ms = now_ms().saturating_add(RECONNECT_WINDOW.as_millis() as u64);
            progress.deadline = Some(Instant::now() + RECONNECT_WINDOW);
            self.announce(forwarding, &selected.carried, reconnect_by_ms, progress)
                .await
                .map_err(|e| (Stage::ReaderPark, e))?;
            if *self.shutdown.borrow() {
                return Err((Stage::ReaderPark, interrupted()));
            }
            let mut cohort = handover::Cohort {
                carried: selected.carried.keys().cloned().collect(),
                restarting: selected.restarting,
                stopped: selected.stopped,
                config_enabled: selected.config_enabled,
            };
            progress.parked = attempt
                .park(selected.carried)
                .await
                .map_err(|e| (Stage::ReaderPark, e))?;
            let exited = attempt
                .remove_exited(&mut progress.parked)
                .await
                .map_err(|e| (Stage::ReaderPark, e))?;
            progress.stop_attempted.extend(
                exited
                    .iter()
                    .filter(|id| !progress.notice_attempted.contains(*id))
                    .cloned(),
            );
            if cohort
                .carried
                .iter()
                .any(|id| !progress.announced.contains(id) && !exited.contains(id))
            {
                return Err((
                    Stage::ReaderPark,
                    io::Error::other("upgrade notice never reached a live child writer"),
                ));
            }
            remove_exited(&mut cohort, exited);
            Ok((cohort, reconnect_by_ms))
        }

        async fn announce(
            &self,
            forwarding: &ForwardingTable,
            carried: &BTreeMap<String, handover::ChildWitness>,
            deadline: u64,
            progress: &mut Progress,
        ) -> io::Result<()> {
            let body =
                serde_json::to_vec(&subc_protocol::session::ModuleControlCommand::Upgrading {
                    reconnect_by_ms: deadline,
                })?;
            let connections = forwarding.module_connections().map_err(io::Error::other)?;
            let inject_only = self
                .hooks
                .notice_failure
                .as_deref()
                .filter(|id| carried.contains_key(*id));
            let mut sends = tokio::task::JoinSet::new();
            let budget = Instant::now() + Duration::from_millis(500);
            for target in connections {
                if !carried.contains_key(&target.module_id) {
                    continue;
                }
                let inject = inject_only == Some(target.module_id.as_str());
                if inject_only.is_some() && !inject {
                    continue;
                }
                let notice = Frame::build_with_version(
                    target.negotiated_ver,
                    FrameType::Push,
                    control_flags(),
                    0,
                    0,
                    0,
                    body.clone(),
                )
                .map_err(io::Error::other)?;
                let goodbye = Frame::build_with_version(
                    target.negotiated_ver,
                    FrameType::Goodbye,
                    control_flags(),
                    0,
                    0,
                    0,
                    Vec::new(),
                )
                .map_err(io::Error::other)?;
                progress.notice_attempted.insert(target.module_id.clone());
                sends.spawn(async move {
                    let flushed = !inject
                        && matches!(
                            tokio::time::timeout_at(budget, async {
                                // Both control frames use the same FIFO as data
                                // requests. Their flush also covers every request
                                // queued before the admission fence.
                                target.sink.send_flushed(notice).await?;
                                target.sink.send_upgrade_goodbye(goodbye).await
                            })
                            .await,
                            Ok(Ok(()))
                        );
                    (target.module_id, target.endpoint.connection_id, flushed)
                });
            }
            while let Some(result) = sends.join_next().await {
                let (id, connection, flushed) = result.map_err(io::Error::other)?;
                if flushed {
                    progress.announced.insert(id);
                } else {
                    warn!(module_id = id, "upgrade notice or GOODBYE did not flush");
                }
                forwarding.request_connection_close(
                    connection,
                    CloseReason::new("daemon_upgrade", "daemon image is changing"),
                );
            }
            if progress.announced.len() == carried.len() {
                Ok(())
            } else {
                Err(io::Error::other(
                    "not every carried module received its upgrade notice and GOODBYE",
                ))
            }
        }
    }

    fn remove_exited(cohort: &mut handover::Cohort, exited: Vec<String>) {
        cohort.carried.retain(|id| !exited.contains(id));
        cohort.restarting.extend(exited);
        cohort.restarting.sort();
    }

    fn interrupted() -> io::Error {
        io::Error::new(io::ErrorKind::Interrupted, "daemon received SIGTERM")
    }

    #[derive(Debug, Clone, Copy)]
    enum Stage {
        PlannedStop,
        ListenerClose,
        ReaderPark,
        HandoverWrite,
        FdFlags,
        Rehash,
        Exec,
    }

    impl Stage {
        fn name(self) -> &'static str {
            match self {
                Self::PlannedStop => "planned_stop",
                Self::ListenerClose => "listener_close",
                Self::ReaderPark => "reader_park",
                Self::HandoverWrite => "handover_write",
                Self::FdFlags => "fd_flags",
                Self::Rehash => "rehash",
                Self::Exec => "exec",
            }
        }
    }

    #[derive(Deserialize)]
    struct BuildInfo {
        core_version: String,
        build_git_sha: Option<String>,
        handover_versions: Vec<u32>,
    }

    struct Candidate {
        file: File,
        identity: VerifiedImage,
        build: BuildInfo,
    }

    pub(crate) struct BootImage {
        path: PathBuf,
        sha256: tokio::sync::watch::Receiver<Option<HexBytes<32>>>,
        core_version: String,
        build_git_sha: Option<String>,
        args: Vec<OsString>,
        environment: Vec<(OsString, OsString)>,
        #[cfg(test)]
        fixture_probe_marker: Option<PathBuf>,
        #[cfg(target_os = "macos")]
        team: Option<String>,
    }

    impl BootImage {
        pub(crate) async fn capture(
            path: PathBuf,
            image: Arc<File>,
            core_version: String,
            build_git_sha: Option<String>,
            verified_digest: Option<HexBytes<32>>,
            _hooks: &Hooks,
        ) -> Self {
            let (result, sha256) = tokio::sync::watch::channel(verified_digest);
            if verified_digest.is_none() {
                #[cfg(feature = "test-support")]
                let boot_hash_gate = _hooks.boot_hash_gate.clone();
                tokio::spawn(async move {
                    #[cfg(feature = "test-support")]
                    if let Some(gate) = boot_hash_gate {
                        if let Err(error) = fixture_tests::wait_at_gate(&gate, 1).await {
                            warn!(%error, "in-place upgrade disabled: boot hash fixture gate failed");
                            return;
                        }
                    }
                    match tokio::task::spawn_blocking(move || hash_file(&image))
                        .await
                        .map_err(io::Error::other)
                        .and_then(|digest| digest)
                    {
                        Ok(digest) => {
                            result.send_replace(Some(digest));
                        }
                        Err(error) => {
                            warn!(%error, "in-place upgrade disabled: could not hash running executable")
                        }
                    }
                });
            }
            #[cfg(target_os = "macos")]
            let team = team_identifier(&path).await;
            Self {
                path,
                sha256,
                core_version,
                build_git_sha,
                args: std::env::args_os().collect(),
                environment: std::env::vars_os().collect(),
                #[cfg(test)]
                fixture_probe_marker: None,
                #[cfg(target_os = "macos")]
                team,
            }
        }

        fn digest(&self) -> Option<HexBytes<32>> {
            *self.sha256.borrow()
        }

        pub(crate) async fn wait_for_digest(&self) -> Option<HexBytes<32>> {
            let mut digest = self.sha256.clone();
            let ready = tokio::time::timeout(BOOT_IDENTITY_WAIT, digest.wait_for(Option::is_some))
                .await
                .ok()?
                .ok()?;
            *ready
        }

        #[cfg(test)]
        pub(crate) fn with_fixture_probe(mut self, marker: PathBuf) -> Self {
            self.fixture_probe_marker = Some(marker);
            self
        }

        async fn verify(
            &self,
            checksum: HexBytes<32>,
            hooks: &Hooks,
        ) -> Result<Candidate, ErrorBody> {
            let mismatch = || {
                ErrorBody::new(
                    UPGRADE_EXECUTABLE_MISMATCH,
                    "startup executable path is unreadable or its sha256 does not match",
                )
            };
            let path = self.path.clone();
            let (file, stamp, digest) = tokio::task::spawn_blocking(move || {
                let file = open_image(&path)?;
                let (stamp, digest) = hash_candidate(&file)?;
                Ok::<_, io::Error>((file, stamp, digest))
            })
            .await
            .map_err(|_| mismatch())?
            .map_err(|_| mismatch())?;
            if digest != checksum {
                return Err(mismatch());
            }
            let unchanged = || {
                let file_matches = ImageStamp::read(&file).ok() == Some(stamp);
                #[cfg(target_os = "macos")]
                let file_matches = file_matches && stamp.matches_path(&self.path);
                file_matches
            };
            if !unchanged() {
                return Err(mismatch());
            }
            #[cfg(target_os = "macos")]
            let cdhash = match verify_signature(&self.path, self.team.as_deref(), hooks).await {
                Ok(cdhash) => cdhash,
                Err(()) => {
                    if !unchanged() {
                        return Err(mismatch());
                    }
                    return Err(ErrorBody::new(
                        UPGRADE_SIGNATURE_MISMATCH,
                        "candidate signature does not match the running image's team",
                    ));
                }
            };
            #[cfg(target_os = "linux")]
            let cdhash = {
                let _ = hooks;
                None
            };
            #[cfg(target_os = "macos")]
            if !unchanged() {
                return Err(mismatch());
            }
            #[cfg(target_os = "linux")]
            let probe_path = PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()));
            #[cfg(target_os = "macos")]
            let probe_path = self.path.clone();
            let mut probe = tokio::process::Command::new(&probe_path);
            probe.arg("--build-info");
            #[cfg(test)]
            if let Some(marker) = &self.fixture_probe_marker {
                let root = marker
                    .parent()
                    .expect("fixture marker has a scratch directory");
                probe
                    .env("SUBC_TEST_UPGRADE_TARGET_MARKER", marker)
                    .env("XDG_DATA_HOME", root)
                    .env("XDG_RUNTIME_DIR", root)
                    .env("XDG_CONFIG_HOME", root);
            }
            let build = bounded_output(probe)
                .await
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| serde_json::from_slice::<BuildInfo>(&output.stdout).ok())
                .filter(|build| build.handover_versions.contains(&handover::FORMAT));
            if !unchanged() {
                return Err(mismatch());
            }
            let build = build.ok_or_else(|| {
                ErrorBody::new(
                    UPGRADE_TARGET_INCOMPATIBLE,
                    "candidate build-info did not declare support for this handover version",
                )
            })?;
            Ok(Candidate {
                file,
                identity: VerifiedImage {
                    sha256: checksum,
                    cdhash,
                },
                build,
            })
        }
    }

    fn open_image(path: &Path) -> io::Result<File> {
        // Nonblocking open and the type check keep accidental FIFOs or devices
        // at the startup pathname from tying up admission.
        let file = File::from(rustix::fs::open(
            path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::NONBLOCK | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        )?);
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("executable is not a regular file"));
        }
        Ok(file)
    }

    fn hash_file(mut file: &File) -> io::Result<HexBytes<32>> {
        let mut hash = Sha256::new();
        file.seek(SeekFrom::Start(0))?;
        io::copy(&mut file, &mut hash)?;
        Ok(HexBytes(hash.finalize().into()))
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct ImageStamp {
        dev: u64,
        inode: u64,
        bytes: u64,
        changed: (i64, i64),
        modified: (i64, i64),
    }

    impl ImageStamp {
        fn from_metadata(stat: fs::Metadata) -> Self {
            Self {
                dev: stat.dev(),
                inode: stat.ino(),
                bytes: stat.len(),
                changed: (stat.ctime(), stat.ctime_nsec()),
                modified: (stat.mtime(), stat.mtime_nsec()),
            }
        }

        fn read(file: &File) -> io::Result<Self> {
            file.metadata().map(Self::from_metadata)
        }

        #[cfg(target_os = "macos")]
        fn matches_path(self, path: &Path) -> bool {
            fs::metadata(path).ok().map(Self::from_metadata) == Some(self)
        }
    }

    fn hash_candidate(file: &File) -> io::Result<(ImageStamp, HexBytes<32>)> {
        hash_candidate_after_stamp(file, || Ok(()))
    }

    fn hash_candidate_after_stamp(
        file: &File,
        after_stamp: impl FnOnce() -> io::Result<()>,
    ) -> io::Result<(ImageStamp, HexBytes<32>)> {
        let stamp = ImageStamp::read(file)?;
        after_stamp()?;
        let digest = hash_file(file)?;
        if ImageStamp::read(file)? != stamp {
            return Err(io::Error::other("candidate changed while hashing"));
        }
        Ok((stamp, digest))
    }

    async fn bounded_output(mut command: tokio::process::Command) -> io::Result<Output> {
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = child_spawn::tokio_spawn(&mut command)?;
        let read = async {
            let mut stdout = Vec::new();
            let mut stderr = Vec::new();
            let mut out = child.stdout.take().expect("piped stdout");
            let mut err = child.stderr.take().expect("piped stderr");
            let (mut out_live, mut err_live) = (true, true);
            let mut chunk = [0; 4096];
            let mut errors = [0; 4096];
            while out_live || err_live {
                let (data, count, live) = tokio::select! {
                    count = out.read(&mut chunk), if out_live => ((&mut stdout, &chunk), count?, &mut out_live),
                    count = err.read(&mut errors), if err_live => ((&mut stderr, &errors), count?, &mut err_live),
                };
                *live = count != 0;
                let (sink, buffer) = data;
                sink.extend_from_slice(&buffer[..count]);
                if (stdout.len() + stderr.len()) as u64 > PROBE_LIMIT {
                    return Err(io::Error::other("candidate probe exceeded output limit"));
                }
            }
            Ok(Output {
                status: child.wait().await?,
                stdout,
                stderr,
            })
        };
        match tokio::time::timeout(PROBE_TIMEOUT, read).await {
            Ok(Ok(output)) => Ok(output),
            error => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                Err(match error {
                    Ok(Err(error)) => error,
                    _ => io::Error::new(io::ErrorKind::TimedOut, "candidate probe timed out"),
                })
            }
        }
    }

    #[cfg(target_os = "macos")]
    async fn team_identifier(path: &Path) -> Option<String> {
        let mut command = tokio::process::Command::new("/usr/bin/codesign");
        command.args(["-d", "--verbose=4", "--"]).arg(path);
        let output = bounded_output(command).await.ok()?;
        if !output.status.success() {
            return None;
        }
        let info = std::str::from_utf8(&output.stderr).ok()?;
        let team = info
            .lines()
            .find_map(|line| line.strip_prefix("TeamIdentifier="))?;
        (!team.is_empty() && team.bytes().all(|b| b.is_ascii_alphanumeric()))
            .then(|| team.to_owned())
    }

    #[cfg(target_os = "macos")]
    async fn verify_signature(
        path: &Path,
        running_team: Option<&str>,
        hooks: &Hooks,
    ) -> Result<Option<HexBytes<20>>, ()> {
        let mut detail = tokio::process::Command::new("/usr/bin/codesign");
        let arch = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        detail
            .args(["-d", "--verbose=4", "-a", arch, "--"])
            .arg(path);
        let detail = bounded_output(detail)
            .await
            .ok()
            .filter(|output| output.status.success());
        let detail = detail
            .as_ref()
            .and_then(|out| std::str::from_utf8(&out.stderr).ok());
        let field = |prefix: &str| {
            detail.and_then(|text| text.lines().find_map(|line| line.strip_prefix(prefix)))
        };
        let cdhash = field("CDHash=").and_then(|hash| HexBytes::from_hex(hash.as_bytes()));
        let required_team = if hooks.enabled {
            match hooks.team.as_deref() {
                Some(team) => team,
                None => return Ok(cdhash),
            }
        } else {
            match running_team {
                Some(team) => team,
                None => return Err(()),
            }
        };
        let mut command = tokio::process::Command::new("/usr/bin/codesign");
        command.args(["--verify", "--strict", "--"]).arg(path);
        if cdhash.is_some()
            && field("TeamIdentifier=") == Some(required_team)
            && bounded_output(command)
                .await
                .is_ok_and(|out| out.status.success())
        {
            Ok(cdhash)
        } else {
            Err(())
        }
    }

    struct PreparedExec {
        key_read: PipeReader,
        key_write: PipeWriter,
        key: [u8; 32],
        exec_path: CString,
        #[cfg(target_os = "linux")]
        override_path: bool,
        exec: ExecImage,
    }

    impl PreparedExec {
        fn new(boot: &BootImage, path: &Path, exec_override: Option<&Path>) -> io::Result<Self> {
            let (key_read, mut key_write) = io::pipe()?;
            let key = random()?;
            key_write.write_all(&key)?;
            let mut cold = Vec::new();
            let mut args = boot.args.iter();
            cold.push(
                args.next()
                    .map_or(boot.path.as_os_str(), OsString::as_os_str),
            );
            while let Some(arg) = args.next() {
                if arg == "--resume-handover" || arg == "--handover-key-fd" {
                    args.next();
                } else {
                    cold.push(arg);
                }
            }
            let fd = key_read.as_raw_fd().to_string();
            cold.extend([
                OsStr::new("--resume-handover"),
                path.as_os_str(),
                OsStr::new("--handover-key-fd"),
                OsStr::new(&fd),
            ]);
            let args = cold
                .into_iter()
                .map(|s| CString::new(s.as_bytes()))
                .collect::<Result<_, _>>()?;
            let env = boot
                .environment
                .iter()
                .map(|(name, value)| {
                    let mut out = name.as_bytes().to_vec();
                    out.push(b'=');
                    out.extend_from_slice(value.as_bytes());
                    CString::new(out)
                })
                .collect::<Result<_, _>>()?;
            Ok(Self {
                key_read,
                key_write,
                key,
                exec_path: CString::new(
                    exec_override.unwrap_or(&boot.path).as_os_str().as_bytes(),
                )?,
                #[cfg(target_os = "linux")]
                override_path: exec_override.is_some(),
                exec: ExecImage::new(args, env),
            })
        }

        fn write_and_exec(
            self,
            boot: &BootImage,
            path: &Path,
            mut document: Handover,
            candidate: &File,
            hooks: &Hooks,
        ) -> (Stage, io::Error) {
            let mut write = || -> io::Result<HandoverFile> {
                if hooks
                    .write_failure
                    .swap(false, std::sync::atomic::Ordering::Relaxed)
                {
                    return Err(io::Error::other("injected handover write failure"));
                }
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(path)?;
                let unlink = HandoverFile(path.to_path_buf());
                document.monotonic_ns = child_spawn::monotonic_ns()?;
                let bytes = handover::encode(&document, &self.key).map_err(io::Error::other)?;
                #[cfg(feature = "test-support")]
                let bytes = if hooks.corrupt_hmac {
                    let mut bytes = bytes;
                    let tag = bytes
                        .iter()
                        .position(|&byte| byte == b'\n')
                        .expect("encoded envelope has two lines")
                        + 1;
                    bytes[tag] = if bytes[tag] == b'0' { b'1' } else { b'0' };
                    bytes
                } else {
                    bytes
                };
                file.write_all(&bytes)?;
                file.sync_all()?;
                Ok(unlink)
            };
            let _handover = match write() {
                Ok(file) => file,
                Err(error) => return (Stage::HandoverWrite, error),
            };
            let mut fds = BTreeSet::from([self.key_read.as_raw_fd()]);
            let members = document.locks.iter().map(|lock| lock.fd).chain(
                document
                    .children
                    .iter()
                    .flat_map(|child| [&child.stdout, &child.stderr])
                    .filter_map(|stream| match stream {
                        handover::StreamWitness::Open { fd, .. } => Some(*fd),
                        handover::StreamWitness::Ended {} => None,
                    }),
            );
            for fd in members {
                if !fds.insert(fd) {
                    return (
                        Stage::FdFlags,
                        io::Error::other("duplicate handover descriptor"),
                    );
                }
            }
            let target = ExecTarget::Path(&self.exec_path);
            #[cfg(target_os = "linux")]
            let target = if self.override_path {
                target
            } else {
                ExecTarget::Descriptor(candidate.as_fd())
            };
            match child_spawn::exec_with_descriptors(
                target,
                &self.exec,
                &fds,
                self.key_write,
                || {
                    let expected = document.verified_image.sha256;
                    // This final digest is the content guard. The candidate
                    // file can still change after its first digest: while its
                    // `--build-info` probe runs, and while modules drain before
                    // exec. The size, ctime and mtime checks in between only
                    // catch such a change earlier without rereading the image;
                    // only this digest proves the bytes we exec are the ones
                    // the operator approved.
                    let (stamp, digest) = hash_candidate_after_stamp(candidate, || {
                        // Pause after the first metadata sample so the fixture
                        // can verify that a change during hashing is refused.
                        #[cfg(feature = "test-support")]
                        if let Some(gate) = &hooks.receipt_gate {
                            fixture_tests::wait_at_gate_blocking(gate, 2)?;
                        }
                        Ok(())
                    })?;
                    #[cfg(target_os = "linux")]
                    let _ = (boot, stamp);
                    #[cfg(target_os = "macos")]
                    if !stamp.matches_path(&boot.path) {
                        return Err(io::Error::other("candidate pathname changed before exec"));
                    }
                    if digest != expected {
                        return Err(io::Error::other("candidate sha256 changed before exec"));
                    }
                    Ok(())
                },
            ) {
                ExecImageError::DescriptorFlags(error) => (Stage::FdFlags, error),
                ExecImageError::Recheck(error) => (Stage::Rehash, error),
                ExecImageError::Exec(error) => (Stage::Exec, error),
            }
        }
    }

    struct HandoverFile(PathBuf);

    impl Drop for HandoverFile {
        fn drop(&mut self) {
            if let Err(error) = fs::remove_file(&self.0) {
                warn!(%error, "failed to remove daemon upgrade handover after unwind");
            }
        }
    }

    fn random<const N: usize>() -> io::Result<[u8; N]> {
        let mut bytes = [0; N];
        getrandom::getrandom(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(bytes)
    }
}
