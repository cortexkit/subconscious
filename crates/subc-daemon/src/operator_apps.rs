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

/// The platform an `app.open` decision runs for. macOS never launches through
/// the daemon; any other platform without a launcher falls back the same way.
///
/// Every variant is constructed: one per build in `AppOpenPlatform::current`,
/// and the rest in tests, which drive every platform on every host. (Check
/// builds without test code would otherwise call the idle variants dead.)
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AppOpenPlatform {
    MacOS,
    Windows,
    Linux,
    Other,
}

impl AppOpenPlatform {
    pub fn current() -> Self {
        #[cfg(target_os = "macos")]
        return Self::MacOS;
        #[cfg(windows)]
        return Self::Windows;
        #[cfg(target_os = "linux")]
        return Self::Linux;
        #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
        return Self::Other;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OperatorAppStamp {
    pub app: OperatorApp,
    peer: CapturedPeer,
    requirement: &'static str,
    floor: Version,
    /// The launch entry incarnation this stamp matched, present only for
    /// stamps derived from a live Windows launch entry. macOS stamps carry
    /// `None` and check the captured audit token instead.
    pub(crate) launch_incarnation: Option<u64>,
}

impl OperatorAppStamp {
    #[cfg(test)]
    pub(crate) fn fixture(app: &str, pid: u32) -> Self {
        Self {
            app: OperatorApp::new(app, pid),
            peer: ObservedPeer::Unobserved.into(),
            requirement: MACOS_REQUIREMENT,
            floor: RELEASE_FLOOR,
            launch_incarnation: None,
        }
    }

    /// A stamp for a pipe client whose pid matches a live Windows launch
    /// entry. Linux never stamps, so every `Some` incarnation on the wire is
    /// a Windows launch.
    #[cfg(windows)]
    pub(crate) fn launch_stamp(app: OperatorApp, peer: CapturedPeer, incarnation: u64) -> Self {
        Self {
            app,
            peer,
            requirement: MACOS_REQUIREMENT,
            floor: RELEASE_FLOOR,
            launch_incarnation: Some(incarnation),
        }
    }

    /// Which platform switch gates delivering this stamp on `route.bind`.
    /// The stamp is a connect-time fact; the switch is checked again at relay
    /// so turning it off stops delivery without dropping connections.
    pub(crate) fn delivery_enabled(&self, policy: &OperatorAppPolicy) -> bool {
        if self.launch_incarnation.is_some() {
            policy.windows_enabled
        } else {
            policy.macos_enabled
        }
    }

    /// A bind-time stamp is not authority: Windows must still name its own live
    /// launch entry, while macOS checks the captured process token again.
    pub(crate) fn revalidate_with_launches(&self, launches: &crate::app_open::LaunchTable) -> bool {
        if let Some(incarnation) = self.launch_incarnation {
            return launches.is_live_incarnation(&self.app.app, incarnation);
        }
        self.revalidate()
    }

    pub(crate) fn revalidate(&self) -> bool {
        matches!(self.peer.check_code_requirement(self.requirement),
            CodeRequirementOutcome::Satisfied(code)
                if meets_release_floor(code.short_version.as_deref(), &self.floor))
    }
}

#[derive(Clone, Debug)]
pub(crate) struct OperatorAppPolicy {
    pub macos_enabled: bool,
    pub windows_enabled: bool,
    pub linux_enabled: bool,
    pub phone_enabled: bool,
    app: CompiledApp,
    requirement: &'static str,
    floor: Version,
    /// Where the installed app lives. The production table leaves both unset;
    /// a known app with no location answers `app_not_installed` once its
    /// platform switch is on.
    pub windows_path: Option<std::path::PathBuf>,
    pub linux_path: Option<std::path::PathBuf>,
    /// Whether the Windows spawn sets `CREATE_BREAKAWAY_FROM_JOB`.
    /// Production always breaks away and fails closed when the job forbids
    /// it. Tests can omit the bit where the job running them forbids it: a
    /// test handler creates no module job, so a plain spawn is already
    /// outside one. Read only by the Windows launcher.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub windows_breakaway: bool,
    /// Tests force the direct-spawn path so a Linux launch test never depends
    /// on the build machine running a user manager. Production always prefers
    /// a transient scope when one is available. Only the Linux launcher reads
    /// it, so other platforms silence the idle field.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub linux_force_direct: bool,
}

impl Default for OperatorAppPolicy {
    fn default() -> Self {
        Self {
            macos_enabled: MACOS_OPERATOR_APP_ENABLED,
            windows_enabled: WINDOWS_OPERATOR_APP_ENABLED,
            linux_enabled: LINUX_OPERATOR_APP_ENABLED,
            phone_enabled: PHONE_OPERATOR_ENABLED,
            app: COMPILED_APP,
            requirement: MACOS_REQUIREMENT,
            floor: RELEASE_FLOOR,
            windows_path: COMPILED_APP.windows_path.map(std::path::PathBuf::from),
            linux_path: COMPILED_APP.linux_path.map(std::path::PathBuf::from),
            windows_breakaway: true,
            linux_force_direct: false,
        }
    }
}

impl OperatorAppPolicy {
    /// The app id this policy launches and stamps, or `None` for the
    /// production table. With the platform switch on, a `None` id answers
    /// `app_unknown` on Windows and Linux; with every switch off, the daemon
    /// answers `app_open_disabled` first. macOS answers `app_open_not_used`
    /// before reaching it.
    pub(crate) fn app_id(&self) -> Option<&'static str> {
        self.app.id
    }

    #[cfg(windows)]
    pub(crate) fn release_floor(&self) -> &Version {
        &self.floor
    }

    pub(crate) fn installed_path(&self, platform: AppOpenPlatform) -> Option<std::path::PathBuf> {
        match platform {
            AppOpenPlatform::Windows => self.windows_path.clone(),
            AppOpenPlatform::Linux => self.linux_path.clone(),
            AppOpenPlatform::MacOS | AppOpenPlatform::Other => None,
        }
    }

    pub(crate) fn switch_enabled(&self, platform: AppOpenPlatform) -> bool {
        match platform {
            AppOpenPlatform::MacOS => self.macos_enabled,
            AppOpenPlatform::Windows => self.windows_enabled,
            AppOpenPlatform::Linux => self.linux_enabled,
            AppOpenPlatform::Other => false,
        }
    }

    /// The expected Authenticode publisher for the installed Windows binary.
    /// `None` (the production value until a certificate exists) verifies
    /// nothing: with a test table the tests supply it, with the production
    /// table `app.open` never reaches verification.
    #[cfg(windows)]
    pub(crate) fn windows_publisher(&self) -> Option<&'static str> {
        self.app.windows_publisher
    }

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
            launch_incarnation: None,
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
    pub windows_enabled: bool,
    pub linux_enabled: bool,
    pub app: &'static str,
    pub requirement: &'static str,
    pub floor: Version,
    pub windows_publisher: Option<&'static str>,
    pub windows_path: Option<std::path::PathBuf>,
    pub linux_path: Option<std::path::PathBuf>,
    pub windows_breakaway: bool,
    pub linux_force_direct: bool,
}

#[cfg(any(test, feature = "test-support"))]
impl From<TestOperatorAppPolicy> for OperatorAppPolicy {
    fn from(test: TestOperatorAppPolicy) -> Self {
        Self {
            macos_enabled: test.macos_enabled,
            windows_enabled: test.windows_enabled,
            linux_enabled: test.linux_enabled,
            phone_enabled: PHONE_OPERATOR_ENABLED,
            app: CompiledApp {
                id: Some(test.app),
                windows_publisher: test.windows_publisher,
                ..COMPILED_APP
            },
            requirement: test.requirement,
            floor: test.floor,
            windows_path: test.windows_path,
            linux_path: test.linux_path,
            windows_breakaway: test.windows_breakaway,
            linux_force_direct: test.linux_force_direct,
        }
    }
}

#[cfg(test)]
mod tests;
