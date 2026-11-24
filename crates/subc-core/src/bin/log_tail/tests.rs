use std::{
    fs,
    path::{Path, PathBuf},
};

use cortexkit_test_support::ScratchDir;
use subc_daemon::stderr_tail::format_capture_stamp;

use super::super::{
    collect_log_text, discover_log_sources_in, fixed_lane, log_entry_json, sort_log_entries,
    LogDirs, LogEntry, LogFilterCounts, LogSource, ModuleLogsOptions,
};
use super::{newest_log_entries, read_bounded_tail, read_bounded_tail_in, tail_and_follower};

thread_local! {
    /// How many log entries this thread's reads have built, to show that a
    /// short tail does not copy every line it reads.
    pub(super) static ENTRIES_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Bytes actually read by the tail-window reader, before dropping any partial line.
    pub(super) static SLICE_BYTES_READ: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Runs once between reading the initial tail and handing over to the
    /// follower, so a test can append in between.
    pub(super) static AFTER_TAIL: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

fn write(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn r1(second: u32, millis: u32, message: &str) -> String {
    format!("2026-10-20T18:43:{second:02}.{millis:03}Z INFO broca {message}")
}

fn stamped(at_ms: u64, text: &str) -> String {
    format!("{} {text}", format_capture_stamp(at_ms))
}

/// The epoch milliseconds of `2026-10-20T18:43:<second>.000Z`.
fn r1_ms(second: u64) -> u64 {
    1_792_521_780_000 + second * 1000
}

/// A module whose history has every case the print order breaks ties on:
/// equal times in two lanes and within one lane, capture lines that inherit
/// a neighbour's stamp or the file's first stamp, undated lines, rotated
/// files, hidden daemon lines, CRLF endings and a last line with no newline.
fn fixture(root: &ScratchDir) -> (LogDirs, Vec<LogSource>) {
    let dirs = LogDirs {
        module_logs: root.join("mod-logs"),
        run_logs: Some(root.join("run-logs")),
    };
    assert_eq!(
        format_capture_stamp(r1_ms(5)),
        "2026-10-20T18:43:05.000Z",
        "the fixture's capture stamps must match its r1 times"
    );
    write(
        &dirs.module_logs.join("broca.log.1"),
        &[
            r1(1, 0, "rotated one"),
            r1(5, 0, "rotated tie"),
            "rotated undated".to_string(),
            String::new(),
        ]
        .join("\n"),
    );
    write(
        &dirs.module_logs.join("broca.log"),
        &[
            r1(5, 0, "active tie a"),
            r1(5, 0, "active tie b"),
            "undated in the middle".to_string(),
            r1(7, 250, "crlf line\r"),
            r1(3, 0, "out of order"),
            r1(9, 0, "no newline at the end"),
        ]
        .join("\n"),
    );
    write(
        &dirs.module_logs.join("broca.plugin.log"),
        &[
            r1(5, 0, "plugin tie"),
            r1(8, 0, "plugin later"),
            String::new(),
        ]
        .join("\n"),
    );
    let run_logs = dirs.run_logs.clone().unwrap();
    write(
        &run_logs.join("broca.stderr.log"),
        &[
            "legacy head before any stamp".to_string(),
            stamped(r1_ms(5), "capture tie"),
            "unstamped after the tie".to_string(),
            stamped(r1_ms(6), "panicked at main.rs"),
            "  stack frame".to_string(),
            String::new(),
        ]
        .join("\n"),
    );
    write(
        &run_logs.join("subc.log"),
        &[
            "2026-10-20T18:43:05.000Z INFO subc supervising module_id=broca".to_string(),
            "2026-10-20T18:43:06.000Z INFO subc supervising module_id=other".to_string(),
            String::new(),
        ]
        .join("\n"),
    );
    let sources = discover_log_sources_in(&dirs, "broca", true).unwrap();
    assert_eq!(sources.len(), 5, "{sources:?}");
    (dirs, sources)
}

/// What `ck module logs` printed before it kept only the newest lines: every
/// line of every source collected, all of them sorted, the last `lines` shown.
fn every_line_then_last(
    sources: &[LogSource],
    options: &ModuleLogsOptions,
) -> (Vec<LogEntry>, LogFilterCounts) {
    let mut entries = Vec::new();
    let mut counts = LogFilterCounts::default();
    let mut order = 0;
    for source in sources {
        let text = fs::read_to_string(&source.path).unwrap();
        collect_log_text(
            "broca",
            source,
            &text,
            options,
            &mut counts,
            &mut entries,
            &mut order,
        );
    }
    sort_log_entries(&mut entries);
    let start = entries.len().saturating_sub(options.lines);
    (entries.split_off(start), counts)
}

/// Both renderings `ck module logs` prints: the plain lines and the JSON rows.
fn rendered(entries: &[LogEntry]) -> (Vec<String>, Vec<String>) {
    (
        entries
            .iter()
            .map(|entry| format!("{} {}", fixed_lane(&entry.lane), entry.line))
            .collect(),
        entries
            .iter()
            .map(|entry| log_entry_json(entry).to_string())
            .collect(),
    )
}

fn count_fields(counts: &LogFilterCounts) -> [usize; 7] {
    [
        counts.total,
        counts.below_level,
        counts.wrong_tag,
        counts.wrong_lane,
        counts.before_since,
        counts.daemon_hidden,
        counts.unparsed,
    ]
}

fn options(lines: usize, lane: Option<&str>) -> ModuleLogsOptions {
    ModuleLogsOptions {
        lines,
        follow: false,
        since: None,
        tag: None,
        level: None,
        lane: lane.map(str::to_string),
    }
}

#[test]
fn newest_lines_print_exactly_what_sorting_every_line_printed() {
    let root = ScratchDir::new("log-tail-newest");
    let (_, sources) = fixture(&root);
    let (everything, _) = every_line_then_last(&sources, &options(usize::MAX, None));
    assert!(everything.len() > 12, "{everything:?}");
    assert!(everything.iter().any(|entry| entry.time_inherited));
    for lane in [None, Some("mod"), Some("stderr")] {
        for lines in [
            0,
            1,
            2,
            3,
            5,
            8,
            everything.len() - 1,
            everything.len(),
            1000,
        ] {
            let options = options(lines, lane);
            let (old, old_counts) = every_line_then_last(&sources, &options);
            let (new, new_counts) = newest_log_entries("broca", &sources, &options).unwrap();
            assert_eq!(
                rendered(&new),
                rendered(&old),
                "-n {lines} --lane {lane:?} printed different lines"
            );
            assert_eq!(count_fields(&new_counts), count_fields(&old_counts));
        }
    }
}

#[test]
fn a_short_tail_builds_entries_only_for_lines_it_may_print() {
    let root = ScratchDir::new("log-tail-bounded");
    let path = root.join("mod-logs").join("broca.log");
    let history: String = (0..10_000u32)
        .map(|i| {
            let age = 9_999 - i;
            format!("{}\n", r1(age / 1000, age % 1000, &format!("line {i}")))
        })
        .collect();
    write(&path, &history);
    let source = LogSource {
        lane: "mod".to_string(),
        path,
        daemon: false,
        capture: false,
    };
    ENTRIES_BUILT.with(|built| built.set(0));
    let (newest, counts) = newest_log_entries("broca", &[source], &options(20, None)).unwrap();
    assert_eq!(counts.total, 10_000);
    // The file is written newest first, so its first 20 lines are the tail.
    let lines: Vec<&str> = newest.iter().map(|entry| entry.line.as_str()).collect();
    let expected: Vec<String> = (0..20u32)
        .rev()
        .map(|i| r1(9, 999 - i, &format!("line {i}")))
        .collect();
    assert_eq!(lines, expected);
    // Every later line is older than all 20 kept, so none of them is copied.
    let built = ENTRIES_BUILT.with(|built| built.get());
    assert_eq!(
        built, 20,
        "entries built for a 20-line tail of 10,000 lines"
    );
}

#[test]
fn a_missing_source_fails_with_its_path_as_before() {
    let root = ScratchDir::new("log-tail-missing");
    let (_, mut sources) = fixture(&root);
    let missing: PathBuf = root.join("mod-logs").join("broca.gone.log");
    sources[0].path = missing.clone();
    let error = newest_log_entries("broca", &sources, &options(5, None))
        .err()
        .expect("a missing source is an error");
    assert!(
        error
            .to_string()
            .starts_with(&missing.display().to_string()),
        "{error}"
    );
}

#[test]
fn bounded_tail_reads_only_the_end_of_each_active_file() {
    let root = ScratchDir::new("log-tail-disk-window");
    let dirs = empty_dirs(&root);
    let module_lines: Vec<_> = (0..30).map(|i| r1(i, 0, &format!("line {i}"))).collect();
    let capture_line = stamped(r1_ms(59), "capture line");
    // The undated history and any fragment of the long line would sort after
    // the dated tail and displace one of the newest 20 entries.
    let history = format!("undated history\n{}\n", "x".repeat(100_000));
    write(
        &dirs.module_logs.join("broca.log"),
        &format!("{history}{}\n", module_lines.join("\n")),
    );
    write(
        &dirs.run_logs.as_ref().unwrap().join("broca.stderr.log"),
        &format!("{history}{capture_line}\n"),
    );
    write(&dirs.module_logs.join("broca.log.1"), "rotated line\n");

    SLICE_BYTES_READ.with(|total| total.set(0));
    let tail = read_bounded_tail_in("broca", &dirs).unwrap().unwrap();

    assert_eq!(SLICE_BYTES_READ.with(std::cell::Cell::get), 16_000);
    assert!(tail.truncated);
    let expected: Vec<_> = module_lines[11..]
        .iter()
        .chain(std::iter::once(&capture_line))
        .collect();
    let actual: Vec<_> = tail.entries.iter().map(|entry| &entry.line).collect();
    assert_eq!(actual, expected);
}

#[test]
fn bounded_tail_reports_a_line_longer_than_the_window_as_truncated() {
    let root = ScratchDir::new("log-tail-disk-long-line");
    let dirs = empty_dirs(&root);
    write(&dirs.module_logs.join("broca.log"), &"x".repeat(8_001));

    let tail = read_bounded_tail_in("broca", &dirs).unwrap().unwrap();

    assert!(tail.truncated);
    assert!(tail.entries.is_empty());
}

#[test]
fn bounded_tail_distinguishes_absent_logs_from_an_empty_read() {
    let root = ScratchDir::new("log-tail-disk-absent");
    let dirs = LogDirs {
        module_logs: root.join("mod-logs"),
        run_logs: Some(root.join("run-logs")),
    };
    assert!(read_bounded_tail_in("broca", &dirs).unwrap().is_none());

    write(&dirs.module_logs.join("broca.log"), "");
    let tail = read_bounded_tail_in("broca", &dirs).unwrap().unwrap();
    assert!(tail.entries.is_empty());
    assert!(!tail.truncated);
}

#[test]
fn bounded_tail_returns_run_folder_warning_without_printing() {
    use std::{env, path::Component, process::Command};

    const CHILD: &str = "CK_TEST_BOUNDED_TAIL_WARNING_CHILD";
    if env::var_os(CHILD).is_some() {
        let (dirs, _) = LogDirs::for_module("broca");
        let cwd = env::current_dir().unwrap();
        let log_dir = cwd.join(dirs.module_logs);
        let relative = log_dir.strip_prefix(&cwd).unwrap();
        assert!(relative
            .components()
            .all(|c| matches!(c, Component::Normal(_))));
        assert!(!relative.as_os_str().is_empty());

        write(&log_dir.join("broca.log"), "module line\n");
        let (tail, warning) = read_bounded_tail("broca");
        let tail = tail.unwrap().unwrap();
        assert_eq!(tail.entries.len(), 1);
        assert_eq!(tail.entries[0].line, "module line");
        assert!(!tail.truncated);
        let warning = warning.unwrap();
        assert!(
            warning.starts_with("skipping daemon capture logs:"),
            "{warning}"
        );
        assert!(warning.contains("relative-data-home"), "{warning}");

        // A file in place of the log directory makes discovery fail.
        fs::remove_file(log_dir.join("broca.log")).unwrap();
        fs::remove_dir(&log_dir).unwrap();
        fs::write(&log_dir, "not a directory").unwrap();
        let (tail, warning_on_error) = read_bounded_tail("broca");
        assert!(tail.is_err());
        assert_eq!(warning_on_error.as_deref(), Some(warning.as_str()));
        return;
    }

    // A subprocess isolates environment changes and exposes anything printed
    // to stderr by the reader.
    let root = ScratchDir::new("log-tail-disk-warning");
    let cwd = root.join(".").canonicalize().unwrap();
    let output = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "log_tail::tests::bounded_tail_returns_run_folder_warning_without_printing",
            "--nocapture",
        ])
        .current_dir(&cwd)
        .env(CHILD, "1")
        .env("HOME", &cwd)
        .env("USERPROFILE", &cwd)
        .env("APPDATA", &cwd)
        .env("XDG_DATA_HOME", "relative-data-home")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "{stdout}");
    assert!(stderr.is_empty(), "{stderr}");
}

// ---- following after the initial tail ---------------------------------------

fn append(path: &Path, text: &str) {
    use std::io::Write as _;
    fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

fn empty_dirs(root: &ScratchDir) -> LogDirs {
    let dirs = LogDirs {
        module_logs: root.join("mod-logs"),
        run_logs: Some(root.join("run-logs")),
    };
    fs::create_dir_all(&dirs.module_logs).unwrap();
    fs::create_dir_all(dirs.run_logs.as_ref().unwrap()).unwrap();
    dirs
}

fn followed_text(follower: &mut super::LogFollower) -> Vec<String> {
    follower
        .poll()
        .unwrap()
        .into_iter()
        .map(|(_, text)| text)
        .collect()
}

/// `ck module logs -f` prints the tail, then follows. A line written after
/// the tail was read but before following began is printed by the first poll.
#[test]
fn a_line_appended_between_the_tail_and_following_is_printed() {
    let root = ScratchDir::new("log-follow-handoff");
    let dirs = empty_dirs(&root);
    let log = dirs.module_logs.join("broca.log");
    append(&log, &format!("{}\n{}\n", r1(1, 0, "one"), r1(2, 0, "two")));
    let sources = discover_log_sources_in(&dirs, "broca", true).unwrap();
    let appended = format!("{}\n", r1(3, 0, "written during the handoff"));
    let (log_for_hook, text_for_hook) = (log.clone(), appended.clone());
    AFTER_TAIL.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || append(&log_for_hook, &text_for_hook)));
    });

    let (entries, _, mut follower) =
        tail_and_follower("broca", dirs, &sources, &options(10, None)).unwrap();

    assert_eq!(entries.len(), 2, "the tail was read before the append");
    assert_eq!(followed_text(&mut follower), [appended]);
    assert!(followed_text(&mut follower).is_empty());
}

/// A line appended while a poll reads is printed by that poll and never again.
#[test]
fn a_line_appended_while_a_poll_reads_is_printed_once() {
    let root = ScratchDir::new("log-follow-mid-poll");
    let dirs = empty_dirs(&root);
    let log = dirs.module_logs.join("broca.log");
    append(&log, &format!("{}\n", r1(1, 0, "one")));
    let sources = discover_log_sources_in(&dirs, "broca", true).unwrap();
    let (_, _, mut follower) =
        tail_and_follower("broca", dirs, &sources, &options(10, None)).unwrap();

    let before = format!("{}\n", r1(2, 0, "before the poll"));
    let during = format!("{}\n", r1(3, 0, "during the poll"));
    append(&log, &before);
    let (log_for_hook, text_for_hook) = (log.clone(), during.clone());
    let mut once = Some(move || append(&log_for_hook, &text_for_hook));
    follower.before_read = Some(Box::new(move |_| {
        if let Some(append_during) = once.take() {
            append_during();
        }
    }));

    assert_eq!(followed_text(&mut follower), [format!("{before}{during}")]);
    assert!(
        followed_text(&mut follower).is_empty(),
        "the next poll printed a line again"
    );
    let after = format!("{}\n", r1(4, 0, "after"));
    append(&log, &after);
    assert_eq!(followed_text(&mut follower), [after]);
}

/// A poll that ends inside a line holds it back and prints it whole once its
/// newline arrives.
#[test]
fn a_partial_line_waits_for_its_newline() {
    let root = ScratchDir::new("log-follow-partial");
    let dirs = empty_dirs(&root);
    let log = dirs.module_logs.join("broca.log");
    append(&log, &format!("{}\n", r1(1, 0, "one")));
    let sources = discover_log_sources_in(&dirs, "broca", true).unwrap();
    let (_, _, mut follower) =
        tail_and_follower("broca", dirs, &sources, &options(10, None)).unwrap();

    let line = r1(2, 0, "written in two parts");
    let (head, rest) = line.split_at(20);
    append(&log, head);
    assert!(followed_text(&mut follower).is_empty());
    append(&log, &format!("{rest}\n"));
    assert_eq!(followed_text(&mut follower), [format!("{line}\n")]);
}

/// `-f` on a module with no log yet waits for the file and prints it from
/// its first line.
#[test]
fn following_starts_a_log_that_did_not_exist_yet() {
    let root = ScratchDir::new("log-follow-absent");
    let dirs = empty_dirs(&root);
    let sources = discover_log_sources_in(&dirs, "broca", true).unwrap();
    assert!(sources.is_empty());
    let (entries, counts, mut follower) =
        tail_and_follower("broca", dirs.clone(), &sources, &options(10, None)).unwrap();
    assert!(entries.is_empty() && counts.total == 0);
    assert!(followed_text(&mut follower).is_empty());

    let first = format!("{}\n", r1(1, 0, "first line"));
    append(&dirs.module_logs.join("broca.log"), &first);
    assert_eq!(followed_text(&mut follower), [first]);
}

/// A line half-written when `-f` starts is not printed as a fragment by the
/// tail and another by the follower: the follower prints it whole.
#[test]
fn a_line_half_written_when_following_starts_is_printed_whole() {
    let root = ScratchDir::new("log-follow-initial-partial");
    let dirs = empty_dirs(&root);
    let log = dirs.module_logs.join("broca.log");
    let line = r1(2, 0, "written across the handoff");
    let (head, rest) = line.split_at(20);
    append(&log, &format!("{}\n{head}", r1(1, 0, "one")));
    let sources = discover_log_sources_in(&dirs, "broca", true).unwrap();
    let mut follow = options(10, None);
    follow.follow = true;

    let (entries, _, mut follower) = tail_and_follower("broca", dirs, &sources, &follow).unwrap();

    assert_eq!(entries.len(), 1, "the half-written line is not in the tail");
    assert!(followed_text(&mut follower).is_empty());
    append(&log, &format!("{rest}\n"));
    assert_eq!(followed_text(&mut follower), [format!("{line}\n")]);
}

/// A rotated file is not followed, so `-f` prints its unterminated last
/// line in the tail, as a plain tail does.
#[test]
fn a_rotated_files_unterminated_last_line_is_in_the_followed_tail() {
    let root = ScratchDir::new("log-follow-rotated-partial");
    let dirs = empty_dirs(&root);
    append(
        &dirs.module_logs.join("broca.log.1"),
        &r1(1, 0, "last line of the rotated file, no newline"),
    );
    append(
        &dirs.module_logs.join("broca.log"),
        &format!("{}\n", r1(2, 0, "active")),
    );
    let sources = discover_log_sources_in(&dirs, "broca", true).unwrap();
    let mut follow = options(10, None);
    follow.follow = true;

    let (entries, _, _) = tail_and_follower("broca", dirs, &sources, &follow).unwrap();

    assert_eq!(entries.len(), 2, "the rotated file's last line was dropped");
}
