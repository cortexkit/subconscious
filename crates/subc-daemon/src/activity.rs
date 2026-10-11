//! A bounded, single-writer activity ledger. File work never runs on a reader.

use std::{
    collections::HashMap,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use subc_control::{
    ActivityActor, ActivityEntry, ActivityRecordRequest, ACTIVITY_KIND_FORBIDDEN,
    ACTIVITY_MALFORMED, ACTIVITY_RATE_LIMITED, ACTIVITY_TOO_LARGE, ACTIVITY_UNAVAILABLE,
};
use tokio::sync::{mpsc, oneshot};

const QUEUE_CAPACITY: usize = 64;
const RETENTION: cortexkit_log::Retention = cortexkit_log::Retention {
    max_file_mb: 1,
    keep: 63,
    max_age_days: 30,
};

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
    reply: oneshot::Sender<Result<ActivityEntry, &'static str>>,
}

/// Clones share the bounded queue and per-module rate credit until this daemon stops.
#[derive(Clone, Debug)]
pub(crate) struct ActivityLedger {
    sender: mpsc::Sender<Append>,
    read_errors: Arc<AtomicU64>,
}

impl ActivityLedger {
    pub(crate) fn read_errors(&self) -> u64 {
        self.read_errors.load(Ordering::Relaxed)
    }
    #[cfg(test)]
    pub(crate) fn remaining_capacity(&self) -> usize {
        self.sender.capacity()
    }
    pub(crate) fn open(path: PathBuf, incarnation: String) -> Self {
        Self::start(path, incarnation, QUEUE_CAPACITY, WriterHooks::default())
    }

    pub(crate) fn start(
        path: PathBuf,
        incarnation: String,
        capacity: usize,
        hooks: WriterHooks,
    ) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Append>(capacity);
        let read_errors = Arc::new(AtomicU64::new(0));
        let writer_errors = read_errors.clone();
        // A dedicated thread also keeps filesystem stalls off Tokio's workers.
        // Dropping the last sender ends it; shutdown never joins a stalled sink.
        let started = std::thread::Builder::new()
            .name("activity-writer".into())
            .spawn(move || {
                let mut writer = Writer::open(path, incarnation, hooks, &writer_errors);
                writer.hooks.wait();
                while let Some(append) = receiver.blocking_recv() {
                    let result = writer.append(&append);
                    let _ = append.reply.send(result);
                }
            });
        if let Err(error) = started {
            tracing::warn!(%error, "activity writer unavailable");
        }
        Self {
            sender,
            read_errors,
        }
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
                reply,
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
    pub(crate) clock: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

impl WriterHooks {
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
    next_seq: u64,
    incarnation: String,
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
    ) -> Self {
        let opened = (|| -> io::Result<_> {
            #[cfg(test)]
            if hooks.open_failure {
                return Err(io::Error::other("injected open failure"));
            }
            let mut sink = cortexkit_log::LineSink::open(&path, RETENTION)?;
            // Frame off a killed append so it cannot swallow the next valid line.
            let mut file = File::open(&path)?;
            if file.metadata()?.len() > 0 {
                file.seek(SeekFrom::End(-1))?;
                let mut last = [0];
                file.read_exact(&mut last)?;
                if last[0] != b'\n' {
                    sink.write_line(b"")?;
                }
            }
            let highest = retained_max(&path, read_errors)?;
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
            next_seq,
            incarnation,
            buckets: HashMap::new(),
            started: Instant::now(),
            hooks,
        }
    }

    fn append(&mut self, append: &Append) -> Result<ActivityEntry, &'static str> {
        validate(&append.module, &append.request)?;
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
        if !bucket.available(now) {
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
        bucket.credit_ms -= 100;
        self.next_seq += 1;
        Ok(entry)
    }
}

fn retained_max(path: &Path, read_errors: &AtomicU64) -> io::Result<u64> {
    let mut highest = 0;
    for generation in 0..=RETENTION.keep {
        let mut name = path.as_os_str().to_owned();
        if generation != 0 {
            name.push(format!(".{generation}"));
        }
        let file = match File::open(PathBuf::from(name)) {
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
