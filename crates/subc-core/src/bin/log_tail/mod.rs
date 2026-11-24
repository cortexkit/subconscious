//! Reading module logs for `ck module logs`: each source is streamed line by
//! line and only the newest requested lines are kept, so a short tail of a
//! long history costs one pass over the files, not a copy and a sort of
//! every line in them.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap},
    fs,
    io::{self, BufRead, BufReader, Seek, SeekFrom},
    path::PathBuf,
    time::SystemTime,
};

use super::{
    app_log_options, body_has_module_id, capture_line_facts, capture_stamp_time, collect_log_text,
    component_matches, discover_log_sources_in, lane_matches, line_facts, log_level_rank,
    rotation_generation, since_stamp, CkError, LogDirs, LogEntry, LogFilterCounts, LogSource,
    ModuleLogsOptions,
};

mod follow;
use follow::LogCursor;
pub(super) use follow::{follow_module_logs, LogFollower};

#[cfg(test)]
mod tests;

/// The order `ck module logs` prints lines in. Undated lines (including
/// captures from files with no daemon stamps) sort AFTER every dated line:
/// `Option`'s natural order puts `None` first, which would render a module's
/// crash stderr above older daemon lines. Within the undated tail, source
/// order holds. Capture lines from stamped files inherit an anchor time
/// before reaching this comparison. `source_order` is unique per entry, so
/// no two entries compare equal.
pub(super) fn compare_log_entries(left: &LogEntry, right: &LogEntry) -> Ordering {
    order_key(left.timestamp, &left.lane, left.source_order).cmp(&order_key(
        right.timestamp,
        &right.lane,
        right.source_order,
    ))
}

fn order_key(
    timestamp: Option<SystemTime>,
    lane: &str,
    source_order: usize,
) -> (bool, Option<SystemTime>, &str, usize) {
    (timestamp.is_none(), timestamp, lane, source_order)
}

/// Where accepted log lines go: every line, or only the newest few.
pub(super) trait LogSink {
    /// Whether an entry with this ordering key would be kept. Lets the reader
    /// skip building an entry the sink would drop at once.
    fn wants(&self, _timestamp: Option<SystemTime>, _lane: &str, _source_order: usize) -> bool {
        true
    }

    fn push(&mut self, entry: LogEntry);
}

impl LogSink for Vec<LogEntry> {
    fn push(&mut self, entry: LogEntry) {
        Vec::push(self, entry);
    }
}

/// The newest `limit` entries seen so far under [`compare_log_entries`].
pub(super) struct NewestEntries {
    limit: usize,
    heap: BinaryHeap<OldestFirst>,
}

/// Reverses the print order so the heap's top is the oldest kept entry,
/// the one a newer entry replaces.
struct OldestFirst(LogEntry);

impl Ord for OldestFirst {
    fn cmp(&self, other: &Self) -> Ordering {
        compare_log_entries(&other.0, &self.0)
    }
}

impl PartialOrd for OldestFirst {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for OldestFirst {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for OldestFirst {}

impl NewestEntries {
    pub(super) fn new(limit: usize) -> NewestEntries {
        NewestEntries {
            limit,
            heap: BinaryHeap::new(),
        }
    }

    /// The kept entries in print order.
    pub(super) fn into_sorted(self) -> Vec<LogEntry> {
        let mut entries = self
            .heap
            .into_vec()
            .into_iter()
            .map(|kept| kept.0)
            .collect::<Vec<_>>();
        entries.sort_by(compare_log_entries);
        entries
    }
}

impl LogSink for NewestEntries {
    fn wants(&self, timestamp: Option<SystemTime>, lane: &str, source_order: usize) -> bool {
        if self.heap.len() < self.limit {
            return true;
        }
        self.heap.peek().is_some_and(|oldest| {
            order_key(timestamp, lane, source_order)
                > order_key(oldest.0.timestamp, &oldest.0.lane, oldest.0.source_order)
        })
    }

    fn push(&mut self, entry: LogEntry) {
        if self.heap.len() < self.limit {
            self.heap.push(OldestFirst(entry));
        } else if let Some(mut oldest) = self.heap.peek_mut() {
            if compare_log_entries(&entry, &oldest.0) == Ordering::Greater {
                *oldest = OldestFirst(entry);
            }
        }
    }
}

pub(crate) struct BoundedLogTail {
    pub(crate) entries: Vec<LogEntry>,
    pub(crate) truncated: bool,
}

/// Reads the newest 20 entries from the ends of a module's active log files.
/// `Ok(None)` means no log source exists; `Ok(Some(...))` with no entries
/// means a successful empty read. The second value is the run-folder warning,
/// returned on both success and error rather than printed.
pub(crate) fn read_bounded_tail(
    module_id: &str,
) -> (Result<Option<BoundedLogTail>, CkError>, Option<String>) {
    let (dirs, warning) = LogDirs::for_module(module_id);
    (read_bounded_tail_in(module_id, &dirs), warning)
}

fn read_bounded_tail_in(
    module_id: &str,
    dirs: &LogDirs,
) -> Result<Option<BoundedLogTail>, CkError> {
    const MAX_LINES: usize = 20;
    let sources = discover_log_sources_in(dirs, module_id, false)?;
    if sources.is_empty() {
        return Ok(None);
    }

    let options = app_log_options(MAX_LINES);
    let mut counts = LogFilterCounts::default();
    let mut newest = NewestEntries::new(MAX_LINES);
    let mut order = 0;
    let mut truncated = false;
    for source in &sources {
        let (text, cut) = follow::read_bounded_log_slice(&source.path, None, MAX_LINES)?;
        truncated |= cut;
        collect_log_text(
            module_id,
            source,
            &text,
            &options,
            &mut counts,
            &mut newest,
            &mut order,
        );
    }
    Ok(Some(BoundedLogTail {
        entries: newest.into_sorted(),
        truncated,
    }))
}

/// The newest `options.lines` lines across `sources`, in print order, with
/// the filter counts over every line read. Gives exactly the lines that
/// collecting every line, sorting them all and keeping the last
/// `options.lines` gives.
#[cfg(test)]
pub(super) fn newest_log_entries(
    module_id: &str,
    sources: &[LogSource],
    options: &ModuleLogsOptions,
) -> Result<(Vec<LogEntry>, LogFilterCounts), CkError> {
    let (entries, counts, _) = newest_log_entries_and_cursors(module_id, sources, options)?;
    Ok((entries, counts))
}

/// The newest lines, as [`newest_log_entries`] gives them, and a follower
/// that continues each active file from exactly where that read stopped,
/// for `ck module logs -f`.
pub(super) fn tail_and_follower(
    module_id: &str,
    dirs: LogDirs,
    sources: &[LogSource],
    options: &ModuleLogsOptions,
) -> Result<(Vec<LogEntry>, LogFilterCounts, LogFollower), CkError> {
    let (entries, counts, cursors) = newest_log_entries_and_cursors(module_id, sources, options)?;
    #[cfg(test)]
    if let Some(hook) = tests::AFTER_TAIL.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
    let follower = LogFollower::from_cursors(module_id, dirs, cursors);
    Ok((entries, counts, follower))
}

type LogCursors = HashMap<PathBuf, LogCursor>;

fn newest_log_entries_and_cursors(
    module_id: &str,
    sources: &[LogSource],
    options: &ModuleLogsOptions,
) -> Result<(Vec<LogEntry>, LogFilterCounts, LogCursors), CkError> {
    let mut counts = LogFilterCounts::default();
    let mut newest = NewestEntries::new(options.lines);
    let mut source_order = 0;
    let mut cursors = HashMap::new();
    for source in sources {
        let cursor = stream_log_source(
            module_id,
            source,
            options,
            &mut counts,
            &mut newest,
            &mut source_order,
        )
        .map_err(|error| CkError::Message(format!("{}: {error}", source.path.display())))?;
        // Only active files are followed; rotated ones no longer grow.
        if rotation_generation(&source.path) == 0 {
            cursors.insert(source.path.clone(), cursor);
        }
    }
    Ok((newest.into_sorted(), counts, cursors))
}

/// Reads `source` line by line into `output`, the way `collect_log_text`
/// reads a whole file's text. Returns a cursor just after the bytes read.
fn stream_log_source(
    module_id: &str,
    source: &LogSource,
    options: &ModuleLogsOptions,
    counts: &mut LogFilterCounts,
    output: &mut impl LogSink,
    source_order: &mut usize,
) -> io::Result<LogCursor> {
    let mut reader = BufReader::new(fs::File::open(&source.path)?);
    let mut line = String::new();
    // An unstamped line at the head of a capture file inherits the file's
    // first stamp, so a capture file is scanned for it first.
    let first_capture_stamp = if source.capture {
        let mut first = None;
        while next_line(&mut reader, &mut line)? != 0 {
            if let Some(stamp) = capture_stamp_time(&line) {
                first = Some(stamp);
                break;
            }
        }
        reader.seek(SeekFrom::Start(0))?;
        first
    } else {
        None
    };
    let mut lines = SourceLines::new(module_id, source, options, first_capture_stamp);
    let mut consumed = 0;
    loop {
        let (read, terminated) = next_line_terminated(&mut reader, &mut line)?;
        if read == 0 {
            return LogCursor::after(reader.get_mut(), consumed);
        }
        // When following, a last line still being written to the active file
        // is left to the follower, which prints it whole once its newline
        // arrives instead of splitting it at this read's end. A rotated file
        // is not followed and no longer grows: its last line is printed now.
        if !terminated && options.follow && rotation_generation(&source.path) == 0 {
            return LogCursor::after(reader.get_mut(), consumed);
        }
        consumed += read as u64;
        lines.push(&line, counts, output, source_order);
    }
}

/// Reads the next line into `line` without its terminator, which is `\n` or
/// `\r\n`, as `str::lines` splits. Returns the bytes read, 0 at the end.
fn next_line(reader: &mut impl BufRead, line: &mut String) -> io::Result<usize> {
    next_line_terminated(reader, line).map(|(read, _)| read)
}

/// [`next_line`], also saying whether the line ended with its newline.
fn next_line_terminated(reader: &mut impl BufRead, line: &mut String) -> io::Result<(usize, bool)> {
    line.clear();
    let read = reader.read_line(line)?;
    let terminated = line.ends_with('\n');
    if terminated {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok((read, terminated))
}

/// Filters and dates the lines of one source in file order, counting what
/// each filter removed.
pub(super) struct SourceLines<'a> {
    module_id: &'a str,
    source: &'a LogSource,
    options: &'a ModuleLogsOptions,
    since: Option<SystemTime>,
    first_capture_stamp: Option<SystemTime>,
    preceding_capture_stamp: Option<SystemTime>,
}

impl<'a> SourceLines<'a> {
    /// `first_capture_stamp` is the first capture stamp anywhere in a capture
    /// source, the time an unstamped line at its head inherits.
    pub(super) fn new(
        module_id: &'a str,
        source: &'a LogSource,
        options: &'a ModuleLogsOptions,
        first_capture_stamp: Option<SystemTime>,
    ) -> SourceLines<'a> {
        SourceLines {
            module_id,
            source,
            options,
            since: options
                .since
                .and_then(|duration| SystemTime::now().checked_sub(duration)),
            first_capture_stamp,
            preceding_capture_stamp: None,
        }
    }

    pub(super) fn push(
        &mut self,
        line: &str,
        counts: &mut LogFilterCounts,
        output: &mut impl LogSink,
        source_order: &mut usize,
    ) {
        let source = self.source;
        let options = self.options;
        counts.total += 1;
        let (mut timestamp, parsed, capture_stamp) = if source.capture {
            capture_line_facts(line)
        } else {
            let parsed = line_facts(line);
            (parsed.as_ref().map(|parsed| parsed.timestamp), parsed, None)
        };
        if let Some(stamp) = capture_stamp {
            self.preceding_capture_stamp = Some(stamp);
        }
        let time_inherited = if source.capture && timestamp.is_none() {
            // Capture files are append-only, so file position orders an
            // unstamped line against the capture's stamped lines. The nearest
            // preceding stamp (or first following stamp for a legacy head) is
            // a safe ordering anchor, not a measured time; JSON marks it as
            // inherited so consumers do not mistake the anchor for the line's
            // own timestamp.
            if let Some(stamp) = self.preceding_capture_stamp.or(self.first_capture_stamp) {
                timestamp = Some(stamp);
                true
            } else {
                false
            }
        } else {
            false
        };
        if source.daemon
            && parsed
                .as_ref()
                .is_some_and(|parsed| !body_has_module_id(parsed.body, self.module_id))
        {
            counts.daemon_hidden += 1;
            return;
        }
        if options
            .lane
            .as_ref()
            .is_some_and(|lane| !lane_matches(lane, &source.lane))
        {
            counts.wrong_lane += 1;
            return;
        }
        if let Some(parsed) = &parsed {
            if options
                .level
                .is_some_and(|minimum| log_level_rank(parsed.level) < log_level_rank(minimum))
            {
                counts.below_level += 1;
                return;
            }
            // `--tag` predates the logger hierarchy and is kept as the operator
            // surface: under r2 it names a COMPONENT, i.e. the logger with the
            // module root stripped (`synapse.perf` -> `perf`), matched as a
            // dotted prefix so `--tag gc` also shows `gc.walk`.
            if options
                .tag
                .as_deref()
                .is_some_and(|tag| !component_matches(parsed.component, tag))
            {
                counts.wrong_tag += 1;
                return;
            }
        }
        // `--since` compares each line's own time. A stamped capture line
        // already carries the daemon's capture time above, whatever the
        // module wrote after it. A stamped line otherwise keeps its own time
        // even when the strict fleet grammar rejects it for something
        // unrelated to the time itself: an unpadded level, a trailing context
        // bracket, or another shape the module's own writer emits. Without a
        // fallback the module's own dated segments are immune to `--since`,
        // and their old lines fill the `-n` tail because undated lines sort
        // after dated ones. Lines with no stamp at all stay undated: capture
        // lines inherit a neighbour's stamp above, and the rest are shown and
        // counted below, exactly as before.
        let since_time = timestamp.or_else(|| since_stamp(line));
        if self
            .since
            .is_some_and(|since| since_time.is_some_and(|time| time < since))
        {
            counts.before_since += 1;
            return;
        }
        if parsed.is_none() {
            counts.unparsed += 1;
        }
        let order = *source_order;
        *source_order += 1;
        if !output.wants(timestamp, &source.lane, order) {
            return;
        }
        #[cfg(test)]
        tests::ENTRIES_BUILT.with(|built| built.set(built.get() + 1));
        output.push(LogEntry {
            lane: source.lane.clone(),
            line: line.to_string(),
            timestamp,
            time_inherited,
            level: parsed.as_ref().map(|parsed| parsed.level),
            tag: parsed.and_then(|parsed| parsed.component.map(str::to_string)),
            source_order: order,
        });
    }
}
