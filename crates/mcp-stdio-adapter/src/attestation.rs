use std::{env, fmt};

use subc_client_rs::launch_nonce::LaunchNonceError;
use subc_protocol::{SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV};

/// The daemon attestation this adapter starts with: its module id and the
/// launch nonce read through the process's one launch-nonce accessor.
#[derive(Clone, PartialEq, Eq)]
pub struct StartupAttestation {
    module_id: String,
    launch_nonce: String,
}

// Hand-written so the launch nonce is never printed. The nonce is the credential
// that attributes a connection to a supervised module, and a derived Debug would
// write it into any log line or panic message that formats this value. Same
// reasoning as ConnectionInfo's Debug in subc-transport.
impl fmt::Debug for StartupAttestation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StartupAttestation")
            .field("module_id", &self.module_id)
            .field(
                "launch_nonce",
                &format_args!("<{} bytes redacted>", self.launch_nonce.len()),
            )
            .finish()
    }
}

impl StartupAttestation {
    /// Require daemon injection before any connection is opened or any child is
    /// spawned: reading the nonce first is what closes the inherited nonce
    /// descriptor before a child could inherit it.
    ///
    /// The environment is left as it is. The accessor caches the nonce for
    /// every later reader in this process (the SDK's HELLO among them), and
    /// removing the variable from a multi-threaded process is unsound. Child
    /// servers do not see it anyway: they are spawned with a cleared
    /// environment.
    pub fn require() -> Result<Self, AttestationError> {
        Self::from_parts(
            required_environment_value(SUBC_MODULE_ID_ENV),
            subc_client_rs::launch_nonce(),
        )
    }

    fn from_parts(
        module_id: Option<String>,
        launch_nonce: Result<Option<subc_client_rs::launch_nonce::LaunchNonce>, LaunchNonceError>,
    ) -> Result<Self, AttestationError> {
        let module_id = module_id.ok_or(AttestationError::MissingModuleId)?;
        let launch_nonce = launch_nonce
            .map_err(AttestationError::LaunchNonce)?
            .map(|nonce| nonce.value().to_string())
            .filter(|value| !value.trim().is_empty())
            .ok_or(AttestationError::MissingLaunchNonce)?;
        Ok(Self {
            module_id,
            launch_nonce,
        })
    }

    pub fn module_id(&self) -> &str {
        &self.module_id
    }

    pub fn launch_nonce(&self) -> &str {
        &self.launch_nonce
    }
}

fn required_environment_value(name: &str) -> Option<String> {
    env::var(name).ok().filter(|value| !value.trim().is_empty())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttestationError {
    MissingModuleId,
    MissingLaunchNonce,
    /// The nonce descriptor was named but could not be read.
    LaunchNonce(LaunchNonceError),
}

impl fmt::Display for AttestationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingModuleId => write!(
                formatter,
                "startup attestation requires {SUBC_MODULE_ID_ENV}"
            ),
            Self::MissingLaunchNonce => {
                write!(
                    formatter,
                    "startup attestation requires {SUBC_LAUNCH_NONCE_ENV}"
                )
            }
            Self::LaunchNonce(error) => {
                write!(formatter, "startup attestation: {error}")
            }
        }
    }
}

impl std::error::Error for AttestationError {}

#[cfg(test)]
mod tests {
    use std::{
        env,
        ffi::OsString,
        sync::{Mutex, OnceLock},
    };

    use subc_client_rs::launch_nonce::{LaunchNonceError, LAUNCH_NONCE_FD_ENV};
    use subc_protocol::{SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV};

    use super::{AttestationError, StartupAttestation};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn missing_module_id_has_a_specific_refusal() {
        let result = StartupAttestation::from_parts(None, Ok(None));
        assert_eq!(result, Err(AttestationError::MissingModuleId));
    }

    #[test]
    fn missing_nonce_has_a_specific_refusal() {
        let result =
            StartupAttestation::from_parts(Some("mcp-stdio-adapter".to_string()), Ok(None));
        assert_eq!(result, Err(AttestationError::MissingLaunchNonce));
    }

    #[test]
    fn an_unreadable_nonce_descriptor_is_refused_by_name() {
        let error = LaunchNonceError::NotOpen { fd: 3, errno: 9 };
        let result = StartupAttestation::from_parts(
            Some("mcp-stdio-adapter".to_string()),
            Err(error.clone()),
        );
        assert_eq!(result, Err(AttestationError::LaunchNonce(error)));
    }

    /// The adapter used to remove the nonce from its environment after reading
    /// it. It no longer does: the accessor caches the value, and removing the
    /// variable would break any other reader still on the environment copy.
    /// This is the only test in this binary that reaches the process-wide
    /// accessor, which reads once and caches.
    #[test]
    fn startup_leaves_the_launch_nonce_in_the_environment() {
        let _guard = environment_lock();
        let module_id = env::var_os(SUBC_MODULE_ID_ENV);
        let nonce = env::var_os(SUBC_LAUNCH_NONCE_ENV);
        let nonce_fd = env::var_os(LAUNCH_NONCE_FD_ENV);
        env::set_var(SUBC_MODULE_ID_ENV, "mcp-stdio-adapter");
        env::set_var(SUBC_LAUNCH_NONCE_ENV, "nonce-kept-in-memory");
        env::remove_var(LAUNCH_NONCE_FD_ENV);

        let attestation = StartupAttestation::require();
        let left = env::var_os(SUBC_LAUNCH_NONCE_ENV);

        restore_environment(SUBC_MODULE_ID_ENV, module_id);
        restore_environment(SUBC_LAUNCH_NONCE_ENV, nonce);
        restore_environment(LAUNCH_NONCE_FD_ENV, nonce_fd);
        let attestation = attestation.unwrap();
        assert_eq!(attestation.module_id(), "mcp-stdio-adapter");
        assert_eq!(attestation.launch_nonce(), "nonce-kept-in-memory");
        assert_eq!(left.as_deref(), Some("nonce-kept-in-memory".as_ref()));
    }

    fn environment_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn restore_environment(name: &str, value: Option<OsString>) {
        match value {
            Some(value) => env::set_var(name, value),
            None => env::remove_var(name),
        }
    }
}

#[cfg(test)]
mod launch_nonce_redaction_tests {
    use super::*;

    #[test]
    fn startup_attestation_debug_never_prints_the_nonce() {
        let attestation = StartupAttestation {
            module_id: "mcp-stdio".to_string(),
            launch_nonce: "nonce-f00dfeed1234abcd".to_string(),
        };
        let printed = format!("{attestation:?}");
        assert!(printed.contains("mcp-stdio"), "{printed}");
        assert!(
            !printed.contains("nonce-f00dfeed1234abcd"),
            "launch nonce printed: {printed}"
        );
    }
}
