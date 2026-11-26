#![forbid(unsafe_code)]

#[cfg(unix)]
mod doctor_cli {
    pub(crate) fn test_settings() -> (Option<std::ffi::OsString>, bool) {
        let (github_api, fail_sigterm) = super::ck::doctor_cli::test_settings();
        (
            std::env::var_os("CK_DOCTOR_TEST_GITHUB_API").or(github_api),
            std::env::var_os("CK_TEST_FAIL_SIGTERM_REGISTRATION").is_some() || fail_sigterm,
        )
    }
}

// Both CLI binaries use the same implementation and forbid unsafe code.
#[path = "ck.rs"]
mod ck;

fn main() {
    ck::main();
}
