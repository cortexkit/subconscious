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

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{SystemTime, UNIX_EPOCH};

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
    /// Retained entries are valid, but capture ended before clean EOF, or the
    /// pipe of a process the supervisor has already moved on from has not
    /// reached EOF yet. The second kind clears when that pipe does reach EOF,
    /// because from then on nothing that process wrote is missing.
    Incomplete { reason: String },
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
        /// Wall-clock Unix milliseconds at which the reader framed this line,
        /// the same instant stamped on its capture-file line. `None` for a line
        /// admitted without one (see [`StderrRing::push_line`]).
        at_ms: Option<u64>,
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

/// One stored entry. Unlike [`TailEntry`], a boundary remembers which process
/// generation it starts, so a line that arrives late from an older process can
/// be put back in that process's section instead of after its successor's
/// boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Slot {
    Line {
        text: String,
        truncated: bool,
        at_ms: Option<u64>,
    },
    ProcessStart {
        generation: u64,
    },
}

impl Slot {
    fn cost(&self) -> usize {
        match self {
            Self::Line { text, .. } => text.len(),
            Self::ProcessStart { .. } => 0,
        }
    }
}

/// Where the stderr reader of one process generation stands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum PumpPhase {
    /// The process is the supervisor's current concern; its lines go at the end.
    Attached,
    /// The supervisor has moved on from the process, but its pipe is still open.
    /// Lines it still delivers belong in its own section.
    Retired,
    /// Retired, and the pipe was still open when the supervisor stopped waiting
    /// for it. Reported as `Incomplete` until the pipe reaches EOF.
    Late { reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StderrTailConfig {
    max_lines: usize,
    max_bytes: usize,
    max_line_bytes: usize,
}

impl StderrTailConfig {
    /// Keeps every retained line within the ring's total byte budget.
    /// Clamp rather than reject so diagnostics degrade without blocking supervisor startup.
    pub const fn new(max_lines: usize, max_bytes: usize, max_line_bytes: usize) -> Self {
        Self {
            max_lines,
            max_bytes,
            max_line_bytes: if max_line_bytes > max_bytes {
                max_bytes
            } else {
                max_line_bytes
            },
        }
    }
}

impl Default for StderrTailConfig {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_LINES, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINE_BYTES)
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
///
/// A process's stderr reader can outlive the supervisor's interest in it: the
/// reader may not have been scheduled yet when the process exited, or a
/// descendant may still hold the pipe open. Lines such a reader delivers after
/// the next process started are placed before that next process's boundary, so
/// the section a line appears in always names the process that wrote it.
#[derive(Debug)]
pub struct StderrRing {
    config: StderrTailConfig,
    entries: VecDeque<Slot>,
    // Count of `TailEntry::Line` entries, kept running because eviction checks
    // it on every push and recounting would walk the whole ring under the
    // mutex each time.
    lines: usize,
    bytes: usize,
    dropped_lines: u64,
    capture: CaptureState,
    /// Generation of the newest process boundary; 0 before the first.
    generation: u64,
    /// Readers that have not reached EOF yet, by the generation they read for.
    pumps: BTreeMap<u64, PumpPhase>,
    /// Highest generation whose boundary was evicted from the front. A late line
    /// from an older generation belongs in front of that boundary, which is
    /// evicted territory, so it is counted as dropped rather than stored.
    evicted_through: u64,
}

impl StderrRing {
    pub fn new(config: StderrTailConfig) -> Self {
        Self {
            config,
            entries: VecDeque::new(),
            lines: 0,
            bytes: 0,
            dropped_lines: 0,
            // Until a reader attaches, the honest answer is that nothing is
            // listening -- not that the module has been quiet.
            capture: CaptureState::NotCaptured {
                reason: "stderr reader has not started".to_string(),
            },
            generation: 0,
            pumps: BTreeMap::new(),
            evicted_through: 0,
        }
    }

    /// Generation of the newest process boundary.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn mark_captured(&mut self) {
        if matches!(self.capture, CaptureState::NotCaptured { .. }) {
            self.capture = CaptureState::Captured;
        }
    }

    pub fn mark_incomplete(&mut self, reason: impl Into<String>) {
        self.capture = CaptureState::Incomplete {
            reason: reason.into(),
        };
    }

    pub fn mark_not_captured(&mut self, reason: impl Into<String>) {
        self.capture = CaptureState::NotCaptured {
            reason: reason.into(),
        };
    }

    /// Record that a new process was spawned for this module, returning the
    /// generation number its lines are attributed to.
    ///
    /// A boundary separates output on either side of it, so one with nothing
    /// before it separates nothing: on the FIRST spawn it would make a module
    /// that printed nothing render as a marker rather than as empty, and the
    /// caller then has to decide whether a one-marker tail counts as silence.
    /// The boundary is still stored, because a late line from an older process
    /// needs it to find its section, but [`Self::snapshot`] shows it only once
    /// there is output before it to divide, which keeps "captured and empty"
    /// literally empty.
    pub fn push_process_start(&mut self) -> u64 {
        self.generation += 1;
        let generation = self.generation;
        // Two boundaries in a row mean the process between them has written
        // nothing retained. The earlier one can go only if no reader from its
        // generation onward is still open: such a reader may yet deliver a line
        // that belongs between the two. Without this a module that restarts
        // silently would grow the ring by one boundary per restart forever.
        if let Some(Slot::ProcessStart {
            generation: previous,
        }) = self.entries.back()
        {
            if self.pumps.range(*previous..).next().is_none() {
                self.entries.pop_back();
            }
        }
        self.push_entry(Slot::ProcessStart { generation });
        generation
    }

    /// [`Self::push_process_start`] for a process whose stderr reader is
    /// attached: the reader is tracked until it calls [`Self::finish_pump`].
    pub(crate) fn begin_process(&mut self) -> u64 {
        let generation = self.push_process_start();
        self.pumps.insert(generation, PumpPhase::Attached);
        generation
    }

    /// The supervisor has moved on from this generation's process. Its reader
    /// keeps running, and any line it still delivers is kept in its own section.
    pub(crate) fn retire_pump(&mut self, generation: u64) {
        if let Some(phase @ PumpPhase::Attached) = self.pumps.get_mut(&generation) {
            *phase = PumpPhase::Retired;
        }
    }

    /// The supervisor stopped waiting for this generation's reader before its
    /// pipe reached EOF. The capture reads as `Incomplete` with `reason` until
    /// the reader finishes. A reader that already finished is left alone: it
    /// got everything.
    pub(crate) fn mark_pump_late(&mut self, generation: u64, reason: impl Into<String>) {
        if let Some(phase) = self.pumps.get_mut(&generation) {
            *phase = PumpPhase::Late {
                reason: reason.into(),
            };
        }
    }

    /// This generation's reader has stopped: at EOF, or on a read error that
    /// has already been recorded with [`Self::mark_incomplete`].
    pub(crate) fn finish_pump(&mut self, generation: u64) {
        self.pumps.remove(&generation);
    }

    /// Admit one complete line, truncating it if it exceeds the per-line cap.
    ///
    /// `line` must not contain a trailing newline; the reader strips it so the
    /// stored text and the byte accounting agree. The line carries no capture
    /// time: only the pipe reader knows when a line arrived, and it records
    /// that through [`Self::push_line_from_at`].
    pub fn push_line(&mut self, line: &str) {
        self.push_line_from(self.generation, line);
    }

    /// [`Self::push_line`] for a line read from `generation`'s pipe.
    pub(crate) fn push_line_from(&mut self, generation: u64, line: &str) {
        self.admit_line(generation, line, None);
    }

    /// [`Self::push_line_from`] for a line the reader framed at `at_ms`
    /// (wall-clock Unix milliseconds).
    pub(crate) fn push_line_from_at(&mut self, generation: u64, line: &str, at_ms: u64) {
        self.admit_line(generation, line, Some(at_ms));
    }

    /// A line from a retired process that arrives after a newer process started
    /// goes in front of the first boundary newer than its own generation. Lines
    /// from a process the supervisor has not retired (the incumbent during a
    /// swap's overlap) go at the end, as they arrive.
    fn admit_line(&mut self, generation: u64, line: &str, at_ms: Option<u64>) {
        let (text, truncated) = truncate_line(line, self.config.max_line_bytes);
        let slot = Slot::Line {
            text,
            truncated,
            at_ms,
        };
        let retired = matches!(
            self.pumps.get(&generation),
            Some(PumpPhase::Retired | PumpPhase::Late { .. })
        );
        if !retired || generation >= self.generation {
            self.push_entry(slot);
            return;
        }
        if self.evicted_through > generation {
            // The section this line belongs to has been evicted, so the line is
            // older than everything retained.
            self.dropped_lines += 1;
            return;
        }
        let index = self.entries.iter().position(
            |slot| matches!(slot, Slot::ProcessStart { generation: start } if *start > generation),
        );
        match index {
            Some(index) => self.insert_entry(index, slot),
            None => self.push_entry(slot),
        }
    }

    fn push_entry(&mut self, entry: Slot) {
        self.insert_entry(self.entries.len(), entry);
    }

    fn insert_entry(&mut self, index: usize, entry: Slot) {
        self.bytes += entry.cost();
        if matches!(entry, Slot::Line { .. }) {
            self.lines += 1;
        }
        self.entries.insert(index, entry);
        self.evict_to_fit();
    }

    fn evict_to_fit(&mut self) {
        while self.lines > self.config.max_lines
            || (self.bytes > self.config.max_bytes && self.entries.len() > 1)
        {
            let Some(evicted) = self.entries.pop_front() else {
                break;
            };
            self.bytes -= evicted.cost();
            match evicted {
                Slot::Line { .. } => {
                    self.lines -= 1;
                    self.dropped_lines += 1;
                }
                Slot::ProcessStart { generation } => {
                    self.evicted_through = self.evicted_through.max(generation);
                }
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

        // The stored boundaries, reduced to the ones worth showing: a boundary
        // with no output before it (retained or evicted) divides nothing, and
        // two in a row say no more than one.
        let mut visible: Vec<TailEntry> = Vec::with_capacity(self.entries.len());
        let mut output_before = self.dropped_lines > 0;
        for slot in &self.entries {
            match slot {
                Slot::Line {
                    text,
                    truncated,
                    at_ms,
                } => {
                    visible.push(TailEntry::Line {
                        text: text.clone(),
                        truncated: *truncated,
                        at_ms: *at_ms,
                    });
                    output_before = true;
                }
                Slot::ProcessStart { .. } => {
                    if output_before && !matches!(visible.last(), Some(TailEntry::ProcessStart)) {
                        visible.push(TailEntry::ProcessStart);
                    }
                }
            }
        }

        let mut taken: Vec<TailEntry> = Vec::new();
        let mut bytes = 0usize;
        let mut lines = 0usize;
        // Walk backwards: a tail is anchored at the newest end, so a caller
        // asking for 20 lines wants the last 20, not the first 20.
        for entry in visible.iter().rev() {
            match entry {
                TailEntry::Line { .. } => {
                    if lines >= line_limit {
                        break;
                    }
                    let cost = entry.cost();
                    if lines > 0 && bytes + cost > byte_limit {
                        break;
                    }
                    bytes += cost;
                    lines += 1;
                    taken.push(entry.clone());
                }
                TailEntry::ProcessStart if lines > 0 => taken.push(entry.clone()),
                TailEntry::ProcessStart => {}
            }
        }
        taken.reverse();

        let withheld = self.lines.saturating_sub(lines);

        // A retired process whose pipe the supervisor stopped waiting for may
        // still be writing; until its reader reaches EOF the tail cannot claim
        // to hold everything. A permanent state (a read failure, no pipe at
        // all) is the more specific fact and is reported as is.
        let late = self.pumps.values().find_map(|phase| match phase {
            PumpPhase::Late { reason } => Some(reason),
            _ => None,
        });
        let capture = match (&self.capture, late) {
            (CaptureState::Captured, Some(reason)) => CaptureState::Incomplete {
                reason: reason.clone(),
            },
            (capture, _) => capture.clone(),
        };

        StderrTailSnapshot {
            capture,
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

/// Read a child's stderr to EOF, retaining the bounded crash tail and forwarding
/// every complete line to the selected capture sink.
pub async fn pump_stderr<R>(source: R, ring: Arc<Mutex<StderrRing>>)
where
    R: AsyncReadExt + Unpin,
{
    pump_stderr_into(source, ring, &mut StderrSink).await
}

/// Shared destination for a child's stdout and stderr pumps.
///
/// The file keeps the historical `.stderr.log` name even though it carries both
/// streams; the stable name is part of the operator contract. Both pumps share
/// one mutex, and cortexkit-log writes each framed line in one call, so partial
/// lines from the two pipes cannot interleave.
#[derive(Clone)]
pub(crate) enum ChildOutputSink {
    File {
        sink: Arc<Mutex<cortexkit_log::LineSink>>,
        path: Arc<PathBuf>,
        failure_reported: Arc<AtomicBool>,
    },
    Stderr,
}

impl ChildOutputSink {
    pub(crate) fn open(path: &Path, retention: cortexkit_log::Retention) -> io::Result<Self> {
        Ok(Self::File {
            sink: Arc::new(Mutex::new(cortexkit_log::LineSink::open(path, retention)?)),
            path: Arc::new(path.to_path_buf()),
            failure_reported: Arc::new(AtomicBool::new(false)),
        })
    }
}

/// Where forwarded complete lines go. It exists so tests can observe framing
/// and so production can serialize the two child pipes through one file sink.
pub trait OutputSink {
    fn write_line(&mut self, line: &[u8]);

    /// Whether each write should begin with its capture time (see
    /// [`format_capture_stamp`]).
    ///
    /// Only a sink that ends every write with a newline may say yes: then each
    /// write is a whole line of its own, and a stamp at the front of the write
    /// is at the front of a line. A sink that passes an unterminated piece
    /// through as-is would have the next piece continue the same line, and a
    /// stamp there would land in the middle of it.
    fn stamps_lines(&self) -> bool {
        false
    }
}

struct StderrSink;

impl OutputSink for StderrSink {
    fn write_line(&mut self, line: &[u8]) {
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        let _ = handle.write_all(line);
    }
}

impl OutputSink for ChildOutputSink {
    fn write_line(&mut self, line: &[u8]) {
        match self {
            Self::File {
                sink,
                path,
                failure_reported,
            } => {
                let result = sink
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .write_line(line);
                if let Err(error) = result {
                    if !failure_reported.swap(true, Ordering::Relaxed) {
                        tracing::warn!(
                            path = %path.display(),
                            error = %error,
                            "child output capture write failed; later failures are suppressed"
                        );
                    }
                }
            }
            Self::Stderr => StderrSink.write_line(line),
        }
    }

    // The capture file is read long after it was written, often next to the
    // daemon's own log, whose lines carry a time. `LineSink::write_line`
    // appends a newline to any write that lacks one, so every write here is a
    // whole line and may carry the stamp. The daemon's inherited stderr gets
    // the module's bytes unchanged: an unterminated piece is continued there by
    // the next one, and whatever collects that stream stamps it itself.
    fn stamps_lines(&self) -> bool {
        matches!(self, Self::File { .. })
    }
}

/// Read one process generation's stderr to EOF. `generation` is the value
/// [`StderrRing::begin_process`] returned for that process; it decides which
/// section of the ring a line lands in if the reader outlives the process.
pub(crate) async fn pump_stderr_to<R, S>(
    source: R,
    ring: Arc<Mutex<StderrRing>>,
    generation: u64,
    mut sink: S,
) where
    R: AsyncReadExt + Unpin,
    S: OutputSink,
{
    pump_lines_into(source, Some((&ring, generation)), &mut sink, "stderr").await;
}

pub(crate) async fn pump_stdout_to<R>(source: R, mut sink: ChildOutputSink)
where
    R: AsyncReadExt + Unpin,
{
    pump_lines_into(source, None, &mut sink, "stdout").await;
}

/// Read stderr for whichever process generation is newest when the reader
/// starts.
async fn pump_stderr_into<R, S>(source: R, ring: Arc<Mutex<StderrRing>>, sink: &mut S)
where
    R: AsyncReadExt + Unpin,
    S: OutputSink,
{
    let generation = lock_ring(&ring).generation();
    pump_lines_into(source, Some((&ring, generation)), sink, "stderr").await;
}

async fn pump_lines_into<R, S>(
    mut source: R,
    ring: Option<(&Arc<Mutex<StderrRing>>, u64)>,
    sink: &mut S,
    stream_name: &str,
) where
    R: AsyncReadExt + Unpin,
    S: OutputSink,
{
    if let Some((ring, _)) = ring {
        lock_ring(ring).mark_captured();
    }

    let mut pending: Vec<u8> = Vec::new();
    // Bytes before `scanned_upto` are already known to hold no newline; searching
    // them again would rescan the whole buffer on every chunk -- for a line with
    // no newline that is about 64 MiB examined per MiB of module output.
    let mut scanned_upto = 0usize;
    // Bytes before `cursor` were emitted as complete lines. They are removed in
    // one compaction per chunk rather than shifting the buffer once per line.
    let mut cursor = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        let read = match source.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(error) => {
                if let Some((ring, generation)) = ring {
                    let mut ring = lock_ring(ring);
                    ring.mark_incomplete(format!("{stream_name} read failed: {error}"));
                    ring.finish_pump(generation);
                } else {
                    tracing::warn!(stream = stream_name, error = %error, "child output capture read failed");
                }
                return;
            }
        };
        pending.extend_from_slice(&chunk[..read]);

        while let Some(relative) = find_newline(&pending[scanned_upto..]) {
            let newline = scanned_upto + relative;
            emit_line(ring, sink, &pending[cursor..newline], true);
            cursor = newline + 1;
            scanned_upto = cursor;
        }
        scanned_upto = pending.len();

        if cursor > 0 {
            pending.drain(..cursor);
            scanned_upto -= cursor;
            cursor = 0;
        }

        if pending.len() >= MAX_PENDING_LINE_BYTES {
            let line = std::mem::take(&mut pending);
            emit_line(ring, sink, &line, false);
            scanned_upto = 0;
        }
    }

    if !pending.is_empty() {
        emit_line(ring, sink, &pending, false);
    }
    if let Some((ring, generation)) = ring {
        lock_ring(ring).finish_pump(generation);
    }
}

// Bytes examined by newline searches, summed across a pump. Tests use this to
// assert the reader does not rescan bytes it already knows contain no newline.
// Per thread, not process-wide: other tests pump concurrently on their own
// threads, and a shared counter picked up their searches too, failing the
// bound by a few bytes under a parallel run. `#[tokio::test]` runs its body
// and every future it awaits on one thread, so a pump's searches land on the
// test's own counter.
#[cfg(test)]
thread_local! {
    static SCANNED_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn take_scanned_bytes() -> usize {
    SCANNED_BYTES.with(|scanned| scanned.replace(0))
}

/// Locate the next newline in `haystack`, counting the bytes examined so a
/// test can observe how much of the pending buffer each search walks.
fn find_newline(haystack: &[u8]) -> Option<usize> {
    let found = memchr::memchr(b'\n', haystack);
    #[cfg(test)]
    SCANNED_BYTES.with(|scanned| {
        scanned.set(scanned.get() + found.map(|index| index + 1).unwrap_or(haystack.len()));
    });
    found
}

fn emit_line<S: OutputSink>(
    ring: Option<(&Arc<Mutex<StderrRing>>, u64)>,
    sink: &mut S,
    raw: &[u8],
    terminated: bool,
) {
    // One instant for both destinations, taken here because this is where the
    // line is complete. The module's bytes do not say when the line arrived,
    // so the time is stored beside it: as `at_ms` in the ring and as the stamp
    // in the file. A time assigned later, when either is read, would look
    // recorded while being off by however long the line sat there.
    let at_ms = unix_ms(SystemTime::now());
    if let Some((ring, generation)) = ring {
        lock_ring(ring).push_line_from_at(generation, &String::from_utf8_lossy(raw), at_ms);
    }

    // Framed and written in ONE call, stamp included. Two writes would let the
    // other pipe land between the pieces.
    let stamp = sink.stamps_lines().then(|| format_capture_stamp(at_ms));
    if stamp.is_none() && !terminated {
        sink.write_line(raw);
        return;
    }
    let mut framed = Vec::with_capacity(CAPTURE_STAMP_PREFIX_LEN + raw.len() + 1);
    if let Some(stamp) = stamp {
        framed.extend_from_slice(stamp.as_bytes());
        framed.push(b' ');
    }
    framed.extend_from_slice(raw);
    if terminated {
        framed.push(b'\n');
    }
    sink.write_line(&framed);
}

fn unix_ms(at: SystemTime) -> u64 {
    // A clock set before 1970 is already wrong about every time it reports;
    // saturating keeps the stamp well-formed rather than failing the write.
    at.duration_since(UNIX_EPOCH)
        .map(|since| u64::try_from(since.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Length of a capture stamp, `2026-09-19T07:04:00.685Z`.
pub const CAPTURE_STAMP_LEN: usize = 24;

/// Length of the prefix on a capture-file line: the stamp and one space.
pub const CAPTURE_STAMP_PREFIX_LEN: usize = CAPTURE_STAMP_LEN + 1;

/// `at_ms` (Unix milliseconds) as RFC 3339 UTC with milliseconds and `Z`,
/// the form the daemon's own log lines begin with, so one parser reads the
/// time of a line in either file. Always [`CAPTURE_STAMP_LEN`] bytes for any
/// time before the year 10000.
pub fn format_capture_stamp(at_ms: u64) -> String {
    let seconds = at_ms / 1000;
    let millis = at_ms % 1000;
    let days = seconds / 86_400;
    let of_day = seconds % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        of_day / 3600,
        (of_day % 3600) / 60,
        of_day % 60,
    )
}

/// Split a capture-file line into the time its stamp records (Unix
/// milliseconds) and the module's bytes after the stamp's space.
///
/// `None` when the line does not begin with a well-formed stamp and one space:
/// a line written before capture lines were stamped has no recorded time, and
/// the caller must leave it undated rather than guess one.
pub fn split_capture_stamp(line: &str) -> Option<(u64, &str)> {
    let bytes = line.as_bytes();
    if bytes.len() < CAPTURE_STAMP_PREFIX_LEN || bytes[CAPTURE_STAMP_LEN] != b' ' {
        return None;
    }
    let stamp = &bytes[..CAPTURE_STAMP_LEN];
    for (index, expected) in [
        (4, b'-'),
        (7, b'-'),
        (10, b'T'),
        (13, b':'),
        (16, b':'),
        (19, b'.'),
        (23, b'Z'),
    ] {
        if stamp[index] != expected {
            return None;
        }
    }
    let number = |from: usize, to: usize| -> Option<u64> {
        let digits = &stamp[from..to];
        if !digits.iter().all(u8::is_ascii_digit) {
            return None;
        }
        Some(
            digits
                .iter()
                .fold(0u64, |total, digit| total * 10 + u64::from(digit - b'0')),
        )
    };
    let year = number(0, 4)?;
    let month = number(5, 7)?;
    let day = number(8, 10)?;
    let hour = number(11, 13)?;
    let minute = number(14, 16)?;
    let second = number(17, 19)?;
    let millis = number(20, 23)?;
    if year < 1970
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3600 + minute * 60 + second;
    // Index 25 follows an ASCII space, so it is a char boundary.
    Some((seconds * 1000 + millis, &line[CAPTURE_STAMP_PREFIX_LEN..]))
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Days since 1970-01-01 to a proleptic Gregorian date and back, after Howard
// Hinnant's `civil_from_days`/`days_from_civil`, restricted to dates from 1970
// on so the arithmetic stays unsigned.
fn civil_from_days(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    (year, month, day)
}

fn days_from_civil(year: u64, month: u64, day: u64) -> u64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year / 400;
    let yoe = year - era * 400;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `entries` with every capture time removed, for tests that compare what a
/// pump stored against expected text: the times are whatever the clock read,
/// and the tests that check them do so on their own.
#[cfg(test)]
pub(crate) fn untimed(entries: Vec<TailEntry>) -> Vec<TailEntry> {
    entries
        .into_iter()
        .map(|entry| match entry {
            TailEntry::Line {
                text, truncated, ..
            } => TailEntry::Line {
                text,
                truncated,
                at_ms: None,
            },
            TailEntry::ProcessStart => TailEntry::ProcessStart,
        })
        .collect()
}

fn lock_ring(ring: &Arc<Mutex<StderrRing>>) -> std::sync::MutexGuard<'_, StderrRing> {
    ring.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Cut `line` to at most `max_bytes`, reporting whether it was shortened.
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
    (line[..end].to_string(), true)
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;
    use tokio::io::{AsyncRead, ReadBuf};

    fn ring(max_lines: usize, max_bytes: usize, max_line_bytes: usize) -> StderrRing {
        StderrRing::new(StderrTailConfig::new(max_lines, max_bytes, max_line_bytes))
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
        let kept = &snapshot.entries;
        assert!(matches!(
            &kept[0],
            TailEntry::Line { text, truncated: false, .. }
                if text == "context line that must survive"
        ));
        let TailEntry::Line {
            text, truncated, ..
        } = &kept[1]
        else {
            panic!("expected a truncated line");
        };
        assert_eq!(text, &"x".repeat(64));
        assert!(*truncated);
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
        let TailEntry::Line {
            text, truncated, ..
        } = &snapshot.entries[0]
        else {
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
                    truncated: false,
                    at_ms: None,
                },
                TailEntry::ProcessStart,
                TailEntry::Line {
                    text: "after the respawn".to_string(),
                    truncated: false,
                    at_ms: None,
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
    fn a_caller_line_limit_keeps_the_boundary_before_the_selected_line() {
        let mut ring = ring(100, 100_000, 128);
        ring.mark_captured();
        ring.push_line("before restart");
        ring.push_process_start();
        ring.push_line("after restart");

        let snapshot = ring.snapshot(Some(1), None);
        assert_eq!(
            snapshot.entries,
            vec![
                TailEntry::ProcessStart,
                TailEntry::Line {
                    text: "after restart".to_string(),
                    truncated: false,
                    at_ms: None,
                },
            ]
        );
    }

    #[test]
    fn a_caller_line_limit_omits_a_trailing_boundary_after_the_selected_line() {
        let mut ring = ring(100, 100_000, 128);
        ring.mark_captured();
        ring.push_line("before restart");
        ring.push_process_start();

        let snapshot = ring.snapshot(Some(1), None);
        assert_eq!(
            snapshot.entries,
            vec![TailEntry::Line {
                text: "before restart".to_string(),
                truncated: false,
                at_ms: None,
            }]
        );
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

    impl OutputSink for RecordingSink {
        fn write_line(&mut self, line: &[u8]) {
            self.writes.push(line.to_vec());
        }
    }

    /// Yields predetermined chunks, one per read, so a test controls exactly
    /// where the byte stream is split.
    struct ChunkedReader {
        chunks: VecDeque<Vec<u8>>,
    }

    impl AsyncRead for ChunkedReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.chunks.pop_front() {
                None => Poll::Ready(Ok(())),
                Some(chunk) => {
                    buf.put_slice(&chunk);
                    Poll::Ready(Ok(()))
                }
            }
        }
    }

    struct FailingReader {
        bytes: Vec<u8>,
        emitted: bool,
    }

    impl AsyncRead for FailingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            if self.emitted {
                return Poll::Ready(Err(io::Error::other("reader failed")));
            }
            self.emitted = true;
            buf.put_slice(&self.bytes);
            Poll::Ready(Ok(()))
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
        assert_eq!(
            sink.writes,
            vec![b"one\n".to_vec(), b"two\n".to_vec(), b"three".to_vec()]
        );
    }

    #[tokio::test]
    async fn a_read_failure_keeps_prior_lines_and_marks_the_capture_incomplete() {
        let ring = shared(10, 10_000, 128);
        let source = FailingReader {
            bytes: b"crash cause\n".to_vec(),
            emitted: false,
        };
        let mut sink = RecordingSink::default();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;

        let snapshot = lock_ring(&ring).snapshot(None, None);
        assert_eq!(lines(&snapshot), vec!["crash cause"]);
        assert!(matches!(
            snapshot.capture,
            CaptureState::Incomplete { ref reason } if reason.contains("reader failed")
        ));
        assert_eq!(sink.writes, vec![b"crash cause\n".to_vec()]);
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
            matches!(ring.entries.back(), Some(Slot::ProcessStart { .. })),
            "a boundary with output before it must be recorded"
        );
        ring.push_line("second process said this");
        assert_eq!(
            ring.snapshot(None, None).entries,
            vec![
                TailEntry::Line {
                    text: "first process said this".to_string(),
                    truncated: false,
                    at_ms: None,
                },
                TailEntry::ProcessStart,
                TailEntry::Line {
                    text: "second process said this".to_string(),
                    truncated: false,
                    at_ms: None,
                },
            ],
            "only the boundary with output before it may be shown"
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
        // Emptying `entries` by hand must also zero the running totals kept
        // beside it, or the ring holds counts for lines it no longer has and
        // any later eviction decision is made against the stale numbers.
        ring.entries.clear();
        ring.lines = 0;
        ring.bytes = 0;
        ring.push_process_start();
        ring.push_line("survivor");
        assert_eq!(
            ring.snapshot(None, None).entries,
            vec![
                TailEntry::ProcessStart,
                TailEntry::Line {
                    text: "survivor".to_string(),
                    truncated: false,
                    at_ms: None,
                },
            ]
        );
    }

    fn line(text: &str) -> TailEntry {
        TailEntry::Line {
            text: text.to_string(),
            truncated: false,
            at_ms: None,
        }
    }

    #[test]
    fn a_late_line_from_a_retired_process_lands_in_that_processs_section() {
        // The reader of a process that already exited may deliver its last
        // lines after the next process started. Appending them would put the
        // crash's own explanation under its successor's boundary.
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        let old = ring.begin_process();
        ring.push_line_from(old, "old: booting");
        ring.retire_pump(old);
        let new = ring.begin_process();
        ring.push_line_from(new, "new: booting");
        ring.push_line_from(old, "old: config error");

        assert_eq!(
            ring.snapshot(None, None).entries,
            vec![
                line("old: booting"),
                line("old: config error"),
                TailEntry::ProcessStart,
                line("new: booting"),
            ]
        );
    }

    #[test]
    fn a_line_from_a_process_that_was_not_retired_is_appended_as_it_arrives() {
        // A swap runs the incumbent alongside its candidate; until the
        // supervisor retires it, the incumbent is live and its lines are news.
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        let incumbent = ring.begin_process();
        ring.push_line_from(incumbent, "incumbent: before");
        let candidate = ring.begin_process();
        ring.push_line_from(candidate, "candidate: booting");
        ring.push_line_from(incumbent, "incumbent: still serving");

        assert_eq!(
            ring.snapshot(None, None).entries,
            vec![
                line("incumbent: before"),
                TailEntry::ProcessStart,
                line("candidate: booting"),
                line("incumbent: still serving"),
            ]
        );
    }

    #[test]
    fn a_late_line_keeps_its_section_when_the_process_had_printed_nothing_before() {
        // A process whose reader had delivered nothing when its successor
        // started has a boundary with nothing after it. Dropping that boundary
        // as redundant would file the late line under the process before.
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        let first = ring.begin_process();
        ring.push_line_from(first, "first: done");
        ring.finish_pump(first);
        let old = ring.begin_process();
        ring.retire_pump(old);
        let new = ring.begin_process();
        ring.push_line_from(new, "new: booting");
        ring.push_line_from(old, "old: config error");

        assert_eq!(
            ring.snapshot(None, None).entries,
            vec![
                line("first: done"),
                TailEntry::ProcessStart,
                line("old: config error"),
                TailEntry::ProcessStart,
                line("new: booting"),
            ]
        );
    }

    #[test]
    fn a_late_line_whose_section_was_evicted_counts_as_dropped() {
        let mut ring = ring(2, 10_000, 128);
        ring.mark_captured();
        let old = ring.begin_process();
        ring.push_line_from(old, "old");
        ring.retire_pump(old);
        let new = ring.begin_process();
        for text in ["new 1", "new 2", "new 3"] {
            ring.push_line_from(new, text);
        }
        // The old section and the boundary after it are gone; the late line
        // belongs in front of everything retained.
        ring.push_line_from(old, "old, late");

        let snapshot = ring.snapshot(None, None);
        assert_eq!(snapshot.entries, vec![line("new 2"), line("new 3")]);
        assert_eq!(snapshot.dropped_lines, 3);
    }

    #[test]
    fn a_late_reader_reads_incomplete_until_its_pipe_reaches_eof() {
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        let old = ring.begin_process();
        ring.retire_pump(old);
        ring.mark_pump_late(old, "still open");
        ring.begin_process();
        assert_eq!(
            ring.snapshot(None, None).capture,
            CaptureState::Incomplete {
                reason: "still open".to_string()
            }
        );

        ring.finish_pump(old);
        assert_eq!(ring.snapshot(None, None).capture, CaptureState::Captured);
    }

    #[test]
    fn silent_restarts_do_not_grow_the_ring() {
        let mut ring = ring(10, 10_000, 128);
        ring.mark_captured();
        ring.push_line("once");
        for _ in 0..100 {
            let generation = ring.begin_process();
            ring.finish_pump(generation);
        }
        assert_eq!(ring.entries.len(), 2);
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
        assert_eq!(
            sink.writes,
            vec![vec![b'x'; MAX_PENDING_LINE_BYTES], vec![b'x'; 4096],],
            "forced flushes and EOF fragments must not invent delimiters"
        );
    }

    #[tokio::test]
    async fn boundaries_truncation_and_framing_do_not_depend_on_chunk_splits() {
        // The same stream split at hostile boundaries -- mid-line, between a CR
        // and its LF, and a line sitting exactly on the per-line cap -- must
        // produce the same ring entries and forwarded bytes as any other split.
        let ring = shared(100, 100_000, 8);
        let source = ChunkedReader {
            chunks: vec![
                b"fir".to_vec(),
                b"st\nsec".to_vec(),
                b"ond\ncarry\r".to_vec(),
                b"\nover\n".to_vec(),
                b"12345678\n".to_vec(),
                b"1234567".to_vec(),
                b"89\n".to_vec(),
                b"tail".to_vec(),
            ]
            .into_iter()
            .collect(),
        };
        let mut sink = RecordingSink::default();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;

        let snapshot = lock_ring(&ring).snapshot(None, None);
        assert_eq!(snapshot.capture, CaptureState::Captured);
        assert_eq!(
            untimed(snapshot.entries),
            vec![
                TailEntry::Line {
                    text: "first".to_string(),
                    truncated: false,
                    at_ms: None,
                },
                TailEntry::Line {
                    text: "second".to_string(),
                    truncated: false,
                    at_ms: None,
                },
                // The pump delimits on '\n' alone; a CR belongs to the line body.
                TailEntry::Line {
                    text: "carry\r".to_string(),
                    truncated: false,
                    at_ms: None,
                },
                TailEntry::Line {
                    text: "over".to_string(),
                    truncated: false,
                    at_ms: None,
                },
                // Exactly at the per-line cap: kept whole.
                TailEntry::Line {
                    text: "12345678".to_string(),
                    truncated: false,
                    at_ms: None,
                },
                // One byte past the cap: cut, and marked as cut.
                TailEntry::Line {
                    text: "12345678".to_string(),
                    truncated: true,
                    at_ms: None,
                },
                TailEntry::Line {
                    text: "tail".to_string(),
                    truncated: false,
                    at_ms: None,
                },
            ]
        );
        assert_eq!(
            sink.writes,
            vec![
                b"first\n".to_vec(),
                b"second\n".to_vec(),
                b"carry\r\n".to_vec(),
                b"over\n".to_vec(),
                b"12345678\n".to_vec(),
                b"123456789\n".to_vec(),
                b"tail".to_vec(),
            ]
        );
    }

    #[tokio::test]
    async fn a_line_with_no_newline_is_not_rescanned_from_byte_zero_on_every_chunk() {
        // One 1 MiB line arrives in 8192-byte reads. Searching the whole
        // pending buffer for a newline on every chunk scans each byte once per
        // chunk that arrived after it -- about 64 MiB examined per MiB of
        // output. Searching only the bytes that arrived since the last search
        // scans each byte once.
        let input = vec![b'x'; MAX_PENDING_LINE_BYTES + 4096];
        let ring = shared(10, 10_000_000, 4 * 1024 * 1024);
        let source = std::io::Cursor::new(input.clone());
        let mut sink = RecordingSink::default();

        take_scanned_bytes();
        pump_stderr_into(source, Arc::clone(&ring), &mut sink).await;
        let scanned = take_scanned_bytes();

        assert!(
            scanned <= 2 * input.len(),
            "newline searches examined {scanned} bytes for {} bytes of input; \
             each chunk must search only newly arrived bytes",
            input.len()
        );
    }

    fn now_ms() -> u64 {
        unix_ms(SystemTime::now())
    }

    #[tokio::test]
    async fn a_captured_line_carries_the_time_the_reader_framed_it() {
        // The ring is read after the fact (`ck module stderr`), and without a
        // time a reader cannot tell a crash's last words from a line written
        // hours before it.
        let ring = shared(10, 10_000, 128);
        let before = now_ms();
        let source = std::io::Cursor::new(b"first\nsecond".to_vec());
        pump_stderr_into(source, Arc::clone(&ring), &mut RecordingSink::default()).await;
        let after = now_ms();

        let entries = lock_ring(&ring).snapshot(None, None).entries;
        assert_eq!(entries.len(), 2);
        for entry in entries {
            let TailEntry::Line { text, at_ms, .. } = entry else {
                panic!("expected only lines, got {entry:?}");
            };
            let at_ms = at_ms.unwrap_or_else(|| panic!("line {text:?} has no capture time"));
            assert!(
                (before..=after).contains(&at_ms),
                "line {text:?} stamped {at_ms}, outside the pump's run {before}..={after}"
            );
        }
    }

    /// Reads a capture file written through the production sink.
    async fn capture_through_file_sink(chunks: Vec<Vec<u8>>) -> (String, u64, u64) {
        let temp = subc_test_support::TestTempDir::new("stderr-capture-stamp");
        let path = temp.path().join("stamped.stderr.log");
        let sink = ChildOutputSink::open(&path, cortexkit_log::Retention::default()).unwrap();
        let ring = shared(10, 10_000, 128);
        let generation = lock_ring(&ring).begin_process();
        let before = now_ms();
        pump_stderr_to(
            ChunkedReader {
                chunks: chunks.into_iter().collect(),
            },
            ring,
            generation,
            sink,
        )
        .await;
        let after = now_ms();
        (std::fs::read_to_string(&path).unwrap(), before, after)
    }

    /// Checks the prefix's shape by position, independently of
    /// [`split_capture_stamp`], so a parser that accepted a malformed stamp
    /// could not vouch for the writer that produced it.
    fn assert_stamp_shape(line: &str) {
        let bytes = line.as_bytes();
        assert!(bytes.len() > 25, "line too short for a stamp: {line:?}");
        for (index, byte) in bytes[..25].iter().enumerate() {
            let expected_separator = match index {
                4 | 7 => Some(b'-'),
                10 => Some(b'T'),
                13 | 16 => Some(b':'),
                19 => Some(b'.'),
                23 => Some(b'Z'),
                24 => Some(b' '),
                _ => None,
            };
            match expected_separator {
                Some(separator) => assert_eq!(*byte, separator, "byte {index} of {line:?}"),
                None => assert!(byte.is_ascii_digit(), "byte {index} of {line:?}"),
            }
        }
    }

    #[tokio::test]
    async fn the_capture_file_stamps_each_line_and_keeps_the_module_bytes_verbatim() {
        // The second line begins with a stamp of its own (a module that logs
        // in the fleet format to stderr). Its bytes must survive untouched:
        // the capture stamp goes in front, nothing is parsed or replaced.
        let module_lines = [
            "plain line",
            "2020-01-01T00:00:00.000Z INFO  mymod: own stamp",
        ];
        let input = format!("{}\n{}\n", module_lines[0], module_lines[1]);
        let (contents, before, after) = capture_through_file_sink(vec![input.into_bytes()]).await;

        assert!(contents.ends_with('\n'));
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "capture file: {contents:?}");
        for (line, module_line) in lines.iter().zip(module_lines) {
            assert_stamp_shape(line);
            assert_eq!(&line[25..], module_line, "module bytes changed");
            let (at_ms, rest) = split_capture_stamp(line).unwrap();
            assert_eq!(rest, module_line);
            // Millisecond stamps round the pump's own bounds down.
            assert!(
                (before..=after).contains(&at_ms),
                "stamped {at_ms}, outside the pump's run {before}..={after}"
            );
        }
    }

    #[tokio::test]
    async fn a_line_split_across_several_writes_is_stamped_once() {
        // A module that writes one line in pieces, and a pipe that delivers it
        // in pieces, must still produce one stamp at the front of the line and
        // none in its middle.
        let (contents, _, _) = capture_through_file_sink(vec![
            b"par".to_vec(),
            b"tial li".to_vec(),
            b"ne\nwhole\n".to_vec(),
        ])
        .await;

        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "capture file: {contents:?}");
        for (line, module_line) in lines.iter().zip(["partial line", "whole"]) {
            assert_stamp_shape(line);
            assert_eq!(&line[25..], module_line, "capture file: {contents:?}");
        }
    }

    #[tokio::test]
    async fn a_line_flushed_at_the_ceiling_is_stamped_only_at_the_start_of_each_file_line() {
        // Past the reassembly ceiling the pending bytes go out without their
        // newline. The file sink ends every write with one, so the rest of
        // the line starts a new file line, and that is where its stamp goes;
        // a stamp anywhere else would sit in the middle of module bytes.
        let mut long = vec![b'x'; MAX_PENDING_LINE_BYTES + 100];
        long.push(b'\n');
        // The reader's buffer holds 8 KiB, so feed it reads no larger than that.
        let chunks = long.chunks(8192).map(<[u8]>::to_vec).collect();
        let (contents, _, _) = capture_through_file_sink(chunks).await;

        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "expected the flushed piece and the rest");
        for line in &lines {
            assert_stamp_shape(line);
            assert!(
                line[25..].bytes().all(|byte| byte == b'x'),
                "a stamp landed inside module bytes"
            );
        }
        let module_bytes: usize = lines.iter().map(|line| line.len() - 25).sum();
        assert_eq!(module_bytes, MAX_PENDING_LINE_BYTES + 100);
    }

    #[test]
    fn the_capture_stamp_is_the_daemon_log_timestamp_form() {
        for (at_ms, text) in [
            (0, "1970-01-01T00:00:00.000Z"),
            (951_868_799_999, "2000-02-29T23:59:59.999Z"),
            (1_789_801_440_685, "2026-09-19T07:04:00.685Z"),
            (4_107_542_400_001, "2100-03-01T00:00:00.001Z"),
        ] {
            let stamp = format_capture_stamp(at_ms);
            assert_eq!(stamp, text);
            assert_eq!(stamp.len(), CAPTURE_STAMP_LEN);
            // The daemon's own log parser reads the same instant from it, so
            // one parser serves both files.
            let daemon_line = format!("{stamp} INFO  subc: probe");
            let parsed = cortexkit_log::parse_line(&daemon_line).unwrap();
            assert_eq!(
                parsed.timestamp,
                UNIX_EPOCH + std::time::Duration::from_millis(at_ms)
            );
            assert_eq!(
                split_capture_stamp(&format!("{stamp} body")),
                Some((at_ms, "body"))
            );
        }
    }

    #[test]
    fn a_line_without_a_well_formed_stamp_has_no_capture_time() {
        for line in [
            "",
            "plain module output",
            "2026-09-19T07:04:00.685Z",
            "2026-09-19T07:04:00.685Zbody",
            "2026-09-19T07:04:00.685z body",
            "2026-09-19 07:04:00.685Z body",
            "2026-09-19T07:04:00Z body",
            "2026-02-30T07:04:00.685Z body",
            "2026-13-19T07:04:00.685Z body",
            "2026-09-19T24:04:00.685Z body",
            "2026-09-19T07:04:00.6a5Z body",
            "1969-12-31T23:59:59.999Z body",
        ] {
            assert_eq!(split_capture_stamp(line), None, "{line:?}");
        }
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

    #[test]
    fn an_incoherent_config_clamps_the_line_cap_and_keeps_its_restart_boundary() {
        let config = StderrTailConfig::new(2, 10, 100);
        assert_eq!(config.max_line_bytes, config.max_bytes);
        let mut ring = StderrRing::new(config);
        ring.mark_captured();
        ring.push_line("old");
        ring.push_process_start();
        ring.push_line("new process line longer than the ring byte cap");

        assert_eq!(
            ring.snapshot(None, None).entries,
            vec![
                TailEntry::ProcessStart,
                TailEntry::Line {
                    text: "new proces".to_string(),
                    truncated: true,
                    at_ms: None,
                },
            ]
        );
    }
}
