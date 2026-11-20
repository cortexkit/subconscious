use std::{
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};

use tokio::sync::mpsc;
#[cfg(unix)]
use tokio::sync::oneshot;

#[cfg(unix)]
use super::DaemonMarker;
use super::{JournalEntry, RETENTION};

const CAPACITY: usize = 64;
const STALL_BUDGET: Duration = Duration::from_secs(5);

enum Job {
    Append(JournalEntry),
    #[cfg(unix)]
    Marker(DaemonMarker),
    #[cfg(unix)]
    Flush(oneshot::Sender<()>),
}

struct Progress {
    pending: usize,
    since: Instant,
    sequence: u64,
}

#[derive(Default)]
struct Failures(AtomicU64);

impl Failures {
    fn count(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) struct JournalWriter {
    send: mpsc::Sender<Job>,
    // Ordinary records cannot fill the slot reserved for the shutdown marker.
    #[cfg(unix)]
    marker: Mutex<Option<mpsc::OwnedPermit<Job>>>,
    progress: Arc<Mutex<Progress>>,
    failures: Arc<Failures>,
}

impl JournalWriter {
    pub(super) fn open(path: PathBuf, file_gate: Arc<Mutex<()>>) -> Self {
        let (send, mut receive) = mpsc::channel(CAPACITY + usize::from(cfg!(unix)));
        #[cfg(unix)]
        let marker = Mutex::new(Some(send.clone().try_reserve_owned().expect("empty queue")));
        let progress = Arc::new(Mutex::new(Progress {
            pending: 1,
            since: Instant::now(),
            sequence: 0,
        }));
        let failures = Arc::new(Failures::default());
        let writer_progress = progress.clone();
        let writer_failures = failures.clone();
        if let Err(error) = std::thread::Builder::new().name("terminal-journal".into()).spawn(move || {
            let mut sink = {
                let _files = file_gate.lock().unwrap_or_else(|p| p.into_inner());
                open_sink(&path)
            };
            complete(&writer_progress);
            while let Some(job) = receive.blocking_recv() {
                let line = match job {
                    Job::Append(entry) => serde_json::to_vec(&entry),
                    #[cfg(unix)]
                    Job::Marker(marker) => serde_json::to_vec(&marker),
                    #[cfg(unix)]
                    Job::Flush(ack) => {
                        let _ = ack.send(());
                        continue;
                    }
                };
                let result = line.map_err(io::Error::other).and_then(|line| {
                    let _files = file_gate.lock().unwrap_or_else(|p| p.into_inner());
                    sink.as_mut()
                        .ok_or_else(|| io::Error::other("terminal journal was not opened"))?
                        .write_line(&line)
                });
                if let Err(error) = result {
                    writer_failures.count();
                    tracing::warn!(path = %path.display(), %error, "terminal journal append failed");
                }
                complete(&writer_progress);
            }
        }) {
            complete(&progress);
            tracing::warn!(%error, "could not start the terminal journal writer");
        }
        Self {
            send,
            #[cfg(unix)]
            marker,
            progress,
            failures,
        }
    }

    pub(super) fn append(&self, mut entry: JournalEntry) {
        let mut progress = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        entry.sequence = progress.sequence + 1;
        if Self::stalled(&progress) || self.send.try_send(Job::Append(entry)).is_err() {
            self.failures.count();
        } else {
            progress.sequence += 1;
            Self::submitted(&mut progress);
        }
    }

    pub(super) fn sequence(&self) -> u64 {
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sequence
    }

    #[cfg(unix)]
    pub(super) fn stamp_shutdown(&self, marker: DaemonMarker) {
        if let Some(permit) = self.marker.lock().unwrap_or_else(|p| p.into_inner()).take() {
            let mut progress = self.progress.lock().unwrap_or_else(|p| p.into_inner());
            if self.send.is_closed() {
                self.failures.count();
            } else {
                permit.send(Job::Marker(marker));
                Self::submitted(&mut progress);
            }
        }
    }

    fn submitted(progress: &mut Progress) {
        if progress.pending == 0 {
            progress.since = Instant::now();
        }
        progress.pending += 1;
    }

    fn stalled(progress: &Progress) -> bool {
        progress.pending != 0 && progress.since.elapsed() > STALL_BUDGET
    }

    pub(super) fn is_stalled(&self) -> bool {
        Self::stalled(&self.progress.lock().unwrap_or_else(|p| p.into_inner()))
    }

    pub(super) fn failures(&self) -> u64 {
        self.failures.0.load(Ordering::Relaxed)
    }

    /// Wait for one FIFO checkpoint, not for the thread to exit. Exits
    /// observed during teardown can still be queued after a checkpoint.
    #[cfg(unix)]
    pub(super) async fn flush(&self, budget: Duration) -> bool {
        let (ack, result) = oneshot::channel();
        tokio::time::timeout(budget, async {
            self.send.send(Job::Flush(ack)).await.is_ok() && result.await.is_ok()
        })
        .await
        .unwrap_or(false)
    }

    #[cfg(test)]
    pub(super) fn flush_for_test(&self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.progress.lock().unwrap().pending != 0 {
            assert!(Instant::now() < deadline, "journal writer failed to flush");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[cfg(test)]
    pub(super) fn age_pending_for_test(&self) {
        let mut progress = self.progress.lock().unwrap();
        assert!(progress.pending != 0);
        progress.since = Instant::now() - STALL_BUDGET - Duration::from_millis(1);
    }
}

fn complete(progress: &Mutex<Progress>) {
    let mut progress = progress.lock().unwrap_or_else(|p| p.into_inner());
    progress.pending -= 1;
    progress.since = Instant::now();
}

fn open_sink(path: &Path) -> Option<cortexkit_log::LineSink> {
    let mut sink = match cortexkit_log::LineSink::open(path, RETENTION) {
        Ok(sink) => sink,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "terminal journal unavailable; keeping the in-memory ring");
            return None;
        }
    };
    // Frame off a torn line so it cannot swallow the next valid record.
    let boundary = (|| -> io::Result<()> {
        let mut file = File::open(path)?;
        if file.metadata()?.len() > 0 {
            file.seek(SeekFrom::End(-1))?;
            let mut last = [0];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                sink.write_line(b"")?;
            }
        }
        Ok(())
    })();
    if let Err(error) = boundary {
        tracing::warn!(path = %path.display(), %error, "terminal journal boundary check failed");
    }
    Some(sink)
}
