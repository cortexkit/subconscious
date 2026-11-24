//! Best-effort terminal observations, not boot-critical state.

use std::{
    collections::HashMap,
    fmt,
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::PathBuf,
    sync::{Arc, Mutex, TryLockError},
};

use serde::{Deserialize, Serialize};
use subc_control::{TerminalEntry, TerminalHistory};

use crate::terminal_ring::{TerminalHistorySnapshot, TerminalRecord};

#[path = "supervise/journal_writer.rs"]
mod journal_writer;
use journal_writer::JournalWriter;

const RETENTION: cortexkit_log::Retention = cortexkit_log::Retention {
    max_file_mb: 1,
    keep: 3,
    max_age_days: 30,
};

/// A non-exit line in the journal.
///
/// `daemon_shutdown` is written when a daemon begins its announced shutdown.
/// It bounds that daemon incarnation's stretch of the journal and records the
/// instant the shutdown began, for someone reading the file. No reader in this
/// daemon applies it: `merge` skips it, because whether an exit belongs to a
/// shutdown is already in that exit's own record (disposition
/// `daemon_shutdown`), so nothing has to be inferred from position.
#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum DaemonMarker {
    DaemonShutdown {
        daemon_incarnation: String,
        at_ms: u64,
    },
}

#[derive(Serialize, Deserialize)]
struct JournalEntry {
    module_id: String,
    daemon_incarnation: String,
    #[serde(default)]
    sequence: u64,
    #[serde(flatten)]
    record: TerminalRecord,
}

/// The writer owns the sink. Readers use the file gate only to pin generations;
/// the in-memory ring and append queue do not depend on it.
pub(crate) struct TerminalJournal {
    path: PathBuf,
    incarnation: String,
    writer: JournalWriter,
    file_gate: Arc<Mutex<()>>,
}

impl fmt::Debug for TerminalJournal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TerminalJournal")
            .field("path", &self.path)
            .field("incarnation", &self.incarnation)
            .finish_non_exhaustive()
    }
}

impl TerminalJournal {
    pub(crate) fn open(path: PathBuf, incarnation: String) -> Self {
        let file_gate = Arc::new(Mutex::new(()));
        Self {
            writer: JournalWriter::open(path.clone(), file_gate.clone()),
            path,
            incarnation,
            file_gate,
        }
    }

    #[cfg(unix)]
    pub(crate) fn stamp_shutdown(&self) {
        self.writer.stamp_shutdown(DaemonMarker::DaemonShutdown {
            daemon_incarnation: self.incarnation.clone(),
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        });
    }

    #[cfg(unix)]
    pub(crate) async fn flush_shutdown(&self) {
        if !self.writer.flush(std::time::Duration::from_secs(2)).await {
            tracing::warn!("terminal journal did not flush within the shutdown budget");
        }
    }

    /// Wait, at most `budget`, for everything queued so far (the shutdown
    /// marker included) to reach the journal file. Returns false when the
    /// writer did not catch up in time; the caller goes on regardless.
    #[cfg(unix)]
    pub(crate) async fn flush_within(&self, budget: std::time::Duration) -> bool {
        self.writer.flush(budget).await
    }

    pub(crate) fn append(&self, module_id: &str, record: &TerminalRecord) {
        self.writer.append(JournalEntry {
            module_id: module_id.to_owned(),
            daemon_incarnation: self.incarnation.clone(),
            sequence: 0,
            record: record.clone(),
        });
    }

    pub(crate) fn sequence(&self) -> u64 {
        self.writer.sequence()
    }

    #[cfg(test)]
    pub(crate) fn flush_for_test(&self) {
        self.writer.flush_for_test();
    }

    #[cfg(test)]
    pub(crate) fn merge(
        &self,
        module_id: &str,
        snapshot: TerminalHistorySnapshot,
    ) -> TerminalHistory {
        self.flush_for_test();
        self.capture_read(snapshot, self.sequence()).read(module_id)
    }

    /// Pin files and their lengths before parsing, so rotation cannot change
    /// the read. The sequence captured with the ring excludes newer appends.
    pub(crate) fn capture_read(
        &self,
        snapshot: TerminalHistorySnapshot,
        sequence: u64,
    ) -> JournalRead {
        let mut history = ring_history(snapshot, Some(&self.incarnation));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let files = loop {
            match self.file_gate.try_lock() {
                Ok(files) => break Some(files),
                Err(TryLockError::Poisoned(p)) => break Some(p.into_inner()),
                Err(TryLockError::WouldBlock)
                    if self.writer.is_stalled() || std::time::Instant::now() >= deadline =>
                {
                    history.journal_read_errors += 1;
                    break None;
                }
                Err(TryLockError::WouldBlock) => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
            }
        };
        history.journal_write_failures = self.writer.failures();
        let mut generations = Vec::new();
        // LineSink names generations by appending .1, .2, ...; only the sink
        // rotates or prunes them. Read oldest first for stable timestamp ties.
        for generation in (0..=RETENTION.keep).rev().filter(|_| files.is_some()) {
            let path = if generation == 0 {
                self.path.clone()
            } else {
                let mut name = self.path.as_os_str().to_owned();
                name.push(format!(".{generation}"));
                PathBuf::from(name)
            };
            let opened = File::open(&path).and_then(|file| {
                let len = file.metadata()?.len();
                Ok((file, len))
            });
            match opened {
                Ok((file, len)) => generations.push((path, file, len)),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    history.journal_read_errors += 1;
                    tracing::warn!(path = %path.display(), %error, "terminal journal read failed");
                }
            }
        }
        JournalRead {
            #[cfg(test)]
            journal_path: self.path.clone(),
            history,
            generations,
            sequence,
            incarnation: self.incarnation.clone(),
        }
    }
}

/// A history read pinned by [`TerminalJournal::capture_read`], to be finished
/// without any journal or ring lock held.
pub(crate) struct JournalRead {
    #[cfg(test)]
    journal_path: PathBuf,
    /// The ring half, already in wire form, plus the counters known at capture.
    history: TerminalHistory,
    /// Open generation files, oldest first, each with its length at capture.
    generations: Vec<(PathBuf, File, u64)>,
    sequence: u64,
    incarnation: String,
}

impl JournalRead {
    /// Read the pinned generations and merge them with the ring snapshot.
    /// This is blocking file I/O: async callers run it on a blocking thread.
    pub(crate) fn read(self, module_id: &str) -> TerminalHistory {
        #[cfg(test)]
        read_pause::wait(&self.journal_path);
        let mut history = self.history;
        let mut entries = Vec::new();
        for (path, file, len) in self.generations {
            let mut reader = BufReader::new(file.take(len));
            let mut line = Vec::new();
            loop {
                line.clear();
                match reader.read_until(b'\n', &mut line) {
                    Ok(0) => break,
                    Ok(_) => match serde_json::from_slice::<JournalEntry>(&line) {
                        Ok(entry) if line.ends_with(b"\n") => {
                            if entry.module_id == module_id
                                && (entry.daemon_incarnation != self.incarnation
                                    || entry.sequence <= self.sequence)
                            {
                                entries.push(wire_entry(
                                    entry.record,
                                    Some(&entry.daemon_incarnation),
                                ));
                            }
                        }
                        // Markers are not exits; see `DaemonMarker`.
                        _ if line.ends_with(b"\n")
                            && serde_json::from_slice::<DaemonMarker>(&line).is_ok() => {}
                        _ => history.journal_skipped_lines += 1,
                    },
                    Err(error) => {
                        history.journal_read_errors += 1;
                        tracing::warn!(path = %path.display(), %error, "terminal journal read interrupted");
                        break;
                    }
                }
            }
        }
        // Match copies as a multiset, not a set: two genuinely identical exits
        // in one millisecond still count twice. Incarnation is part of the key,
        // so even identical observations from different daemons never collapse.
        let mut copies = HashMap::<String, usize>::new();
        for entry in &entries {
            *copies
                .entry(serde_json::to_string(entry).expect("terminal entry serializes"))
                .or_default() += 1;
        }
        for entry in history.entries.drain(..) {
            let key = serde_json::to_string(&entry).expect("terminal entry serializes");
            let count = copies.entry(key).or_default();
            if *count > 0 {
                *count -= 1;
            } else {
                entries.push(entry);
            }
        }
        // Wall-clock observation order across incarnations, stable file order
        // for ties. Clock rollback can reorder lifetimes; the incarnation token
        // preserves identity and is deliberately not treated as a sortable clock.
        entries.sort_by_key(|entry| entry.at_ms);
        history.entries = entries;
        history
    }
}

/// Test-only pause at the start of a journal history read, keyed by journal
/// path so parallel tests never see each other's pauses. It makes a read
/// "slow" deterministically: the read parks until the test releases it.
#[cfg(test)]
pub(crate) mod read_pause {
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{mpsc, Arc, Mutex, OnceLock},
        time::Duration,
    };

    struct Pause {
        started: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    fn pauses() -> &'static Mutex<HashMap<PathBuf, Arc<Pause>>> {
        static PAUSES: OnceLock<Mutex<HashMap<PathBuf, Arc<Pause>>>> = OnceLock::new();
        PAUSES.get_or_init(Default::default)
    }

    /// Returns (a receiver told when a read reaches the pause, a sender that
    /// releases it). An unreleased read gives up after five seconds so a failing
    /// test cannot wedge the process.
    pub(crate) fn install(path: &Path) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (started, started_rx) = mpsc::channel();
        let (release_tx, release) = mpsc::channel();
        pauses().lock().unwrap().insert(
            path.to_path_buf(),
            Arc::new(Pause {
                started,
                release: Mutex::new(release),
            }),
        );
        (started_rx, release_tx)
    }

    pub(crate) fn wait(path: &Path) {
        let pause = pauses().lock().unwrap().get(path).cloned();
        if let Some(pause) = pause {
            let _ = pause.started.send(());
            let _ = pause
                .release
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(5));
        }
    }
}

pub(crate) fn ring_history(
    snapshot: TerminalHistorySnapshot,
    incarnation: Option<&str>,
) -> TerminalHistory {
    TerminalHistory {
        daemon_started_at_ms: snapshot.daemon_started_at_ms,
        entries: snapshot
            .entries
            .into_iter()
            .map(|entry| wire_entry(entry, incarnation))
            .collect(),
        // This remains the current ring's eviction count, NOT a missing-record
        // count: journal retention may recover those exits, while expired files
        // contain unknowable numbers of older exits. Never sum the two sources.
        dropped: snapshot.dropped,
        journal_skipped_lines: 0,
        journal_read_errors: 0,
        journal_write_failures: 0,
    }
}

fn wire_entry(entry: TerminalRecord, incarnation: Option<&str>) -> TerminalEntry {
    TerminalEntry {
        daemon_incarnation: incarnation.map(str::to_owned),
        exit_code: entry.exit_code,
        exit_signal: entry.exit_signal,
        at_ms: entry.at_ms,
        disposition: entry.disposition,
        exit_kind: Some(entry.exit_kind),
        disposition_detail: entry.disposition_detail,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::terminal_ring::{TerminalRing, TerminalRingConfig};
    use cortexkit_test_support::ScratchDir;
    use std::sync::Arc;
    use subc_control::{TerminalDisposition, TerminalExitKind};

    fn record(at_ms: u64) -> TerminalRecord {
        TerminalRecord {
            exit_code: None,
            exit_signal: Some(9),
            at_ms,
            disposition: TerminalDisposition::Failed,
            exit_kind: TerminalExitKind::Crash,
            disposition_detail: Some(
                "crash budget exhausted: max_restarts=3 within window_secs=600".into(),
            ),
        }
    }

    #[test]
    fn stalled_writer_does_not_block_terminal_publication() {
        let dir = ScratchDir::new("terminal-journal-stall");
        let journal = Arc::new(TerminalJournal::open(
            dir.join("terminals.jsonl"),
            "daemon".into(),
        ));
        let writer = journal.file_gate.lock().unwrap();
        let mut ring = TerminalRing::new(TerminalRingConfig::default(), 10)
            .with_journal(Some(journal.clone()));
        let (published, read) = std::sync::mpsc::channel();
        let producer = std::thread::spawn(move || {
            ring.record_exit("module", record(12));
            published.send(ring.snapshot()).unwrap();
        });
        let result = read.recv_timeout(std::time::Duration::from_secs(1));
        drop(writer);
        producer.join().unwrap();
        let snapshot = result.expect("terminal publication must not wait for the file writer");
        assert_eq!(snapshot.entries, [record(12)]);
        journal.flush_for_test();
    }

    #[test]
    fn full_and_stalled_queue_drop_records_but_keep_ring_history() {
        let dir = ScratchDir::new("terminal-journal-losses");
        let journal = Arc::new(TerminalJournal::open(
            dir.join("terminals.jsonl"),
            "daemon".into(),
        ));
        journal.flush_for_test();
        let writer = journal.file_gate.lock().unwrap();
        let mut ring =
            TerminalRing::new(TerminalRingConfig::new(1), 10).with_journal(Some(journal.clone()));
        for at_ms in 0..256 {
            ring.record_exit("module", record(at_ms));
        }
        let overflow = journal.writer.failures();
        assert!(overflow > 0, "queue must be bounded");
        journal.writer.age_pending_for_test();
        ring.record_exit("module", record(256));
        let observed = ring.capture_durable_history().read("module");
        assert_eq!(observed.entries[0].at_ms, 256);
        assert_eq!(observed.journal_write_failures, overflow + 1);
        drop(writer);
        journal.flush_for_test();
    }

    #[test]
    #[cfg(unix)]
    fn shutdown_marker_survives_reopen_without_becoming_corruption_or_an_exit() {
        let dir = ScratchDir::new("terminal-journal-shutdown");
        let path = dir.join("terminals.jsonl");
        let journal = TerminalJournal::open(path.clone(), "cut-daemon".into());
        journal.append("module", &record(8));
        journal.stamp_shutdown();
        journal.flush_for_test();
        drop(journal);
        let journal = TerminalJournal::open(path, "next-daemon".into());
        let history = journal.merge(
            "module",
            TerminalRing::new(TerminalRingConfig::default(), 10).snapshot(),
        );
        assert_eq!(history.journal_skipped_lines, 0);
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.entries[0].at_ms, 8);
    }

    /// The shutdown drain waits on `flush_within` before announcing the
    /// shutdown, so a stalled writer must cost only the budget, and a writer
    /// that catches up must have the marker in the file when it returns true.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[cfg(unix)]
    async fn flush_within_reports_whether_the_queued_shutdown_marker_reached_the_file() {
        let dir = ScratchDir::new("terminal-journal-marker-flush");
        let path = dir.join("terminals.jsonl");
        let journal = TerminalJournal::open(path.clone(), "cut-daemon".into());
        let marker_on_disk = || {
            std::fs::read_to_string(&path)
                .unwrap_or_default()
                .contains("daemon_shutdown")
        };
        // Holding the file gate stalls the writer before it writes anything.
        // A plain thread holds it, so no lock guard lives across an await.
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let gate = journal.file_gate.clone();
        let holder = std::thread::spawn(move || {
            let _held = gate.lock().unwrap();
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        locked_rx.recv().unwrap();
        journal.stamp_shutdown();
        assert!(
            !journal
                .flush_within(std::time::Duration::from_millis(50))
                .await
        );
        assert!(
            !marker_on_disk(),
            "a stalled writer has not written the marker"
        );
        release_tx.send(()).unwrap();
        holder.join().unwrap();
        assert!(
            journal
                .flush_within(std::time::Duration::from_secs(5))
                .await
        );
        assert!(
            marker_on_disk(),
            "a completed flush has the marker in the file"
        );
    }

    #[test]
    fn journal_preserves_exit_fields_filters_modules_and_isolates_a_torn_line() {
        let dir = ScratchDir::new("terminal-journal-fields");
        let path = dir.join("terminals.jsonl");
        std::fs::write(&path, b"{\"torn\":").unwrap();
        let journal = TerminalJournal::open(path, "old-daemon".into());
        journal.append("module", &record(8));
        journal.append("other", &record(5));
        let history = journal.merge(
            "module",
            TerminalRing::new(TerminalRingConfig::default(), 10).snapshot(),
        );
        assert_eq!(
            (history.entries, history.journal_skipped_lines),
            (
                vec![TerminalEntry {
                    daemon_incarnation: Some("old-daemon".into()),
                    exit_code: None,
                    exit_signal: Some(9),
                    at_ms: 8,
                    disposition: TerminalDisposition::Failed,
                    exit_kind: Some(TerminalExitKind::Crash),
                    disposition_detail: Some(
                        "crash budget exhausted: max_restarts=3 within window_secs=600".into()
                    ),
                }],
                1
            )
        );
    }

    #[test]
    fn journal_merge_recovers_ring_evictions_orders_exits_and_preserves_multiplicity() {
        let dir = ScratchDir::new("terminal-journal-merge");
        let journal = Arc::new(TerminalJournal::open(
            dir.join("terminals.jsonl"),
            "daemon".into(),
        ));
        let mut ring =
            TerminalRing::new(TerminalRingConfig::new(2), 10).with_journal(Some(journal.clone()));
        for at_ms in [30, 20, 20] {
            let exit = record(at_ms);
            journal.append("module", &exit);
            ring.push(exit);
        }
        let history = ring.durable_history("module");
        assert_eq!(
            (
                history
                    .entries
                    .iter()
                    .map(|entry| entry.at_ms)
                    .collect::<Vec<_>>(),
                history.dropped
            ),
            (vec![20, 20, 30], 1)
        );
    }

    #[test]
    fn journal_reads_rotated_generations_after_reopening() {
        let dir = ScratchDir::new("terminal-journal-rotation");
        let path = dir.join("terminals.jsonl");
        let journal = TerminalJournal::open(path.clone(), "first".into());
        let mut large = record(1);
        large.disposition_detail = Some("x".repeat(600_000));
        journal.append("module", &large);
        large.at_ms = 2;
        journal.append("module", &large);
        journal.flush_for_test();
        drop(journal);
        let journal = TerminalJournal::open(path, "second".into());
        let history = journal.merge(
            "module",
            TerminalRing::new(TerminalRingConfig::default(), 10).snapshot(),
        );
        assert_eq!(
            history
                .entries
                .iter()
                .map(|entry| (entry.at_ms, entry.daemon_incarnation.as_deref()))
                .collect::<Vec<_>>(),
            vec![(1, Some("first")), (2, Some("first"))]
        );
    }

    #[test]
    fn journal_failed_append_leaves_the_ring_readable_and_reports_failure() {
        let dir = ScratchDir::new("terminal-journal-write-failure");
        let path = dir.join("terminals.jsonl");
        std::fs::create_dir(&path).unwrap();
        let journal = Arc::new(TerminalJournal::open(path, "daemon".into()));
        let mut ring =
            TerminalRing::new(TerminalRingConfig::default(), 10).with_journal(Some(journal));
        let exit = record(12);
        ring.append_journal("module", &exit);
        ring.push(exit);
        let history = ring.durable_history("module");
        assert_eq!(
            (
                history.entries.len(),
                history.entries[0].exit_signal,
                history.journal_write_failures
            ),
            (1, Some(9), 1)
        );
    }
}
