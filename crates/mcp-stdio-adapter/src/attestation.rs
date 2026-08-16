use std::{env, fmt};

use subc_protocol::{SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV};

/// Startup-only daemon attestation retained after its environment carrier is scrubbed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupAttestation {
    module_id: String,
    launch_nonce: String,
}

impl StartupAttestation {
    /// Require daemon injection before any connection is opened, then remove the
    /// nonce from the process environment so it cannot reach future child setup.
    pub fn require_and_scrub() -> Result<Self, AttestationError> {
        let module_id = required_environment_value(SUBC_MODULE_ID_ENV)
            .ok_or(AttestationError::MissingModuleId)?;
        let launch_nonce = required_environment_value(SUBC_LAUNCH_NONCE_ENV)
            .ok_or(AttestationError::MissingLaunchNonce)?;

        env::remove_var(SUBC_LAUNCH_NONCE_ENV);

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

    use subc_protocol::{SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV};

    use super::{AttestationError, StartupAttestation};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn missing_module_id_has_a_specific_refusal() {
        let _guard = environment_lock();
        let module_id = env::var_os(SUBC_MODULE_ID_ENV);
        let nonce = env::var_os(SUBC_LAUNCH_NONCE_ENV);
        env::remove_var(SUBC_MODULE_ID_ENV);
        env::remove_var(SUBC_LAUNCH_NONCE_ENV);

        let result = StartupAttestation::require_and_scrub();

        restore_environment(SUBC_MODULE_ID_ENV, module_id);
        restore_environment(SUBC_LAUNCH_NONCE_ENV, nonce);
        assert_eq!(result, Err(AttestationError::MissingModuleId));
    }

    #[test]
    fn startup_scrubs_process_nonce_and_retains_the_memory_copy() {
        let _guard = environment_lock();
        let module_id = env::var_os(SUBC_MODULE_ID_ENV);
        let nonce = env::var_os(SUBC_LAUNCH_NONCE_ENV);
        env::set_var(SUBC_MODULE_ID_ENV, "mcp-stdio-adapter");
        env::set_var(SUBC_LAUNCH_NONCE_ENV, "nonce-kept-in-memory");

        let attestation = StartupAttestation::require_and_scrub().unwrap();

        assert_eq!(attestation.module_id(), "mcp-stdio-adapter");
        assert_eq!(attestation.launch_nonce(), "nonce-kept-in-memory");
        assert!(env::var_os(SUBC_LAUNCH_NONCE_ENV).is_none());

        restore_environment(SUBC_MODULE_ID_ENV, module_id);
        restore_environment(SUBC_LAUNCH_NONCE_ENV, nonce);
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
