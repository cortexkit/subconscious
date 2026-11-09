//! Bounded report-only reads, isolated from supervisor actors and connection readers.

#[cfg(test)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};
#[cfg(test)]
use subc_control::SubtreeReading;

pub(crate) const READ_TIMEOUT: Duration = Duration::from_millis(200);
const MAX_READERS: usize = 2;

#[derive(Debug, Clone)]
pub(crate) enum Target {
    Unavailable(&'static str),
    #[cfg(target_os = "linux")]
    Cgroup(std::path::PathBuf),
    #[cfg(windows)]
    Job(std::sync::Weak<std::sync::Mutex<subc_jobobject::JobObject>>),
    #[cfg(test)]
    Held(Arc<HeldRead>),
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct HeldRead {
    started: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
pub(crate) fn held() -> (
    Target,
    std::sync::mpsc::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let (started, began) = tokio::sync::oneshot::channel();
    let (release, wait) = std::sync::mpsc::channel();
    (
        Target::Held(Arc::new(HeldRead {
            started: std::sync::Mutex::new(Some(started)),
            release: std::sync::Mutex::new(wait),
        })),
        release,
        began,
    )
}

#[cfg(test)]
pub(crate) async fn fault_read_guard() -> tokio::sync::MutexGuard<'static, ()> {
    // Fault-injection tests share the production worker cap. Do not let their
    // deliberately stalled readers consume each other's test capacity.
    static GUARD: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| tokio::sync::Mutex::new(()))
        .lock()
        .await
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Unavailable(a), Self::Unavailable(b)) => a == b,
            #[cfg(target_os = "linux")]
            (Self::Cgroup(a), Self::Cgroup(b)) => a == b,
            #[cfg(windows)]
            (Self::Job(a), Self::Job(b)) => a.ptr_eq(b),
            #[cfg(test)]
            (Self::Held(a), Self::Held(b)) => Arc::ptr_eq(a, b),
            #[cfg(any(test, target_os = "linux", windows))]
            _ => false,
        }
    }
}

// Kernel container counters remain readable after the last member exits.
// An empty group list must not hide those cumulative CPU and memory totals.
#[cfg(any(target_os = "linux", windows))]
fn container_reading(
    usage: Option<subc_os::TreeUsage>,
    source: subc_control::TreeSource,
) -> subc_control::TreeReading {
    usage
        .map(|usage| crate::resource_totals::wire(usage, source.clone()))
        .unwrap_or_else(|| {
            subc_control::TreeReading::new(
                0,
                0,
                if source == subc_control::TreeSource::Job {
                    subc_control::ChildMemoryKind::WindowsWorkingSet
                } else {
                    subc_control::ChildMemoryKind::Unknown("cgroup_memory".to_owned())
                },
                0,
                source,
            )
        })
}

impl Target {
    pub(crate) fn counter_cpu(&self) -> Option<(u128, u64)> {
        match self {
            #[cfg(test)]
            Self::Held(_) => Some((self.counter_identity()?, 0)),
            #[cfg(target_os = "linux")]
            Self::Cgroup(path) => Some((
                self.counter_identity()?,
                subc_cgroup::read_cpu_ms(path).ok()?,
            )),
            #[cfg(windows)]
            Self::Job(job) => {
                let owner = job.upgrade()?;
                let query = owner.try_lock().ok()?.query().ok()?;
                Some((job.as_ptr() as usize as u128, query.cpu_ms().ok()?))
            }
            _ => None,
        }
    }

    pub(crate) fn counter_identity(&self) -> Option<u128> {
        match self {
            #[cfg(test)]
            Self::Held(read) => Some(Arc::as_ptr(read) as usize as u128),
            #[cfg(target_os = "linux")]
            Self::Cgroup(path) => {
                use std::os::unix::fs::MetadataExt;
                let stat = std::fs::metadata(path).ok()?;
                Some((u128::from(stat.dev()) << 64) | u128::from(stat.ino()))
            }
            #[cfg(windows)]
            Self::Job(job) => Some(job.as_ptr() as usize as u128),
            _ => None,
        }
    }

    pub(crate) fn contains_identity(
        &self,
        pid: u32,
        start: u64,
        process: &subc_os::Process,
    ) -> Option<bool> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Cgroup(path) => {
                let groups = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
                let group = groups.lines().find_map(|line| line.strip_prefix("0::"))?;
                let actual =
                    std::path::Path::new("/sys/fs/cgroup").join(group.trim_start_matches('/'));
                let member = actual.starts_with(path);
                (subc_os::start_time(pid) == Some(start)).then_some(member)
            }
            #[cfg(windows)]
            Self::Job(owner) => {
                let owner = owner.upgrade()?;
                let query = owner.try_lock().ok()?.query().ok()?;
                query.contains_process(process).ok()
            }
            Self::Unavailable("read_failed") => None,
            _ => {
                let _ = (pid, start, process);
                Some(false)
            }
        }
    }

    pub(crate) fn has_container(&self) -> bool {
        match self {
            #[cfg(test)]
            Self::Held(_) => true,
            #[cfg(target_os = "linux")]
            Self::Cgroup(_) => true,
            #[cfg(windows)]
            Self::Job(_) => true,
            _ => false,
        }
    }

    pub(crate) fn process_ids(&self) -> Option<Vec<u32>> {
        let deadline = Instant::now() + READ_TIMEOUT;
        match self {
            #[cfg(target_os = "linux")]
            Self::Cgroup(path) => subc_cgroup::process_ids(path, deadline).ok(),
            #[cfg(windows)]
            Self::Job(owner) => owner
                .upgrade()?
                .try_lock()
                .ok()?
                .query()
                .ok()?
                .process_ids(deadline)
                .ok(),
            _ => {
                let _ = deadline;
                None
            }
        }
    }

    pub(crate) fn tree(
        &self,
        table: &subc_os::ProcessTable,
    ) -> Option<(
        subc_control::TreeReading,
        std::collections::BTreeSet<(u32, u64)>,
    )> {
        #[cfg(test)]
        if matches!(self, Self::Held(_)) {
            self.clone()
                .read(Instant::now() + READ_TIMEOUT, &AtomicU64::new(0));
            return Some((
                subc_control::TreeReading::new(
                    0,
                    1,
                    subc_control::ChildMemoryKind::ResidentSet,
                    1,
                    subc_control::TreeSource::ProcessTree,
                ),
                Default::default(),
            ));
        }
        let pids = self.process_ids()?;
        let members: Vec<_> = pids
            .into_iter()
            .map(|pid| {
                let member = table.process(pid)?;
                (subc_os::start_time(pid) == Some(member.start_time)).then(|| member.clone())
            })
            .collect::<Option<_>>()?;
        let identities: std::collections::BTreeSet<_> = members
            .iter()
            .map(|member| (member.pid, member.start_time))
            .collect();
        let empty = members.is_empty();
        let usage = subc_os::ProcessTable::sum(members);
        if !empty && usage.is_none() {
            return None;
        }
        #[cfg(target_os = "linux")]
        if let Self::Cgroup(path) = self {
            let mut reading = container_reading(usage, subc_control::TreeSource::Cgroup);
            reading.cpu_ms = subc_cgroup::read_cpu_ms(path).ok()?;
            let memory = subc_cgroup::read_memory(
                path,
                std::path::Path::new("/proc"),
                Instant::now() + READ_TIMEOUT,
            )
            .ok()?;
            reading.memory_bytes = memory.memory_bytes;
            reading.process_count = memory.process_count;
            reading.memory_kind = if memory.source == subc_cgroup::MemorySource::CgroupMemory {
                subc_control::ChildMemoryKind::Unknown("cgroup_memory".to_owned())
            } else {
                subc_control::ChildMemoryKind::ResidentSet
            };
            return Some((reading, identities));
        }
        #[cfg(windows)]
        if let Self::Job(owner) = self {
            let query = owner.upgrade()?.try_lock().ok()?.query().ok()?;
            let mut reading = container_reading(usage, subc_control::TreeSource::Job);
            reading.cpu_ms = query.cpu_ms().ok()?;
            let memory = query.memory(Instant::now() + READ_TIMEOUT).ok()?;
            reading.memory_bytes = memory.memory_bytes;
            reading.process_count = memory.process_count;
            return Some((reading, identities));
        }
        #[cfg(not(any(target_os = "linux", windows)))]
        let _ = (usage, identities);
        None
    }

    #[cfg(test)]
    fn read(self, deadline: Instant, counted: &AtomicU64) -> SubtreeReading {
        #[cfg(not(any(test, target_os = "linux", windows)))]
        let _ = counted;
        if Instant::now() >= deadline {
            return SubtreeReading::unavailable("timeout");
        }
        match self {
            Self::Unavailable(reason) => SubtreeReading::unavailable(reason),
            #[cfg(test)]
            Self::Held(read) => {
                counted.store(1, Ordering::Relaxed);
                if let Some(started) = read.started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                let _ = read.release.lock().unwrap().recv();
                SubtreeReading::process_sum(1, None, 1)
            }
            #[cfg(target_os = "linux")]
            Self::Cgroup(path) => {
                match subc_cgroup::read_memory_counted(
                    &path,
                    std::path::Path::new("/proc"),
                    deadline,
                    |count| counted.store(count, Ordering::Relaxed),
                ) {
                    Ok(reading) => match reading.source {
                        subc_cgroup::MemorySource::CgroupMemory => SubtreeReading::cgroup_memory(
                            reading.memory_bytes,
                            reading.swap_bytes,
                            reading.process_count,
                        ),
                        subc_cgroup::MemorySource::ProcessSum => SubtreeReading::process_sum(
                            reading.memory_bytes,
                            reading.swap_bytes,
                            reading.process_count,
                        ),
                        _ => SubtreeReading::unavailable("unsupported_source"),
                    },
                    Err(error) => {
                        let mut reading = unavailable(&error.error);
                        reading.process_count = error.process_count;
                        reading
                    }
                }
            }
            #[cfg(windows)]
            Self::Job(owner) => {
                let query = owner.upgrade().and_then(|owner| {
                    // The snapshot stores only a weak owner. Duplicate per read,
                    // without holding a lock while walking processes.
                    owner.try_lock().ok().and_then(|job| job.query().ok())
                });
                let Some(query) = query else {
                    return SubtreeReading::unavailable("read_failed");
                };
                match query
                    .memory_counted(deadline, |count| counted.store(count, Ordering::Relaxed))
                {
                    Ok(reading) => SubtreeReading::process_sum(
                        reading.memory_bytes,
                        None,
                        reading.process_count,
                    ),
                    Err(error) => {
                        let mut reading = unavailable(&error.error);
                        reading.process_count = error.process_count;
                        reading
                    }
                }
            }
        }
    }
}

#[cfg(any(target_os = "linux", windows))]
#[cfg(test)]
fn unavailable(error: &std::io::Error) -> SubtreeReading {
    SubtreeReading::unavailable(if error.kind() == std::io::ErrorKind::TimedOut {
        "timeout"
    } else {
        "read_failed"
    })
}

#[cfg(test)]
pub(crate) async fn read(targets: Vec<Target>) -> Vec<SubtreeReading> {
    let mut readings = vec![SubtreeReading::unavailable("read_failed"); targets.len()];
    let mut tasks = tokio::task::JoinSet::new();
    let mut pending = targets.into_iter().enumerate();
    // Start budgets only as targets enter the bounded pipeline, not while
    // later modules are queued behind earlier work in this list.
    for (index, target) in pending.by_ref().take(MAX_READERS) {
        tasks.spawn(async move { (index, read_target(target).await) });
    }
    while let Some(result) = tasks.join_next().await {
        if let Ok((index, reading)) = result {
            readings[index] = reading;
        }
        if let Some((index, target)) = pending.next() {
            tasks.spawn(async move { (index, read_target(target).await) });
        }
    }
    readings
}

#[cfg(test)]
async fn read_target(target: Target) -> SubtreeReading {
    if matches!(target, Target::Unavailable(_)) {
        return target.read(Instant::now() + READ_TIMEOUT, &AtomicU64::new(u64::MAX));
    }
    let counted = Arc::new(AtomicU64::new(u64::MAX));
    let worker_count = Arc::clone(&counted);
    bounded(
        Instant::now() + READ_TIMEOUT,
        move |deadline| target.read(deadline, &worker_count),
        move |reason| {
            let mut reading = SubtreeReading::unavailable(reason);
            let count = counted.load(Ordering::Relaxed);
            if count != u64::MAX {
                reading.process_count = Some(count);
            }
            reading
        },
    )
    .await
}

pub(crate) async fn read_process(
    module: crate::supervise::SupervisedModule,
) -> subc_control::ChildResourceUsage {
    let module_id = module.module_id().to_string();
    let deadline = Instant::now() + READ_TIMEOUT;
    let deadline = crate::control::list_diagnostics::deadline()
        .map_or(deadline, |list| deadline.min(list.into_std()));
    bounded(
        deadline,
        move |_| module.child_resource_usage(),
        move |reason| {
            if reason == "timeout" {
                log_timeout(&module_id, "process");
            }
            subc_control::ChildResourceUsage::Unavailable {
                reason: subc_control::ChildResourceUnavailableReason::Unreadable,
            }
        },
    )
    .await
}

pub(crate) fn log_timeout(module_id: &str, read: &str) {
    static LAST: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Instant>>> =
        OnceLock::new();
    let Ok(mut last) = LAST.get_or_init(Default::default).try_lock() else {
        return;
    };
    if !should_log_timeout(&mut last, module_id, Instant::now()) {
        return;
    }
    drop(last);
    tracing::debug!(module_id, read, "module memory read timed out");
}

fn should_log_timeout(
    last: &mut std::collections::HashMap<String, Instant>,
    module_id: &str,
    now: Instant,
) -> bool {
    last.retain(|_, at| now.duration_since(*at) < Duration::from_secs(60));
    if last.contains_key(module_id) {
        return false;
    }
    last.insert(module_id.to_string(), now);
    true
}

pub(crate) async fn bounded<T: Send + 'static>(
    deadline: Instant,
    read: impl FnOnce(Instant) -> T + Send + 'static,
    unavailable: impl Fn(&'static str) -> T,
) -> T {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_READERS)));
    if Instant::now() >= deadline {
        return unavailable("timeout");
    }
    let Ok(Ok(permit)) = tokio::time::timeout(
        deadline.saturating_duration_since(Instant::now()),
        Arc::clone(slots).acquire_owned(),
    )
    .await
    else {
        return unavailable("timeout");
    };
    if Instant::now() >= deadline {
        return unavailable("timeout");
    }
    let task = tokio::task::spawn_blocking(move || {
        // Keep the slot until the blocking call really returns, even if its
        // caller times out. A stuck filesystem cannot grow the thread pool.
        let _permit = permit;
        let reading = read(deadline);
        (reading, Instant::now())
    });
    match tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), task).await {
        // A delayed runtime poll must not let a completed but overdue read
        // win the race against the timer.
        Ok(Ok((reading, finished))) if finished < deadline => reading,
        Ok(Ok(_)) => unavailable("timeout"),
        Ok(Err(_)) => unavailable("read_failed"),
        Err(_) => unavailable("timeout"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_membership_rechecks_the_same_native_start_identity() {
        let pid = std::process::id();
        let process = subc_os::Process::open(pid).unwrap().unwrap();
        let start = process.observe().unwrap().start_time;
        let target = Target::Cgroup("/sys/fs/cgroup".into());
        assert_eq!(target.contains_identity(pid, start, &process), Some(true));
        assert_eq!(target.contains_identity(pid, start + 1, &process), None);
        assert_eq!(
            Target::Unavailable("read_failed").contains_identity(pid, start, &process),
            None
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_counter_identity_changes_when_a_path_is_recreated() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            "../../target/scratch/counter-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        let old_directory = std::fs::File::open(&path).unwrap();
        let target = Target::Cgroup(path.clone());
        let before = target.counter_identity().unwrap();
        assert_eq!(target.counter_identity(), Some(before));
        std::fs::remove_dir(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let after = target.counter_identity().unwrap();
        std::fs::remove_dir(&path).unwrap();
        drop(old_directory);
        assert_ne!(
            before, after,
            "a recreated path must not inherit the old counter"
        );
    }

    #[test]
    fn timeout_logs_are_limited_per_module_for_one_minute() {
        let mut last = std::collections::HashMap::new();
        let now = Instant::now();
        assert!(should_log_timeout(&mut last, "one", now));
        assert!(!should_log_timeout(&mut last, "one", now));
        assert!(should_log_timeout(&mut last, "two", now));
        assert!(!should_log_timeout(
            &mut last,
            "one",
            now + Duration::from_secs(59)
        ));
        assert!(should_log_timeout(
            &mut last,
            "one",
            now + Duration::from_secs(60)
        ));
    }

    #[tokio::test]
    async fn unavailable_never_substitutes_a_process_reading() {
        assert_eq!(
            read(vec![Target::Unavailable("no_cgroup")]).await,
            vec![SubtreeReading::unavailable("no_cgroup")]
        );
    }

    #[tokio::test]
    async fn stalled_target_does_not_discard_completed_target() {
        let _guard = fault_read_guard().await;
        let (stalled, release, _started) = held();
        let (quick, quick_release, quick_started) = held();
        quick_release.send(()).unwrap();
        drop(quick_release);
        let readings = read(vec![stalled, quick]).await;
        release.send(()).unwrap();
        quick_started.await.unwrap();
        assert_eq!(readings[0].reason.as_deref(), Some("timeout"));
        assert_eq!(readings[1], SubtreeReading::process_sum(1, None, 1));
    }

    #[tokio::test]
    async fn later_targets_start_with_a_fresh_budget() {
        let _guard = fault_read_guard().await;
        let mut targets = Vec::new();
        let mut releasers = tokio::task::JoinSet::new();
        for _ in 0..6 {
            let (target, release, started) = held();
            targets.push(target);
            releasers.spawn(async move {
                started.await.unwrap();
                tokio::time::sleep(READ_TIMEOUT / 2).await;
                release.send(()).unwrap();
            });
        }
        // Three waves of individually timely reads exceed a single budget.
        let readings = read(targets).await;
        while let Some(result) = releasers.join_next().await {
            result.unwrap();
        }
        assert_eq!(readings, vec![SubtreeReading::process_sum(1, None, 1); 6]);
    }

    #[tokio::test]
    async fn a_stalled_read_times_out_without_blocking_the_runtime() {
        let _guard = fault_read_guard().await;
        let (release, wait) = std::sync::mpsc::channel();
        let (started, began) = tokio::sync::oneshot::channel();
        let reading = tokio::spawn(bounded(
            Instant::now() + READ_TIMEOUT,
            move |_| {
                let _ = started.send(());
                let _ = wait.recv();
                vec![SubtreeReading::process_sum(1, None, 1)]
            },
            |reason| vec![SubtreeReading::unavailable(reason)],
        ));
        began.await.unwrap();
        // A single-thread runtime must continue serving unrelated work while
        // the blocking reader is held behind the channel.
        tokio::task::yield_now().await;
        let result = reading.await.unwrap();
        release.send(()).unwrap();
        assert_eq!(result, vec![SubtreeReading::unavailable("timeout")]);
    }
}
