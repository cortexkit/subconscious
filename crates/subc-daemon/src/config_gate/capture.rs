//! Bounded, handle-bound proposal capture. Raw bytes are ephemeral until review.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use subc_config::{InputState, Layer, LayerValue, RecordedValue, Schema, ValidationContext};
use subc_protocol::config::{
    ConfigDeclareRequest, ConfigLayer, ConfigLayerSnapshot, ConfigStatus, ConfigView,
};

use super::{CapturedInput, GateError, GateSnapshot, InputKey, Result};

// A configuration document is small control-plane input, not an unbounded stream.
/// Bound untrusted JSONC reads and parsing to 1 MiB per document.
const CONFIG_FILE_LIMIT: u64 = 1024 * 1024;
/// Bound streamed executable hashing so a proposal cannot demand an endless scan.
const PROGRAM_FILE_LIMIT: u64 = 512 * 1024 * 1024;

/// A file-object id is distinct from its bytes and from its pathname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum FileObjectId {
    Unix {
        #[serde(with = "decimal_id")]
        device: u64,
        #[serde(with = "decimal_id")]
        inode: u64,
    },
    Windows {
        #[serde(with = "decimal_id")]
        volume_serial_number: u64,
        file_id: [u8; 16],
    },
}

mod decimal_id {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(
        value: &u64,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<u64, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

fn file_object_id(file: &File) -> Result<FileObjectId> {
    let id = subc_os::retained_file_identity(file).map_err(capture_error)?;
    #[cfg(unix)]
    return Ok(FileObjectId::Unix {
        device: id.device,
        inode: id.inode,
    });
    #[cfg(windows)]
    return Ok(FileObjectId::Windows {
        volume_serial_number: id.volume_serial_number,
        file_id: id.file_id,
    });
}

/// The planning adapter must supply a complete authoritative revision, never
/// roots proposed by a module or a caller's current working directory.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryProject {
    pub project_id: String,
    pub workspace_id: Option<String>,
    pub project_root: PathBuf,
    pub project_identity: FileObjectId,
    pub workspace_root: Option<PathBuf>,
    pub workspace_identity: Option<FileObjectId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryRevision {
    pub revision: u64,
    pub projects: Vec<RegistryProject>,
}

pub trait ProjectRegistry: Send + Sync {
    fn snapshot(&self) -> Result<RegistryRevision>;
}

/// Delivery needs no registry connection. Only a new project-layer plan needs
/// this adapter, which refuses until an authoritative registry reader is wired.
pub struct UnavailableProjectRegistry;
impl ProjectRegistry for UnavailableProjectRegistry {
    fn snapshot(&self) -> Result<RegistryRevision> {
        Err(GateError::new(
            "project_registry_unavailable",
            "project/workspace registry is unavailable",
        ))
    }
}

pub struct ConfigCapturer {
    pub daemon_path: PathBuf,
    pub config_home: PathBuf,
    pub projects: Arc<dyn ProjectRegistry>,
}

/// The handle remains open until capture/validation completes. It is never
/// serialized: only its file identity, byte digest and redacted values are.
pub struct CapturedFile {
    _file: Option<File>,
    pub path: PathBuf,
    pub bytes: Option<Vec<u8>>,
    pub state: InputState,
}

fn capture_error(_: impl std::fmt::Display) -> GateError {
    // OS/parser errors can echo submitted text. Do not put it in a retained view.
    GateError::new(
        "input_unavailable",
        "configuration input could not be captured",
    )
}

impl CapturedFile {
    pub fn open(path: &Path) -> Result<Self> {
        if !path.is_absolute() {
            return Err(GateError::new(
                "path_invalid",
                "configuration input must be absolute",
            ));
        }
        let before = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Self {
                    _file: None,
                    path: path.into(),
                    bytes: None,
                    state: InputState::Absent,
                });
            }
            Err(error) => return Err(capture_error(error)),
        };
        if !before.is_file() || before.file_type().is_symlink() {
            return Err(GateError::new(
                "path_invalid",
                "configuration input must be a regular non-symlink file",
            ));
        }
        let file = subc_os::open_regular_nofollow(path).map_err(capture_error)?;
        let metadata = file.metadata().map_err(capture_error)?;
        if !metadata.is_file() || metadata.len() > CONFIG_FILE_LIMIT {
            return Err(GateError::new(
                "input_too_large",
                "configuration document exceeds the 1 MiB JSONC limit",
            ));
        }
        let id = file_object_id(&file)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        (&file)
            .take(CONFIG_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(capture_error)?;
        let after = file.metadata().map_err(capture_error)?;
        let supplied = std::fs::symlink_metadata(path).map_err(capture_error)?;
        let reopened = subc_os::open_regular_nofollow(path).map_err(capture_error)?;
        if bytes.len() as u64 > CONFIG_FILE_LIMIT {
            return Err(GateError::new(
                "input_too_large",
                "configuration document exceeds the 1 MiB JSONC limit",
            ));
        }
        if bytes.len() as u64 != metadata.len()
            || after.len() != metadata.len()
            || after.modified().ok() != metadata.modified().ok()
            || !supplied.is_file()
            || supplied.file_type().is_symlink()
            || file_object_id(&reopened)? != id
        {
            return Err(GateError::stale());
        }
        let digest = subc_config::raw_digest(&serde_json::to_vec(&json!({
            "file": id, "raw": subc_config::raw_digest(&bytes), "length": metadata.len(),
            "modified": metadata.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| (d.as_secs(), d.subsec_nanos())),
        })).map_err(GateError::invalid)?);
        Ok(Self {
            _file: Some(file),
            path: path.into(),
            bytes: Some(bytes),
            state: InputState::Present { digest },
        })
    }

    fn parse(&self) -> Result<Value> {
        match &self.bytes {
            Some(bytes) => {
                let text = std::str::from_utf8(bytes).map_err(capture_error)?;
                let normalized = subc_jsonc::jsonc_to_json(text).map_err(capture_error)?;
                super::strict_json::parse(&normalized)
            }
            None => Ok(json!({})),
        }
    }
}

pub struct CapturedModule {
    pub declaration: ConfigDeclareRequest,
    pub schema: Schema,
    pub view: ConfigView,
    pub file: CapturedFile,
}

pub struct CapturedConfiguration {
    pub diagnostic: GateSnapshot,
    pub daemon: CapturedFile,
    pub modules: BTreeMap<String, CapturedModule>,
    _program_handles: Vec<File>,
}

impl CapturedConfiguration {
    pub(crate) fn take_handles(&mut self) -> Vec<File> {
        let mut handles = std::mem::take(&mut self._program_handles);
        handles.extend(self.daemon._file.take());
        for module in self.modules.values_mut() {
            handles.extend(module.file._file.take());
        }
        handles
    }

    /// A rollback may restore a path different from the current configuration's
    /// path. Bind both programs in the observed read set without restoring any
    /// historical executable bytes or trusting a historical file identity.
    pub(crate) fn capture_programs(&mut self, keys: &[InputKey]) -> Result<()> {
        for key in keys {
            if self.diagnostic.inputs.contains_key(key) {
                continue;
            }
            let InputKey::Program { path, .. } = key else {
                unreachable!()
            };
            let (file, captured) = capture_program(Path::new(path))?;
            self.diagnostic.inputs.insert(key.clone(), captured);
            self._program_handles.push(file);
        }
        Ok(())
    }
}

impl ConfigCapturer {
    pub fn capture(
        &self,
        declarations: &BTreeMap<String, ConfigDeclareRequest>,
    ) -> Result<CapturedConfiguration> {
        self.capture_selected(declarations, None)
    }

    pub fn capture_modules(
        &self,
        declarations: &BTreeMap<String, ConfigDeclareRequest>,
        modules: &BTreeSet<String>,
    ) -> Result<CapturedConfiguration> {
        self.capture_selected(declarations, Some(modules))
    }

    fn capture_selected(
        &self,
        declarations: &BTreeMap<String, ConfigDeclareRequest>,
        selected: Option<&BTreeSet<String>>,
    ) -> Result<CapturedConfiguration> {
        let daemon = CapturedFile::open(&self.daemon_path)?;
        self.capture_layers(daemon, declarations, selected, CapturedFile::open)
    }

    /// Re-evaluate saved raw configuration rather than today's edited files.
    /// Program capture still opens the files at the restored configured paths.
    pub(crate) fn capture_retained(
        &self,
        target: &GateSnapshot,
        delivery: &crate::config_store::DeliverySnapshot,
    ) -> Result<CapturedConfiguration> {
        let invalid = || {
            GateError::new(
                "rollback_snapshot_unavailable",
                "retained raw configuration is absent or inconsistent",
            )
        };
        let retained_file = |layer: &ConfigLayerSnapshot, key: &InputKey| -> Result<CapturedFile> {
            let path = layer.path.as_ref().ok_or_else(invalid)?;
            let input = target.inputs.get(key).ok_or_else(invalid)?;
            if layer.bytes.as_deref().map(subc_config::raw_digest) != layer.digest {
                return Err(invalid());
            }
            Ok(CapturedFile {
                path: path.into(),
                bytes: layer.bytes.clone(),
                state: input.state.clone(),
                _file: None,
            })
        };
        let daemon = retained_file(
            delivery.daemon.as_ref().ok_or_else(invalid)?,
            &InputKey::Daemon,
        )?;
        if daemon.path != self.daemon_path || daemon.bytes.is_none() {
            return Err(invalid());
        }
        let mut files = BTreeMap::new();
        for (module, view) in &delivery.modules {
            let [layer] = view.layers.as_slice() else {
                return Err(invalid());
            };
            if layer.layer != ConfigLayer::User
                || layer.module_id != *module
                || layer.project_id.is_some()
                || layer.workspace_id.is_some()
            {
                return Err(invalid());
            }
            let path = layer.path.as_ref().ok_or_else(invalid)?;
            let file = retained_file(
                layer,
                &InputKey::Layer {
                    module: module.clone(),
                    path: path.clone(),
                },
            )?;
            files.insert(file.path.clone(), file);
        }
        let captured = self.capture_layers(daemon, &delivery.declarations, None, |path| {
            files.remove(path).ok_or_else(invalid)
        })?;
        if !files.is_empty()
            || target
                .inputs
                .iter()
                .filter(|(key, _)| !key.is_program())
                .ne(captured
                    .diagnostic
                    .inputs
                    .iter()
                    .filter(|(key, _)| !key.is_program()))
        {
            return Err(invalid());
        }
        Ok(captured)
    }

    fn capture_layers(
        &self,
        daemon: CapturedFile,
        declarations: &BTreeMap<String, ConfigDeclareRequest>,
        selected: Option<&BTreeSet<String>>,
        mut layer_file: impl FnMut(&Path) -> Result<CapturedFile>,
    ) -> Result<CapturedConfiguration> {
        let doc = daemon.parse()?;
        let mut launch_specs = BTreeMap::new();
        if let Some(bytes) = &daemon.bytes {
            let (config, _) = crate::daemon_config::parse_with_activity(
                std::str::from_utf8(bytes).map_err(capture_error)?,
                &self.daemon_path,
            )
            .map_err(capture_error)?;
            launch_specs.extend(
                config
                    .modules
                    .into_iter()
                    .map(|module| (module.module_id.clone(), module.module_spec())),
            );
            refuse_expansions(&doc, "subc", "")?;
        }
        let mut program_handles = Vec::new();
        // Launch arguments and environment participate in review verbatim;
        // credential reference objects remain redacted in diagnostic views.
        let safe_doc = redact_launch_values(&doc)?;
        let mut diagnostic = GateSnapshot::default();
        diagnostic.inputs.insert(
            InputKey::Daemon,
            CapturedInput {
                state: daemon.state.clone(),
                value: RecordedValue::new(safe_doc.clone()),
            },
        );
        if let Some(entries) = safe_doc.get("modules").and_then(Value::as_object) {
            for (module, entry) in entries {
                diagnostic.inputs.insert(
                    InputKey::Entry(module.clone()),
                    CapturedInput {
                        state: InputState::Present {
                            digest: subc_config::raw_digest(
                                &serde_json::to_vec(entry).map_err(GateError::invalid)?,
                            ),
                        },
                        value: RecordedValue::new(entry.clone()),
                    },
                );
            }
        }
        if let Some(entries) = doc.get("modules").and_then(Value::as_object) {
            for (module, entry) in entries {
                if selected.is_some_and(|modules| !modules.contains(module)) {
                    continue;
                }
                if let Some(spec) = launch_specs.get(module) {
                    let covered = super::program::program_files(entry)?;
                    let launch = super::program::capture(spec, &covered).map_err(|error| {
                        GateError::new(&error.code, format!("{module}: {}", error.detail))
                    })?;
                    diagnostic.inputs.extend(launch.inputs);
                    program_handles.extend(launch.files);
                }
            }
        }
        let mut modules = BTreeMap::new();
        for (module, declaration) in declarations {
            if selected.is_some_and(|modules| !modules.contains(module)) {
                continue;
            }
            if !declaration.project_dirs.is_empty() {
                // Module-authored directories never become registry roots.
                self.projects.snapshot().map_err(|_| {
                    GateError::new(
                        "project_registry_unavailable",
                        format!("{module}: project/workspace layer registry is unavailable"),
                    )
                })?;
                return Err(GateError::new(
                    "project_registry_unavailable",
                    format!("{module}: project/workspace layer capture is unavailable"),
                ));
            }
            if module.is_empty() || module.contains(['/', '\\']) || module == "." || module == ".."
            {
                return Err(GateError::new(
                    "path_invalid",
                    "module identity cannot form a layer path",
                ));
            }
            let expected = self
                .config_home
                .join("cortexkit")
                .join(format!("{module}.jsonc"));
            if Path::new(&declaration.user_file) != expected || !expected.is_absolute() {
                return Err(GateError::new(
                    "path_invalid",
                    format!("{module}: user layer must use its canonical config-home path"),
                ));
            }
            let schema = Schema::new(declaration.schema.clone()).map_err(|refusal| {
                GateError::new("schema_invalid", format!("{module}: {}", refusal.path))
            })?;
            let file = layer_file(&expected)?;
            let value = file.parse()?;
            refuse_expansions(&value, module, "")?;
            // Record even invalid literals through the schema projection first;
            // nothing raw is retained on the refusal path.
            let recorded = schema.record(&value).map_err(GateError::invalid)?;
            let layers = if file.bytes.is_some() {
                vec![LayerValue::new(Layer::User, value)]
            } else {
                Vec::new()
            };
            let reviewed = schema
                .review_tuple(&layers, &ValidationContext::new(module, None))
                .map_err(|refusal| {
                    GateError::new(
                        refusal_code(refusal.code),
                        format!("{module}: {}", refusal.path),
                    )
                })?;
            let layer = ConfigLayerSnapshot::file(
                module,
                ConfigLayer::User,
                expected.to_string_lossy(),
                file.bytes.clone(),
            );
            let view = ConfigView::new(
                0,
                reviewed.effective,
                reviewed.sources,
                vec![layer],
                ConfigStatus::Unstamped,
            );
            diagnostic.inputs.insert(
                InputKey::Layer {
                    module: module.clone(),
                    path: expected.to_string_lossy().into_owned(),
                },
                CapturedInput {
                    state: file.state.clone(),
                    value: RecordedValue::new(recorded),
                },
            );
            let decl = serde_json::to_value(declaration).map_err(GateError::invalid)?;
            diagnostic.inputs.insert(
                InputKey::Declaration(module.clone()),
                CapturedInput {
                    state: InputState::Present {
                        digest: subc_config::digest(subc_config::PURPOSE_DECLARATION, &decl)
                            .map_err(GateError::invalid)?,
                    },
                    value: RecordedValue::new(redact_launch_values(&decl)?),
                },
            );
            modules.insert(
                module.clone(),
                CapturedModule {
                    declaration: declaration.clone(),
                    schema,
                    view,
                    file,
                },
            );
        }
        Ok(CapturedConfiguration {
            diagnostic,
            daemon,
            modules,
            _program_handles: program_handles,
        })
    }
}

fn redact_launch_values(value: &Value) -> Result<Value> {
    if subc_config::is_vault_reference(value) {
        return serde_json::to_value(RecordedValue::redacted(value).map_err(GateError::invalid)?)
            .map_err(GateError::invalid);
    }
    match value {
        Value::Object(object) => object
            .iter()
            .map(|(key, value)| {
                let value = redact_launch_values(value)?;
                Ok((key.clone(), value))
            })
            .collect::<Result<serde_json::Map<_, _>>>()
            .map(Value::Object),
        Value::Array(array) => array
            .iter()
            .map(redact_launch_values)
            .collect::<Result<Vec<_>>>()
            .map(Value::Array),
        value => Ok(value.clone()),
    }
}

pub(crate) fn capture_program(path: &Path) -> Result<(File, CapturedInput)> {
    use sha2::{Digest as _, Sha256};
    if !path.is_absolute() {
        return Err(GateError::new(
            "path_invalid",
            "covered program path must be absolute",
        ));
    }
    let resolved = std::fs::canonicalize(path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            GateError::new("program_missing", "configured program file is absent")
        } else {
            capture_error(error)
        }
    })?;
    let file = subc_os::open_regular_nofollow(&resolved).map_err(capture_error)?;
    let before = file.metadata().map_err(capture_error)?;
    if !before.is_file() {
        return Err(GateError::new(
            "path_invalid",
            "covered program must be a regular file",
        ));
    }
    if before.len() > PROGRAM_FILE_LIMIT {
        return Err(GateError::new(
            "input_too_large",
            "covered program exceeds the 512 MiB streamed-hash limit",
        ));
    }
    let id = file_object_id(&file)?;
    let mut reader = &file;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];
    let mut length = 0;
    loop {
        let count = reader.read(&mut buffer).map_err(capture_error)?;
        if count == 0 {
            break;
        }
        length += count as u64;
        if length > PROGRAM_FILE_LIMIT {
            return Err(GateError::new(
                "input_too_large",
                "covered program exceeds the 512 MiB streamed-hash limit",
            ));
        }
        hasher.update(&buffer[..count]);
    }
    let after = file.metadata().map_err(capture_error)?;
    let reopened = subc_os::open_regular_nofollow(&resolved).map_err(capture_error)?;
    if length != before.len()
        || std::fs::canonicalize(path).map_err(capture_error)? != resolved
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || file_object_id(&reopened)? != id
    {
        return Err(GateError::stale());
    }
    let identity = json!({ "path": resolved, "file": id, "length": length, "raw": format!("sha256:{:x}", hasher.finalize()) });
    let state = InputState::Present {
        digest: subc_config::raw_digest(
            &serde_json::to_vec(&identity).map_err(GateError::invalid)?,
        ),
    };
    Ok((
        file,
        CapturedInput {
            state,
            value: RecordedValue::new(identity),
        },
    ))
}

fn refusal_code(code: subc_config::RefusalCode) -> &'static str {
    use subc_config::RefusalCode::*;
    match code {
        SchemaInvalid => "schema_invalid",
        LayerForbidden => "layer_forbidden",
        BoundViolated => "bound_violated",
        CredentialShared => "credential_shared",
        PlanStale => "plan_stale",
        _ => "value_invalid",
    }
}

fn refuse_expansions(value: &Value, module: &str, path: &str) -> Result<()> {
    match value {
        Value::String(text) if text.contains("{env:") || text.contains("{file:") => {
            Err(GateError::new("value_invalid", format!("{module}: {path}")))
        }
        Value::Object(values) => {
            for (key, value) in values {
                refuse_expansions(
                    value,
                    module,
                    &format!("{path}/{}", key.replace('~', "~0").replace('/', "~1")),
                )?;
            }
            Ok(())
        }
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                refuse_expansions(value, module, &format!("{path}/{index}"))?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Rig {
        root: PathBuf,
        capturer: ConfigCapturer,
        declarations: BTreeMap<String, ConfigDeclareRequest>,
    }
    impl Rig {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let root = std::env::temp_dir().join(format!(
                "subc-gate-capture-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(root.join("cortexkit")).unwrap();
            let root = root.canonicalize().unwrap();
            let daemon_path = root.join("cortexkit/subc.jsonc");
            std::fs::write(&daemon_path, br#"{"version":1,"modules":{}}"#).unwrap();
            let schema = json!({
                "$schema":"https://json-schema.org/draft/2020-12/schema", "type":"object", "unevaluatedProperties":false,
                "x-ck-layers":["user"], "x-ck-apply":"live",
                "properties": {
                    "credential": {"type":"object", "description":"Vault credential", "x-ck-kind":"credential_ref", "x-ck-required":true},
                    "limit": {"type":"integer", "description":"Request limit", "default":10}
                }
            });
            let declaration = ConfigDeclareRequest::new(
                schema,
                root.join("cortexkit/a.jsonc").to_string_lossy(),
                Vec::new(),
            );
            Self {
                capturer: ConfigCapturer {
                    daemon_path,
                    config_home: root.clone(),
                    projects: Arc::new(UnavailableProjectRegistry),
                },
                declarations: BTreeMap::from([("a".into(), declaration)]),
                root,
            }
        }
        fn write(&self, bytes: &[u8]) {
            std::fs::write(self.root.join("cortexkit/a.jsonc"), bytes).unwrap();
        }
    }
    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn config_capture_preserves_raw_reference_bytes_and_redacts_diagnostics() {
        let rig = Rig::new();
        let bytes = b"{ // reference, not its secret\n\"credential\":{\"vault_ref\":\"opaque-credential-id\"}, \"limit\":11,}\n";
        rig.write(bytes);
        let captured = rig.capturer.capture(&rig.declarations).unwrap();
        assert_eq!(
            captured.modules["a"].view.layers[0].bytes.as_deref(),
            Some(bytes.as_slice())
        );
        let diagnostic = serde_json::to_string(&captured.diagnostic.to_store().unwrap()).unwrap();
        assert!(!diagnostic.contains("opaque-credential-id"));
        assert!(diagnostic.contains("redacted"));
    }

    #[test]
    fn config_capture_refuses_literal_and_expansion_at_field_without_retaining_bytes() {
        let rig = Rig::new();
        for value in [
            json!("literal-password-never-retained"),
            json!("{env:PRIVATE_TOKEN}"),
        ] {
            rig.write(&serde_json::to_vec(&json!({"credential":value})).unwrap());
            let error = rig.capturer.capture(&rig.declarations).err().unwrap();
            assert_eq!(error.code, "value_invalid");
            assert_eq!(error.detail, "a: /credential");
            assert!(!error.to_string().contains(value.as_str().unwrap()));
        }
    }

    #[test]
    fn config_capture_replacement_with_identical_bytes_changes_read_set() {
        let rig = Rig::new();
        let bytes = br#"{"credential":{"vault_ref":"ref"}}"#;
        rig.write(bytes);
        let before = rig.capturer.capture(&rig.declarations).unwrap();
        let replacement = rig.root.join("replacement");
        std::fs::write(&replacement, bytes).unwrap();
        std::fs::remove_file(rig.root.join("cortexkit/a.jsonc")).unwrap();
        std::fs::rename(replacement, rig.root.join("cortexkit/a.jsonc")).unwrap();
        let after = rig.capturer.capture(&rig.declarations).unwrap();
        let modules = ["a".into()].into_iter().collect();
        assert_ne!(
            before.diagnostic.read_set(&modules).unwrap(),
            after.diagnostic.read_set(&modules).unwrap()
        );
    }

    #[test]
    fn config_capture_refuses_project_directories_without_opening_them() {
        let mut rig = Rig::new();
        rig.declarations.get_mut("a").unwrap().project_dirs = vec!["/not-a-registry-root".into()];
        let error = rig.capturer.capture(&rig.declarations).err().unwrap();
        assert_eq!(error.code, "project_registry_unavailable");
        assert!(error.detail.contains("a: project/workspace"));
    }

    #[test]
    fn config_capture_distinguishes_absent_empty_and_over_bound_files() {
        let rig = Rig::new();
        let path = rig.root.join("cortexkit/missing.jsonc");
        assert_eq!(CapturedFile::open(&path).unwrap().state, InputState::Absent);
        std::fs::write(&path, b"").unwrap();
        assert!(matches!(
            CapturedFile::open(&path).unwrap().state,
            InputState::Present { .. }
        ));
        File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(CONFIG_FILE_LIMIT + 1)
            .unwrap();
        assert_eq!(
            CapturedFile::open(&path).err().unwrap().code,
            "input_too_large"
        );
    }
}
