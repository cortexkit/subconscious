//! Blocking OS authentication for the daemon, isolated from module SDKs.
//!
//! The caller must run both [`prompt`] and [`Withdraw::withdraw`] off its async
//! executor. Publish the withdraw handle promptly: a timeout or disconnect may
//! already have happened while the operating system was being initialized.

#![deny(unsafe_code)]

use std::sync::Arc;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// The meaning assigned to an error returned by LocalAuthentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalAuthenticationFailure {
    Declined,
    NoPresence,
    ProviderError,
}

/// Only an explicit successful OS authentication approves a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Approved,
    Declined,
    ProviderError,
    NoPresence,
    UnsupportedPlatform,
    LocalAuthenticationError {
        failure: LocalAuthenticationFailure,
        domain: String,
        code: isize,
    },
}

/// Convert a LocalAuthentication callback into an outcome without depending on
/// macOS bindings, so its fail-closed error mapping is exercised on every host.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn local_authentication_outcome(success: bool, error: Option<(&str, isize)>) -> Outcome {
    let Some((domain, code)) = error else {
        return if success {
            Outcome::Approved
        } else {
            Outcome::ProviderError
        };
    };

    // LAErrorDomain and the LAError codes, written out so the mapping compiles
    // and is tested on hosts without the macOS bindings.
    const LA_ERROR_DOMAIN: &str = "com.apple.LocalAuthentication";
    const AUTHENTICATION_FAILED: isize = -1;
    const USER_CANCEL: isize = -2;
    // The system cancelled the prompt, as it does while the screen is locked.
    const SYSTEM_CANCEL: isize = -4;
    const NOT_INTERACTIVE: isize = -1004;
    let failure = if !success && domain == LA_ERROR_DOMAIN {
        match code {
            AUTHENTICATION_FAILED | USER_CANCEL => LocalAuthenticationFailure::Declined,
            SYSTEM_CANCEL | NOT_INTERACTIVE => LocalAuthenticationFailure::NoPresence,
            _ => LocalAuthenticationFailure::ProviderError,
        }
    } else {
        LocalAuthenticationFailure::ProviderError
    };

    Outcome::LocalAuthenticationError {
        failure,
        domain: domain.to_string(),
        code,
    }
}

/// May be invoked from another blocking thread, including during setup.
pub trait Withdraw: Send + Sync + 'static {
    fn withdraw(&self);
}

/// Show the exact daemon-constructed sentence and wait for the OS result.
/// No result is cached. The callback is invoked at most once.
pub fn prompt(text: &str, publish: Box<dyn FnOnce(Arc<dyn Withdraw>) + Send>) -> Outcome {
    #[cfg(target_os = "macos")]
    return macos::prompt(text, publish);
    #[cfg(windows)]
    return windows::prompt(text, publish);
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        // Linux authentication agents may display only a fixed policy message,
        // not the requested write. Do not ask for blind approval.
        let _ = (text, publish);
        Outcome::UnsupportedPlatform
    }
}

/// The Windows pre-check is deliberately a function of only these two facts.
/// Failure to find Hello alone still permits the secure-desktop fallback.
pub fn no_presence(hello_available: bool, input_desktop_available: bool) -> bool {
    !hello_available && !input_desktop_available
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_precheck_covers_all_four_combinations() {
        assert!(no_presence(false, false));
        assert!(!no_presence(false, true));
        assert!(!no_presence(true, false));
        assert!(!no_presence(true, true));
    }

    #[test]
    fn local_authentication_error_mapping_is_fail_closed() {
        let domain = "com.apple.LocalAuthentication";
        let cases = [
            ("success", true, None, Outcome::Approved),
            (
                "user cancel",
                false,
                Some((domain, -2)),
                Outcome::LocalAuthenticationError {
                    failure: LocalAuthenticationFailure::Declined,
                    domain: domain.into(),
                    code: -2,
                },
            ),
            (
                "authentication failed",
                false,
                Some((domain, -1)),
                Outcome::LocalAuthenticationError {
                    failure: LocalAuthenticationFailure::Declined,
                    domain: domain.into(),
                    code: -1,
                },
            ),
            (
                "system cancel",
                false,
                Some((domain, -4)),
                Outcome::LocalAuthenticationError {
                    failure: LocalAuthenticationFailure::NoPresence,
                    domain: domain.into(),
                    code: -4,
                },
            ),
            (
                "not interactive",
                false,
                Some((domain, -1004)),
                Outcome::LocalAuthenticationError {
                    failure: LocalAuthenticationFailure::NoPresence,
                    domain: domain.into(),
                    code: -1004,
                },
            ),
            (
                "unknown code",
                false,
                Some((domain, 123)),
                Outcome::LocalAuthenticationError {
                    failure: LocalAuthenticationFailure::ProviderError,
                    domain: domain.into(),
                    code: 123,
                },
            ),
            (
                "a cancel code from another error domain",
                false,
                Some(("NSOSStatusErrorDomain", -2)),
                Outcome::LocalAuthenticationError {
                    failure: LocalAuthenticationFailure::ProviderError,
                    domain: "NSOSStatusErrorDomain".into(),
                    code: -2,
                },
            ),
        ];

        for (case, success, error, expected) in cases {
            assert_eq!(
                local_authentication_outcome(success, error),
                expected,
                "{case}"
            );
        }
        assert_eq!(
            local_authentication_outcome(true, Some((domain, -4))),
            Outcome::LocalAuthenticationError {
                failure: LocalAuthenticationFailure::ProviderError,
                domain: domain.into(),
                code: -4,
            },
            "a success flag paired with an error must not approve"
        );
        assert_eq!(
            local_authentication_outcome(false, None),
            Outcome::ProviderError,
            "a failure without an error remains unavailable"
        );
    }

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn unsupported_platform_never_publishes_a_prompt() {
        assert_eq!(
            prompt(
                "approve a request from module: write (requested by a local program)",
                Box::new(|_| { panic!("unsupported platforms must not create a prompt") })
            ),
            Outcome::UnsupportedPlatform
        );
    }
}
