//! Following a module's active log files, for `ck module logs -f` and the
//! app's log view.
//!
//! Each file has a cursor: the bytes of it already read, its creation time
//! and first bytes (which tell a rotation from an append), and any trailing
//! partial line held back until its newline arrives. `ck module logs -f`
//! takes its cursors from the very reads that produced the initial tail, so a
//! line appended between that tail and the first poll is not lost, and each
//! poll advances by the bytes it actually read, so a line appended while a
//! poll is reading is printed once.

use std::{
    collections::HashMap,
    fs,
    io::{self, Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    time::SystemTime,
};

use subc_core::cli_output::stdout_result;
use tokio::time;

use super::super::{
    app_log_options, collect_log_text, discover_log_sources_in, print_log_entries,
    rotation_generation, sort_log_entries, CkError, JsonOutput, LogDirs, LogEntry, LogFilterCounts,
    LogSource, ModuleLogsOptions,
};

/// `ck module logs -f` after the initial tail: prints the complete lines each
/// poll reads, four times a second, until interrupted.
pub(crate) async fn follow_module_logs(
    module_id: &str,
    options: &ModuleLogsOptions,
    mut follower: LogFollower,
    json_output: JsonOutput,
) -> Result<(), CkError> {
    loop {
        time::sleep(std::time::Duration::from_millis(250)).await;
        for (source, text) in follower.poll()? {
            print_follow_text(module_id, options, &source, &text, json_output)?;
        }
        use std::io::Write as _;
        stdout_result(io::stdout().flush()).map_err(|error| CkError::Message(error.to_string()))?;
    }
}

fn print_follow_text(
    module_id: &str,
    options: &ModuleLogsOptions,
    source: &LogSource,
    text: &str,
    json_output: JsonOutput,
) -> Result<(), CkError> {
    let mut entries = Vec::new();
    let mut counts = LogFilterCounts::default();
    let mut order = 0;
    collect_log_text(
        module_id,
        source,
        text,
        options,
        &mut counts,
        &mut entries,
        &mut order,
    );
    print_log_entries(&entries, json_output)
}

/// How many leading bytes of a file are compared to notice a rotation.
const PREFIX_BYTES: u64 = 64;

/// Follows a module's active log files: each poll reads what was appended
/// since the last, plus each file's first few bytes to notice a rotation,
/// and when a file was rotated (it shrank, was recreated, or its first bytes
/// changed) drains the rotated file's unseen tail before starting the new one
/// from its beginning.
pub(crate) struct LogFollower {
    pub(crate) module_id: String,
    dirs: LogDirs,
    cursors: HashMap<PathBuf, LogCursor>,
    /// How many times a whole file was read, for the test that keeps
    /// following incremental.
    #[cfg(test)]
    pub(crate) whole_reads: usize,
    /// Runs after a poll has looked at a file's length and before it reads
    /// the file, so a test can append in between.
    #[cfg(test)]
    pub(crate) before_read: Option<BeforeRead>,
}

#[cfg(test)]
type BeforeRead = Box<dyn FnMut(&Path) + Send>;

/// Where following one file has got to.
#[derive(Clone, Debug)]
pub(crate) struct LogCursor {
    /// Bytes of the file already read.
    offset: u64,
    created: Option<SystemTime>,
    /// The file's first bytes when the cursor was taken.
    prefix: Vec<u8>,
    /// Bytes read after the file's last newline, printed once the line ends.
    pending: Vec<u8>,
}

impl LogCursor {
    /// A cursor after the first `offset` bytes of `file`, as read through
    /// this same handle.
    pub(crate) fn after(file: &mut fs::File, offset: u64) -> io::Result<LogCursor> {
        let created = file.metadata()?.created().ok();
        let prefix = read_prefix(file, offset.min(PREFIX_BYTES))?;
        Ok(LogCursor {
            offset,
            created,
            prefix,
            pending: Vec::new(),
        })
    }
}

impl LogFollower {
    /// Starts following at the current end of each active (unrotated) file
    /// among `initial_sources`, for the app, which then reads its tail up to
    /// those ends.
    pub(crate) fn start(
        module_id: &str,
        dirs: LogDirs,
        initial_sources: &[LogSource],
    ) -> Result<LogFollower, CkError> {
        let mut cursors = HashMap::new();
        for source in initial_sources
            .iter()
            .filter(|source| rotation_generation(&source.path) == 0)
        {
            let Ok(mut file) = fs::File::open(&source.path) else {
                continue;
            };
            let cursor = file
                .metadata()
                .and_then(|metadata| LogCursor::after(&mut file, metadata.len()))
                .map_err(|error| read_error(&source.path, error))?;
            cursors.insert(source.path.clone(), cursor);
        }
        Ok(LogFollower::from_cursors(module_id, dirs, cursors))
    }

    /// Follows from exactly where earlier reads of the active files stopped.
    /// A file without a cursor is read from its start when it appears.
    pub(crate) fn from_cursors(
        module_id: &str,
        dirs: LogDirs,
        cursors: HashMap<PathBuf, LogCursor>,
    ) -> LogFollower {
        LogFollower {
            module_id: module_id.to_string(),
            dirs,
            cursors,
            #[cfg(test)]
            whole_reads: 0,
            #[cfg(test)]
            before_read: None,
        }
    }

    /// The complete lines appended to each active file since the last poll,
    /// with the file they came from. A trailing partial line is held until
    /// its newline arrives.
    pub(crate) fn poll(&mut self) -> Result<Vec<(LogSource, String)>, CkError> {
        let mut out = Vec::new();
        for source in discover_log_sources_in(&self.dirs, &self.module_id, false)? {
            // One opened handle answers every question about this file for
            // this poll, so a rename between them cannot mix two files.
            let mut file = match fs::File::open(&source.path) {
                Ok(file) => file,
                // Rotated away since it was listed: the next poll sees the
                // replacement.
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(read_error(&source.path, error)),
            };
            let metadata = file
                .metadata()
                .map_err(|error| read_error(&source.path, error))?;
            let length = metadata.len();
            let created = metadata.created().ok();
            let cursor = self
                .cursors
                .entry(source.path.clone())
                .or_insert(LogCursor {
                    offset: 0,
                    created,
                    prefix: Vec::new(),
                    pending: Vec::new(),
                });
            let current_prefix = read_prefix(&mut file, cursor.prefix.len() as u64)
                .map_err(|error| read_error(&source.path, error))?;
            let rotated = length < cursor.offset
                || created != cursor.created
                || current_prefix != cursor.prefix;
            if rotated {
                // A rotation may move bytes appended after the previous poll into
                // `.1`. Drain the rotated file's unseen tail, partial last line
                // included since nothing more is written there, before
                // switching the cursor to the replacement active file.
                let mut rotated_name = source.path.as_os_str().to_os_string();
                rotated_name.push(".1");
                let rotated_path = PathBuf::from(rotated_name);
                let mut tail = std::mem::take(&mut cursor.pending);
                if let Ok(mut rotated_file) = fs::File::open(&rotated_path) {
                    read_from(&mut rotated_file, cursor.offset, &mut tail)
                        .map_err(|error| read_error(&rotated_path, error))?;
                }
                if !tail.is_empty() {
                    out.push((source.clone(), String::from_utf8_lossy(&tail).into_owned()));
                }
                cursor.offset = 0;
                cursor.created = created;
                cursor.prefix = read_prefix(&mut file, length.min(PREFIX_BYTES))
                    .map_err(|error| read_error(&source.path, error))?;
            } else if cursor.prefix.is_empty() && length != 0 {
                cursor.prefix = read_prefix(&mut file, length.min(PREFIX_BYTES))
                    .map_err(|error| read_error(&source.path, error))?;
            }
            if length == cursor.offset {
                continue;
            }
            #[cfg(test)]
            if cursor.offset == 0 {
                self.whole_reads += 1;
            }
            #[cfg(test)]
            if let Some(hook) = &mut self.before_read {
                hook(&source.path);
            }
            // Read to the end as it is now, which may be past `length`, and
            // advance by what was read.
            let mut bytes = std::mem::take(&mut cursor.pending);
            let read = read_from(&mut file, cursor.offset, &mut bytes)
                .map_err(|error| read_error(&source.path, error))?;
            cursor.offset += read;
            match bytes.iter().rposition(|byte| *byte == b'\n') {
                Some(last_newline) => {
                    cursor.pending = bytes.split_off(last_newline + 1);
                    out.push((source, String::from_utf8_lossy(&bytes).into_owned()));
                }
                None => cursor.pending = bytes,
            }
        }
        Ok(out)
    }

    /// The newest lines of the active files up to where following starts,
    /// at most `max` of them, merged and ordered as `ck module logs` orders
    /// them. Reads only the end of each file (about `max` lines' worth),
    /// never the whole set.
    pub(crate) fn tail(&self, max: usize) -> Result<Vec<LogEntry>, CkError> {
        let options = app_log_options(max);
        let mut counts = LogFilterCounts::default();
        let mut entries = Vec::new();
        let mut order = 0;
        for source in discover_log_sources_in(&self.dirs, &self.module_id, false)? {
            let Some(cursor) = self.cursors.get(&source.path) else {
                continue;
            };
            let (text, _) = read_bounded_log_slice(&source.path, Some(cursor.offset), max)?;
            collect_log_text(
                &self.module_id,
                &source,
                &text,
                &options,
                &mut counts,
                &mut entries,
                &mut order,
            );
        }
        sort_log_entries(&mut entries);
        let from = entries.len().saturating_sub(max);
        Ok(entries.split_off(from))
    }
}

fn read_error(path: &Path, error: io::Error) -> CkError {
    CkError::Message(format!("{}: {error}", path.display()))
}

/// Reads a tail window up to `end`, or up to the opened file's current length.
/// The flag tells the caller whether the window cut off any of the file.
pub(super) fn read_bounded_log_slice(
    path: &Path,
    end: Option<u64>,
    max: usize,
) -> Result<(String, bool), CkError> {
    const BYTES_PER_LINE: u64 = 400;
    let mut file = fs::File::open(path).map_err(|error| read_error(path, error))?;
    let end = match end {
        Some(end) => end,
        None => file
            .metadata()
            .map_err(|error| read_error(path, error))?
            .len(),
    };
    let start = end.saturating_sub(BYTES_PER_LINE.saturating_mul(max as u64));
    let mut text =
        read_log_slice(&mut file, start, end).map_err(|error| read_error(path, error))?;
    // A read that starts mid-file drops its first, partial line.
    if start > 0 {
        let next_line = text.find('\n').map_or(text.len(), |index| index + 1);
        text.drain(..next_line);
    }
    Ok((text, start > 0))
}

/// The bytes of a file from `start` up to `end`, as text.
fn read_log_slice(file: &mut fs::File, start: u64, end: u64) -> io::Result<String> {
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(end.saturating_sub(start))
        .read_to_end(&mut bytes)?;
    #[cfg(test)]
    super::tests::SLICE_BYTES_READ.with(|total| total.set(total.get() + bytes.len()));
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Up to `length` bytes from the start of `file`. A file shorter than that
/// gives what it has: a replacement written since the last poll can be
/// shorter than the prefix kept for the file it replaced, and its differing
/// prefix is how the rotation is noticed.
fn read_prefix(file: &mut fs::File, length: u64) -> io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(0))?;
    let mut prefix = Vec::with_capacity(length as usize);
    file.take(length).read_to_end(&mut prefix)?;
    Ok(prefix)
}

/// Appends everything in `file` from `offset` to its current end to
/// `bytes`, and returns how many bytes that was.
fn read_from(file: &mut fs::File, offset: u64, bytes: &mut Vec<u8>) -> io::Result<u64> {
    file.seek(SeekFrom::Start(offset))?;
    Ok(file.read_to_end(bytes)? as u64)
}
