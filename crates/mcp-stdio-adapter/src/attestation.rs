use std::{env, fmt};

use subc_client_rs::launch_nonce::LaunchNonceError;
use subc_protocol::{SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV};

/// A validated registration handoff or the legacy launch-nonce preflight.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupAttestation {
    module_id: String,
}

impl StartupAttestation {
    /// Refuse hand launches before opening any local files. The SDK takes the
    /// same validated channel, or uses the cached nonce on older launches.
    /// The daemon still has to accept our HELLO before settings can be changed.
    ///
    /// Reading the nonce also closes its inherited descriptor, when present,
    /// so the adapter's child servers cannot inherit it.
    pub fn require() -> Result<Self, AttestationError> {
        Self::from_parts(
            required_environment_value(SUBC_MODULE_ID_ENV),
            subc_os::registration_channel::supervised(),
            subc_client_rs::launch_nonce(),
        )
    }

    fn from_parts(
        module_id: Option<String>,
        registration_validated: bool,
        launch_nonce: Result<Option<subc_client_rs::launch_nonce::LaunchNonce>, LaunchNonceError>,
    ) -> Result<Self, AttestationError> {
        let module_id = module_id.ok_or(AttestationError::MissingModuleId)?;
        if !registration_validated {
            launch_nonce
                .map_err(AttestationError::LaunchNonce)?
                .filter(|nonce| !nonce.value().trim().is_empty())
                .ok_or(AttestationError::MissingLaunchNonce)?;
        }
        Ok(Self { module_id })
    }

    pub fn module_id(&self) -> &str {
        &self.module_id
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
                    "startup attestation requires a validated registration channel or {SUBC_LAUNCH_NONCE_ENV}"
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
        let result = StartupAttestation::from_parts(None, false, Ok(None));
        assert_eq!(result, Err(AttestationError::MissingModuleId));
    }

    #[test]
    fn missing_nonce_has_a_specific_refusal() {
        let result =
            StartupAttestation::from_parts(Some("mcp-stdio-adapter".to_string()), false, Ok(None));
        assert_eq!(result, Err(AttestationError::MissingLaunchNonce));
    }

    #[test]
    fn an_unreadable_nonce_descriptor_is_refused_by_name() {
        let error = LaunchNonceError::NotOpen { fd: 3, errno: 9 };
        let result = StartupAttestation::from_parts(
            Some("mcp-stdio-adapter".to_string()),
            false,
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
        assert!(!format!("{attestation:?}").contains("nonce-kept-in-memory"));
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
    #[test]
    fn startup_accepts_a_validated_channel_without_a_nonce() {
        let attestation =
            StartupAttestation::from_parts(Some("mcp-stdio-adapter".to_string()), true, Ok(None))
                .unwrap();
        assert_eq!(attestation.module_id(), "mcp-stdio-adapter");
    }
}
