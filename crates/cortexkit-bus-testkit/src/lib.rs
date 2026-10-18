//! Private, disposable broker and daemon processes for module integration tests.
//!
//! `CK_SUBC_BIN` and `CK_BUS_BIN` must name test builds, never installed binaries.
//! Every executable is staged through `cortexkit_test_support::ckdev_binary`.
//! See the crate README for a complete setup and the missing-binary skip policy.

mod binaries;
mod bus;
mod config;
mod daemon;
mod process;
pub mod signer;
pub mod trust;

pub use binaries::{ck_bus_binary, ck_subc_binary, nats_server_bin};
pub use bus::{BusOptions, BusServer, PreloadedAccount, LOOPBACK};
pub use config::{subc_config, Module};
pub use daemon::{new_machine_id, write_machine_id, DaemonOptions, PrivateDaemon};

/// A missing prerequisite is skippable; a broken rig is not.
#[derive(Debug)]
pub enum RigError {
    MissingBinary {
        what: &'static str,
        env_var: &'static str,
        how_to_fix: String,
    },
    Io(std::io::Error),
    Invalid(String),
    Timeout {
        waiting_for: String,
        details: String,
    },
}

impl std::fmt::Display for RigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingBinary {
                what,
                env_var,
                how_to_fix,
            } => write!(f, "missing {what} ({env_var}); {how_to_fix}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::Invalid(message) => write!(f, "{message}"),
            Self::Timeout {
                waiting_for,
                details,
            } => write!(f, "timed out waiting for {waiting_for}: {details}"),
        }
    }
}

impl std::error::Error for RigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<std::io::Error> for RigError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

/// Call in an error arm, then return from the test. CI can require prerequisites
/// with `CK_BUS_TESTKIT_REQUIRE=1`; all non-prerequisite errors always panic.
pub fn skip_or_fail(error: RigError) {
    if matches!(error, RigError::MissingBinary { .. })
        && std::env::var_os("CK_BUS_TESTKIT_REQUIRE").as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        eprintln!("SKIP cortexkit-bus-testkit: {error}");
    } else {
        panic!("cortexkit-bus-testkit: {error}");
    }
}
