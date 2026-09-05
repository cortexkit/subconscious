use std::borrow::Cow;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tempfile::TempDir;
use tracing::Level;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::Registry;

use super::*;

const FIXTURE_RELATIVE_PATH: &str = "../subc-core/tests/fixtures/log_format_golden.json";
const TAGS: &[&str] = &["perf", "wire"];

#[derive(Clone, Default)]
struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

impl CaptureWriter {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture lock").clone()).expect("UTF-8 capture")
    }
}

impl Write for CaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .expect("capture lock")
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[derive(Deserialize)]
struct GoldenFixture {
    cases: Vec<GoldenCase>,
    parse_rejects: Vec<ParseReject>,
    level_filter: LevelFilterCases,
}

#[derive(Deserialize)]
struct GoldenCase {
    name: String,
    event: GoldenEvent,
    line: String,
}

#[derive(Deserialize)]
struct GoldenEvent {
    at_ms: u64,
    module: String,
    session: Option<GoldenSession>,
    tag: Option<String>,
}

#[derive(Deserialize)]
struct GoldenSession {
    issuer: String,
    id: String,
}

#[derive(Deserialize)]
struct ParseReject {
    name: String,
    line: String,
    reason: String,
}

#[derive(Deserialize)]
struct LevelFilterCases {
    cases: Vec<LevelFilterCase>,
}

#[derive(Deserialize)]
struct LevelFilterCase {
    spec: String,
    level: String,
    tag: Option<String>,
    emit: bool,
}

fn fixture() -> GoldenFixture {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_RELATIVE_PATH);
    let contents = fs::read_to_string(path).expect("golden fixture");
    serde_json::from_str(&contents).expect("valid golden fixture")
}

fn fixed_time(milliseconds: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(milliseconds)
}

fn config(logs_dir: PathBuf, module_id: &str) -> Config {
    Config {
        module_id: module_id.to_owned(),
        logs_dir,
        lane: Lane::Module,
        spec: Some("info".to_owned()),
        retention: Retention::default(),
        redactor: None,
        clock: Some(Arc::new(|| fixed_time(1_788_604_863_123))),
    }
}

fn build_test_layer(
    config: Config,
    tags: &'static [&'static str],
    capture: &CaptureWriter,
) -> (LogLayer, Handle) {
    build_layer(config, tags, Box::new(capture.clone())).expect("build test layer")
}

fn dispatch(layer: LogLayer) -> tracing::Dispatch {
    tracing::Dispatch::new(Registry::default().with(layer))
}

#[test]
fn golden_lines_render_byte_identically() {
    for case in fixture().cases {
        let temp = TempDir::new().expect("temp dir");
        let capture = CaptureWriter::default();
        let at = fixed_time(case.event.at_ms);
        let mut test_config = config(temp.path().to_owned(), &case.event.module);
        test_config.spec = Some("trace".to_owned());
        test_config.clock = Some(Arc::new(move || at));
        let (layer, handle) = build_test_layer(test_config, TAGS, &capture);
        let dispatcher = dispatch(layer);

        tracing::dispatcher::with_default(&dispatcher, || {
            let span = case
                .event
                .session
                .as_ref()
                .map_or_else(tracing::Span::none, |session| {
                    session_span(&session.issuer, &session.id)
                });
            let _entered = span.enter();
            emit_golden_case(&case.name);
        });

        let actual = fs::read_to_string(handle.path()).expect("golden output");
        assert_eq!(
            actual,
            format!("{}\n", case.line),
            "fixture case {}",
            case.name
        );
    }
}

fn emit_golden_case(name: &str) {
    match name {
        "plain-info-no-session" => tracing::info!(
            version = "1788526509641",
            eras = "22",
            facts_changed = "0",
            arrived = "2",
            "poll changed"
        ),
        "session-and-tag" => tracing::warn!(
            target: "perf",
            ms = "412",
            retry = "2",
            "transform stage folded"
        ),
        "error-level-padding" => tracing::error!(
            provider = "codex",
            account = "ufuk3",
            class = "auth_invalid",
            "refresh failed"
        ),
        "debug-and-trace-padding" => tracing::debug!(
            run = "r_12",
            model = "anthropic/claude-sonnet-4-5",
            "dispatch admitted"
        ),
        "trace-level" => tracing::trace!(
            target: "wire",
            channel = "716",
            bytes = "1024",
            "frame"
        ),
        "value-with-space-is-quoted" => tracing::info!(
            name = "aft_search",
            root = "/Users/x/My Project",
            ms = "1602",
            "slow tool_call"
        ),
        "value-with-quote-and-newline-is-escaped" => {
            tracing::warn!(body = "line one\nsaid \"no\"", "vendor error")
        }
        "message-newline-is-escaped" => {
            tracing::error!("engine crashed\nlast stderr: boom")
        }
        "two-session-fields" => tracing::info!(
            broca_session = "broca:8f3a1c02",
            agent = "ag_9",
            "run started"
        ),
        "empty-value-is-quoted" => tracing::info!(reason = "", "handshake"),
        "backslash-in-message-and-value" => tracing::info!(
            root = "C:\\Users\\x\\My Project",
            bare = "C:\\x",
            "path C:\\Users\\x"
        ),
        other => panic!("unimplemented golden fixture case: {other}"),
    }
}

#[test]
fn every_golden_line_parses() {
    for case in fixture().cases {
        let parsed = parse_line(&case.line)
            .unwrap_or_else(|error| panic!("fixture case {} did not parse: {error}", case.name));
        assert_eq!(
            parsed.module_id, case.event.module,
            "fixture case {}",
            case.name
        );
        assert_eq!(
            parsed.tag,
            case.event.tag.as_deref(),
            "fixture case {}",
            case.name
        );
    }
}

#[test]
fn fixture_parse_rejections_have_named_reasons() {
    for rejection in fixture().parse_rejects {
        let error = parse_line(&rejection.line)
            .unwrap_err_or_else(|| panic!("fixture rejection {} parsed", rejection.name));
        assert_eq!(
            error.reason(),
            rejection.reason,
            "fixture rejection {}",
            rejection.name
        );
    }
}

trait UnwrapErrOrElse<T, E> {
    fn unwrap_err_or_else(self, function: impl FnOnce() -> E) -> E;
}

impl<T, E> UnwrapErrOrElse<T, E> for Result<T, E> {
    fn unwrap_err_or_else(self, function: impl FnOnce() -> E) -> E {
        match self {
            Ok(_) => function(),
            Err(error) => error,
        }
    }
}

#[test]
fn tracing_layer_carries_session_and_ordered_fields() {
    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let mut test_config = config(temp.path().to_owned(), "magic-context");
    test_config.spec = Some("warn".to_owned());
    let (layer, handle) = build_test_layer(test_config, TAGS, &capture);
    let dispatcher = dispatch(layer);

    tracing::dispatcher::with_default(&dispatcher, || {
        let span = session_span("opencode", "ses_00fc88222ffe");
        let _entered = span.enter();
        tracing::warn!(target: "perf", ms = 412, retry = 2, "transform stage folded");
    });

    let line = fs::read_to_string(handle.path()).expect("log line");
    assert_eq!(
        line,
        "2026-09-05T10:41:03.123Z WARN  magic-context session=opencode:ses_00fc88222ffe tag=perf transform stage folded ms=412 retry=2\n"
    );
}

#[test]
fn debug_session_fields_are_normalized_and_global_is_absent() {
    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let (layer, handle) =
        build_test_layer(config(temp.path().to_owned(), "sessions"), TAGS, &capture);
    let dispatcher = dispatch(layer);

    tracing::dispatcher::with_default(&dispatcher, || {
        let session = "opencode:ses_debug";
        let span = tracing::info_span!("raw-session", session = ?session);
        let _entered = span.enter();
        tracing::info!("debug session");
    });
    tracing::dispatcher::with_default(&dispatcher, || {
        // An empty id is the "no session" form; the line must carry no field.
        let span = session_span("opencode", "");
        let _entered = span.enter();
        tracing::info!("no session");
    });

    let contents = fs::read_to_string(handle.path()).expect("session lines");
    let lines: Vec<_> = contents.lines().collect();
    assert!(lines[0].contains(" session=opencode:ses_debug debug session"));
    assert!(!lines[1].contains(" session="));
}

#[test]
fn declared_targets_are_the_only_rendered_tags() {
    const EDGE_TAGS: &[&str] = &["perf", "module-a", "some_crate::worker"];

    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let (layer, handle) = build_test_layer(
        config(temp.path().to_owned(), "module-a"),
        EDGE_TAGS,
        &capture,
    );
    let dispatcher = dispatch(layer);

    tracing::dispatcher::with_default(&dispatcher, || {
        tracing::info!(target: "perf", "declared");
        tracing::info!(target: "module-a", "module target");
        tracing::info!(target: "some_crate::worker", "crate path");
        tracing::info!(target: "undeclared", "unknown target");
    });

    let lines = fs::read_to_string(handle.path()).expect("log lines");
    let lines: Vec<_> = lines.lines().collect();
    assert!(lines[0].contains(" tag=perf declared"));
    assert!(!lines[1].contains(" tag="));
    assert!(!lines[2].contains(" tag="));
    assert!(!lines[3].contains(" tag="));
}

#[test]
fn every_fixture_level_filter_case_is_enforced() {
    for (index, case) in fixture().level_filter.cases.into_iter().enumerate() {
        let temp = TempDir::new().expect("temp dir");
        let capture = CaptureWriter::default();
        let module_id = format!("filter-{index}");
        let mut test_config = config(temp.path().to_owned(), &module_id);
        test_config.spec = Some(case.spec.clone());
        let (layer, handle) = build_test_layer(test_config, TAGS, &capture);
        let dispatcher = dispatch(layer);

        tracing::dispatcher::with_default(&dispatcher, || {
            emit_filter_probe(&case.level, case.tag.as_deref())
        });
        let contents = fs::read_to_string(handle.path()).expect("filter log");
        assert_eq!(
            !contents.is_empty(),
            case.emit,
            "filter spec {:?}, level {}, tag {:?}",
            case.spec,
            case.level,
            case.tag
        );
        if case.spec == "garbage=" {
            assert_eq!(capture.text().matches("invalid CK_LOG value").count(), 1);
        }
    }
}

fn emit_filter_probe(level: &str, tag: Option<&str>) {
    match (level, tag) {
        ("trace", Some("wire")) => tracing::trace!(target: "wire", "probe"),
        ("debug", Some("perf")) => tracing::debug!(target: "perf", "probe"),
        ("debug", Some("wire")) => tracing::debug!(target: "wire", "probe"),
        ("debug", None) => tracing::debug!("probe"),
        ("info", None) => tracing::info!("probe"),
        ("error", None) => tracing::error!("probe"),
        combination => panic!("unsupported fixture filter combination: {combination:?}"),
    }
}

#[test]
fn bearer_credentials_are_redacted() {
    assert_eq!(
        fleet_redact("token=Bearer abc.DEF-123"),
        "token=Bearer [REDACTED]"
    );
}

#[test]
fn jwt_credentials_are_redacted() {
    assert_eq!(
        fleet_redact("token=eyJhbGciOiJub25l.eyJzdWIiOiIxIn0.signature"),
        "token=[REDACTED]"
    );
}

#[test]
fn cortexkit_handles_are_redacted() {
    assert_eq!(
        fleet_redact("handle=ckh_private-handle"),
        "handle=[REDACTED]"
    );
}

#[test]
fn openai_keys_are_redacted() {
    assert_eq!(fleet_redact("key=sk-project_secret"), "key=[REDACTED]");
}

#[test]
fn github_tokens_are_redacted() {
    assert_eq!(
        fleet_redact("one=ghp_private two=gho_private"),
        "one=[REDACTED] two=[REDACTED]"
    );
}

#[test]
fn authorization_values_are_redacted() {
    assert_eq!(
        fleet_redact("request Authorization: Basic cHJpdmF0ZQ== status=sent"),
        "request Authorization: [REDACTED] status=sent"
    );
    assert_eq!(
        fleet_redact(r#"header="Authorization: Basic cHJpdmF0ZQ==" status=sent"#),
        r#"header="Authorization: [REDACTED]" status=sent"#
    );
    assert_eq!(
        fleet_redact(r#"header=\"Authorization: Basic cHJpdmF0ZQ==\" status=sent"#),
        r#"header=\"Authorization: [REDACTED]\" status=sent"#
    );
}

#[test]
fn ordinary_lines_pass_through_redaction_unchanged() {
    let line = "poll changed version=1788526509641 arrived=2";
    assert!(matches!(fleet_redact(line), Cow::Borrowed(value) if value == line));
}

#[test]
fn module_redactor_runs_after_fleet_redaction() {
    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let observed_fleet_redaction = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&observed_fleet_redaction);
    let redactor: Arc<Redactor> = Arc::new(move |line: &str| {
        observed.store(line.contains("[REDACTED]"), Ordering::Relaxed);
        Cow::Owned(line.replace("[REDACTED]", "<module-redacted>"))
    });
    let mut test_config = config(temp.path().to_owned(), "redactor");
    test_config.redactor = Some(redactor);
    let (layer, handle) = build_test_layer(test_config, TAGS, &capture);
    let dispatcher = dispatch(layer);

    tracing::dispatcher::with_default(&dispatcher, || {
        tracing::info!(credential = "sk-private", "request");
    });

    let contents = fs::read_to_string(handle.path()).expect("redacted line");
    assert!(observed_fleet_redaction.load(Ordering::Relaxed));
    assert!(contents.contains("credential=<module-redacted>"));
    assert!(!contents.contains("sk-private"));
}

#[test]
fn ansi_is_stripped_from_rendered_lines() {
    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let redactor: Arc<Redactor> =
        Arc::new(|line: &str| Cow::Owned(format!("\u{1b}[31m{line}\u{1b}[0m")));
    let mut test_config = config(temp.path().to_owned(), "ansi");
    test_config.redactor = Some(redactor);
    let (layer, handle) = build_test_layer(test_config, TAGS, &capture);
    let dispatcher = dispatch(layer);

    tracing::dispatcher::with_default(&dispatcher, || {
        tracing::info!("\u{1b}[32mclean\u{1b}[0m \u{009b}31mc1\u{009b}0m");
    });

    let contents = fs::read_to_string(handle.path()).expect("ANSI-free line");
    assert!(!contents.contains(['\u{1b}', '\u{009b}']));
    assert!(contents.ends_with("ansi clean c1\n"));
}

#[test]
fn rotation_keeps_only_the_configured_generations() {
    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let mut test_config = config(temp.path().to_owned(), "rotate");
    test_config.retention = Retention::from_bytes_for_testing(120, 2, 14);
    let (layer, handle) = build_test_layer(test_config, TAGS, &capture);
    let path = handle.path().to_owned();
    let dispatcher = dispatch(layer);

    for sequence in 0..4 {
        tracing::dispatcher::with_default(&dispatcher, || {
            tracing::info!(
                sequence,
                padding = "abcdefghijklmnopqrstuvwxyz0123456789",
                "event"
            );
        });
    }

    assert!(path.exists());
    assert!(sink::rotated_path(&path, 1).exists());
    assert!(sink::rotated_path(&path, 2).exists());
    assert!(!sink::rotated_path(&path, 3).exists());
    assert!(fs::read_to_string(&path)
        .expect("active")
        .contains("sequence=3"));
    assert!(fs::read_to_string(sink::rotated_path(&path, 1))
        .expect("generation one")
        .contains("sequence=2"));
    assert!(fs::read_to_string(sink::rotated_path(&path, 2))
        .expect("generation two")
        .contains("sequence=1"));
}

#[test]
fn old_generations_are_pruned_at_init_and_rotation() {
    let temp = TempDir::new().expect("temp dir");
    let path = temp.path().join("age.log");
    fs::write(&path, "quiet active line\n").expect("quiet active file");
    fs::write(sink::rotated_path(&path, 1), "old\n").expect("old generation");
    let future = SystemTime::now() + Duration::from_secs(15 * 24 * 60 * 60);
    let capture = CaptureWriter::default();
    let test_config = Config {
        module_id: "age".to_owned(),
        logs_dir: temp.path().to_owned(),
        lane: Lane::Module,
        spec: Some("info".to_owned()),
        retention: Retention::from_bytes_for_testing(80, 2, 14),
        redactor: None,
        clock: Some(Arc::new(move || future)),
    };
    let (layer, handle) = build_test_layer(test_config, TAGS, &capture);
    assert_eq!(
        fs::read_to_string(handle.path()).expect("quiet active file after init"),
        "quiet active line\n"
    );
    assert!(!sink::rotated_path(handle.path(), 1).exists());

    fs::write(sink::rotated_path(handle.path(), 1), "old again\n")
        .expect("old generation after init");
    let dispatcher = dispatch(layer);
    for sequence in 0..2 {
        tracing::dispatcher::with_default(&dispatcher, || {
            tracing::info!(sequence, padding = "abcdefghijklmnopqrstuvwxyz", "rotation");
        });
    }
    assert!(!sink::rotated_path(handle.path(), 2).exists());
}

#[test]
fn file_open_failure_falls_back_and_announces_it_first() {
    let temp = TempDir::new().expect("temp dir");
    let blocker = temp.path().join("not-a-directory");
    fs::write(&blocker, "block").expect("blocking file");
    let capture = CaptureWriter::default();
    let (layer, handle) = build_test_layer(config(blocker, "fallback"), TAGS, &capture);
    assert!(handle.fallback_active());
    let dispatcher = dispatch(layer);
    tracing::dispatcher::with_default(&dispatcher, || tracing::info!("after fallback"));

    let output = capture.text();
    let mut lines = output.lines();
    assert!(lines
        .next()
        .expect("announcement")
        .contains("file sink unavailable; falling back to stderr"));
    assert!(lines
        .next()
        .expect("event")
        .ends_with("fallback after fallback"));
}

#[cfg(unix)]
#[test]
fn unwritable_directory_activates_captured_stderr_fallback() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new().expect("temp dir");
    let logs_dir = temp.path().join("unwritable");
    fs::create_dir(&logs_dir).expect("logs directory");
    fs::set_permissions(&logs_dir, fs::Permissions::from_mode(0o500))
        .expect("make directory unwritable");
    let path = logs_dir.join("custom.log");
    let capture = CaptureWriter::default();
    let mut test_config = config(PathBuf::from("ignored"), "fallback");
    test_config.lane = Lane::Custom(path);
    let (_, handle) = build_test_layer(test_config, TAGS, &capture);
    fs::set_permissions(&logs_dir, fs::Permissions::from_mode(0o700))
        .expect("restore directory permissions");

    assert!(handle.fallback_active());
    assert!(capture
        .text()
        .contains("file sink unavailable; falling back to stderr"));
}

#[test]
fn write_failures_are_swallowed_and_reported_once() {
    WRITE_FAILURE_REPORTED.store(false, Ordering::Relaxed);
    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let (_, handle) = build_test_layer(config(temp.path().to_owned(), "failure"), TAGS, &capture);
    *handle.inner.destination.lock().expect("destination lock") = Destination::AlwaysFail;

    handle.inner.emit(&Level::INFO, None, None, "one", &[]);
    handle.inner.emit(&Level::INFO, None, None, "two", &[]);

    assert_eq!(handle.swallowed_writes(), 2);
    assert_eq!(capture.text().matches("log write failed").count(), 1);
}

#[test]
fn eight_threads_write_complete_parseable_lines() {
    const THREADS: usize = 8;
    const EVENTS: usize = 1_000;

    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let (layer, handle) =
        build_test_layer(config(temp.path().to_owned(), "concurrent"), TAGS, &capture);
    let dispatcher = dispatch(layer);
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut threads = Vec::new();
    for worker in 0..THREADS {
        let dispatcher = dispatcher.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(thread::spawn(move || {
            barrier.wait();
            tracing::dispatcher::with_default(&dispatcher, || {
                for sequence in 0..EVENTS {
                    tracing::info!(worker, sequence, "concurrent event");
                }
            });
        }));
    }
    for thread in threads {
        thread.join().expect("logging thread");
    }

    let contents = fs::read_to_string(handle.path()).expect("concurrent log");
    let lines: Vec<_> = contents.lines().collect();
    assert_eq!(lines.len(), THREADS * EVENTS);
    for line in lines {
        parse_line(line).unwrap_or_else(|error| panic!("torn line ({error}): {line}"));
    }
}

#[test]
fn paths_and_unix_permissions_follow_the_lane_contract() {
    let temp = TempDir::new().expect("temp dir");
    let capture = CaptureWriter::default();
    let (_, module) = build_test_layer(config(temp.path().join("module"), "broca"), TAGS, &capture);
    assert!(module.path().ends_with("broca.log"));

    let mut plugin_config = config(temp.path().join("plugin"), "magic-context");
    plugin_config.lane = Lane::Plugin("opencode".to_owned());
    let (_, plugin) = build_test_layer(plugin_config, TAGS, &capture);
    assert!(plugin.path().ends_with("magic-context.opencode.log"));

    let custom_path = temp.path().join("custom").join("subc.log");
    let mut custom_config = config(PathBuf::from("ignored"), "subc");
    custom_config.lane = Lane::Custom(custom_path.clone());
    let (_, custom) = build_test_layer(custom_config, TAGS, &capture);
    assert_eq!(custom.path(), custom_path);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(plugin.path().parent().expect("parent"))
                .expect("directory metadata")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(plugin.path())
                .expect("file metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn relative_custom_paths_are_rejected() {
    let capture = CaptureWriter::default();
    let mut test_config = config(PathBuf::from("ignored"), "subc");
    test_config.lane = Lane::Custom(PathBuf::from("run/logs/subc.log"));
    let error = match build_layer(test_config, TAGS, Box::new(capture)) {
        Ok(_) => panic!("relative custom path was accepted"),
        Err(error) => error,
    };
    assert_eq!(
        error,
        InitError::CustomPathNotAbsolute(PathBuf::from("run/logs/subc.log"))
    );
}

#[test]
fn panic_hook_logs_each_line_and_chains_to_previous_hook() {
    let temp = TempDir::new().expect("temp dir");
    let log_path = temp.path().join("panic.log");
    let marker_path = temp.path().join("previous-hook-ran");
    let status = Command::new(std::env::current_exe().expect("test executable"))
        .args(["--exact", "tests::panic_hook_child", "--ignored"])
        .env("CORTEXKIT_LOG_PANIC_CHILD", "1")
        .env("CORTEXKIT_LOG_PANIC_PATH", &log_path)
        .env("CORTEXKIT_LOG_PANIC_MARKER", &marker_path)
        .status()
        .expect("panic child");
    assert!(status.success());
    assert!(marker_path.exists(), "previous panic hook did not run");

    let contents = fs::read_to_string(log_path).expect("panic log");
    let panic_lines: Vec<_> = contents
        .lines()
        .filter(|line| line.contains(" tag=panic "))
        .collect();
    assert!(!panic_lines.is_empty());
    assert!(panic_lines
        .iter()
        .any(|line| line.contains("panic fixture")));
    let timestamp = &panic_lines[0][..24];
    assert!(panic_lines.iter().all(|line| &line[..24] == timestamp));
}

#[test]
#[ignore]
fn panic_hook_child() {
    if std::env::var_os("CORTEXKIT_LOG_PANIC_CHILD").is_none() {
        return;
    }
    let log_path = PathBuf::from(std::env::var_os("CORTEXKIT_LOG_PANIC_PATH").expect("log path"));
    let marker_path =
        PathBuf::from(std::env::var_os("CORTEXKIT_LOG_PANIC_MARKER").expect("marker path"));
    std::panic::set_hook(Box::new(move |_| {
        fs::write(&marker_path, "previous hook ran\n").expect("write hook marker");
    }));
    let handle = init(Config {
        module_id: "panic-test".to_owned(),
        logs_dir: PathBuf::from("ignored"),
        lane: Lane::Custom(log_path),
        spec: Some("off".to_owned()),
        retention: Retention::default(),
        redactor: None,
        clock: Some(Arc::new(|| fixed_time(1_788_604_863_123))),
    })
    .expect("initialize child logger");

    assert!(
        thread::spawn(|| panic!("panic fixture\nsecond source line"))
            .join()
            .is_err()
    );
    assert!(!handle.fallback_active());
}
