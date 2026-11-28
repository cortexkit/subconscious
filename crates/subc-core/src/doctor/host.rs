use std::{
    collections::BTreeMap,
    fs,
    future::Future,
    io::ErrorKind,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc,
    },
    thread,
    time::{Duration, Instant},
};

use subc_client_rs::{ConnectionState, ConsumerOptions, SubcConsumer, SupervisorListResponse};
use subc_control::{StderrTail, SupervisorHealthEntry, TerminalHistory};
use subc_daemon::daemon_config;
use subc_os::{host::DiskSpace, ResourceUsage};
use subc_transport::connection_file;
use tokio::{
    runtime::Handle,
    sync::{oneshot, Notify},
};

use crate::{
    setup::{
        self,
        runtime::{self, RuntimePlatform, SystemCommandRunner},
    },
    stack::{self, manager::Job, MarkerState},
    window::{RevertResult, Windows},
};

use super::{stack_checks::Binary, window_checks::WindowRow, ProcessEntry};

#[derive(Default)]
pub(crate) struct DescribeOutcome {
    pub(crate) answered: bool,
    pub(crate) connection_pid_hint: Option<u32>,
    pub(crate) override_path: Option<PathBuf>,
}

pub(crate) trait DoctorHost: Send + Sync + 'static {
    fn binaries(&self) -> Result<[Binary; 2], String>;
    fn binary_present(&self, path: &Path) -> Result<bool, String>;
    fn versions(&self) -> Result<BTreeMap<String, String>, String>;
    fn manager(&self) -> (Result<Job, String>, MarkerState);
    fn process_entry(&self, pid: u32) -> Option<ProcessEntry>;
    fn process_start_time_ms(&self, pid: u32) -> Option<u64>;
    fn process_start_time(&self, pid: u32) -> Option<u64>;
    fn resource_usage(&self, pid: u32) -> Option<ResourceUsage>;
    fn wait_for_cpu_sample(&self) {
        thread::sleep(Duration::from_secs(3));
    }
    fn data_directory(&self) -> Result<PathBuf, String>;
    fn settings_directory(&self) -> Option<PathBuf>;
    fn local_connection_file(&self) -> Result<Option<PathBuf>, String>;
    fn disk_space(&self, path: &Path) -> Option<DiskSpace>;
    fn windows(&self) -> Vec<WindowRow>;
    fn configured_modules(&self) -> Result<Vec<String>, String>;
    fn describe(
        &self,
        path: Option<&Path>,
        deadline: Instant,
        hint: &dyn Fn(u32),
    ) -> RpcResult<Arc<dyn DoctorDaemon>>;
    fn revert(&self, id: &str) -> RevertResult;
}

pub(crate) type RpcResult<T> = Result<T, RpcError>;

#[derive(Debug)]
pub(crate) enum RpcError {
    Failed(String),
    TimedOut,
    ConnectionLost,
}

pub(crate) enum SetEnabledError {
    Reply(String),
    Rpc(RpcError),
}

pub(crate) trait DoctorDaemon: Send + Sync + 'static {
    fn list(&self, deadline: Instant) -> RpcResult<SupervisorListResponse>;
    fn health(&self, deadline: Instant) -> RpcResult<Vec<SupervisorHealthEntry>>;
    fn terminals(&self, module: &str, deadline: Instant) -> RpcResult<TerminalHistory>;
    fn stderr_tail(
        &self,
        module: &str,
        max_lines: u32,
        max_bytes: u32,
        deadline: Instant,
    ) -> RpcResult<StderrTail>;

    fn set_enabled(
        &self,
        _module: &str,
        _enabled: bool,
        _deadline: Instant,
    ) -> Result<bool, SetEnabledError> {
        Err(SetEnabledError::Rpc(RpcError::Failed("unsupported".into())))
    }
}

pub(super) struct SystemHost {
    pub(super) call_timeout: Duration,
}

impl Default for SystemHost {
    fn default() -> Self {
        Self {
            call_timeout: Duration::from_secs(4),
        }
    }
}

impl DoctorHost for SystemHost {
    fn binaries(&self) -> Result<[Binary; 2], String> {
        let root = setup::data_directory()?;
        let binary_home = root.join("bin");
        let platform = RuntimePlatform::current();
        let daemon = runtime::runtime_paths(platform, &binary_home, &setup::user_home()?).daemon;
        let sign = platform == RuntimePlatform::Macos;
        let bundle = sign.then(|| daemon.ancestors().nth(3).unwrap_or(&daemon).to_path_buf());
        Ok([
            Binary {
                name: "ck-subc",
                signature: bundle,
                path: daemon,
                legacy_path: None,
            },
            Binary::ck(&root, sign),
        ])
    }

    fn binary_present(&self, path: &Path) -> Result<bool, String> {
        match fs::metadata(path) {
            Ok(metadata) => Ok(metadata.is_file()),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(false),
            Err(error) => Err(format!("{}: {error}", path.display())),
        }
    }

    fn versions(&self) -> Result<BTreeMap<String, String>, String> {
        Ok(setup::dashboard_installed_binaries()?
            .into_iter()
            .map(|(binary, info)| (binary, info.version))
            .collect())
    }

    fn manager(&self) -> (Result<Job, String>, MarkerState) {
        match setup::data_directory() {
            Ok(root) => stack::observe_manager_and_marker(
                RuntimePlatform::current(),
                &mut SystemCommandRunner,
                &root,
            ),
            Err(error) => (Err(error), MarkerState::Unknown),
        }
    }

    fn process_entry(&self, pid: u32) -> Option<ProcessEntry> {
        subc_os::Process::open(pid)
            .ok()
            .flatten()
            .map(|_| ProcessEntry {
                pid,
                start_time_ms: subc_os::process_start_time_ms(pid),
            })
    }

    fn process_start_time_ms(&self, pid: u32) -> Option<u64> {
        subc_os::process_start_time_ms(pid)
    }

    fn process_start_time(&self, pid: u32) -> Option<u64> {
        subc_os::start_time(pid)
    }

    fn resource_usage(&self, pid: u32) -> Option<ResourceUsage> {
        subc_os::resource_usage(pid)
    }

    fn data_directory(&self) -> Result<PathBuf, String> {
        setup::data_directory()
    }

    fn settings_directory(&self) -> Option<PathBuf> {
        daemon_config::default_config_path()
            .parent()
            .map(Path::to_owned)
    }

    fn local_connection_file(&self) -> Result<Option<PathBuf>, String> {
        for path in connection_file::discovery_candidates(None, None) {
            match fs::symlink_metadata(&path) {
                Ok(_) => return Ok(Some(path)),
                Err(error) if error.kind() == ErrorKind::NotFound => (),
                Err(error) => return Err(format!("{}: {error}", path.display())),
            }
        }
        Ok(None)
    }

    fn disk_space(&self, path: &Path) -> Option<DiskSpace> {
        subc_os::host::disk_space(path)
    }

    fn windows(&self) -> Vec<WindowRow> {
        read_windows(self.data_directory(), || {
            Ok(Windows::discover(None)
                .map_err(|error| error.message)?
                .disk_rows()
                .into_iter()
                .map(|row| (row.row, row.owner_liveness))
                .collect())
        })
    }

    fn configured_modules(&self) -> Result<Vec<String>, String> {
        Ok(daemon_config::load(daemon_config::default_config_path())
            .map_err(|error| error.to_string())?
            .into_iter()
            .flat_map(|config| config.modules)
            .map(|module| module.module_id)
            .collect())
    }

    fn describe(
        &self,
        path: Option<&Path>,
        deadline: Instant,
        hint: &dyn Fn(u32),
    ) -> RpcResult<Arc<dyn DoctorDaemon>> {
        match connection_file::discover(path) {
            Ok(found) => {
                hint(found.info.pid);
                SystemDaemon::connect(found.path, deadline, self.call_timeout)
                    .map(|daemon| daemon as Arc<dyn DoctorDaemon>)
            }
            Err(error) => {
                // Even a file with an unsupported wire version can supply a PID hint.
                for candidate in connection_file::discovery_candidates(path, None) {
                    if Instant::now() >= deadline {
                        break;
                    }
                    if let Ok(info) = connection_file::read(candidate) {
                        hint(info.pid);
                        break;
                    }
                }
                Err(RpcError::Failed(error.to_string()))
            }
        }
    }

    fn revert(&self, id: &str) -> RevertResult {
        match Windows::discover(None) {
            Ok(windows) => windows.revert(Some(id)),
            Err(error) => {
                let mut result = RevertResult::default();
                result.report.message = Some(error.message);
                result
            }
        }
    }
}

pub(super) fn read_windows(
    data: Result<PathBuf, String>,
    discover: impl FnOnce() -> Result<Vec<WindowRow>, String>,
) -> Vec<WindowRow> {
    let rows = data.and_then(|path| match path.try_exists() {
        Ok(false) => Ok(Vec::new()),
        Ok(true) => discover(),
        Err(error) => Err(format!("{}: {error}", path.display())),
    });
    rows.unwrap_or_else(|error| vec![(Err(("windows".into(), error)), None)])
}

#[derive(Default)]
pub(super) struct ConnectionFence {
    dropped: AtomicBool,
    notify: Notify,
}

impl ConnectionFence {
    pub(super) fn drop_observed(&self) {
        self.dropped.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn lost(&self) -> bool {
        self.dropped.load(Ordering::Acquire)
    }

    pub(super) async fn read<T, E: std::fmt::Display>(
        &self,
        deadline: Instant,
        read: impl Future<Output = Result<T, E>>,
    ) -> RpcResult<T> {
        let dropped = self.notify.notified();
        tokio::pin!(dropped);
        dropped.as_mut().enable();
        if self.lost() {
            return Err(RpcError::ConnectionLost);
        }
        if Instant::now() >= deadline {
            return Err(RpcError::TimedOut);
        }
        let result = tokio::select! {
            biased;
            () = &mut dropped => return Err(RpcError::ConnectionLost),
            result = tokio::time::timeout_at(deadline.into(), read) => result,
        };
        if self.lost() {
            return Err(RpcError::ConnectionLost);
        }
        match result {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(error)) => Err(RpcError::Failed(error.to_string())),
            Err(_) => Err(RpcError::TimedOut),
        }
    }
}

struct SystemDaemon {
    consumer: Arc<SubcConsumer>,
    fence: Arc<ConnectionFence>,
    runtime: Handle,
    _stay_open: oneshot::Sender<()>,
}

impl SystemDaemon {
    fn connect(path: PathBuf, deadline: Instant, call_timeout: Duration) -> RpcResult<Arc<Self>> {
        let (reply, result) = mpsc::sync_channel(1);
        thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = reply.send(Err(RpcError::Failed(error.to_string())));
                    return;
                }
            };
            runtime.block_on(async move {
                let daemon = Self::open(&path, deadline, call_timeout).await;
                match daemon {
                    Ok((connection, lifetime)) => {
                        if reply.send(Ok(Arc::new(connection))).is_ok() {
                            let _ = lifetime.await;
                        }
                    }
                    Err(error) => {
                        let _ = reply.send(Err(error));
                    }
                }
            });
        });
        receive_rpc(result, deadline)
    }

    async fn open(
        path: &Path,
        deadline: Instant,
        call_timeout: Duration,
    ) -> RpcResult<(Self, oneshot::Receiver<()>)> {
        if Instant::now() >= deadline {
            return Err(RpcError::TimedOut);
        }
        let opts = ConsumerOptions {
            handshake_timeout: Duration::from_secs(2),
            call_timeout,
            ..ConsumerOptions::default()
        };
        let consumer = tokio::time::timeout_at(deadline.into(), SubcConsumer::connect(path, opts))
            .await
            .map_err(|_| RpcError::TimedOut)?
            .map_err(|error| RpcError::Failed(error.to_string()))?;
        let fence = Arc::new(ConnectionFence::default());
        let observed = Arc::clone(&fence);
        // The SDK may reconnect in the background. This pass never uses that
        // connection, and dropping the consumer ends any reconnect attempt.
        consumer.on_connection_state(move |state| {
            if matches!(state, ConnectionState::Dropped) {
                observed.drop_observed();
            }
        });
        fence.read(deadline, consumer.server_describe()).await?;
        let (stay_open, lifetime) = oneshot::channel();
        Ok((
            Self {
                consumer: Arc::new(consumer),
                fence,
                runtime: Handle::current(),
                _stay_open: stay_open,
            },
            lifetime,
        ))
    }

    fn request<T, F, E>(
        &self,
        deadline: Instant,
        call: impl FnOnce(Arc<SubcConsumer>) -> F + Send + 'static,
    ) -> RpcResult<T>
    where
        T: Send + 'static,
        F: Future<Output = Result<T, E>> + Send + 'static,
        E: std::fmt::Display,
    {
        let (reply, result) = mpsc::sync_channel(1);
        let consumer = Arc::clone(&self.consumer);
        let fence = Arc::clone(&self.fence);
        self.runtime.spawn(async move {
            let result = fence
                .read(deadline, async move { call(consumer).await })
                .await;
            let _ = reply.send(result);
        });
        receive_rpc(result, deadline)
    }
}

impl DoctorDaemon for SystemDaemon {
    fn list(&self, deadline: Instant) -> RpcResult<SupervisorListResponse> {
        self.request(deadline, |consumer| async move {
            consumer.supervisor_list().await
        })
    }

    fn health(&self, deadline: Instant) -> RpcResult<Vec<SupervisorHealthEntry>> {
        self.request(deadline, |consumer| async move {
            consumer
                .supervisor_health()
                .await
                .map(|response| response.modules)
        })
    }

    fn terminals(&self, module: &str, deadline: Instant) -> RpcResult<TerminalHistory> {
        let module = module.to_owned();
        self.request(deadline, move |consumer| async move {
            consumer.supervisor_terminals(module).await
        })
    }

    fn stderr_tail(
        &self,
        module: &str,
        max_lines: u32,
        max_bytes: u32,
        deadline: Instant,
    ) -> RpcResult<StderrTail> {
        let module = module.to_owned();
        self.request(deadline, move |consumer| async move {
            consumer
                .supervisor_stderr_tail(module, Some(max_lines), Some(max_bytes))
                .await
                .map(|response| response.tail)
        })
    }

    fn set_enabled(
        &self,
        module: &str,
        enabled: bool,
        deadline: Instant,
    ) -> Result<bool, SetEnabledError> {
        let module = module.to_owned();
        // Keep an actual daemon Error frame separate from a locally failed call:
        // only the former proves the daemon is done handling the request.
        let result = self
            .request(deadline, move |consumer| async move {
                Ok::<_, std::convert::Infallible>(
                    consumer.supervisor_set_enabled(module, enabled).await,
                )
            })
            .map_err(SetEnabledError::Rpc)?;
        match result {
            Ok(ack) => Ok(ack.applied),
            Err(subc_client_rs::CallError::Module(body)) => Err(SetEnabledError::Reply(body.code)),
            Err(error)
                if matches!(
                    error.outcome_cause(),
                    Some(subc_client_rs::OutcomeUnknownCause::ConnectionFailed)
                ) =>
            {
                Err(SetEnabledError::Rpc(RpcError::ConnectionLost))
            }
            Err(error)
                if matches!(
                    error.outcome_cause(),
                    Some(subc_client_rs::OutcomeUnknownCause::Deadline)
                ) =>
            {
                Err(SetEnabledError::Rpc(RpcError::TimedOut))
            }
            Err(_) => Err(SetEnabledError::Rpc(RpcError::Failed(
                "set_enabled failed without an error reply".into(),
            ))),
        }
    }
}

fn receive_rpc<T>(result: mpsc::Receiver<RpcResult<T>>, deadline: Instant) -> RpcResult<T> {
    match result.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(RpcError::TimedOut),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            Err(RpcError::Failed("daemon worker did not finish".into()))
        }
    }
}
