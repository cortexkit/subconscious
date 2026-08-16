use std::{
    collections::BTreeMap,
    env, fmt, fs,
    path::{Path, PathBuf},
};

use serde::Deserialize;
use serde_json::Value;
use subc_jsonc::jsonc_to_json;

use crate::constants::{DEFAULT_DEADLINE_MS, DEFAULT_FRAME_CEILING_BYTES, DEFAULT_IDLE_TTL_MS};

pub const CONFIG_RELATIVE_PATH: &str = "cortexkit/mcp-servers.jsonc";
const MIN_IDLE_TTL_WARNING_MS: u64 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerRegistry {
    servers: BTreeMap<String, ServerConfig>,
}

impl ServerRegistry {
    pub fn servers(&self) -> &BTreeMap<String, ServerConfig> {
        &self.servers
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub env: BTreeMap<String, EnvironmentValue>,
    pub idle_ttl_ms: u64,
    pub disabled: bool,
    /// Raising this beyond the route caller's budget produces orphaned child
    /// results that the caller will never read.
    pub deadline_ms: u64,
    pub frame_ceiling_bytes: u64,
    pub cache_tools_list: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvironmentValue {
    Handle(String),
    Literal(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryWarning {
    ShortIdleTtl { server: String, idle_ttl_ms: u64 },
}

impl fmt::Display for RegistryWarning {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ShortIdleTtl {
                server,
                idle_ttl_ms,
            } => write!(
                formatter,
                "registry server '{server}' has idle_ttl_ms={idle_ttl_ms}, below the 10000 ms floor"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    MissingConfigHome,
    Read { path: PathBuf },
    Jsonc { path: PathBuf, message: String },
    Json { path: PathBuf, message: String },
    RootMustBeObject { path: PathBuf },
    InvalidServerName { server: String },
    ServerMustBeObject { server: String },
    InvalidServerConfig { server: String },
    BareEnvironmentList { server: String },
    EnvironmentMustBeObject { server: String },
    InvalidEnvironmentBinding { server: String, variable: String },
    EnvironmentTagCount { server: String, variable: String },
    InvalidHandle { server: String, variable: String },
    InvalidLiteralValue { server: String, variable: String },
    ReservedSubcEnvironment { server: String, variable: String },
    HandleNamespaceLiteral { server: String, variable: String },
}

impl fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingConfigHome => write!(formatter, "XDG_CONFIG_HOME is required to locate the MCP registry"),
            Self::Read { path } => write!(formatter, "could not read MCP registry {}", path.display()),
            Self::Jsonc { path, .. } | Self::Json { path, .. } => {
                write!(formatter, "could not parse MCP registry {}", path.display())
            }
            Self::RootMustBeObject { path } => {
                write!(formatter, "MCP registry {} must be a server object", path.display())
            }
            Self::InvalidServerName { server } => write!(formatter, "invalid MCP server name '{server}'"),
            Self::ServerMustBeObject { server } => {
                write!(formatter, "MCP server '{server}' must be an object")
            }
            Self::InvalidServerConfig { server } => {
                write!(formatter, "MCP server '{server}' has invalid configuration")
            }
            Self::BareEnvironmentList { server } => write!(
                formatter,
                "MCP server '{server}' env must be an explicit tagged variable map, not a bare-name list"
            ),
            Self::EnvironmentMustBeObject { server } => {
                write!(formatter, "MCP server '{server}' env must be an object")
            }
            Self::InvalidEnvironmentBinding { server, variable } => write!(
                formatter,
                "MCP server '{server}' environment variable '{variable}' must be a tagged object"
            ),
            Self::EnvironmentTagCount { server, variable } => write!(
                formatter,
                "MCP server '{server}' environment variable '{variable}' must have exactly one of handle or value"
            ),
            Self::InvalidHandle { server, variable } => write!(
                formatter,
                "MCP server '{server}' environment variable '{variable}' has a non-string handle"
            ),
            Self::InvalidLiteralValue { server, variable } => write!(
                formatter,
                "MCP server '{server}' environment variable '{variable}' has a non-string value"
            ),
            Self::ReservedSubcEnvironment { server, variable } => write!(
                formatter,
                "MCP server '{server}' environment variable '{variable}' may not use the SUBC_ namespace"
            ),
            Self::HandleNamespaceLiteral { server, variable } => write!(
                formatter,
                "MCP server '{server}' environment variable '{variable}' value resembles a credential handle"
            ),
        }
    }
}

impl std::error::Error for RegistryError {}

pub fn default_config_path() -> Result<PathBuf, RegistryError> {
    let config_home = env::var_os("XDG_CONFIG_HOME").ok_or(RegistryError::MissingConfigHome)?;
    Ok(PathBuf::from(config_home).join(CONFIG_RELATIVE_PATH))
}

pub fn load(path: &Path) -> Result<(ServerRegistry, Vec<RegistryWarning>), RegistryError> {
    let document = fs::read_to_string(path).map_err(|_| RegistryError::Read {
        path: path.to_path_buf(),
    })?;
    parse_document(path, &document)
}

pub fn parse_document(
    path: &Path,
    document: &str,
) -> Result<(ServerRegistry, Vec<RegistryWarning>), RegistryError> {
    let normalized = jsonc_to_json(document).map_err(|message| RegistryError::Jsonc {
        path: path.to_path_buf(),
        message,
    })?;
    let root: Value = serde_json::from_str(&normalized).map_err(|error| RegistryError::Json {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let servers = root
        .as_object()
        .ok_or_else(|| RegistryError::RootMustBeObject {
            path: path.to_path_buf(),
        })?;

    let mut parsed_servers = BTreeMap::new();
    let mut warnings = Vec::new();
    for (server, raw_config) in servers {
        validate_server_name(server)?;
        let (config, server_warnings) = parse_server(server, raw_config.clone())?;
        parsed_servers.insert(server.clone(), config);
        warnings.extend(server_warnings);
    }

    Ok((
        ServerRegistry {
            servers: parsed_servers,
        },
        warnings,
    ))
}

fn validate_server_name(server: &str) -> Result<(), RegistryError> {
    let valid = !server.is_empty()
        && server.len() <= 64
        && server
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-');
    if valid {
        Ok(())
    } else {
        Err(RegistryError::InvalidServerName {
            server: server.to_string(),
        })
    }
}

fn parse_server(
    server: &str,
    raw_config: Value,
) -> Result<(ServerConfig, Vec<RegistryWarning>), RegistryError> {
    let mut object =
        raw_config
            .as_object()
            .cloned()
            .ok_or_else(|| RegistryError::ServerMustBeObject {
                server: server.to_string(),
            })?;
    let env = match object.remove("env") {
        Some(raw_env) => parse_environment(server, raw_env)?,
        None => BTreeMap::new(),
    };
    let raw: RawServerConfig = serde_json::from_value(Value::Object(object)).map_err(|_| {
        RegistryError::InvalidServerConfig {
            server: server.to_string(),
        }
    })?;
    let idle_ttl_ms = raw
        .idle_ttl_ms
        .filter(|value| *value != 0)
        .unwrap_or(DEFAULT_IDLE_TTL_MS);
    let mut warnings = Vec::new();
    if idle_ttl_ms < MIN_IDLE_TTL_WARNING_MS {
        warnings.push(RegistryWarning::ShortIdleTtl {
            server: server.to_string(),
            idle_ttl_ms,
        });
    }

    Ok((
        ServerConfig {
            command: raw.command,
            args: raw.args,
            cwd: raw.cwd,
            env,
            idle_ttl_ms,
            disabled: raw.disabled,
            deadline_ms: raw.deadline_ms.unwrap_or(DEFAULT_DEADLINE_MS),
            frame_ceiling_bytes: raw
                .frame_ceiling_bytes
                .unwrap_or(DEFAULT_FRAME_CEILING_BYTES),
            cache_tools_list: raw.cache_tools_list.unwrap_or(true),
        },
        warnings,
    ))
}

fn parse_environment(
    server: &str,
    raw_environment: Value,
) -> Result<BTreeMap<String, EnvironmentValue>, RegistryError> {
    let object = match raw_environment {
        Value::Object(object) => object,
        Value::Array(_) => {
            return Err(RegistryError::BareEnvironmentList {
                server: server.to_string(),
            })
        }
        _ => {
            return Err(RegistryError::EnvironmentMustBeObject {
                server: server.to_string(),
            })
        }
    };

    object
        .into_iter()
        .map(|(variable, raw_binding)| {
            if variable.starts_with("SUBC_") {
                return Err(RegistryError::ReservedSubcEnvironment {
                    server: server.to_string(),
                    variable,
                });
            }
            let binding = raw_binding.as_object().ok_or_else(|| {
                RegistryError::InvalidEnvironmentBinding {
                    server: server.to_string(),
                    variable: variable.clone(),
                }
            })?;
            if binding.len() != 1 {
                return Err(RegistryError::EnvironmentTagCount {
                    server: server.to_string(),
                    variable,
                });
            }
            if let Some(handle) = binding.get("handle") {
                let handle = handle
                    .as_str()
                    .ok_or_else(|| RegistryError::InvalidHandle {
                        server: server.to_string(),
                        variable: variable.clone(),
                    })?;
                return Ok((variable, EnvironmentValue::Handle(handle.to_string())));
            }
            if let Some(value) = binding.get("value") {
                let value = value
                    .as_str()
                    .ok_or_else(|| RegistryError::InvalidLiteralValue {
                        server: server.to_string(),
                        variable: variable.clone(),
                    })?;
                if resembles_handle_namespace(value) {
                    return Err(RegistryError::HandleNamespaceLiteral {
                        server: server.to_string(),
                        variable,
                    });
                }
                return Ok((variable, EnvironmentValue::Literal(value.to_string())));
            }
            Err(RegistryError::EnvironmentTagCount {
                server: server.to_string(),
                variable,
            })
        })
        .collect()
}

fn resembles_handle_namespace(value: &str) -> bool {
    ["apikey:", "oauth:", "vault:"]
        .iter()
        .any(|prefix| value.starts_with(prefix))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    idle_ttl_ms: Option<u64>,
    #[serde(default)]
    disabled: bool,
    #[serde(default)]
    deadline_ms: Option<u64>,
    #[serde(default)]
    frame_ceiling_bytes: Option<u64>,
    #[serde(default)]
    cache_tools_list: Option<bool>,
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{
        parse_document, EnvironmentValue, RegistryError, RegistryWarning, CONFIG_RELATIVE_PATH,
    };
    use crate::constants::{DEFAULT_DEADLINE_MS, DEFAULT_FRAME_CEILING_BYTES, DEFAULT_IDLE_TTL_MS};

    static NEXT_TEMP_FILE: AtomicU64 = AtomicU64::new(1);

    fn parse(document: &str) -> Result<super::ServerRegistry, RegistryError> {
        parse_document(PathBuf::from("test-registry.jsonc").as_path(), document)
            .map(|(registry, _)| registry)
    }

    #[test]
    fn parses_jsonc_full_server_schema_and_defaults() {
        let (registry, warnings) = parse_document(
            PathBuf::from("registry.jsonc").as_path(),
            r#"
            {
              // this comment and comma exercise JSONC normalization
              "github": {
                "command": "/usr/local/bin/github-mcp",
                "args": ["--stdio",],
                "cwd": "/work/project",
                "env": {
                  "TOKEN": { "handle": "vault:github-token" },
                  "MODE": { "value": "readonly" },
                },
                "idle_ttl_ms": 0,
                "disabled": true,
                "deadline_ms": 45000,
                "frame_ceiling_bytes": 8192,
                "cache_tools_list": false,
              },
              "defaults": { "command": "defaults" },
            }
            "#,
        )
        .unwrap();

        let github = &registry.servers()["github"];
        assert_eq!(github.command, "/usr/local/bin/github-mcp");
        assert_eq!(github.args, ["--stdio"]);
        assert_eq!(github.cwd, Some(PathBuf::from("/work/project")));
        assert_eq!(
            github.env["TOKEN"],
            EnvironmentValue::Handle("vault:github-token".to_string())
        );
        assert_eq!(
            github.env["MODE"],
            EnvironmentValue::Literal("readonly".to_string())
        );
        assert_eq!(github.idle_ttl_ms, DEFAULT_IDLE_TTL_MS);
        assert!(github.disabled);
        assert_eq!(github.deadline_ms, 45_000);
        assert_eq!(github.frame_ceiling_bytes, 8192);
        assert!(!github.cache_tools_list);
        let defaults = &registry.servers()["defaults"];
        assert_eq!(defaults.deadline_ms, DEFAULT_DEADLINE_MS);
        assert_eq!(defaults.frame_ceiling_bytes, DEFAULT_FRAME_CEILING_BYTES);
        assert!(defaults.cache_tools_list);
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_server_name_is_named() {
        let error = parse(r#"{ "GitHub": { "command": "mcp" } }"#).unwrap_err();

        assert_eq!(
            error,
            RegistryError::InvalidServerName {
                server: "GitHub".to_string()
            }
        );
    }

    #[test]
    fn non_object_server_entry_is_named() {
        let error = parse(r#"{ "github": "mcp" }"#).unwrap_err();

        assert_eq!(
            error,
            RegistryError::ServerMustBeObject {
                server: "github".to_string()
            }
        );
    }

    #[test]
    fn unknown_server_fields_are_refused_with_the_server_name() {
        let error = parse(r#"{ "github": { "command": "mcp", "unknown": true } }"#).unwrap_err();

        assert_eq!(
            error,
            RegistryError::InvalidServerConfig {
                server: "github".to_string()
            }
        );
    }

    #[test]
    fn bare_environment_list_is_refused_with_the_server_name() {
        let error = parse(r#"{ "github": { "command": "mcp", "env": ["TOKEN"] } }"#).unwrap_err();

        assert_eq!(
            error,
            RegistryError::BareEnvironmentList {
                server: "github".to_string()
            }
        );
    }

    #[test]
    fn non_object_environment_is_refused_with_the_server_name() {
        let error = parse(r#"{ "github": { "command": "mcp", "env": "TOKEN" } }"#).unwrap_err();

        assert_eq!(
            error,
            RegistryError::EnvironmentMustBeObject {
                server: "github".to_string()
            }
        );
    }

    #[test]
    fn bare_environment_binding_is_refused_with_server_and_variable() {
        let error = parse(r#"{ "github": { "command": "mcp", "env": { "TOKEN": "value" } } }"#)
            .unwrap_err();

        assert_eq!(
            error,
            RegistryError::InvalidEnvironmentBinding {
                server: "github".to_string(),
                variable: "TOKEN".to_string(),
            }
        );
    }

    #[test]
    fn missing_or_multiple_environment_tags_are_refused_with_server_and_variable() {
        let missing =
            parse(r#"{ "github": { "command": "mcp", "env": { "TOKEN": {} } } }"#).unwrap_err();
        let multiple = parse(
            r#"{ "github": { "command": "mcp", "env": { "TOKEN": { "handle": "x", "value": "y" } } } }"#,
        )
        .unwrap_err();

        let expected = RegistryError::EnvironmentTagCount {
            server: "github".to_string(),
            variable: "TOKEN".to_string(),
        };
        assert_eq!(missing, expected);
        assert_eq!(multiple, expected);
    }

    #[test]
    fn non_string_handle_is_refused_with_server_and_variable() {
        let error =
            parse(r#"{ "github": { "command": "mcp", "env": { "TOKEN": { "handle": 7 } } } }"#)
                .unwrap_err();

        assert_eq!(
            error,
            RegistryError::InvalidHandle {
                server: "github".to_string(),
                variable: "TOKEN".to_string(),
            }
        );
    }

    #[test]
    fn non_string_literal_is_refused_with_server_and_variable() {
        let error =
            parse(r#"{ "github": { "command": "mcp", "env": { "TOKEN": { "value": 7 } } } }"#)
                .unwrap_err();

        assert_eq!(
            error,
            RegistryError::InvalidLiteralValue {
                server: "github".to_string(),
                variable: "TOKEN".to_string(),
            }
        );
    }

    #[test]
    fn subc_environment_key_is_refused_with_server_and_variable() {
        let error = parse(
            r#"{ "github": { "command": "mcp", "env": { "SUBC_MODULE_ID": { "value": "x" } } } }"#,
        )
        .unwrap_err();

        assert_eq!(
            error,
            RegistryError::ReservedSubcEnvironment {
                server: "github".to_string(),
                variable: "SUBC_MODULE_ID".to_string(),
            }
        );
    }

    #[test]
    fn handle_namespace_literal_is_refused_without_echoing_the_value() {
        let secret_shaped_value = "vault:do-not-echo";
        let error = parse(&format!(
            r#"{{ "github": {{ "command": "mcp", "env": {{ "TOKEN": {{ "value": "{secret_shaped_value}" }} }} }} }}"#
        ))
        .unwrap_err();

        assert_eq!(
            error,
            RegistryError::HandleNamespaceLiteral {
                server: "github".to_string(),
                variable: "TOKEN".to_string(),
            }
        );
        assert!(!error.to_string().contains(secret_shaped_value));
    }

    #[test]
    fn every_handle_namespace_prefix_is_refused() {
        for prefix in ["apikey:", "oauth:", "vault:"] {
            let error = parse(&format!(
                r#"{{ "github": {{ "command": "mcp", "env": {{ "TOKEN": {{ "value": "{prefix}opaque" }} }} }} }}"#
            ))
            .unwrap_err();
            assert_eq!(
                error,
                RegistryError::HandleNamespaceLiteral {
                    server: "github".to_string(),
                    variable: "TOKEN".to_string(),
                }
            );
        }
    }

    #[test]
    fn short_ttl_parses_and_warns_with_the_server_name() {
        let (registry, warnings) = parse_document(
            PathBuf::from("registry.jsonc").as_path(),
            r#"{ "github": { "command": "mcp", "idle_ttl_ms": 9999 } }"#,
        )
        .unwrap();

        assert_eq!(registry.servers()["github"].idle_ttl_ms, 9999);
        assert_eq!(
            warnings,
            vec![RegistryWarning::ShortIdleTtl {
                server: "github".to_string(),
                idle_ttl_ms: 9999,
            }]
        );
        assert!(warnings[0].to_string().contains("github"));
    }

    #[test]
    fn unparseable_file_error_names_the_file() {
        let path = std::env::temp_dir().join(format!(
            "mcp-stdio-adapter-registry-{}-{}.jsonc",
            std::process::id(),
            NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::write(&path, "{ invalid").unwrap();

        let error = super::load(&path).unwrap_err();

        let _ = fs::remove_file(&path);
        assert!(matches!(error, RegistryError::Json { path: ref actual, .. } if actual == &path));
        assert!(error.to_string().contains(&path.display().to_string()));
    }

    #[test]
    fn configured_relative_path_is_stable() {
        assert_eq!(CONFIG_RELATIVE_PATH, "cortexkit/mcp-servers.jsonc");
    }
}
