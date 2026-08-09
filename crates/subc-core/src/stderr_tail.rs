//! Bounded per-module stderr capture.
//!
//! # Why this exists
//!
//! `last_exit_code` survives a respawn because the supervisor holds it in memory.
//! Stderr had no such path: it went to the daemon's inherited fd, and from there
//! to whatever rotates or evicts it. On the box this was written for, that window
//! was about three hours; on another it was bounded by a log file reaching 908 MB
//! with one module accounting for 98% of it. Two hosts, two mechanisms, the same
//! outcome -- the text explaining a crash is gone by the time anyone asks.
//!
//! So this keeps the last few lines where `last_exit` already lives: in supervisor
//! memory, immune to whatever happens to the log.
//!
//! # What it is not
//!
//! Not a log. The ring is deliberately small and lossy, and callers are expected
//! to know they are reading a tail rather than a history. The daemon log keeps
//! doing its job; this exists because that job has a time limit.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncReadExt;

/// Longest single line admitted to the ring before truncation.
///
/// A module emitting a 40 MB backtrace on one line satisfies any line-count cap
/// while evicting everything else -- the pathological emitter wins twice, once by
/// filling the ring and once by being unreadable itself. Truncating on the way in
/// costs that emitter one line instead of the whole tail.
pub const DEFAULT_MAX_LINE_BYTES: usize = 2048;

/// Lines retained per module.
pub const DEFAULT_MAX_LINES: usize = 200;

/// Total bytes retained per module, across all lines.
///
/// Both this and [`DEFAULT_MAX_LINES`] apply; whichever binds first wins. A line
/// cap alone is satisfied by 200 lines of 2 KB, which is not a budget worth
/// holding for fifteen modules.
pub const DEFAULT_MAX_BYTES: usize = 64 * 1024;

/// Marker appended to a line cut at [`StderrTailConfig::max_line_bytes`].
///
/// Visible rather than silent: a reader must be able to tell a truncated line
/// from a short one, or the truncation becomes a second way to read wrong text
/// confidently.
pub const TRUNCATION_MARKER: &str = "…[truncated]";

/// Whether a module's stderr is being captured, and if not, why not.
///
/// Typed rather than nullable so `NotCaptured` has to be handled rather than
/// defaulted past. An empty tail and an uncaptured one send an operator in
/// opposite directions -- one says the module printed nothing before dying, the
/// other says nobody was listening -- and rendering them alike is the defect this
/// module exists to fix, reproduced one layer up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureState {
    /// A reader is attached, or was attached and reached clean EOF.
    Captured,
    /// No reader was attached. The tail says nothing about what the module wrote.
    NotCaptured { reason: String },
}

/// One retained entry.
///
/// Boundaries are in-band rather than a separate field because their position
/// relative to the lines is the whole point: "these three lines came from the
/// process that died, those came from its replacement" is unanswerable from a
/// count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailEntry {
    Line {
        text: String,
        /// This line was cut at the per-line cap.
        truncated: bool,
    },
    /// The supervisor spawned a new process for this module. Lines after this
    /// entry come from the new one.
    ProcessStart,
}

impl TailEntry {
    fn cost(&self) -> usize {
        match self {
            Self::Line { text, .. } => text.len(),
            Self::ProcessStart => 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StderrTailConfig {
    pub max_lines: usize,
    pub max_bytes: usize,
    pub max_line_bytes: usize,
}

impl Default for StderrTailConfig {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            max_line_bytes: DEFAULT_MAX_LINE_BYTES,
        }
    }
}

/// A module's retained stderr, oldest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StderrTailSnapshot {
    pub capture: CaptureState,
    pub entries: Vec<TailEntry>,
    /// Lines evicted since the module was first supervised.
    ///
    /// Non-zero means the tail starts mid-stream. That is the ring working as
    /// intended, but a reader diagnosing a crash needs to know the first retained
    /// line is not the first line the module wrote -- otherwise an absent cause
    /// reads as a module that never explained itself.
    pub dropped_lines: u64,
}

impl StderrTailSnapshot {
    /// The uncaptured case, for a module whose stderr was never piped.
    pub fn not_captured(reason: impl Into<String>) -> Self {
        Self {
            capture: CaptureState::NotCaptured {
                reason: reason.into(),
            },
            entries: Vec::new(),
            dropped_lines: 0,
        }
    }
}

/// Bounded ring of a single module's stderr lines.
///
/// Survives respawn deliberately. The stderr explaining an exit is written
/// *before* that exit, so clearing on restart would discard the lines exactly
/// when they become the thing being asked for. [`TailEntry::ProcessStart`] keeps
/// the generations distinguishable instead.
#[derive(Debug)]
pub struct StderrRing {
    config: StderrTailConfig,
    entries: VecDeque<TailEntry>,
    bytes: usize,
    dropped_lines: u64,
    capture: CaptureState,
}

impl StderrRing {
    pub fn new(config: StderrTailConfig) -> Self {
        Self {
            config,
            entries: VecDeque::new(),
            bytes: 0,
            dropped_lines: 0,
            // Until a reader attaches, the honest answer is that nothing is
            // listening -- not that the module has been quiet.
            capture: CaptureState::NotCaptured {
                reason: "stderr reader has not started".to_string(),
            },
        }
    }

    pub fn mark_captured(&mut self) {
        self.capture = CaptureState::Captured;
    }

    pub fn mark_not_captured(&mut self, reason: impl Into<String>) {
        self.capture = CaptureState::NotCaptured {
            reason: reason.into(),
        };
    }

    /// Record that a new process was spawned for this module.
    ///
    /// A boundary separates output on either side of it, so one with nothing
    /// before it separates nothing: on the FIRST spawn it would make a module
    /// that printed nothing render as a marker rather than as empty, and the
    /// caller then has to decide whether a one-marker tail counts as silence.
    /// Recording it only once there is something to divide keeps "captured and
    /// empty" literally empty.
    pub fn push_process_start(&mut self) {
        if self.entries.is_empty() && self.dropped_lines == 0 {
            return;
        }
        self.push_entry(TailEntry::ProcessStart);
    }

    /// Admit one complete line, truncating it if it exceeds the per-line cap.
    ///
    /// `line` must not contain a trailing newline; the reader strips it so the
    /// stored text and the byte accounting agree.
    pub fn push_line(&mut self, line: &str) {
        let (text, truncated) = truncate_line(line, self.config.max_line_bytes);
        self.push_entry(TailEntry::Line { text, truncated });
    }

    fn push_entry(&mut self, entry: TailEntry) {
        self.bytes += entry.cost();
        self.entries.push_back(entry);
        self.evict_to_fit();
    }

    fn evict_to_fit(&mut self) {
        while self.entries.len() > self.config.max_lines
            || (self.bytes > self.config.max_bytes && self.entries.len() > 1)
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= evicted.cost();
            if matches!(evicted, TailEntry::Line { .. }) {
                self.dropped_lines += 1;
            }
        }
    }

    /// The most recent entries, oldest first, bounded by the caller's limits.
    ///
    /// `max_lines`/`max_bytes` narrow the ring's own caps; they cannot widen them.
    pub fn snapshot(
        &self,
        max_lines: Option<usize>,
        max_bytes: Option<usize>,
    ) -> StderrTailSnapshot {
        let line_limit = max_lines.unwrap_or(self.config.max_lines);
        let byte_limit = max_bytes.unwrap_or(self.config.max_bytes);

        let mut taken: Vec<TailEntry> = Vec::new();
        let mut bytes = 0usize;
        // Walk backwards: a tail is anchored at the newest end, so a caller
        // asking for 20 lines wants the last 20, not the first 20.
        for entry in self.entries.iter().rev() {
            if taken.len() >= line_limit {
                break;
            }
            let cost = entry.cost();
            if !taken.is_empty() && bytes + cost > byte_limit {
                break;
            }
            bytes += cost;
            taken.push(entry.clone());
        }
        taken.reverse();

        let withheld = self
            .entries
            .iter()
            .filter(|entry| matches!(entry, TailEntry::Line { .. }))
            .count()
            .saturating_sub(
                taken
                    .iter()
                    .filter(|entry| matches!(entry, TailEntry::Line { .. }))
                    .count(),
            );

        StderrTailSnapshot {
            capture: self.capture.clone(),
            entries: taken,
            // Lines the ring evicted plus lines this request's own limits held
            // back. Both mean the same thing to the reader -- the text above is
            // not the beginning -- and separating them would invite treating a
            // narrow request as evidence of a quiet module.
            dropped_lines: self.dropped_lines + withheld as u64,
        }
    }
}

/// Reassembly buffer ceiling for a line with no newline in sight.
///
/// The ring truncates what it stores, but the READER has to hold the bytes until
/// it finds a delimiter. A module emitting a gigabyte with no newline would grow
/// this buffer without bound and take the daemon down with it -- a module fault
/// escalating into a fleet fault, which is exactly what supervision exists to
/// prevent. At this ceiling the pending bytes are flushed as a line and
/// reassembly restarts.
const MAX_PENDING_LINE_BYTES: usize = 1024 * 1024;

/// Read a child's stderr to EOF: retain a bounded tail, and forward every line on.
///
/// # Forwarding is mandatory, not a courtesy
///
/// Measured on the live daemon log: 4727 of the last 5000 lines carried a module
/// tag. That file is a module log with some daemon lines in it, not the reverse.
/// A tap that captured without forwarding would leave it nearly empty, and every
/// existing reader -- including fleet scripts -- would report clean on nothing.
/// An absence that reads as calm is worse than the interleaving this replaces.
///
/// # Bytes are forwarded verbatim
///
/// The ring stores lossy UTF-8 because it renders into JSON; the forward writes
/// the ORIGINAL bytes. Anything else silently rewrites a log other tools parse.
///
/// # One write per complete line
///
/// Inheriting the daemon's fd gave line atomicity for free: a module's own write
/// reached the fd in one syscall. Reading a pipe and re-emitting can split a line
/// that used to be atomic, so this reassembles first and writes each complete
/// line in a single call -- otherwise the fix introduces a defect the previous
/// design did not have.
pub async fn pump_stderr<R>(source: R, ring: Arc<Mutex<StderrRing>>)
where
    R: AsyncReadExt + Unpin,
{
    pump_stderr_into(source, ring, &mut StderrSink).await
}

/// Where forwarded lines go. Exists so tests can assert that forwarding HAPPENS
/// and that each line arrives in one write -- the property that makes this a
/// replacement for inherited stdio rather than a regression from it.
pub trait LineSink {
    fn write_line(&mut self, line: &[u8]);
}

struct StderrSink;

impl LineSink for StderrSink {
    fn write_line(&mut self, line: &[u8]) {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        // Best-effort: a failed forward must not stop capture. Losing a log line
        // is recoverable; losing the tail that explains a crash is the thing
        // being fixed.
        let _ = handle.write_all(line);
    }
}

async fn pump_stderr_into<R, S>(mut source: R, ring: Arc<Mutex<StderrRing>>, sink: &mut S)
where
    R: AsyncReadExt + Unpin,
    S: LineSink,
{
    {
        let mut guard = lock_ring(&ring);
        guard.mark_captured();
    }

    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    loop {
        let read = match source.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(err) => {
                let mut guard = lock_ring(&ring);
                // The tail up to this point stays valid and readable; what changes
                // is that it is no longer complete, and saying so beats letting a
                // truncated capture read as a module that stopped talking.
                guard.mark_not_captured(format!("stderr read failed: {err}"));
                return;
            }
        };

        pending.extend_from_slice(&chunk[..read]);

        while let Some(newline) = pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<u8> = pending.drain(..=newline).collect();
            emit_line(&ring, sink, &line[..line.len() - 1]);
        }

        if pending.len() >= MAX_PENDING_LINE_BYTES {
            let line = std::mem::take(&mut pending);
            emit_line(&ring, sink, &line);
        }
    }

    // A process that dies mid-line still wrote the bytes, and on a crash that
    // fragment is disproportionately likely to be the message worth reading.
    if !pending.is_empty() {
        emit_line(&ring, sink, &pending);
    }
}

fn emit_line<S: LineSink>(ring: &Arc<Mutex<StderrRing>>, sink: &mut S, raw: &[u8]) {
    {
        let mut guard = lock_ring(ring);
        guard.push_line(&String::from_utf8_lossy(raw));
    }

    // Framed and written in ONE call. Two writes -- body then newline -- would
    // reintroduce exactly the interleaving that inheriting the fd avoided.
    let mut framed = Vec::with_capacity(raw.len() + 1);
    framed.extend_from_slice(raw);
    framed.push(b'\n');
    sink.write_line(&framed);
}

fn lock_ring(ring: &Arc<Mutex<StderrRing>>) -> std::sync::MutexGuard<'_, StderrRing> {
    ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Cut `line` to at most `max_bytes`, appending [`TRUNCATION_MARKER`] when it did.
///
/// Cuts on a char boundary: slicing a multi-byte sequence would produce invalid
/// UTF-8, and a panic while capturing a crash message is the worst possible time
/// to discover that.
fn truncate_line(line: &str, max_bytes: usize) -> (String, bool) {
    if line.len() <= max_bytes {
        return (line.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !line.is_char_boundary(end) {
        end -= 1;
    }
    let mut text = line[..end].to_string();
    text.push_str(TRUNCATION_MARKER);
    (text, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring(max_lines: usize, max_bytes: usize, max_line_bytes: usize) -> StderrRing {
        StderrRing::new(StderrTailConfig {
            max_lines,
            max_bytes,
            max_line_bytes,
        })
    }

    fn lines(snapshot: &StderrTailSnapshot) -> Vec<String> {
        snapshot
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TailEntry::Line { text, .. } => Some(text.clone()),
                TailEntry::ProcessStart => None,
            })
            .collect()
    }

    #[test]
    fn a_fresh_ring_reports_not_captured_rather_than_empty() {
        // The distinction this whole module exists for: "nobody was listening"
        // must not render as "the module said nothing".
        let ring = ring(10, 1024, 128);
        let snapshot = ring.snapshot(None, None);
        assert!(matches!(snapshot.capture, CaptureState::NotCaptured { .. }));
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn a_captured_module_that_printed_nothing_is_distinguishable_from_an_uncaptured_one() {
        let mut captured = ring(10, 1024, 128);
        captured.mark_captured();
        let uncaptured = ring(10, 1024, 128);

        let captured = captured.snapshot(None, None);
        let uncaptured = uncaptured.snapshot(None, None);

        // Both are empty. Only the capture state separates them, which is the
        // point -- an assertion on emptiness alone would pass either way.
        assert!(captured.entries.is_empty());
        assert!(uncaptured.entries.is_empty());
        assert_eq!(captured.capture, CaptureState::Captured);
        assert!(matches!(
            uncaptured.capture,
            CaptureState::NotCaptured { .. }
        ));
    }

    #[test]
    fn the_line_cap_evicts_oldest_first_and_counts_what_it_dropped() {
        let mut ring = ring(3, 10_000, 128);
        ring.mark_captured();
        for i in 0..6 {
            ring.push_line(&format!("line{i}"));
        }
        let snapshot = ring.snapshot(None, None);
        assert_eq!(lines(&snapshot), vec!["line3", "line4", "line5"]);
        // Without this the tail silently becomes "the last lines that happened
        // to survive" and reads as complete.
        assert_eq!(snapshot.dropped_lines, 3);
    }

    #[test]
    fn the_byte_cap_binds_before_the_line_cap_when_lines_are_large() {
        // 100 lines allowed, but only ~30 bytes of them.
        let mut ring = ring(100, 30, 128);
        ring.mark_captured();
        for i in 0..10 {
            ring.push_line(&format!("{i}--------")); // 9 bytes each
        }
        let snapshot = ring.snapshot(None, None);
        assert!(
            snapshot.entries.len() < 10,
            "byte cap did not bind: {} entries retained",
            snapshot.entries.len()
        );
        let retained: usize = lines(&snapshot).iter().map(String::len).sum();
        assert!(
            retained <= 30,
            "retained {retained} bytes over a 30 byte cap"
        );
        assert!(snapshot.dropped_lines > 0);
    }

    #[test]
    fn one_enormous_line_is_truncated_rather_than_evicting_the_tail() {
        // The pathological-emitter case: without per-line truncation this single
        // line would evict every other line AND be unreadable itself.
        let mut ring = ring(10, 10_000, 64);
        ring.mark_captured();
        ring.push_line("context line that must survive");
        ring.push_line(&"x".repeat(40_000));

        let snapshot = ring.snapshot(None, None);
        let kept = lines(&snapshot);
        assert_eq!(kept.len(), 2, "the earlier line was evicted by the big one");
        assert_eq!(kept[0], "context line that must survive");
        assert!(kept[1].ends_with(TRUNCATION_MARKER));
        assert!(kept[1].len() < 200);
    }

    #[test]
    fn truncation_is_visible_so_a_cut_line_is_not_mistaken_for_a_short_one() {
        let mut ring = ring(10, 10_000, 16);
        ring.mark_captured();
        ring.push_line("0123456789abcdefghij");
        ring.push_line("short");

        let snapshot = ring.snapshot(None, None);
        let TailEntry::Line { truncated, .. } = &snapshot.entries[0] else {
            panic!("expected a line");
        };
        assert!(truncated);
        let TailEntry::Line { truncated, .. } = &snapshot.entries[1] else {
            panic!("expected a line");
        };
        assert!(!truncated, "a short line must not be reported as truncated");
    }

    #[test]
    fn truncation_cuts_on_a_char_boundary_rather_than_splitting_utf8() {
        // A panic message with non-ASCII in it is not exotic, and slicing mid
        // sequence would panic while capturing a crash.
        let mut ring = ring(10, 10_000, 5);
        ring.mark_captured();
        ring.push_line("aa€€€€");
        let snapshot = ring.snapshot(None, None);
        let TailEntry::Line { text, truncated } = &snapshot.entries[0] else {
            panic!("expected a line");
        };
        assert!(truncated);
        assert!(text.starts_with("aa"));
    }

    #[test]
    fn a_restart_boundary_keeps_generations_distinguishable() {
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        ring.push_line("before the crash");
        ring.push_process_start();
        ring.push_line("after the respawn");

        let snapshot = ring.snapshot(None, None);
        assert_eq!(
            snapshot.entries,
            vec![
                TailEntry::Line {
                    text: "before the crash".to_string(),
                    truncated: false
                },
                TailEntry::ProcessStart,
                TailEntry::Line {
                    text: "after the respawn".to_string(),
                    truncated: false
                },
            ]
        );
    }

    #[test]
    fn the_ring_survives_respawn_because_the_cause_is_written_before_the_exit() {
        // Clearing on restart would discard the lines at the exact moment they
        // become the thing being asked for.
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        ring.push_line("Error: storage section missing");
        ring.push_process_start();

        let snapshot = ring.snapshot(None, None);
        assert!(lines(&snapshot).contains(&"Error: storage section missing".to_string()));
    }

    #[test]
    fn a_caller_limit_returns_the_newest_lines_not_the_oldest() {
        let mut ring = ring(100, 100_000, 128);
        ring.mark_captured();
        for i in 0..10 {
            ring.push_line(&format!("line{i}"));
        }
        let snapshot = ring.snapshot(Some(3), None);
        assert_eq!(lines(&snapshot), vec!["line7", "line8", "line9"]);
    }

    #[test]
    fn a_caller_limit_reports_what_it_withheld_rather_than_looking_complete() {
        let mut ring = ring(100, 100_000, 128);
        ring.mark_captured();
        for i in 0..10 {
            ring.push_line(&format!("line{i}"));
        }
        // Nothing was evicted; the narrowing is the caller's own. It still has to
        // be reported, or a 3-line request reads as a module that wrote 3 lines.
        assert_eq!(ring.snapshot(Some(3), None).dropped_lines, 7);
        assert_eq!(ring.snapshot(None, None).dropped_lines, 0);
    }

    #[test]
    fn a_caller_limit_cannot_widen_the_rings_own_caps() {
        let mut ring = ring(2, 10_000, 128);
        ring.mark_captured();
        for i in 0..5 {
            ring.push_line(&format!("line{i}"));
        }
        let snapshot = ring.snapshot(Some(1000), Some(1_000_000));
        assert_eq!(lines(&snapshot).len(), 2);
    }

    fn shared(max_lines: usize, max_bytes: usize, max_line_bytes: usize) -> Arc<Mutex<StderrRing>> {
        Arc::new(Mutex::new(ring(max_lines, max_bytes, max_line_bytes)))
    }

    /// Records each forwarded write separately, so a test can tell one write of
    /// `b"abc\n"` from two writes of `b"abc"` and `b"\n"`.
    #[derive(Default)]
    struct RecordingSink {
        writes: Vec<Vec<u8>>,
    }

    impl LineSink for RecordingSink {
        fn write_line(&mut self, line: &[u8]) {
            self.writes.push(line.to_vec());
        }
    }

    #[tokio::test]
    async fn the_pump_splits_on_newlines_and_keeps_a_trailing_fragment() {
        let ring = shared(10, 10_000, 128);
        // No trailing newline on the last line: a crashing process routinely dies
        // mid-line, and that fragment is often the message worth reading.
        let source = std::io::Cursor::new(b"one\ntwo\nthree".to_vec());
        let mut sink = RecordingSink::default();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;

        let snapshot = lock_ring(&ring).snapshot(None, None);
        assert_eq!(lines(&snapshot), vec!["one", "two", "three"]);
        assert_eq!(snapshot.capture, CaptureState::Captured);
    }

    #[tokio::test]
    async fn every_captured_line_is_also_forwarded() {
        // Forwarding is not optional. The daemon log is overwhelmingly module
        // output; a tap that captured without forwarding would leave it nearly
        // empty and every existing reader would report clean on nothing.
        let ring = shared(10, 10_000, 128);
        let source = std::io::Cursor::new(b"alpha\nbeta\n".to_vec());
        let mut sink = RecordingSink::default();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;

        assert_eq!(sink.writes, vec![b"alpha\n".to_vec(), b"beta\n".to_vec()]);
    }

    #[tokio::test]
    async fn each_forwarded_line_is_exactly_one_write() {
        // Inheriting the fd gave line atomicity for free. Reading a pipe and
        // re-emitting can split a line that used to be atomic, so the framing
        // must be one syscall per complete line -- asserted as one write per
        // line, not merely as correct bytes.
        let ring = shared(10, 10_000, 128);
        let source = std::io::Cursor::new(b"first\nsecond\nthird\n".to_vec());
        let mut sink = RecordingSink::default();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;

        assert_eq!(sink.writes.len(), 3);
        for write in &sink.writes {
            assert_eq!(
                write.iter().filter(|byte| **byte == b'\n').count(),
                1,
                "a write carried something other than exactly one complete line"
            );
            assert_eq!(*write.last().unwrap(), b'\n');
        }
    }

    #[test]
    fn the_first_process_start_is_not_recorded_because_it_divides_nothing() {
        // Otherwise a module that printed nothing renders as a lone boundary
        // marker, and every caller has to decide whether that counts as silence.
        let mut ring = ring(10, 10_000, 128);
        ring.push_process_start();
        assert!(ring.snapshot(None, None).entries.is_empty());

        ring.push_line("first process said this");
        ring.push_process_start();
        assert!(
            matches!(
                ring.snapshot(None, None).entries.last(),
                Some(TailEntry::ProcessStart)
            ),
            "a boundary with output before it must be recorded"
        );
    }

    #[test]
    fn a_process_start_is_recorded_when_only_dropped_lines_precede_it() {
        // The ring can be non-empty in the sense that matters -- lines were
        // written and evicted -- while `entries` is empty. Suppressing the
        // boundary there would attribute surviving output to the wrong process.
        let mut ring = ring(1, 10_000, 128);
        ring.push_line("evicted");
        ring.push_line("also evicted");
        let mut ring = ring;
        ring.entries.clear();
        ring.push_process_start();
        assert!(matches!(
            ring.snapshot(None, None).entries.first(),
            Some(TailEntry::ProcessStart)
        ));
    }

    #[tokio::test]
    async fn the_pump_marks_captured_even_when_the_module_writes_nothing() {
        // Clean EOF with no output is a module that was quiet, not one nobody
        // listened to -- and the two must not render alike.
        let ring = shared(10, 10_000, 128);
        let source = std::io::Cursor::new(Vec::new());
        let mut sink = RecordingSink::default();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;

        let snapshot = lock_ring(&ring).snapshot(None, None);
        assert!(snapshot.entries.is_empty());
        assert_eq!(snapshot.capture, CaptureState::Captured);
        assert!(sink.writes.is_empty());
    }

    #[tokio::test]
    async fn a_line_with_no_newline_cannot_grow_the_reader_without_bound() {
        // A module fault must not become a daemon fault: without the pending
        // ceiling this buffer grows to whatever the module writes.
        let ring = shared(10, 10_000_000, 4 * 1024 * 1024);
        let source = std::io::Cursor::new(vec![b'x'; MAX_PENDING_LINE_BYTES + 4096]);
        let mut sink = RecordingSink::default();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;

        let snapshot = lock_ring(&ring).snapshot(None, None);
        assert_eq!(
            lines(&snapshot).len(),
            2,
            "expected a forced flush at the ceiling plus the remainder"
        );
    }

    #[test]
    fn a_byte_limit_smaller_than_one_line_still_returns_that_line() {
        // Returning nothing would be indistinguishable from a quiet module, which
        // is the failure this module exists to prevent.
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        ring.push_line("a line considerably longer than the request limit");
        let snapshot = ring.snapshot(None, Some(4));
        assert_eq!(snapshot.entries.len(), 1);
    }
}
