//! The configuration store's sealing key and revision anchor in one keychain
//! item. Only a valid team-signed macOS image can open the native store.
//!
//! Absence is not access denial: callers must never turn an error into a
//! fallback that trusts an unsealed store's plain digest. Creating an item is a
//! separate recovery or first-stamp operation, authorized by the caller's
//! configuration approval flow. Serialize writes across handles to one item.

use std::{fmt, io};
use zeroize::Zeroizing;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(any(target_os = "macos", test))]
mod requirement;
#[cfg(feature = "test-support")]
pub mod testing;

/// A sealing key whose debug representation never includes its bytes.
/// Both the key and temporary serialized copies are zeroized on drop.
pub struct StoreKey(Zeroizing<[u8; 32]>);

impl StoreKey {
    /// The caller must supply 32 fresh cryptographically random bytes.
    pub fn new(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    /// Borrow the secret for MAC computation; do not log or persist it elsewhere.
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for StoreKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StoreKey([REDACTED])")
    }
}

/// A revision bound to the digest of the complete committed store bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreAnchor {
    pub revision: u64,
    pub sha256: [u8; 32],
}

/// Clearing the current pair does not reset the revision high-water mark.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnchorState {
    pub current: Option<StoreAnchor>,
    pub high_water: u64,
}

impl AnchorState {
    fn update(self, anchor: StoreAnchor) -> Result<Self, KeychainError> {
        if self.current == Some(anchor) {
            return Ok(self);
        }
        if anchor.revision <= self.high_water {
            return Err(KeychainError::RevisionConflict);
        }
        Ok(Self {
            current: Some(anchor),
            high_water: anchor.revision,
        })
    }

    fn clear(self, revision: u64) -> Result<Self, KeychainError> {
        // The unstamp is its own committed revision, so it must pass the stamp
        // it removes. Equal is accepted only to repeat a clear that already
        // happened, when nothing is stamped.
        let repeat = self.current.is_none() && revision == self.high_water;
        if revision == 0 || (revision <= self.high_water && !repeat) {
            return Err(KeychainError::RevisionConflict);
        }
        Ok(Self {
            current: None,
            high_water: revision,
        })
    }
}

/// One atomic keychain value: a secret and its current anchor/high-water.
#[derive(Debug)]
pub struct KeychainState {
    pub key: StoreKey,
    pub anchor: AnchorState,
}

#[cfg(any(target_os = "macos", feature = "test-support"))]
const MAGIC: &[u8; 8] = b"SUBCKEY1";
#[cfg(any(target_os = "macos", feature = "test-support"))]
const ENCODED_LEN: usize = 89;

#[cfg(any(target_os = "macos", feature = "test-support"))]
impl KeychainState {
    fn encode(&self) -> Zeroizing<Vec<u8>> {
        let mut bytes = Zeroizing::new(Vec::with_capacity(ENCODED_LEN));
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(self.key.as_bytes());
        bytes.extend_from_slice(&self.anchor.high_water.to_be_bytes());
        bytes.push(u8::from(self.anchor.current.is_some()));
        let current = self.anchor.current.unwrap_or(StoreAnchor {
            revision: 0,
            sha256: [0; 32],
        });
        bytes.extend_from_slice(&current.revision.to_be_bytes());
        bytes.extend_from_slice(&current.sha256);
        bytes
    }

    fn decode(bytes: &[u8]) -> Result<Self, KeychainError> {
        if bytes.len() != ENCODED_LEN || &bytes[..8] != MAGIC {
            return Err(KeychainError::InvalidData);
        }
        // The length check above bounds every fixed-width field.
        let high_water = u64::from_be_bytes(bytes[40..48].try_into().unwrap());
        let revision = u64::from_be_bytes(bytes[49..57].try_into().unwrap());
        let sha256 = bytes[57..89].try_into().unwrap();
        let current = match bytes[48] {
            0 if revision == 0 && sha256 == [0; 32] => None,
            1 if revision != 0 && revision == high_water => Some(StoreAnchor { revision, sha256 }),
            _ => return Err(KeychainError::InvalidData),
        };
        Ok(Self {
            key: StoreKey::new(bytes[8..40].try_into().unwrap()),
            anchor: AnchorState {
                current,
                high_water,
            },
        })
    }
}

/// Failure to reach/read an eligible keychain is never `Unsupported`.
#[derive(Debug)]
#[non_exhaustive]
pub enum KeychainError {
    Unsupported,
    Unavailable,
    Denied,
    Missing,
    AlreadyExists,
    InvalidData,
    InvalidRequirement,
    RevisionConflict,
    OsStatus(i32),
    Io(io::Error),
}

impl fmt::Display for KeychainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unsupported => "sealing requires a valid team-signed macOS image",
            Self::Unavailable => "keychain or signing information unavailable",
            Self::Denied => "key access denied; restore keychain access",
            Self::Missing => "keychain item missing",
            Self::AlreadyExists => "keychain item already exists",
            Self::InvalidData => "invalid or unsupported keychain item format",
            Self::InvalidRequirement => {
                "signing requirement must bind the Apple anchor, identifier and signer (team, or development certificate)"
            }
            Self::RevisionConflict => "keychain revision must advance the high-water mark",
            Self::OsStatus(_) => "Security framework operation failed",
            Self::Io(_) => "test key-store I/O failed",
        })
    }
}

impl std::error::Error for KeychainError {}

/// A generic-password item, scoped by service and the fixed `config-gate-v1`
/// account in the user's default file keychain. Use a stable service across
/// upgrades. `open` does not read or create the item or enable interaction UI.
pub struct KeychainStore {
    #[cfg(target_os = "macos")]
    inner: macos::Store,
}

impl KeychainStore {
    /// Checks the running image, not an executable path that could be replaced.
    /// Unsigned/ad-hoc images and non-macOS platforms return `Unsupported`.
    pub fn open(service: &str) -> Result<Self, KeychainError> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                inner: macos::Store::open(service)?,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = service;
            Err(KeychainError::Unsupported)
        }
    }

    /// `None` means the item is absent; denial, lock and malformed data are errors.
    pub fn load(&self) -> Result<Option<KeychainState>, KeychainError> {
        #[cfg(target_os = "macos")]
        {
            self.inner.load()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(KeychainError::Unsupported)
        }
    }

    /// Creates an unanchored item. Never replaces an existing item or its ACL.
    pub fn create(&self, key: StoreKey) -> Result<(), KeychainError> {
        #[cfg(target_os = "macos")]
        {
            self.inner.create(key)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = key;
            Err(KeychainError::Unsupported)
        }
    }

    /// Atomically persists the new pair and high-water with the existing key.
    /// Repeating the exact current pair is idempotent; every other update must
    /// advance high-water, including the first stamp after a clear.
    pub fn update_anchor(&self, anchor: StoreAnchor) -> Result<(), KeychainError> {
        self.change_anchor(|old| old.update(anchor))
    }

    /// Removes the pair while retaining/advancing high-water to the committed
    /// unstamp revision. Re-stamping must use a revision greater than this one.
    pub fn clear_anchor(&self, unstamp_revision: u64) -> Result<(), KeychainError> {
        self.change_anchor(|old| old.clear(unstamp_revision))
    }

    fn change_anchor(
        &self,
        change: impl FnOnce(AnchorState) -> Result<AnchorState, KeychainError>,
    ) -> Result<(), KeychainError> {
        let mut state = self.load()?.ok_or(KeychainError::Missing)?;
        state.anchor = change(state.anchor)?;
        #[cfg(target_os = "macos")]
        {
            self.inner.replace(&state)
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(KeychainError::Unsupported)
        }
    }

    /// Explicit deletion for operator recovery. Normal unstamp uses
    /// `clear_anchor`, so it cannot discard the high-water or sealing key.
    pub fn delete(&self) -> Result<(), KeychainError> {
        #[cfg(target_os = "macos")]
        {
            self.inner.delete()
        }
        #[cfg(not(target_os = "macos"))]
        {
            Err(KeychainError::Unsupported)
        }
    }
}

#[cfg(test)]
mod tests;
