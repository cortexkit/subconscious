//! Best-effort terminal observations, not boot-critical state.

use std::{
    collections::HashMap,
    fmt,
    fs::File,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::Mutex,
};

use serde::{Deserialize, Serialize};
use subc_control::{TerminalEntry, TerminalHistory};

use crate::terminal_ring::{TerminalHistorySnapshot, TerminalRecord};

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
    #[serde(flatten)]
    record: TerminalRecord,
}

struct Writer {
    sink: Option<cortexkit_log::LineSink>,
    failures: u64,
}

/// One shared sink serializes append and rotation across all modules. A history
/// read holds the same lock only to pin the files it will read, never while
/// reading them; see [`TerminalJournal::capture_read`].
pub(crate) struct TerminalJournal {
    path: PathBuf,
    incarnation: String,
    writer: Mutex<Writer>,
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
        let sink = match cortexkit_log::LineSink::open(&path, RETENTION) {
            Ok(mut sink) => {
                // A killed append can leave a fragment without a newline. Frame
                // it off before the next exit so that corruption costs only the
                // damaged record, rather than swallowing the next valid append.
                let boundary = (|| -> io::Result<()> {
                    let mut file = File::open(&path)?;
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
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "terminal journal unavailable; keeping the in-memory ring");
                None
            }
        };
        Self {
            path,
            incarnation,
            writer: Mutex::new(Writer { sink, failures: 0 }),
        }
    }

    #[cfg(unix)]
    pub(crate) fn stamp_shutdown(&self) {
        self.append_serialized(serde_json::to_vec(&DaemonMarker::DaemonShutdown {
            daemon_incarnation: self.incarnation.clone(),
            at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        }));
    }

    pub(crate) fn append(&self, module_id: &str, record: &TerminalRecord) {
        let entry = JournalEntry {
            module_id: module_id.to_owned(),
            daemon_incarnation: self.incarnation.clone(),
            record: record.clone(),
        };
        self.append_serialized(serde_json::to_vec(&entry));
    }

    fn append_serialized(&self, line: Result<Vec<u8>, serde_json::Error>) {
        let mut writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let result = line.map_err(io::Error::other).and_then(|line| {
            writer
                .sink
                .as_mut()
                .ok_or_else(|| io::Error::other("terminal journal was not opened"))?
                .write_line(&line)
        });
        if let Err(error) = result {
            writer.failures = writer.failures.saturating_add(1);
            tracing::warn!(path = %self.path.display(), %error, "terminal journal append failed");
        }
    }

    /// Capture and read in one step, for tests that own the journal directly.
    #[cfg(test)]
    pub(crate) fn merge(
        &self,
        module_id: &str,
        snapshot: TerminalHistorySnapshot,
    ) -> TerminalHistory {
        self.capture_read(snapshot).read(module_id)
    }

    /// Pin what a history read will see, holding the writer lock only for that.
    ///
    /// Under the lock no append or rotation is in flight, so every generation
    /// file ends on a line boundary. Each existing generation is OPENED here and
    /// its current length recorded; the slow part (reading and parsing up to
    /// every retained generation) happens later in [`JournalRead::read`], with
    /// the lock released, so exit recording for every module proceeds while a
    /// history is read.
    ///
    /// How a write racing that read is handled: an append lands past the
    /// recorded length and is not read, and a rotation renames or prunes paths
    /// but not the files already open here (Unix keeps an open file's inode;
    /// Rust's Windows `rename`/`remove_file` use POSIX semantics against the
    /// delete-sharing handles `File::open` makes). So the read describes the
    /// journal exactly as of this call. Callers take the ring snapshot at the
    /// same moment, under the ring lock that recording also holds across its
    /// append and push, so an exit recorded after this call is in neither half
    /// of the merge and the next read has it once.
    pub(crate) fn capture_read(&self, snapshot: TerminalHistorySnapshot) -> JournalRead {
        let writer = self.writer.lock().unwrap_or_else(|p| p.into_inner());
        let mut history = ring_history(snapshot, Some(&self.incarnation));
        history.journal_write_failures = writer.failures;
        let mut generations = Vec::new();
        // LineSink names generations by appending .1, .2, ...; only the sink
        // rotates or prunes them. Read oldest first for stable timestamp ties.
        for generation in (0..=RETENTION.keep).rev() {
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
        drop(writer);
        JournalRead {
            #[cfg(test)]
            journal_path: self.path.clone(),
            history,
            generations,
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
                            if entry.module_id == module_id {
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
    use std::sync::Arc;
    use subc_control::{TerminalDisposition, TerminalExitKind};
    use subc_test_support::TestTempDir;

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
    #[cfg(unix)]
    fn shutdown_marker_survives_reopen_without_becoming_corruption_or_an_exit() {
        let dir = TestTempDir::new("terminal-journal-shutdown");
        let path = dir.join("terminals.jsonl");
        let journal = TerminalJournal::open(path.clone(), "cut-daemon".into());
        journal.append("module", &record(8));
        journal.stamp_shutdown();
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

    #[test]
    fn journal_preserves_exit_fields_filters_modules_and_isolates_a_torn_line() {
        let dir = TestTempDir::new("terminal-journal-fields");
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
        let dir = TestTempDir::new("terminal-journal-merge");
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
        let dir = TestTempDir::new("terminal-journal-rotation");
        let path = dir.join("terminals.jsonl");
        let journal = TerminalJournal::open(path.clone(), "first".into());
        let mut large = record(1);
        large.disposition_detail = Some("x".repeat(600_000));
        journal.append("module", &large);
        large.at_ms = 2;
        journal.append("module", &large);
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
        let dir = TestTempDir::new("terminal-journal-write-failure");
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
