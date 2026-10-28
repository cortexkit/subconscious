//! Compiled operator-app policy. Configuration and caller-provided peer facts
//! cannot enable shortcuts or establish an app's identity.

use semver::Version;
use subc_os::local_peer::{CapturedPeer, CodeRequirementOutcome, ObservedPeer};
use subc_protocol::peer::OperatorApp;

pub const MACOS_OPERATOR_APP_ENABLED: bool = false;
pub const WINDOWS_OPERATOR_APP_ENABLED: bool = false;
pub const LINUX_OPERATOR_APP_ENABLED: bool = false;
pub const PHONE_OPERATOR_ENABLED: bool = false;

/// No release is admitted until an enabling release replaces this sentinel.
pub const RELEASE_FLOOR: Version = Version::new(u64::MAX, u64::MAX, u64::MAX);

/// Hardened runtime is checked separately in the native verifier because it is
/// a signed CodeDirectory flag, not a code-requirement language predicate.
pub const MACOS_REQUIREMENT: &str = concat!(
    "anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] exists ",
    "and certificate leaf[field.1.2.840.113635.100.6.1.13] exists ",
    "and certificate leaf[subject.OU] = \"5R5846NBPW\" ",
    "and identifier \"io.cortexkit.alfonso.desktop\" ",
    "and entitlement[\"com.apple.security.get-task-allow\"] absent ",
    "and entitlement[\"com.apple.security.cs.disable-library-validation\"] absent ",
    "and entitlement[\"com.apple.security.cs.allow-dyld-environment-variables\"] absent"
);

#[derive(Clone, Copy, Debug)]
#[non_exhaustive]
pub struct CompiledApp {
    pub id: Option<&'static str>,
    pub macos_bundle_id: &'static str,
    pub macos_executable: &'static str,
    pub macos_bundle: &'static str,
    pub windows_executable: &'static str,
    pub linux_executable: &'static str,
    pub windows_publisher: Option<&'static str>,
    pub windows_path: Option<&'static str>,
    pub linux_path: Option<&'static str>,
    pub linux_release_index_identity: Option<&'static str>,
}

pub const COMPILED_APP: CompiledApp = CompiledApp {
    id: None,
    macos_bundle_id: "io.cortexkit.alfonso.desktop",
    macos_executable: "Alfonso",
    macos_bundle: "Alfonso.app",
    windows_executable: "Alfonso.exe",
    linux_executable: "alfonso",
    windows_publisher: None,
    windows_path: None,
    linux_path: None,
    linux_release_index_identity: None,
};

pub fn meets_release_floor(version: Option<&str>, floor: &Version) -> bool {
    // The reserved floor admits nothing, including its own numeric spelling.
    *floor != RELEASE_FLOOR
        && version
            .and_then(|version| Version::parse(version).ok())
            .is_some_and(|version| version.cmp_precedence(floor).is_ge())
}

#[derive(Clone, Debug)]
pub(crate) struct OperatorAppStamp {
    pub app: OperatorApp,
    peer: CapturedPeer,
    requirement: &'static str,
    floor: Version,
}

impl OperatorAppStamp {
    #[cfg(test)]
    pub(crate) fn fixture(app: &str, pid: u32) -> Self {
        Self {
            app: OperatorApp::new(app, pid),
            peer: ObservedPeer::Unobserved.into(),
            requirement: MACOS_REQUIREMENT,
            floor: RELEASE_FLOOR,
        }
    }

    /// A bind-time stamp is not authority. Prompt shortcuts must repeat both
    /// checks against the captured process incarnation at their decision point.
    #[allow(dead_code)]
    pub(crate) fn revalidate(&self) -> bool {
        matches!(self.peer.check_code_requirement(self.requirement),
            CodeRequirementOutcome::Satisfied(code)
                if meets_release_floor(code.short_version.as_deref(), &self.floor))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OperatorAppPolicy {
    pub macos_enabled: bool,
    app: CompiledApp,
    requirement: &'static str,
    floor: Version,
}

impl Default for OperatorAppPolicy {
    fn default() -> Self {
        Self {
            macos_enabled: MACOS_OPERATOR_APP_ENABLED,
            app: COMPILED_APP,
            requirement: MACOS_REQUIREMENT,
            floor: RELEASE_FLOOR,
        }
    }
}

impl OperatorAppPolicy {
    pub(crate) fn stamp(&self, peer: CapturedPeer) -> Option<OperatorAppStamp> {
        if !self.macos_enabled || !cfg!(target_os = "macos") {
            return None;
        }
        let app = self.app.id?;
        let ObservedPeer::Observed(observation) = peer.observation() else {
            return None;
        };
        let pid = observation.pid;
        let CodeRequirementOutcome::Satisfied(code) = peer.check_code_requirement(self.requirement)
        else {
            return None;
        };
        if !meets_release_floor(code.short_version.as_deref(), &self.floor) {
            return None;
        }
        Some(OperatorAppStamp {
            app: OperatorApp::new(app, pid),
            peer,
            requirement: self.requirement,
            floor: self.floor.clone(),
        })
    }
}

/// Explicit per-handler fixture policy, compiled only for unit tests or builds
/// with the `test-support` feature, and only in effect when a test passes it to
/// a handler. No environment variable or daemon configuration key can select it.
#[cfg(any(test, feature = "test-support"))]
#[derive(Clone, Debug)]
pub struct TestOperatorAppPolicy {
    pub macos_enabled: bool,
    pub app: &'static str,
    pub requirement: &'static str,
    pub floor: Version,
}

#[cfg(any(test, feature = "test-support"))]
impl From<TestOperatorAppPolicy> for OperatorAppPolicy {
    fn from(test: TestOperatorAppPolicy) -> Self {
        Self {
            macos_enabled: test.macos_enabled,
            app: CompiledApp {
                id: Some(test.app),
                ..COMPILED_APP
            },
            requirement: test.requirement,
            floor: test.floor,
        }
    }
}

#[cfg(test)]
mod tests;
