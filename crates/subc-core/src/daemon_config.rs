use std::{
    collections::BTreeMap,
    env,
    error::Error,
    ffi::OsString,
    fmt, fs, io,
    path::{Path, PathBuf},
};

use serde::Deserialize;

const DAEMON_CONFIG_RELATIVE_PATH: &str = "cortexkit/subc.jsonc";
const SUPPORTED_CONFIG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonConfig {
    pub path: PathBuf,
    pub port: Option<u16>,
    pub modules: Vec<ConfiguredModule>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfiguredModule {
    pub module_id: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub enabled: bool,
}

#[derive(Debug)]
pub enum DaemonConfigError {
    Read {
        path: PathBuf,
        source: io::Error,
    },
    InvalidJsonc {
        path: PathBuf,
        message: String,
    },
    InvalidJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    UnsupportedVersion {
        path: PathBuf,
        version: u32,
    },
}

#[derive(Debug, Deserialize)]
struct RawDaemonConfig {
    version: u32,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    modules: BTreeMap<String, RawModuleConfig>,
}

#[derive(Debug, Deserialize)]
struct RawModuleConfig {
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default = "default_enabled")]
    enabled: bool,
}

pub fn default_config_path() -> PathBuf {
    if let Some(config_home) = non_empty_os_var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join(DAEMON_CONFIG_RELATIVE_PATH);
    }

    #[cfg(windows)]
    {
        if let Some(app_data) = non_empty_os_var("APPDATA") {
            return PathBuf::from(app_data).join("cortexkit").join("subc.jsonc");
        }
        if let Some(user_profile) = non_empty_os_var("USERPROFILE") {
            return PathBuf::from(user_profile)
                .join("AppData")
                .join("Roaming")
                .join("cortexkit")
                .join("subc.jsonc");
        }
    }

    if let Some(home) = non_empty_os_var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join(DAEMON_CONFIG_RELATIVE_PATH);
    }

    PathBuf::from(".config").join(DAEMON_CONFIG_RELATIVE_PATH)
}

pub fn load(path: impl AsRef<Path>) -> Result<Option<DaemonConfig>, DaemonConfigError> {
    let path = path.as_ref();
    let doc = match fs::read_to_string(path) {
        Ok(doc) => doc,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(DaemonConfigError::Read {
                path: path.to_path_buf(),
                source,
            })
        }
    };

    parse_doc(&doc, path).map(Some)
}

fn parse_doc(doc: &str, path: &Path) -> Result<DaemonConfig, DaemonConfigError> {
    let json = jsonc_to_json(doc).map_err(|message| DaemonConfigError::InvalidJsonc {
        path: path.to_path_buf(),
        message,
    })?;
    let raw: RawDaemonConfig =
        serde_json::from_str(&json).map_err(|source| DaemonConfigError::InvalidJson {
            path: path.to_path_buf(),
            source,
        })?;

    if raw.version != SUPPORTED_CONFIG_VERSION {
        return Err(DaemonConfigError::UnsupportedVersion {
            path: path.to_path_buf(),
            version: raw.version,
        });
    }

    let modules = raw
        .modules
        .into_iter()
        .map(|(module_id, module)| ConfiguredModule {
            module_id,
            program: module.program,
            args: module.args,
            env: module.env.into_iter().collect(),
            enabled: module.enabled,
        })
        .collect();

    Ok(DaemonConfig {
        path: path.to_path_buf(),
        port: raw.port,
        modules,
    })
}

fn default_enabled() -> bool {
    true
}

fn jsonc_to_json(doc: &str) -> Result<String, String> {
    let mut out = String::with_capacity(doc.len());
    let mut chars = doc.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        match ch {
            '"' => {
                in_string = true;
                out.push(ch);
            }
            '/' if chars.peek() == Some(&'/') => {
                let _ = chars.next();
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                let _ = chars.next();
                let mut closed = false;
                let mut prev = '\0';
                for next in chars.by_ref() {
                    if next == '\n' {
                        out.push('\n');
                    }
                    if prev == '*' && next == '/' {
                        closed = true;
                        break;
                    }
                    prev = next;
                }
                if !closed {
                    return Err("unterminated block comment".to_owned());
                }
            }
            _ => out.push(ch),
        }
    }

    if in_string {
        return Err("unterminated string".to_owned());
    }

    Ok(remove_json_trailing_commas(&out))
}

fn remove_json_trailing_commas(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;

    while let Some(ch) = chars.next() {
        if in_string {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_string = false;
            }
            continue;
        }

        if ch == '"' {
            in_string = true;
            out.push(ch);
            continue;
        }

        if ch == ',' {
            let mut lookahead = chars.clone();
            while matches!(lookahead.peek(), Some(next) if next.is_whitespace()) {
                let _ = lookahead.next();
            }
            if matches!(lookahead.peek(), Some('}' | ']')) {
                continue;
            }
        }

        out.push(ch);
    }

    out
}

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

impl fmt::Display for DaemonConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "failed to read daemon config {}: {source}", path.display())
            }
            Self::InvalidJsonc { path, message } => {
                write!(f, "invalid JSONC in daemon config {}: {message}", path.display())
            }
            Self::InvalidJson { path, source } => {
                write!(f, "invalid daemon config {}: {source}", path.display())
            }
            Self::UnsupportedVersion { path, version } => write!(
                f,
                "invalid daemon config {}: version {version} is unsupported (expected {SUPPORTED_CONFIG_VERSION})",
                path.display()
            ),
        }
    }
}

impl Error for DaemonConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::InvalidJson { source, .. } => Some(source),
            Self::InvalidJsonc { .. } | Self::UnsupportedVersion { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_jsonc_defaults_and_ignores_unknown_fields() {
        let path = Path::new("/tmp/subc.jsonc");
        let config = parse_doc(
            r#"
            {
              // forward-compatible root field
              "version": 1,
              "unknown": { "ignored": true },
              "modules": {
                "aft": {
                  "program": "aft",
                  "args": ["module",],
                  "env": { "A": "B", },
                  "future": 42,
                },
                "disabled": { "program": "disabled", "enabled": false }
              },
            }
            "#,
            path,
        )
        .unwrap();

        assert_eq!(config.port, None);
        assert_eq!(config.modules.len(), 2);
        assert_eq!(config.modules[0].module_id, "aft");
        assert_eq!(config.modules[0].program, PathBuf::from("aft"));
        assert_eq!(config.modules[0].args, ["module"]);
        assert_eq!(config.modules[0].env, [("A".to_string(), "B".to_string())]);
        assert!(config.modules[0].enabled);
        assert!(!config.modules[1].enabled);
    }

    #[test]
    fn reject_unsupported_version() {
        let err = parse_doc(
            r#"{ "version": 2, "modules": {} }"#,
            Path::new("subc.jsonc"),
        )
        .unwrap_err();
        assert!(matches!(
            err,
            DaemonConfigError::UnsupportedVersion { version: 2, .. }
        ));
    }

    #[test]
    fn reject_unterminated_block_comment() {
        let err = parse_doc(r#"{ "version": 1, /*"#, Path::new("subc.jsonc")).unwrap_err();
        assert!(matches!(err, DaemonConfigError::InvalidJsonc { .. }));
    }
}
