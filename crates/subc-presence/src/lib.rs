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

/// Only an explicit successful OS authentication approves a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Approved,
    Declined,
    ProviderError,
    NoPresence,
    UnsupportedPlatform,
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

    #[cfg(not(any(target_os = "macos", windows)))]
    #[test]
    fn unsupported_platform_never_publishes_a_prompt() {
        assert_eq!(
            prompt(
                "module asks: write (requested by a local program)",
                Box::new(|_| { panic!("unsupported platforms must not create a prompt") })
            ),
            Outcome::UnsupportedPlatform
        );
    }
}
