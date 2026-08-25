//! Offline capability-manifest evaluation for `ck fleet lint`.
//!
//! The evaluator deliberately starts only each configured program's `--manifest`
//! mode. It never contacts the daemon, so its findings describe static assembly
//! coherence rather than runtime availability.

use std::{
    collections::{BTreeMap, BTreeSet, HashSet},
    fmt, fs,
    path::Path,
    process::Stdio,
    time::Duration,
};

use serde::{
    de::{self, MapAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::Value;
use subc_protocol::{
    manifest::{validate_manifest_capability_grammar, CapabilityNeed, ModuleManifest},
    PROTOCOL_VERSION,
};
use tokio::{process::Command, time};

use crate::daemon_config::{self, ConfiguredModule};

/// Each manifest probe gets a bounded, non-configurable budget so a broken
/// module cannot make an offline fleet inspection wait forever.
pub const MANIFEST_TIMEOUT: Duration = Duration::from_secs(10);

/// The only per-program operational failures that lint classifies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum OperationalClass {
    ProgramMissing,
    ProgramNotExecutable,
    ManifestTimeout,
    ManifestExitNonzero,
    ManifestUnparsable,
    ManifestVersionUnsupported,
    DuplicateModuleId,
    ManifestInvalid,
}

impl OperationalClass {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ProgramMissing => "program_missing",
            Self::ProgramNotExecutable => "program_not_executable",
            Self::ManifestTimeout => "manifest_timeout",
            Self::ManifestExitNonzero => "manifest_exit_nonzero",
            Self::ManifestUnparsable => "manifest_unparsable",
            Self::ManifestVersionUnsupported => "manifest_version_unsupported",
            Self::DuplicateModuleId => "duplicate_module_id",
            Self::ManifestInvalid => "manifest_invalid",
        }
    }
}

/// Lint's externally meaningful process status.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LintOutcome {
    Clean,
    SemanticViolation,
    OperationalFailure,
}

impl LintOutcome {
    pub const fn exit_code(self) -> i32 {
        match self {
            Self::Clean => 0,
            Self::SemanticViolation => 1,
            Self::OperationalFailure => 2,
        }
    }
}

/// A deterministic, line-oriented lint report.
#[derive(Debug)]
pub struct LintReport {
    pub outcome: LintOutcome,
    pub examined: usize,
    pub configured: usize,
    lines: Vec<String>,
    #[cfg(test)]
    failures: Vec<OperationalFailure>,
}

impl LintReport {
    /// Render the operator-facing report. Newlines are deliberately stable so
    /// callers can use the output in package assembly logs and golden tests.
    pub fn render(&self) -> String {
        self.lines.join("\n")
    }

    #[cfg(test)]
    fn has_failure(&self, class: OperationalClass, module: &str) -> bool {
        self.failures
            .iter()
            .any(|failure| failure.class == class && failure.module == module)
    }
}

#[derive(Debug)]
pub struct LintConfigError(String);

impl fmt::Display for LintConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for LintConfigError {}

#[derive(Debug)]
struct OperationalFailure {
    class: OperationalClass,
    module: String,
}

#[derive(Debug)]
struct ExaminedManifest {
    module_id: String,
    enabled: bool,
    manifest: ModuleManifest,
}

#[derive(Debug)]
struct RequirementLine {
    consumer: String,
    capability: String,
    text: String,
}

/// Evaluate the configured module set without connecting to the daemon.
pub async fn lint(path: impl AsRef<Path>, verbose: bool) -> Result<LintReport, LintConfigError> {
    lint_with_timeout(path.as_ref(), verbose, MANIFEST_TIMEOUT).await
}

async fn lint_with_timeout(
    path: &Path,
    verbose: bool,
    manifest_timeout: Duration,
) -> Result<LintReport, LintConfigError> {
    let duplicate_module_ids = duplicate_module_ids(path)?;
    let config = daemon_config::load(path)
        .map_err(|error| LintConfigError(format!("failed to parse {}: {error}", path.display())))?
        .ok_or_else(|| {
            LintConfigError(format!("daemon config {} does not exist", path.display()))
        })?;

    let mut modules = config.modules.iter().collect::<Vec<_>>();
    modules.sort_by(|left, right| left.module_id.cmp(&right.module_id));
    let mut failures = duplicate_module_ids
        .into_iter()
        .map(|module| OperationalFailure {
            class: OperationalClass::DuplicateModuleId,
            module,
        })
        .collect::<Vec<_>>();
    let mut skipped_daemons = Vec::new();
    let mut examined = Vec::new();

    for module in modules {
        if is_daemon_entry(module) {
            skipped_daemons.push(module.module_id.clone());
            continue;
        }

        match read_manifest(module, manifest_timeout).await {
            Ok(manifest) => examined.push(ExaminedManifest {
                module_id: module.module_id.clone(),
                enabled: module.enabled,
                manifest,
            }),
            Err(class) => failures.push(OperationalFailure {
                class,
                module: module.module_id.clone(),
            }),
        }
    }

    let configured = config
        .modules
        .iter()
        .filter(|module| !is_daemon_entry(module))
        .count();
    let mut lines = vec![format!(
        "examined {} of {configured} configured",
        examined.len()
    )];

    if verbose {
        for module in &skipped_daemons {
            lines.push(format!("verbose: skipped daemon entry {module}"));
        }
    }

    failures.sort_by(|left, right| {
        left.class
            .cmp(&right.class)
            .then_with(|| left.module.cmp(&right.module))
    });
    for failure in &failures {
        lines.push(format!(
            "partial: evaluation incomplete ({}: {})",
            failure.class.as_str(),
            failure.module
        ));
    }
    if examined.is_empty() {
        // The eight named classes classify failed configured programs. An empty
        // set has no program to name, but must still be an operational failure
        // rather than a vacuous clean report.
        lines.push("operational failure: no modules examined (vacuity floor)".to_string());
    }

    lines.push("deny consistency = self-contradiction check".to_string());

    let enabled_providers = capability_claimants(&examined, true);
    let all_providers = capability_claimants(&examined, false);
    let mut has_semantic_violation = false;
    let mut deny_violations = Vec::new();
    let mut requirement_lines = Vec::new();

    for entry in &examined {
        let Some(capabilities) = &entry.manifest.capabilities else {
            continue;
        };
        if entry.enabled {
            for requirement in &capabilities.requires {
                let provided = enabled_providers.contains_key(&requirement.capability);
                match requirement.need {
                    CapabilityNeed::Required => {
                        let text = if provided {
                            format!(
                                "required {} {}: provided",
                                entry.module_id, requirement.capability
                            )
                        } else {
                            let text = format!(
                                "required {} {}: no enabled provider",
                                entry.module_id, requirement.capability
                            );
                            has_semantic_violation = true;
                            text
                        };
                        requirement_lines.push(RequirementLine {
                            consumer: entry.module_id.clone(),
                            capability: requirement.capability.clone(),
                            text,
                        });
                    }
                    CapabilityNeed::Optional if verbose && !provided => {
                        requirement_lines.push(RequirementLine {
                            consumer: entry.module_id.clone(),
                            capability: requirement.capability.clone(),
                            text: format!(
                                "optional {}: no provider (consumer degrades, by declaration)",
                                requirement.capability
                            ),
                        });
                    }
                    CapabilityNeed::Optional => {}
                }
            }
        }

        let denied = capabilities
            .must_never_reach
            .iter()
            .collect::<BTreeSet<_>>();
        for requirement in &capabilities.requires {
            if denied.contains(&requirement.capability) {
                has_semantic_violation = true;
                deny_violations.push(format!(
                    "requires_deny_conflict module={} capability={}",
                    entry.module_id, requirement.capability
                ));
            }
        }
    }

    requirement_lines.sort_by(|left, right| {
        left.consumer
            .cmp(&right.consumer)
            .then_with(|| left.capability.cmp(&right.capability))
    });
    lines.extend(requirement_lines.into_iter().map(|line| line.text));

    deny_violations.sort();
    deny_violations.dedup();
    lines.extend(deny_violations);

    let mut reserved_lines = Vec::new();
    let mut reserved_violation = false;
    for (capability, bound_module) in &config.reserved_capabilities {
        let claimants = all_providers.get(capability);
        match claimants {
            None => reserved_lines.push(format!(
                "warning: reserved capability {capability} has no configured claimant for {bound_module}"
            )),
            Some(claimants) => {
                for claimant in claimants {
                    if claimant != bound_module {
                        reserved_violation = true;
                        reserved_lines.push(format!(
                            "reserved capability {capability}: claimant {claimant} conflicts with binding {bound_module}"
                        ));
                    }
                }
            }
        }
    }
    lines.extend(reserved_lines);

    let mut disabled_notes = Vec::new();
    for entry in &examined {
        if entry.enabled {
            continue;
        }
        let Some(capabilities) = &entry.manifest.capabilities else {
            continue;
        };
        for capability in &capabilities.provides {
            if !enabled_providers.contains_key(capability) {
                disabled_notes.push(format!(
                    "note: {} (disabled) claims {capability}",
                    entry.module_id
                ));
            }
        }
    }
    disabled_notes.sort();
    disabled_notes.dedup();
    lines.extend(disabled_notes);

    let outcome = if !failures.is_empty() || examined.is_empty() {
        LintOutcome::OperationalFailure
    } else if has_semantic_violation || reserved_violation {
        LintOutcome::SemanticViolation
    } else {
        LintOutcome::Clean
    };

    Ok(LintReport {
        outcome,
        examined: examined.len(),
        configured,
        lines,
        #[cfg(test)]
        failures,
    })
}

fn capability_claimants(
    examined: &[ExaminedManifest],
    enabled_only: bool,
) -> BTreeMap<String, BTreeSet<String>> {
    let mut claims = BTreeMap::<String, BTreeSet<String>>::new();
    for entry in examined {
        if enabled_only && !entry.enabled {
            continue;
        }
        let Some(capabilities) = &entry.manifest.capabilities else {
            continue;
        };
        for capability in &capabilities.provides {
            claims
                .entry(capability.clone())
                .or_default()
                .insert(entry.module_id.clone());
        }
    }
    claims
}

fn is_daemon_entry(module: &ConfiguredModule) -> bool {
    module
        .program
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "ck-subc" | "ck-subc.exe"))
}

async fn read_manifest(
    module: &ConfiguredModule,
    manifest_timeout: Duration,
) -> Result<ModuleManifest, OperationalClass> {
    let metadata = fs::metadata(&module.program).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            OperationalClass::ProgramMissing
        } else {
            OperationalClass::ProgramNotExecutable
        }
    })?;
    if !is_executable_file(&metadata) {
        return Err(OperationalClass::ProgramNotExecutable);
    }

    let mut command = Command::new(&module.program);
    command
        .arg("--manifest")
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = match time::timeout(manifest_timeout, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(_)) => return Err(OperationalClass::ProgramNotExecutable),
        Err(_) => return Err(OperationalClass::ManifestTimeout),
    };
    if !output.status.success() {
        return Err(OperationalClass::ManifestExitNonzero);
    }

    let value: Value =
        serde_json::from_slice(&output.stdout).map_err(|_| OperationalClass::ManifestUnparsable)?;
    validate_manifest_capability_grammar(&value).map_err(|_| OperationalClass::ManifestInvalid)?;
    let manifest: ModuleManifest =
        serde_json::from_value(value).map_err(|_| OperationalClass::ManifestUnparsable)?;
    if manifest.protocol_ver != PROTOCOL_VERSION {
        return Err(OperationalClass::ManifestVersionUnsupported);
    }
    Ok(manifest)
}

#[cfg(unix)]
fn is_executable_file(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file()
}

fn duplicate_module_ids(path: &Path) -> Result<Vec<String>, LintConfigError> {
    let document = fs::read_to_string(path)
        .map_err(|error| LintConfigError(format!("failed to read {}: {error}", path.display())))?;
    let json = subc_jsonc::jsonc_to_json(&document)
        .map_err(|error| LintConfigError(format!("failed to parse {}: {error}", path.display())))?;
    let probe: ModuleIdProbe = serde_json::from_str(&json)
        .map_err(|error| LintConfigError(format!("failed to parse {}: {error}", path.display())))?;
    Ok(probe.modules)
}

#[derive(Deserialize)]
struct ModuleIdProbe {
    #[serde(default, deserialize_with = "deserialize_module_ids")]
    modules: Vec<String>,
}

fn deserialize_module_ids<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct ModuleIdsVisitor;

    impl<'de> Visitor<'de> for ModuleIdsVisitor {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("an object keyed by module id")
        }

        fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
        where
            M: MapAccess<'de>,
        {
            let mut duplicates = Vec::new();
            let mut seen = HashSet::new();
            while let Some(module_id) = map.next_key::<String>()? {
                if !seen.insert(module_id.clone()) {
                    duplicates.push(module_id);
                }
                map.next_value::<de::IgnoredAny>()?;
            }
            Ok(duplicates)
        }
    }

    deserializer.deserialize_map(ModuleIdsVisitor)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime, UNIX_EPOCH},
    };

    use serde_json::{json, Map, Value};
    use subc_protocol::PROTOCOL_VERSION;

    use super::{lint_with_timeout, LintOutcome, OperationalClass, MANIFEST_TIMEOUT};

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(name: &str) -> Self {
            let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let stamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("subc-fleet-lint-{name}-{stamp}-{nonce}"));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[derive(serde::Serialize)]
    struct FixtureSpec {
        stdout: String,
        exit_code: i32,
        sleep_ms: u64,
        #[serde(skip)]
        executable: bool,
    }

    impl Default for FixtureSpec {
        fn default() -> Self {
            Self {
                stdout: String::new(),
                exit_code: 0,
                sleep_ms: 0,
                executable: true,
            }
        }
    }

    /// Mirrors `control.rs::fake_aft_stub_path`: library tests have no
    /// `CARGO_BIN_EXE_*`, so the stub is the sibling two directories above the
    /// test executable. Keep the existence panic and its remedy: `--lib` does
    /// not build this binary, while `cargo test -p subc-core` does.
    fn fake_aft_stub_path() -> PathBuf {
        let mut path = std::env::current_exe().expect("current_exe available in tests");
        path.pop(); // .../deps/
        path.pop(); // .../<profile>/
        path.push(if cfg!(windows) {
            "fake-aft-stub.exe"
        } else {
            "fake-aft-stub"
        });
        assert!(
            path.exists(),
            "fake-aft-stub not built at {}: run `cargo test -p subc-core` (which builds [[bin]] targets) rather than `cargo test -p subc-core --lib` (which does not)",
            path.display()
        );
        path
    }

    fn write_fixture_program(temp: &TempDir, name: &str, fixture: FixtureSpec) -> PathBuf {
        let filename = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        let path = temp.path().join(filename);
        fs::copy(fake_aft_stub_path(), &path).unwrap();

        // Per-temp-dir sidecars keep parallel tests isolated without environment
        // variables, which are process-global in this multi-threaded test binary.
        let mut sidecar = path.as_os_str().to_os_string();
        sidecar.push(".fixture.json");
        fs::write(
            PathBuf::from(sidecar),
            serde_json::to_vec(&fixture).unwrap(),
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(
                &path,
                fs::Permissions::from_mode(if fixture.executable { 0o755 } else { 0o644 }),
            )
            .unwrap();
        }
        // Windows has no executable bit. The copied `.exe` is spawnable there,
        // so this flag only changes the unix permission check.
        #[cfg(not(unix))]
        let _ = fixture.executable;
        path
    }

    #[test]
    fn fixture_sidecar_absent_preserves_existing_stub_behavior() {
        let output = Command::new(fake_aft_stub_path())
            .env("FAKE_AFT_EXIT_CODE", "17")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(17));
    }

    fn manifest(module_id: &str, capabilities: Value, protocol_ver: u8) -> String {
        json!({
            "module_id": module_id,
            "module_version": "0.1.0",
            "protocol_ver": protocol_ver,
            "trust_tier": "first_party",
            "provides": [],
            "consumes": [],
            "bindings": {
                "storage": {"kind": "sqlite", "scope": "project", "owns_schema": false},
                "vault_grants": [],
                "identity": {"requires": [], "optional": []}
            },
            "capabilities": capabilities,
            "runtime_computed": []
        })
        .to_string()
    }

    fn manifest_fixture(temp: &TempDir, module_id: &str, capabilities: Value) -> PathBuf {
        let document = manifest(module_id, capabilities, PROTOCOL_VERSION);
        write_fixture_program(
            temp,
            module_id,
            FixtureSpec {
                stdout: document,
                ..FixtureSpec::default()
            },
        )
    }

    fn write_config(
        temp: &TempDir,
        modules: Vec<(&str, &Path, bool)>,
        reserved_capabilities: Value,
    ) -> PathBuf {
        let mut entries = Map::new();
        for (module_id, program, enabled) in modules {
            entries.insert(
                module_id.to_string(),
                json!({"program": program, "enabled": enabled}),
            );
        }
        let path = temp.path().join("subc.jsonc");
        fs::write(
            &path,
            json!({
                "version": 1,
                "modules": entries,
                "reserved_capabilities": reserved_capabilities
            })
            .to_string(),
        )
        .unwrap();
        path
    }

    async fn lint_config(path: &Path, verbose: bool) -> super::LintReport {
        // Fixture processes are intentionally tiny; a long test-only budget keeps
        // concurrent CI scheduling from masquerading as the production 10s class.
        lint_with_timeout(path, verbose, Duration::from_secs(60))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn fixture_program_missing_classifies_operational_failure() {
        let temp = TempDir::new("program-missing");
        let config = write_config(
            &temp,
            vec![("missing", &temp.path().join("missing"), true)],
            json!({}),
        );

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(report.has_failure(OperationalClass::ProgramMissing, "missing"));
    }

    #[tokio::test]
    async fn fixture_program_not_executable_classifies_operational_failure() {
        let temp = TempDir::new("program-not-executable");
        let script = write_fixture_program(
            &temp,
            "not-executable",
            FixtureSpec {
                executable: false,
                ..FixtureSpec::default()
            },
        );
        let config = write_config(&temp, vec![("not-executable", &script, true)], json!({}));

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        #[cfg(unix)]
        assert!(report.has_failure(OperationalClass::ProgramNotExecutable, "not-executable"));
        #[cfg(not(unix))]
        {
            // Windows has no executable permission bit, so the copied `.exe`
            // spawns successfully and its empty stdout is classified instead.
            assert!(report.has_failure(OperationalClass::ManifestUnparsable, "not-executable"));
        }
    }

    #[tokio::test]
    async fn fixture_manifest_timeout_classifies_operational_failure() {
        let temp = TempDir::new("manifest-timeout");
        let script = write_fixture_program(
            &temp,
            "timeout",
            FixtureSpec {
                sleep_ms: (MANIFEST_TIMEOUT + Duration::from_secs(1)).as_millis() as u64,
                ..FixtureSpec::default()
            },
        );
        let config = write_config(&temp, vec![("timeout", &script, true)], json!({}));

        let report = lint_with_timeout(&config, false, Duration::from_millis(5))
            .await
            .unwrap();
        assert_eq!(MANIFEST_TIMEOUT, Duration::from_secs(10));
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(report.has_failure(OperationalClass::ManifestTimeout, "timeout"));
    }

    #[tokio::test]
    async fn fixture_manifest_exit_nonzero_classifies_operational_failure() {
        let temp = TempDir::new("manifest-exit-nonzero");
        let script = write_fixture_program(
            &temp,
            "nonzero",
            FixtureSpec {
                exit_code: 7,
                ..FixtureSpec::default()
            },
        );
        let config = write_config(&temp, vec![("nonzero", &script, true)], json!({}));

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(report.has_failure(OperationalClass::ManifestExitNonzero, "nonzero"));
    }

    #[tokio::test]
    async fn fixture_manifest_unparsable_classifies_operational_failure() {
        let temp = TempDir::new("manifest-unparsable");
        let script = write_fixture_program(
            &temp,
            "unparsable",
            FixtureSpec {
                stdout: "not json\\n".to_string(),
                ..FixtureSpec::default()
            },
        );
        let config = write_config(&temp, vec![("unparsable", &script, true)], json!({}));

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(
            report.has_failure(OperationalClass::ManifestUnparsable, "unparsable"),
            "report:\n{}",
            report.render()
        );
    }

    #[tokio::test]
    async fn fixture_manifest_version_unsupported_classifies_operational_failure() {
        let temp = TempDir::new("manifest-version-unsupported");
        let document = manifest(
            "unsupported",
            json!({"provides": [], "requires": [], "must_never_reach": []}),
            PROTOCOL_VERSION.saturating_add(1),
        );
        let script = write_fixture_program(
            &temp,
            "unsupported",
            FixtureSpec {
                stdout: document,
                ..FixtureSpec::default()
            },
        );
        let config = write_config(&temp, vec![("unsupported", &script, true)], json!({}));

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(
            report.has_failure(OperationalClass::ManifestVersionUnsupported, "unsupported"),
            "report:\n{}",
            report.render()
        );
    }

    #[tokio::test]
    async fn fixture_duplicate_module_id_classifies_operational_failure() {
        let temp = TempDir::new("duplicate-module-id");
        let script = manifest_fixture(&temp, "duplicate", Value::Null);
        let config = temp.path().join("subc.jsonc");
        // Hand-written JSON because serde_json cannot emit the duplicate key
        // this test exists to exercise -- but the PATH must still be a valid
        // JSON string: on Windows `display()` yields backslashes, which are
        // invalid JSON escapes and fail the parse before the duplicate-id
        // check ever runs. serde-encode the path (quotes included) instead.
        let program = serde_json::to_string(&script.display().to_string()).unwrap();
        fs::write(
            &config,
            format!(
                r#"{{"version":1,"modules":{{"duplicate":{{"program":{program}}},"duplicate":{{"program":{program}}}}}}}"#
            ),
        )
        .unwrap();

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(report.has_failure(OperationalClass::DuplicateModuleId, "duplicate"));
    }

    #[tokio::test]
    async fn fixture_manifest_invalid_classifies_operational_failure() {
        let temp = TempDir::new("manifest-invalid");
        let script = manifest_fixture(
            &temp,
            "invalid",
            json!({"provides": ["Not-valid/v1"], "requires": [], "must_never_reach": []}),
        );
        let config = write_config(&temp, vec![("invalid", &script, true)], json!({}));

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(report.has_failure(OperationalClass::ManifestInvalid, "invalid"));
    }

    #[tokio::test]
    async fn disabled_modules_are_still_manifest_validated() {
        let temp = TempDir::new("disabled-manifest-invalid");
        let script = manifest_fixture(
            &temp,
            "disabled-invalid",
            json!({"provides": ["Not-valid/v1"], "requires": [], "must_never_reach": []}),
        );
        let config = write_config(&temp, vec![("disabled-invalid", &script, false)], json!({}));

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(report.has_failure(OperationalClass::ManifestInvalid, "disabled-invalid"));
    }

    #[tokio::test]
    async fn golden_disabled_claimant_count_daemon_skip_and_verbose_optional_inventory() {
        let temp = TempDir::new("disabled-claimant");
        let consumer = manifest_fixture(
            &temp,
            "consumer",
            json!({
                "provides": [],
                "requires": [
                    {"capability": "credentials-provider/v1", "need": "required"},
                    {"capability": "context-transform/v1", "need": "optional"}
                ],
                "must_never_reach": []
            }),
        );
        let disabled = manifest_fixture(
            &temp,
            "disabled",
            json!({"provides": ["credentials-provider/v1"], "requires": [], "must_never_reach": []}),
        );
        let daemon = temp.path().join("ck-subc");
        let config = write_config(
            &temp,
            vec![
                ("daemon", &daemon, true),
                ("consumer", &consumer, true),
                ("disabled", &disabled, false),
            ],
            json!({}),
        );

        let report = lint_config(&config, true).await;
        assert_eq!(report.outcome, LintOutcome::SemanticViolation);
        assert_eq!(report.examined, 2);
        assert_eq!(report.configured, 2);
        assert_eq!(
            report.render(),
            "examined 2 of 2 configured\n\
verbose: skipped daemon entry daemon\n\
deny consistency = self-contradiction check\n\
optional context-transform/v1: no provider (consumer degrades, by declaration)\n\
required consumer credentials-provider/v1: no enabled provider\n\
note: disabled (disabled) claims credentials-provider/v1"
        );
        let default_report = lint_config(&config, false).await;
        assert!(
            !default_report
                .render()
                .contains("optional context-transform/v1"),
            "default report must not style declared optional degradation as a warning:\n{}",
            default_report.render()
        );
    }

    #[tokio::test]
    async fn golden_requirement_lines_sort_by_consumer_then_capability() {
        let temp = TempDir::new("requirement-order");
        let alpha = manifest_fixture(
            &temp,
            "alpha",
            json!({"provides": [], "requires": [{"capability": "alpha/v1", "need": "required"}], "must_never_reach": []}),
        );
        let zeta = manifest_fixture(
            &temp,
            "zeta",
            json!({"provides": [], "requires": [{"capability": "zeta/v1", "need": "required"}], "must_never_reach": []}),
        );
        let config = write_config(
            &temp,
            vec![("zeta", &zeta, true), ("alpha", &alpha, true)],
            json!({}),
        );

        let report = lint_config(&config, false).await;
        let rendered = report.render();
        assert!(
            rendered.find("required alpha alpha/v1").unwrap()
                < rendered.find("required zeta zeta/v1").unwrap(),
            "report:\n{rendered}"
        );
    }

    #[tokio::test]
    async fn deny_self_contradiction_mutation_proof_requires_overlap() {
        let temp = TempDir::new("deny-self-contradiction");
        let self_contradiction = manifest_fixture(
            &temp,
            "contradictory",
            json!({
                "provides": [],
                "requires": [{"capability": "credentials-provider/v1", "need": "required"}],
                "must_never_reach": ["credentials-provider/v1"]
            }),
        );
        let config = write_config(
            &temp,
            vec![("contradictory", &self_contradiction, true)],
            json!({}),
        );

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::SemanticViolation);
        assert!(report
            .render()
            .contains("deny consistency = self-contradiction check"));
        assert!(report.render().contains(
            "requires_deny_conflict module=contradictory capability=credentials-provider/v1"
        ));
    }

    #[tokio::test]
    async fn operational_failure_overrides_semantic_exit_classification() {
        let temp = TempDir::new("operational-trump");
        let consumer = manifest_fixture(
            &temp,
            "consumer",
            json!({"provides": [], "requires": [{"capability": "credentials-provider/v1", "need": "required"}], "must_never_reach": []}),
        );
        let broken = write_fixture_program(
            &temp,
            "broken",
            FixtureSpec {
                exit_code: 1,
                ..FixtureSpec::default()
            },
        );
        let config = write_config(
            &temp,
            vec![("consumer", &consumer, true), ("broken", &broken, true)],
            json!({}),
        );

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert!(
            report
                .render()
                .contains("partial: evaluation incomplete (manifest_exit_nonzero: broken)"),
            "report:\n{}",
            report.render()
        );
        assert!(report
            .render()
            .contains("required consumer credentials-provider/v1: no enabled provider"));
    }

    #[tokio::test]
    async fn zero_examined_is_an_operational_failure_not_a_vacuous_pass() {
        let temp = TempDir::new("vacuity-floor");
        let config = write_config(&temp, Vec::new(), json!({}));

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::OperationalFailure);
        assert_eq!(report.render(), "examined 0 of 0 configured\noperational failure: no modules examined (vacuity floor)\ndeny consistency = self-contradiction check");
    }

    #[tokio::test]
    async fn reserved_bindings_warn_when_unclaimed_and_fail_for_conflicting_claimants() {
        let temp = TempDir::new("reserved-bindings");
        let claimant = manifest_fixture(
            &temp,
            "other",
            json!({"provides": ["credentials-provider/v1"], "requires": [], "must_never_reach": []}),
        );
        let config = write_config(
            &temp,
            vec![("other", &claimant, true)],
            json!({
                "credentials-provider/v1": "bound",
                "context-transform/v1": "not-installed"
            }),
        );

        let report = lint_config(&config, false).await;
        assert_eq!(report.outcome, LintOutcome::SemanticViolation);
        let rendered = report.render();
        assert!(rendered.contains(
            "reserved capability credentials-provider/v1: claimant other conflicts with binding bound"
        ));
        assert!(rendered.contains(
            "warning: reserved capability context-transform/v1 has no configured claimant for not-installed"
        ));
    }
}
