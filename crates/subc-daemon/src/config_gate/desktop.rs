//! Development identity is an explicit constructor input, never a client claim.

#[cfg(any(feature = "dev-desktop-approval", test))]
use super::{GateError, Result};
#[cfg(any(feature = "dev-desktop-approval", test))]
use subc_os::local_peer::CapturedPeer;

/// No production development-desktop identity policy has been specified. The
/// shipped daemon leaves this value unset even when the Cargo feature is on.
#[derive(Clone, Default)]
pub enum DevDesktopVerifier {
    #[default]
    Unspecified,
    /// A trusted embedding/test caller supplies identity verification. Owner
    /// credentials, complete-plan delivery and OS presence are still required.
    #[cfg(feature = "dev-desktop-approval")]
    Supplied(std::sync::Arc<dyn DesktopVerifier>),
}

impl std::fmt::Debug for DevDesktopVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unspecified => f.write_str("Unspecified"),
            #[cfg(feature = "dev-desktop-approval")]
            Self::Supplied(_) => f.write_str("Supplied"),
        }
    }
}

/// Verifies the retained peer incarnation, not an advisory path or numeric PID.
/// Only the daemon constructor can provide an implementation of this seam.
#[cfg(feature = "dev-desktop-approval")]
pub trait DesktopVerifier: Send + Sync + 'static {
    fn verify(&self, peer: &CapturedPeer) -> bool;
}

impl DevDesktopVerifier {
    #[cfg(any(feature = "dev-desktop-approval", test))]
    pub(crate) fn verify(&self, _peer: Option<&CapturedPeer>) -> Result<bool> {
        match self {
            Self::Unspecified => Err(GateError::new(
                "dev_desktop_verifier_unspecified",
                "development desktop identity verifier is unspecified",
            )),
            #[cfg(feature = "dev-desktop-approval")]
            Self::Supplied(verifier) => Ok(_peer.is_some_and(|peer| verifier.verify(peer))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_desktop_development_identity_has_no_implicit_verifier() {
        // Even an observed owner user is insufficient without a verifier.
        let peer = subc_os::local_peer::ObservedPeer::Unobserved.into();
        assert_eq!(
            DevDesktopVerifier::default()
                .verify(Some(&peer))
                .unwrap_err()
                .code,
            "dev_desktop_verifier_unspecified"
        );
    }
}
