//! The vault root keys ck-bus signs with, and how it notices one rotated.
//!
//! Root keys are created once by an operator ceremony (`ck auth mint-signing-key`) and
//! reached by credential id. Every credential id comes from `cortexkit-bus-naming`'s
//! `root_credential_id`; ck-bus writes only the provider token and generation, never
//! the id as a literal.
//!
//! The operator has two keys (`docs/designs/nats-install-trust-chain.md`, sections 1
//! and 7). The ROOT is the operator identity: it self-signs the operator JWT and signs
//! the system account JWT, once, by ceremony at install, and ck-bus never uses it. The
//! SIGNER is listed in the operator JWT's `signing_keys` and signs the box account JWT,
//! including every revocation update; ck-bus holds `sign` and `read` on it.

use std::{
    collections::HashMap,
    fmt,
    num::NonZeroU32,
    sync::{Mutex, MutexGuard},
};

use cortexkit_bus_naming::{root_credential_id, NamingError, RootCredentialKind};

/// The recorded condition name for a name the naming crate cannot construct yet.
pub const NAMING_CONSTRUCTOR_ABSENT: &str = "naming-constructor-absent";

/// The roots the credential design uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RootCredential {
    /// Signs participant and ck-bus box-account user JWTs.
    BoxAccount,
    /// Signs ck-bus's system-account user JWT.
    SystemAccount,
    /// The operator identity. Signs only the operator JWT and the system account JWT,
    /// by ceremony at install. ck-bus has no use for it and never asks the vault for it.
    OperatorRoot,
    /// The operator signing key: signs the box account JWT, including every revocation
    /// claims update.
    OperatorSigner,
    /// Signs ck-bus's federation-account user JWT.
    FederationAccount,
    /// Signs per-message sender signatures for cross-machine deliveries.
    MessageSigning,
}

/// Why ck-bus has no credential id to call the vault with for a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootIdRefusal {
    /// The root is used only by the install ceremony, never by ck-bus.
    NoCkBusUse { root: RootCredential },
    /// The naming crate refused the provider token.
    Naming(NamingError),
}

impl fmt::Display for RootIdRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoCkBusUse { root } => write!(
                f,
                "the {root:?} key signs only by operator ceremony; ck-bus never uses it"
            ),
            Self::Naming(error) => write!(f, "{NAMING_CONSTRUCTOR_ABSENT}: {error}"),
        }
    }
}

impl RootCredential {
    /// The provider token and generation the operator minted this root under
    /// (`ck auth mint-signing-key --id signing:<provider>[:<generation>]`).
    pub const fn provider(self) -> (&'static str, Option<u32>) {
        match self {
            Self::BoxAccount => ("ck-bus-account", Some(1)),
            Self::SystemAccount => ("ck-bus-sysaccount", Some(1)),
            Self::OperatorRoot => ("ck-bus-operator-root", Some(1)),
            Self::OperatorSigner => ("ck-bus-operator-signer", Some(1)),
            Self::FederationAccount => ("ck-bus-fedaccount", Some(1)),
            // CKCRED's latest note names it without a generation.
            Self::MessageSigning => ("msgsig", None),
        }
    }

    /// The credential id to call the vault with. The operator root refuses: ck-bus
    /// never signs with it.
    pub fn credential_id(self) -> Result<String, RootIdRefusal> {
        if self == Self::OperatorRoot {
            return Err(RootIdRefusal::NoCkBusUse { root: self });
        }
        let (provider, generation) = self.provider();
        root_credential_id(
            RootCredentialKind::Signing,
            provider,
            generation.and_then(NonZeroU32::new),
        )
        .map_err(RootIdRefusal::Naming)
    }
}

/// What a newly seen `key_id` means for a root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyIdObservation {
    /// The first `key_id` this process has seen for the root.
    First,
    Unchanged,
    /// The root was rotated (`mint-signing-key --replace`): every JWT it signed under
    /// `previous` must be re-issued.
    Rotated {
        previous: String,
    },
}

/// The `key_id` of every root this process signed with, kept in memory.
///
/// A rotation is detected by comparing `key_id`, never by `record_version`: the vault's
/// record version is not monotonic across a delete-and-remint or a restore.
#[derive(Debug, Default)]
pub struct KeyIdLedger {
    seen: Mutex<HashMap<String, String>>,
}

impl KeyIdLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `key_id` for `credential_id` and says whether it changed.
    pub fn observe(&self, credential_id: &str, key_id: &str) -> KeyIdObservation {
        let mut seen = self.lock();
        match seen.insert(credential_id.to_string(), key_id.to_string()) {
            None => KeyIdObservation::First,
            Some(previous) if previous == key_id => KeyIdObservation::Unchanged,
            Some(previous) => KeyIdObservation::Rotated { previous },
        }
    }

    pub fn current(&self, credential_id: &str) -> Option<String> {
        self.lock().get(credential_id).cloned()
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, String>> {
        // Every mutation is a single insert, so a poisoned map is still consistent.
        self.seen
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
