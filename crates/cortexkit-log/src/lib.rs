//! A synchronous `tracing` layer for CortexKit's module-owned fleet logs.

mod format;
mod redaction;
mod sink;

use std::backtrace::Backtrace;
use std::borrow::Cow;
use std::collections::HashSet;
use std::env;
use std::fmt;
use std::io::{self, Write};
use std::panic;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::SystemTime;

pub use format::{ParseError, ParsedLevel, ParsedLine};
use redaction::fleet_redact;
use sink::Destination;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata, Subscriber};
use tracing_subscriber::filter::EnvFilter;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::{Layer, Registry};

static DECLARED_TAGS: OnceLock<&'static [&'static str]> = OnceLock::new();
static WRITE_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);
static FILTER_FAILURE_REPORTED: AtomicBool = AtomicBool::new(false);

/// A module process's log lane.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Lane {
    /// The supervised module process (`<module_id>.log`).
    Module,
    /// A harness-hosted plugin process (`<module_id>.<harness>.log`).
    Plugin(String),
    /// An absolute path used by process owners such as the daemon.
    Custom(PathBuf),
}

/// Size and age bounds for rotated generations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Retention {
    /// Maximum active-file size in mebibytes.
    pub max_file_mb: u32,
    /// Number of rotated generations to retain.
    pub keep: u8,
    /// Maximum age of rotated generations in days.
    pub max_age_days: u32,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_file_mb: 32,
            keep: 2,
            max_age_days: 14,
        }
    }
}

impl Retention {
    const TEST_BYTES_MARKER: u32 = 1 << 31;

    /// Constructs a byte-sized cap so retention tests do not write whole MiB files.
    #[doc(hidden)]
    pub fn from_bytes_for_testing(max_file_bytes: u32, keep: u8, max_age_days: u32) -> Self {
        assert!(max_file_bytes < Self::TEST_BYTES_MARKER);
        Self {
            max_file_mb: Self::TEST_BYTES_MARKER | max_file_bytes,
            keep,
            max_age_days,
        }
    }

    pub(crate) fn max_bytes(self) -> u64 {
        if self.max_file_mb & Self::TEST_BYTES_MARKER != 0 {
            return u64::from(self.max_file_mb & !Self::TEST_BYTES_MARKER);
        }

        u64::from(self.max_file_mb) * 1024 * 1024
    }
}

/// A complete-line redactor composed after the fleet credential redactor.
pub type Redactor = dyn for<'line> Fn(&'line str) -> Cow<'line, str> + Send + Sync;

/// Logger configuration for one process-owned file.
pub struct Config {
    /// Stable fleet module identifier stamped onto every event.
    pub module_id: String,
    /// Temporary path input while this crate is hosted outside `cortexkit/commons`.
    /// Its permanent fleet API is `Config::for_module(module_id)`, backed by
    /// `cortexkit_store_types::module_data_dir`; callers are not intended to
    /// assemble the fleet path.
    pub logs_dir: PathBuf,
    /// Selects the process lane and its file name.
    pub lane: Lane,
    /// `CK_LOG` override; `None` reads the process environment.
    pub spec: Option<String>,
    /// Rotation and age-pruning policy.
    pub retention: Retention,
    /// Optional module redactor, applied after fleet credential redaction.
    pub redactor: Option<Arc<Redactor>>,
    /// Optional clock override for deterministic callers and tests.
    pub clock: Option<Arc<dyn Fn() -> SystemTime + Send + Sync>>,
}

/// A live logger handle suitable for inclusion in module health reports.
#[derive(Clone)]
pub struct Handle {
    inner: Arc<LoggerInner>,
}

impl Handle {
    /// Returns the number of lines dropped after a write or rotation failure.
    pub fn swallowed_writes(&self) -> u64 {
        self.inner.swallowed_writes.load(Ordering::Relaxed)
    }

    /// Returns the active log path, including a custom path when configured.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Reports whether opening the file failed and events are going to stderr.
    pub fn fallback_active(&self) -> bool {
        self.inner.fallback_active
    }
}

/// An error that prevents installation of the global logger.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum InitError {
    /// `Lane::Custom` was not given an absolute path.
    CustomPathNotAbsolute(PathBuf),
    /// A process-global tracing subscriber was already installed.
    GlobalSubscriberAlreadySet,
}

impl fmt::Display for InitError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CustomPathNotAbsolute(path) => {
                write!(
                    formatter,
                    "custom log path is not absolute: {}",
                    path.display()
                )
            }
            Self::GlobalSubscriberAlreadySet => {
                formatter.write_str("a global tracing subscriber is already installed")
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Declares the target names that should be rendered as `tag=<target>`.
///
/// Pass a module-owned `const` slice before [`init`]. The first declaration is
/// process-wide because `tracing` also permits only one global subscriber.
pub fn declared_tags(tags: &'static [&'static str]) {
    let _ = DECLARED_TAGS.set(tags);
}

/// Installs the fleet logger as the process-global `tracing` subscriber.
pub fn init(config: Config) -> Result<Handle, InitError> {
    let tags = DECLARED_TAGS.get().copied().unwrap_or(&[]);
    let (layer, handle) = build_layer(config, tags, Box::new(io::stderr()))?;
    let panic_inner = Arc::clone(&handle.inner);
    let subscriber = Registry::default().with(layer);
    tracing::subscriber::set_global_default(subscriber)
        .map_err(|_| InitError::GlobalSubscriberAlreadySet)?;
    install_panic_hook(panic_inner);
    Ok(handle)
}

/// Creates a span carrying canonical session lineage for nested events.
pub fn session_span(issuer: &str, id: &str) -> tracing::Span {
    // An empty half means "no session": the line carries no field rather
    // than a placeholder. Callers that used to log a synthetic id must pass
    // nothing here instead; the crate does not know their sentinels.
    if issuer.is_empty() || id.is_empty() {
        tracing::Span::none()
    } else {
        let session = format!("{issuer}:{id}");
        tracing::info_span!("cortexkit.session", session = %session)
    }
}

/// Parses the fixed columns used by merged and filtered fleet log views.
pub fn parse_line(line: &str) -> Result<ParsedLine<'_>, ParseError> {
    format::parse(line)
}

struct LoggerInner {
    module_id: String,
    path: PathBuf,
    destination: Mutex<Destination>,
    stderr: Mutex<Box<dyn Write + Send>>,
    swallowed_writes: AtomicU64,
    fallback_active: bool,
    redactor: Option<Arc<Redactor>>,
    clock: Arc<dyn Fn() -> SystemTime + Send + Sync>,
}

impl LoggerInner {
    fn emit(
        &self,
        level: &tracing::Level,
        session: Option<&str>,
        tag: Option<&str>,
        message: &str,
        fields: &[(String, String)],
    ) {
        self.emit_at((self.clock)(), level, session, tag, message, fields);
    }

    fn emit_at(
        &self,
        at: SystemTime,
        level: &tracing::Level,
        session: Option<&str>,
        tag: Option<&str>,
        message: &str,
        fields: &[(String, String)],
    ) {
        let raw = format::render_line(at, level, &self.module_id, session, tag, message, fields);
        let fleet_redacted = fleet_redact(&raw);
        let module_redacted = self.redactor.as_ref().map_or_else(
            || Cow::Borrowed(fleet_redacted.as_ref()),
            |redactor| redactor(&fleet_redacted),
        );
        // Redactors are extensibility points, so the final guard preserves the one-line,
        // no-ANSI contract even if a module redactor introduces such bytes.
        let guarded = format::strip_ansi(&module_redacted)
            .replace('\r', "\\r")
            .replace('\n', "\\n");
        self.write_line(&guarded);
    }

    fn write_line(&self, line: &str) {
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');

        // A file mutex keeps each line intact without a queue or background flusher;
        // callers wait only for the write or rotation currently using the file.
        let result = {
            let mut destination = self
                .destination
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if matches!(*destination, Destination::Fallback) {
                let mut stderr = self
                    .stderr
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                stderr.write_all(&bytes)
            } else {
                destination.write(&bytes, (self.clock)())
            }
        };

        if let Err(error) = result {
            self.swallowed_writes.fetch_add(1, Ordering::Relaxed);
            if !WRITE_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                self.report(&format!(
                    "cortexkit-log: log write failed; future failures will be swallowed: {error}\n"
                ));
            }
        }
    }

    fn report(&self, report: &str) {
        let mut stderr = self
            .stderr
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _ = stderr.write_all(report.as_bytes());
    }

    fn write_panic(&self, information: &panic::PanicHookInfo<'_>) {
        let at = (self.clock)();
        let panic_text = information.to_string();
        for line in panic_text.lines() {
            self.emit_at(at, &tracing::Level::ERROR, None, Some("panic"), line, &[]);
        }
        let backtrace = Backtrace::force_capture().to_string();
        for line in backtrace.lines() {
            self.emit_at(at, &tracing::Level::ERROR, None, Some("panic"), line, &[]);
        }
    }
}

struct LogLayer {
    inner: Arc<LoggerInner>,
    tags: HashSet<&'static str>,
    filter: EnvFilter,
}

impl<S> Layer<S> for LogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn enabled(&self, metadata: &Metadata<'_>, context: Context<'_, S>) -> bool {
        metadata.is_span() || self.filter.enabled(metadata, context)
    }

    fn on_new_span(&self, attributes: &Attributes<'_>, id: &Id, context: Context<'_, S>) {
        self.filter.on_new_span(attributes, id, context.clone());
        let mut visitor = SessionVisitor::default();
        attributes.record(&mut visitor);
        if let (Some(session), Some(span)) = (visitor.session, context.span(id)) {
            span.extensions_mut().insert(SpanSession(session));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, context: Context<'_, S>) {
        self.filter.on_record(id, values, context.clone());
        let mut visitor = SessionVisitor::default();
        values.record(&mut visitor);
        if let (Some(session), Some(span)) = (visitor.session, context.span(id)) {
            span.extensions_mut().insert(SpanSession(session));
        }
    }

    fn on_close(&self, id: Id, context: Context<'_, S>) {
        self.filter.on_close(id, context);
    }

    fn on_event(&self, event: &Event<'_>, context: Context<'_, S>) {
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        let session = context.event_scope(event).and_then(|scope| {
            scope
                .from_root()
                .filter_map(|span| span.extensions().get::<SpanSession>().cloned())
                .last()
                .map(|session| session.0)
        });
        let target = event.metadata().target();
        let tag = (target != self.inner.module_id
            && !target.contains("::")
            && self.tags.contains(target))
        .then_some(target);
        self.inner.emit(
            event.metadata().level(),
            session.as_deref(),
            tag,
            visitor.message.as_deref().unwrap_or(""),
            &visitor.fields,
        );
    }
}

#[derive(Clone)]
struct SpanSession(String);

#[derive(Default)]
struct SessionVisitor {
    session: Option<String>,
}

impl Visit for SessionVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "session" && !value.is_empty() {
            self.session = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "session" {
            let rendered = format!("{value:?}");
            let value = rendered
                .strip_prefix('"')
                .and_then(|unquoted| unquoted.strip_suffix('"'))
                .unwrap_or(&rendered);
            if !value.is_empty() {
                self.session = Some(value.to_owned());
            }
        }
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: Vec<(String, String)>,
}

impl EventVisitor {
    fn record(&mut self, field: &Field, value: String) {
        if field.name() == "message" {
            self.message = Some(value);
        } else {
            self.fields.push((field.name().to_owned(), value));
        }
    }
}

impl Visit for EventVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, value.to_string());
    }

    fn record_i128(&mut self, field: &Field, value: i128) {
        self.record(field, value.to_string());
    }

    fn record_u128(&mut self, field: &Field, value: u128) {
        self.record(field, value.to_string());
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, value.to_owned());
    }

    fn record_error(&mut self, field: &Field, value: &(dyn std::error::Error + 'static)) {
        self.record(field, value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.record(field, format!("{value:?}"));
    }
}

fn build_layer(
    config: Config,
    tags: &'static [&'static str],
    stderr: Box<dyn Write + Send>,
) -> Result<(LogLayer, Handle), InitError> {
    let (path, enforce_directory_mode) = resolve_path(&config)?;
    let clock = config.clock.unwrap_or_else(|| Arc::new(SystemTime::now));
    let now = clock();
    let (destination, open_error) =
        match Destination::open(&path, config.retention, now, enforce_directory_mode) {
            Ok(destination) => (destination, None),
            Err(error) => (Destination::Fallback, Some(error)),
        };
    let fallback_active = open_error.is_some();
    let inner = Arc::new(LoggerInner {
        module_id: config.module_id,
        path,
        destination: Mutex::new(destination),
        stderr: Mutex::new(stderr),
        swallowed_writes: AtomicU64::new(0),
        fallback_active,
        redactor: config.redactor,
        clock,
    });

    if let Some(error) = open_error {
        inner.emit(
            &tracing::Level::ERROR,
            None,
            None,
            "file sink unavailable; falling back to stderr",
            &[
                ("path".to_owned(), inner.path.display().to_string()),
                ("error".to_owned(), error.to_string()),
            ],
        );
    }

    let filter = make_filter(config.spec, &inner);
    let layer = LogLayer {
        inner: Arc::clone(&inner),
        tags: tags.iter().copied().collect(),
        filter,
    };
    Ok((layer, Handle { inner }))
}

fn resolve_path(config: &Config) -> Result<(PathBuf, bool), InitError> {
    match &config.lane {
        Lane::Module => Ok((
            config.logs_dir.join(format!("{}.log", config.module_id)),
            true,
        )),
        Lane::Plugin(harness) => Ok((
            config
                .logs_dir
                .join(format!("{}.{harness}.log", config.module_id)),
            true,
        )),
        Lane::Custom(path) if path.is_absolute() => Ok((path.clone(), false)),
        Lane::Custom(path) => Err(InitError::CustomPathNotAbsolute(path.clone())),
    }
}

fn make_filter(spec_override: Option<String>, inner: &LoggerInner) -> EnvFilter {
    let spec = spec_override.or_else(|| env::var("CK_LOG").ok());
    let spec = spec
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    match spec {
        Some(spec) => parse_filter(spec).unwrap_or_else(|error| {
            if !FILTER_FAILURE_REPORTED.swap(true, Ordering::Relaxed) {
                inner.report(&format!(
                    "cortexkit-log: invalid CK_LOG value {spec:?}; using info: {error}\n"
                ));
            }
            EnvFilter::new("info")
        }),
        None => EnvFilter::new("info"),
    }
}

fn parse_filter(spec: &str) -> Result<EnvFilter, String> {
    if spec.split(',').any(|directive| {
        directive
            .rsplit_once('=')
            .is_some_and(|(_, level)| level.trim().is_empty())
    }) {
        return Err("a directive has no level after '='".to_owned());
    }
    EnvFilter::try_new(spec).map_err(|error| error.to_string())
}

fn install_panic_hook(inner: Arc<LoggerInner>) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |information| {
        inner.write_panic(information);
        previous(information);
    }));
}

#[cfg(test)]
mod tests;
