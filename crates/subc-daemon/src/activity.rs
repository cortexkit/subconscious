//! A bounded, single-writer activity ledger. File work never runs on a reader.

use std::{
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, Condvar, Mutex,
    },
    time::Instant,
};

use subc_control::{
    ActivityActor, ActivityCursor, ActivityEntry, ActivityQueryRequest, ActivityQueryResponse,
    ActivityRecordRequest, ACTIVITY_CURSOR_INCARNATION_MISMATCH, ACTIVITY_CURSOR_TOO_OLD,
    ACTIVITY_KIND_FORBIDDEN, ACTIVITY_MALFORMED, ACTIVITY_RATE_LIMITED, ACTIVITY_SUBSCRIBER_LAGGED,
    ACTIVITY_TOO_LARGE, ACTIVITY_UNAVAILABLE,
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_CAPACITY: usize = 64;
const RING_CAPACITY: usize = 1024;
const SUBSCRIBER_CAPACITY: usize = 256;

/// Bounds include the active file, not just rotated generations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ActivityConfig {
    pub max_age_days: u32,
    pub max_total_mb: u32,
}

impl Default for ActivityConfig {
    fn default() -> Self {
        Self {
            max_age_days: 30,
            max_total_mb: 64,
        }
    }
}

impl ActivityConfig {
    fn retention(self) -> cortexkit_log::Retention {
        let total = self.max_total_mb.max(1);
        // Retention allows at most 255 rotated files. Scale the file bound for
        // larger budgets while keeping active + rotated capacity within total.
        let file_mb = total.div_ceil(256);
        cortexkit_log::Retention {
            max_file_mb: file_mb,
            keep: (total / file_mb - 1) as u8,
            max_age_days: self.max_age_days,
        }
    }
}

#[derive(Default)]
struct SharedWriter {
    writer: Mutex<Option<Writer>>,
    ready: Condvar,
    failed: AtomicBool,
}

impl SharedWriter {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<Writer>>, &'static str> {
        let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        while writer.is_none() {
            if self.failed.load(Ordering::Relaxed) {
                return Err(ACTIVITY_UNAVAILABLE);
            }
            writer = self.ready.wait(writer).unwrap_or_else(|p| p.into_inner());
        }
        Ok(writer)
    }
}

struct Subscriber {
    sender: mpsc::Sender<ActivityEntry>,
    terminal: Arc<Mutex<Option<&'static str>>>,
}

pub(crate) struct ActivitySubscription {
    pub(crate) replay: VecDeque<ActivityEntry>,
    pub(crate) receiver: mpsc::Receiver<ActivityEntry>,
    terminal: Arc<Mutex<Option<&'static str>>>,
}

impl ActivitySubscription {
    pub(crate) fn terminal_code(&self) -> Option<&'static str> {
        *self.terminal.lock().unwrap_or_else(|p| p.into_inner())
    }
}

pub(crate) fn validate(module: &str, request: &ActivityRecordRequest) -> Result<(), &'static str> {
    let name = request
        .kind
        .strip_prefix(module)
        .and_then(|s| s.strip_prefix('.'));
    if !name.is_some_and(|name| identifier(name, 64)) {
        return Err(ACTIVITY_KIND_FORBIDDEN);
    }
    let summary_len = request.summary.chars().count();
    if summary_len == 0 {
        return Err(ACTIVITY_MALFORMED);
    }
    if summary_len > 200 {
        return Err(ACTIVITY_TOO_LARGE);
    }
    if let Some(subject) = &request.subject {
        if subject.subject_type.len() > 32 || subject.id.chars().count() > 256 {
            return Err(ACTIVITY_TOO_LARGE);
        }
        if !identifier(&subject.subject_type, 32) || subject.id.is_empty() {
            return Err(ACTIVITY_MALFORMED);
        }
    }
    if let Some(details) = &request.details {
        let bytes = serde_json::to_vec(details).map_err(|_| ACTIVITY_MALFORMED)?;
        if bytes.len() > 4096 {
            return Err(ACTIVITY_TOO_LARGE);
        }
    }
    Ok(())
}

fn identifier(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

struct Append {
    module: String,
    request: ActivityRecordRequest,
    actor: ActivityActor,
    at_ms: u64,
    reply: Option<oneshot::Sender<Result<ActivityEntry, &'static str>>>,
    lifecycle: Option<lifecycle::PendingLifecycle>,
}

/// Clones share the bounded queue and per-module rate credit until this daemon stops.
#[derive(Clone)]
pub(crate) struct ActivityLedger {
    sender: mpsc::Sender<Append>,
    read_errors: Arc<AtomicU64>,
    shared: Arc<SharedWriter>,
    lifecycle_counters: Arc<lifecycle::LifecycleCounters>,
}

impl std::fmt::Debug for ActivityLedger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActivityLedger").finish_non_exhaustive()
    }
}

impl ActivityLedger {
    pub(crate) fn read_errors(&self) -> u64 {
        self.read_errors.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn remaining_capacity(&self) -> usize {
        self.sender.capacity()
    }
    #[cfg(test)]
    pub(crate) fn with_writer_lock(&self, work: impl FnOnce()) {
        let _guard = self.shared.lock().unwrap();
        work();
    }
    pub(crate) fn open(path: PathBuf, incarnation: String) -> Self {
        Self::start(path, incarnation, QUEUE_CAPACITY, WriterHooks::default())
    }

    pub(crate) fn open_config(path: PathBuf, incarnation: String, config: ActivityConfig) -> Self {
        Self::start_config(
            path,
            incarnation,
            QUEUE_CAPACITY,
            WriterHooks::default(),
            config,
        )
    }

    pub(crate) fn start(
        path: PathBuf,
        incarnation: String,
        capacity: usize,
        hooks: WriterHooks,
    ) -> Self {
        Self::start_config(
            path,
            incarnation,
            capacity,
            hooks,
            ActivityConfig::default(),
        )
    }

    fn start_config(
        path: PathBuf,
        incarnation: String,
        capacity: usize,
        hooks: WriterHooks,
        config: ActivityConfig,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Append>(capacity);
        let read_errors = Arc::new(AtomicU64::new(0));
        let writer_errors = read_errors.clone();
        let shared = Arc::new(SharedWriter::default());
        let worker = shared.clone();
        // A dedicated thread also keeps filesystem stalls off Tokio's workers.
        // Dropping the last sender ends it; shutdown never joins a stalled sink.
        let started = std::thread::Builder::new()
            .name("activity-writer".into())
            .spawn(move || {
                let writer = Writer::open(path, incarnation, hooks, &writer_errors, config);
                let pause = writer.hooks.pause_hook();
                *worker.writer.lock().unwrap_or_else(|p| p.into_inner()) = Some(writer);
                worker.ready.notify_all();
                pause.wait();
                while let Some(append) = receiver.blocking_recv() {
                    let mut append = append;
                    let mut guard = worker.lock().unwrap();
                    let writer = guard.as_mut().unwrap();
                    match writer.prepare_lifecycle(&mut append, &writer_errors) {
                        Ok(false) => {
                            append.lifecycle.as_mut().unwrap().completed = true;
                            continue;
                        }
                        Err(_) => continue,
                        Ok(true) => {}
                    }
                    let result = writer.append(&append);
                    if result.is_ok() {
                        if let Some(pending) = &mut append.lifecycle {
                            pending.completed = true;
                        }
                    }
                    if let Some(reply) = append.reply.take() {
                        let _ = reply.send(result);
                    }
                }
            });
        if let Err(error) = started {
            tracing::warn!(%error, "activity writer unavailable");
            let _guard = shared.writer.lock().unwrap_or_else(|p| p.into_inner());
            shared.failed.store(true, Ordering::Relaxed);
            shared.ready.notify_all();
        }
        Self {
            sender,
            read_errors,
            shared,
            lifecycle_counters: Arc::default(),
        }
    }

    pub(crate) async fn query(
        &self,
        request: ActivityQueryRequest,
    ) -> Result<ActivityQueryResponse, &'static str> {
        let shared = self.shared.clone();
        let errors = self.read_errors.clone();
        tokio::task::spawn_blocking(move || {
            let pinned = {
                let guard = shared.lock()?;
                guard.as_ref().unwrap().capture_read()?
            };
            pinned.read(request, &errors)
        })
        .await
        .unwrap_or(Err(ACTIVITY_UNAVAILABLE))
    }

    pub(crate) async fn subscribe(
        &self,
        since: Option<ActivityCursor>,
    ) -> Result<ActivitySubscription, &'static str> {
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || shared.lock()?.as_mut().unwrap().subscribe(since))
            .await
            .unwrap_or(Err(ACTIVITY_UNAVAILABLE))
    }

    pub(crate) async fn configure(&self, config: ActivityConfig) -> Result<(), &'static str> {
        let shared = self.shared.clone();
        tokio::task::spawn_blocking(move || shared.lock()?.as_mut().unwrap().configure(config))
            .await
            .unwrap_or(Err(ACTIVITY_UNAVAILABLE))
    }

    pub(crate) async fn record(
        &self,
        module: String,
        request: ActivityRecordRequest,
        actor: ActivityActor,
        at_ms: u64,
    ) -> Result<ActivityEntry, &'static str> {
        let (reply, received) = oneshot::channel();
        self.sender
            .try_send(Append {
                module,
                request,
                actor,
                at_ms,
                reply: Some(reply),
                lifecycle: None,
            })
            .map_err(|_| ACTIVITY_UNAVAILABLE)?;
        received.await.unwrap_or(Err(ACTIVITY_UNAVAILABLE))
    }
}

#[derive(Default)]
pub(crate) struct WriterHooks {
    #[cfg(test)]
    pub(crate) open_failure: bool,
    #[cfg(test)]
    pub(crate) write_failure: bool,
    #[cfg(test)]
    pub(crate) pause: Option<Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>>,
    #[cfg(test)]
    pub(crate) read_pause: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
    pub(crate) clock: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

impl WriterHooks {
    fn pause_hook(&self) -> Self {
        Self {
            #[cfg(test)]
            pause: self.pause.clone(),
            ..Default::default()
        }
    }
    fn wait(&self) {
        #[cfg(test)]
        if let Some(pause) = &self.pause {
            let (lock, ready) = &**pause;
            let mut paused = lock.lock().unwrap();
            while *paused {
                paused = ready.wait(paused).unwrap();
            }
        }
    }
}

struct Bucket {
    // One token is 100 ms of credit, avoiding floating-point boundary drift.
    credit_ms: u64,
    updated_ms: u64,
}

impl Bucket {
    fn available(&mut self, now: u64) -> bool {
        self.credit_ms = self
            .credit_ms
            .saturating_add(now.saturating_sub(self.updated_ms))
            .min(6000);
        self.updated_ms = self.updated_ms.max(now);
        self.credit_ms >= 100
    }
}

struct Writer {
    sink: Option<cortexkit_log::LineSink>,
    path: PathBuf,
    config: ActivityConfig,
    next_seq: u64,
    first_seq: u64,
    incarnation: String,
    ring: VecDeque<ActivityEntry>,
    subscribers: Vec<Subscriber>,
    buckets: HashMap<String, Bucket>,
    started: Instant,
    hooks: WriterHooks,
}

impl Writer {
    fn open(
        path: PathBuf,
        incarnation: String,
        hooks: WriterHooks,
        read_errors: &AtomicU64,
        config: ActivityConfig,
    ) -> Self {
        let opened = (|| -> io::Result<_> {
            #[cfg(test)]
            if hooks.open_failure {
                return Err(io::Error::other("injected open failure"));
            }
            let sink = open_sink(&path, config)?;
            let highest = retained_max(&path, config.retention().keep, read_errors)?;
            Ok((
                sink,
                highest
                    .checked_add(1)
                    .ok_or_else(|| io::Error::other("activity seq exhausted"))?,
            ))
        })();
        let (sink, next_seq) = match opened {
            Ok((sink, seq)) => (Some(sink), seq),
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "activity ledger unavailable");
                (None, 1)
            }
        };
        Self {
            sink,
            path,
            config,
            next_seq,
            first_seq: next_seq,
            incarnation,
            ring: VecDeque::new(),
            subscribers: Vec::new(),
            buckets: HashMap::new(),
            started: Instant::now(),
            hooks,
        }
    }

    fn append(&mut self, append: &Append) -> Result<ActivityEntry, &'static str> {
        if append.lifecycle.is_none() {
            validate(&append.module, &append.request)?;
        }
        if self.sink.is_none() || self.next_seq == u64::MAX {
            return Err(ACTIVITY_UNAVAILABLE);
        }
        let now = self.hooks.clock.as_ref().map_or_else(
            || self.started.elapsed().as_millis() as u64,
            |clock| clock(),
        );
        let bucket = self.buckets.entry(append.module.clone()).or_insert(Bucket {
            credit_ms: 6000,
            updated_ms: now,
        });
        if append.lifecycle.is_none() && !bucket.available(now) {
            return Err(ACTIVITY_RATE_LIMITED);
        }
        let entry = ActivityEntry {
            seq: self.next_seq,
            at_ms: append.at_ms,
            daemon_incarnation: self.incarnation.clone(),
            module: append.module.clone(),
            kind: append.request.kind.clone(),
            summary: append.request.summary.clone(),
            subject: append.request.subject.clone(),
            details: append.request.details.clone().unwrap_or_default(),
            actor: append.actor.clone(),
        };
        let written = (|| -> io::Result<()> {
            #[cfg(test)]
            if self.hooks.write_failure {
                return Err(io::Error::other("injected write failure"));
            }
            let bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
            self.sink
                .as_mut()
                .ok_or_else(|| io::Error::other("activity sink unavailable"))?
                .write_line(&bytes)
        })();
        if let Err(error) = written {
            // A partial write must not be followed by another append until reopen
            // repairs its boundary. Failed attempts also leave rate credit intact.
            self.sink = None;
            tracing::warn!(kind = %entry.kind, %error, "activity append failed");
            return Err(ACTIVITY_UNAVAILABLE);
        }
        if append.lifecycle.is_none() {
            bucket.credit_ms -= 100;
        }
        self.next_seq += 1;
        self.ring.push_back(entry.clone());
        if self.ring.len() > RING_CAPACITY {
            self.ring.pop_front();
        }
        self.subscribers.retain(
            |subscriber| match subscriber.sender.try_send(entry.clone()) {
                Ok(()) => true,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    *subscriber
                        .terminal
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = Some(ACTIVITY_SUBSCRIBER_LAGGED);
                    false
                }
                Err(mpsc::error::TrySendError::Closed(_)) => false,
            },
        );
        Ok(entry)
    }

    fn subscribe(
        &mut self,
        since: Option<ActivityCursor>,
    ) -> Result<ActivitySubscription, &'static str> {
        if let Some(cursor) = &since {
            if cursor.daemon_incarnation != self.incarnation {
                return Err(ACTIVITY_CURSOR_INCARNATION_MISMATCH);
            }
            // After restart, a query's head cursor uses the new daemon's ID
            // with the highest retained seq from the prior daemon. That cursor
            // remains replayable until ring eviction would leave a gap; then
            // query recovery is required rather than an incomplete replay.
            if let Some(first) = self.ring.front() {
                if first.seq > self.first_seq && cursor.seq.saturating_add(1) < first.seq {
                    return Err(ACTIVITY_CURSOR_TOO_OLD);
                }
            }
        }
        let replay = self
            .ring
            .iter()
            .filter(|entry| since.as_ref().is_some_and(|cursor| entry.seq > cursor.seq))
            .cloned()
            .collect();
        let (sender, receiver) = mpsc::channel(SUBSCRIBER_CAPACITY);
        let terminal = Arc::new(Mutex::new(None));
        self.subscribers
            .retain(|subscriber| !subscriber.sender.is_closed());
        self.subscribers.push(Subscriber {
            sender,
            terminal: terminal.clone(),
        });
        Ok(ActivitySubscription {
            replay,
            receiver,
            terminal,
        })
    }

    fn configure(&mut self, config: ActivityConfig) -> Result<(), &'static str> {
        if config == self.config {
            return Ok(());
        }
        let sink = open_sink(&self.path, config).map_err(|error| {
            tracing::warn!(%error, "activity retention update failed");
            ACTIVITY_UNAVAILABLE
        })?;
        self.sink = Some(sink);
        self.config = config;
        Ok(())
    }

    fn capture_read(&self) -> Result<ActivityRead, &'static str> {
        if self.sink.is_none() {
            return Err(ACTIVITY_UNAVAILABLE);
        }
        let mut files = Vec::new();
        for generation in 0..=self.config.retention().keep {
            match File::open(generation_path(&self.path, generation)) {
                Ok(file) => {
                    let len = file.metadata().map_err(|_| ACTIVITY_UNAVAILABLE)?.len();
                    files.push((file, len));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(_) => return Err(ACTIVITY_UNAVAILABLE),
            }
        }
        Ok(ActivityRead {
            files,
            #[cfg(test)]
            read_pause: self.hooks.read_pause.clone(),
            head: ActivityCursor {
                daemon_incarnation: self.incarnation.clone(),
                seq: self.next_seq - 1,
            },
        })
    }
}

struct ActivityRead {
    files: Vec<(File, u64)>,
    head: ActivityCursor,
    #[cfg(test)]
    read_pause: Option<Arc<(std::sync::Barrier, std::sync::Barrier)>>,
}

impl ActivityRead {
    fn read(
        self,
        request: ActivityQueryRequest,
        read_errors: &AtomicU64,
    ) -> Result<ActivityQueryResponse, &'static str> {
        #[cfg(test)]
        if let Some(pause) = &self.read_pause {
            pause.0.wait();
            pause.1.wait();
        }
        let mut entries = Vec::new();
        for (file, len) in self.files {
            for line in BufReader::new(file.take(len)).split(b'\n') {
                let line = line.map_err(|_| ACTIVITY_UNAVAILABLE)?;
                let entry: ActivityEntry = match serde_json::from_slice(&line) {
                    Ok(entry) => entry,
                    Err(error) => {
                        read_errors.fetch_add(1, Ordering::Relaxed);
                        tracing::warn!(%error, "skipping unparseable activity line");
                        continue;
                    }
                };
                if request
                    .agent
                    .as_ref()
                    .is_some_and(|v| entry.actor.agent_id.as_ref() != Some(v))
                    || request
                        .project
                        .as_ref()
                        .is_some_and(|v| entry.actor.project_root.as_ref() != Some(v))
                    || request.module.as_ref().is_some_and(|v| &entry.module != v)
                    || request.kind.as_ref().is_some_and(|v| &entry.kind != v)
                    || request.since_ms.is_some_and(|v| entry.at_ms < v)
                    || request.until_ms.is_some_and(|v| entry.at_ms > v)
                    || request.cursor.is_some_and(|v| entry.seq >= v)
                {
                    continue;
                }
                entries.push(entry);
            }
        }
        entries.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.seq));
        entries.truncate(request.limit as usize);
        Ok(ActivityQueryResponse {
            cursor: entries.last().map(|entry| entry.seq),
            head: self.head,
            entries,
        })
    }
}

fn open_sink(path: &Path, config: ActivityConfig) -> io::Result<cortexkit_log::LineSink> {
    let mut sink = cortexkit_log::LineSink::open(path, config.retention())?;
    // A killed append without a trailing newline must not join the next valid
    // record into one unparseable line. Add the missing separator on reopen.
    let mut file = File::open(path)?;
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last[0] != b'\n' {
            sink.write_line(b"")?;
        }
    }
    Ok(sink)
}

fn generation_path(path: &Path, generation: u8) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    if generation != 0 {
        name.push(format!(".{generation}"));
    }
    PathBuf::from(name)
}

fn retained_max(path: &Path, keep: u8, read_errors: &AtomicU64) -> io::Result<u64> {
    let mut highest = 0;
    for generation in 0..=keep {
        let file = match File::open(generation_path(path, generation)) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        for line in BufReader::new(file).split(b'\n') {
            match serde_json::from_slice::<ActivityEntry>(&line?) {
                Ok(entry) => highest = highest.max(entry.seq),
                Err(error) => {
                    read_errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(%error, "skipping unparseable activity line");
                }
            }
        }
    }
    Ok(highest)
}

#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;

#[path = "activity_lifecycle.rs"]
mod lifecycle;

pub(crate) use lifecycle::{ExecOutcome, ExecOwner, ExecState, ImageFacts, RegistrationImage};

#[cfg(test)]
#[path = "activity_lifecycle_tests.rs"]
mod lifecycle_tests;
