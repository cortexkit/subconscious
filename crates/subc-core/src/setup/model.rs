use std::{collections::BTreeMap, fmt};

use super::{
    detection,
    mc_detection::{self, McDetection},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReleaseResolutionStrategy {
    Latest,
    TagPrefix(&'static str),
}

/// The independently selectable pieces of an alpha CortexKit installation.
///
/// Core owns the daemon and MCP bridge. Every other entry is independently
/// addable, so adding one cannot disturb another component's known-good state.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Component {
    Core,
    Aft,
    Mc,
    Insula,
    Claustrum,
    Synapse,
}

impl Component {
    pub const ALL: [Self; 6] = [
        Self::Core,
        Self::Aft,
        Self::Mc,
        Self::Insula,
        Self::Claustrum,
        Self::Synapse,
    ];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Aft => "aft",
            Self::Mc => "mc",
            Self::Insula => "insula",
            Self::Claustrum => "claustrum",
            Self::Synapse => "synapse",
        }
    }

    pub const fn module_id(self) -> Option<&'static str> {
        match self {
            Self::Core => None,
            Self::Aft => Some("aft"),
            Self::Mc => Some("magic-context"),
            Self::Insula => Some("insula"),
            Self::Claustrum => Some("claustrum"),
            Self::Synapse => Some("synapse"),
        }
    }

    pub const fn repository(self) -> &'static str {
        match self {
            Self::Core => "subconscious",
            Self::Aft => "aft",
            Self::Mc => "magic-context",
            Self::Insula => "insula",
            Self::Claustrum => "claustrum",
            Self::Synapse => "synapse",
        }
    }

    pub const fn release_resolution_strategy(self) -> ReleaseResolutionStrategy {
        match self {
            // Owner-ruled: magic-context's releases/latest surface belongs to
            // its standalone npm product and its users' tooling. A fleet
            // channel must not squat on a repo's public Latest surface, so mc
            // module releases are prerelease-tagged and resolved by tag
            // pattern. Do not "simplify" this back to latest-resolution.
            Self::Mc => ReleaseResolutionStrategy::TagPrefix("ck-mc-"),
            Self::Core | Self::Aft | Self::Insula | Self::Claustrum | Self::Synapse => {
                ReleaseResolutionStrategy::Latest
            }
        }
    }

    pub const fn is_declared_unsupported_on(self, target: AlphaTarget) -> bool {
        matches!((self, target), (Self::Mc, AlphaTarget::WindowsX64))
    }

    pub const fn unavailable_message(self, target: AlphaTarget) -> Option<&'static str> {
        match (self, target) {
            (Self::Mc, AlphaTarget::WindowsX64) => {
                Some("magic-context: not available on windows in alpha")
            }
            _ => None,
        }
    }
}

impl fmt::Display for Component {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

/// The fixed alpha host tuples. Other hosts are refused before release lookup,
/// so an absent archive cannot be misreported as an unsupported platform.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AlphaTarget {
    DarwinArm64,
    LinuxX64,
    WindowsX64,
}

impl AlphaTarget {
    pub const fn label(self) -> &'static str {
        match self {
            Self::DarwinArm64 => "darwin-arm64",
            Self::LinuxX64 => "linux-x64",
            Self::WindowsX64 => "windows-x64",
        }
    }

    pub fn from_parts(os: &str, arch: &str) -> Option<Self> {
        match (os, arch) {
            ("macos" | "darwin", "aarch64" | "arm64") => Some(Self::DarwinArm64),
            ("linux", "x86_64" | "x64") => Some(Self::LinuxX64),
            ("windows", "x86_64" | "x64") => Some(Self::WindowsX64),
            _ => None,
        }
    }
}

impl fmt::Display for AlphaTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HostTarget {
    pub os: String,
    pub arch: String,
}

impl HostTarget {
    pub fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_string(),
            arch: std::env::consts::ARCH.to_string(),
        }
    }
}

impl fmt::Display for HostTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}-{}", self.os, self.arch)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlatformObservation {
    Supported(AlphaTarget),
    Unsupported(HostTarget),
}

impl PlatformObservation {
    pub fn current() -> Self {
        let host = HostTarget::current();
        AlphaTarget::from_parts(&host.os, &host.arch)
            .map(Self::Supported)
            .unwrap_or(Self::Unsupported(host))
    }
}

impl fmt::Display for PlatformObservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Supported(target) => target.fmt(formatter),
            Self::Unsupported(target) => target.fmt(formatter),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComponentState {
    Missing,
    Correct,
}

/// Release resolution stays explicit so a missing archive is never reported as
/// a broken installation or as a permanently unsupported host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseAvailability {
    Available,
    Incomplete {
        missing_asset: String,
    },
    NotYetPublished {
        release_tag: String,
        missing_asset: String,
    },
    /// A component can be intentionally unavailable on a host without querying
    /// its release repository.
    NotRequired,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeState {
    Missing,
    Correct,
}

// Filesystem configuration probing is supplied by the setup backend. Keeping
// conflicts in the model prevents an executor from treating a proposed write as
// authorization to overwrite a user-owned key.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConfigurationState {
    Additive,
    Conflict { key: String },
}

/// Read-only standalone-detection evidence. Detection can affect an offer, but
/// it never authorizes the corresponding installation mutation on its own.
#[derive(Clone, Debug, Eq, PartialEq)]
#[allow(dead_code)]
pub enum DetectionOutcome {
    None,
    OfferConversion,
    InstalledAndLive,
    Unknown,
    OwnerGated { reason: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupObserved {
    pub platform: PlatformObservation,
    pub components: BTreeMap<Component, ComponentState>,
    pub releases: BTreeMap<Component, ReleaseAvailability>,
    pub runtime: RuntimeState,
    pub configuration: ConfigurationState,
    /// Retains the MC database probe result so the planner can distinguish an
    /// absent installation from a state that is unsafe for automatic conversion.
    pub mc_detection: Option<McDetection>,
    pub detections: BTreeMap<Component, DetectionOutcome>,
}

impl SetupObserved {
    /// A safe host snapshot for the command surface before the installation
    /// backend supplies manifest and filesystem probes. It reads host facts and
    /// the MC database through the non-mutating detector, then deliberately
    /// assumes no managed state rather than inferring ownership from user data.
    pub fn unconfigured_current_host() -> Self {
        let mut components = BTreeMap::new();
        let mut releases = BTreeMap::new();
        for component in Component::ALL {
            components.insert(component, ComponentState::Missing);
            releases.insert(component, ReleaseAvailability::Available);
        }
        let mc_detection = mc_detection::detect_current();
        let mut detections = BTreeMap::new();
        // AFT automatic detection is disabled for alpha. Its owner has not
        // supplied the marker contract needed to avoid false-positive conversion.
        detections.insert(
            Component::Mc,
            detection::mc_detection_outcome(&mc_detection),
        );
        Self {
            platform: PlatformObservation::current(),
            components,
            releases,
            runtime: RuntimeState::Missing,
            configuration: ConfigurationState::Additive,
            mc_detection: Some(mc_detection),
            detections,
        }
    }

    pub fn component_state(&self, component: Component) -> ComponentState {
        self.components
            .get(&component)
            .copied()
            .unwrap_or(ComponentState::Missing)
    }

    pub fn release(&self, component: Component) -> ReleaseAvailability {
        self.releases
            .get(&component)
            .cloned()
            .unwrap_or(ReleaseAvailability::Available)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetupRequest {
    pub optional_components: Vec<Component>,
    pub uninstall: bool,
    pub dry_run: bool,
    pub convert: Option<Component>,
    pub conversion_confirmed: bool,
    /// The one key-file answer used for both claustrum bootstrap and daemon env.
    pub claustrum_key_path: Option<std::path::PathBuf>,
}

impl SetupRequest {
    pub fn install(optional_components: Vec<Component>) -> Self {
        Self {
            optional_components,
            uninstall: false,
            dry_run: false,
            convert: None,
            conversion_confirmed: false,
            claustrum_key_path: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlanOutcome {
    UnsupportedPlatform {
        target: HostTarget,
    },
    ReleaseIncomplete {
        component: Component,
        release_tag: String,
        missing_asset: String,
    },
    DeclaredUnavailable {
        component: Component,
        message: String,
    },
    Refusal {
        reason: String,
    },
    Noop {
        scope: String,
    },
    OwnerGatedDetection {
        component: Component,
        reason: String,
    },
}

impl PlanOutcome {
    pub fn blocks_execution(&self) -> bool {
        matches!(
            self,
            Self::UnsupportedPlatform { .. }
                | Self::ReleaseIncomplete { .. }
                | Self::Refusal { .. }
        )
    }
}

impl fmt::Display for PlanOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedPlatform { target } => {
                write!(formatter, "unsupported-platform: {target}")
            }
            Self::ReleaseIncomplete {
                component,
                release_tag,
                missing_asset,
            } => write!(
                formatter,
                "{component}: no {missing_asset} asset in {release_tag} yet — the module's owner has not published this platform"
            ),
            Self::DeclaredUnavailable { message, .. } => formatter.write_str(message),
            Self::Refusal { reason } => write!(formatter, "refusal: {reason}"),
            Self::Noop { scope } => write!(formatter, "no-op: {scope}"),
            Self::OwnerGatedDetection { component, reason } => {
                write!(formatter, "owner-gated detection: {component}: {reason}")
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SetupOperation {
    ObservePlatform,
    OfferOptionalComponents,
    OfferConversion {
        component: Component,
    },
    ConfirmConversion {
        component: Component,
    },
    InstallComponent {
        component: Component,
    },
    ConfigureComponent {
        component: Component,
    },
    BootstrapClaustrum {
        key_path: Option<std::path::PathBuf>,
    },
    RescanComponent {
        component: Component,
    },
    EnableComponent {
        component: Component,
    },
    RegisterRuntime,
    StartRuntime,
    Validate {
        instrument: &'static str,
    },
    DeregisterRuntime,
    RemoveManagedComponent {
        component: Component,
    },
    RetainUserData,
}

impl SetupOperation {
    pub const fn mutates(&self) -> bool {
        matches!(
            self,
            Self::InstallComponent { .. }
                | Self::ConfigureComponent { .. }
                | Self::BootstrapClaustrum { .. }
                | Self::RescanComponent { .. }
                | Self::EnableComponent { .. }
                | Self::RegisterRuntime
                | Self::StartRuntime
                | Self::DeregisterRuntime
                | Self::RemoveManagedComponent { .. }
        )
    }
}

impl fmt::Display for SetupOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservePlatform => formatter.write_str("observe alpha platform support"),
            Self::OfferOptionalComponents => {
                // One line per component so a caveat reads against its owner
                // rather than swallowing the components listed after it.
                formatter.write_str(
                    "offer optional components:\n\
                     \x20 aft\n\
                     \x20 mc\n\
                     \x20 insula — browser-cookie providers dark by construction on Windows (Chrome App-Bound Encryption); file/API providers full; cookie lane via claustrum deposit\n\
                     \x20 claustrum\n\
                     \x20 synapse — healthy immediately with an empty catalog; inference remains typed-refused until model.load arrives",
                )
            }
            Self::OfferConversion { component } => {
                write!(formatter, "offer standalone {component} conversion")
            }
            Self::ConfirmConversion { component } => {
                write!(formatter, "confirm explicit {component} conversion")
            }
            Self::InstallComponent { component } => write!(formatter, "install {component}"),
            Self::ConfigureComponent { component } => write!(formatter, "configure {component}"),
            Self::BootstrapClaustrum {
                key_path: Some(key_path),
            } => write!(
                formatter,
                "bootstrap claustrum with ck auth bootstrap --key-path {}",
                key_path.display()
            ),
            Self::BootstrapClaustrum { key_path: None } => {
                formatter.write_str("bootstrap claustrum with ck auth bootstrap")
            }
            Self::RescanComponent { component } => {
                write!(formatter, "rescan {component} module entry")
            }
            Self::EnableComponent { component } => write!(formatter, "enable {component} module"),
            Self::RegisterRuntime => formatter.write_str("register the per-user daemon runtime"),
            Self::StartRuntime => formatter.write_str("start the per-user daemon runtime"),
            Self::Validate { instrument } => write!(formatter, "validate with {instrument}"),
            Self::DeregisterRuntime => {
                formatter.write_str("deregister the managed per-user runtime")
            }
            Self::RemoveManagedComponent { component } => {
                write!(
                    formatter,
                    "remove manifest-owned {component} binaries and links"
                )
            }
            Self::RetainUserData => {
                formatter.write_str("retain user configuration and component stores")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpgradeTarget {
    SubcMcp,
    Aft,
    Daemon,
    Ck,
}

impl UpgradeTarget {
    /// MC is intentionally absent: alpha wires it but has no MC release archive.
    pub const ORDERED: [Self; 4] = [Self::SubcMcp, Self::Aft, Self::Daemon, Self::Ck];

    pub const fn label(self) -> &'static str {
        match self {
            Self::SubcMcp => "ck-subc-mcp",
            Self::Aft => "ck-aft",
            Self::Daemon => "ck-subc",
            Self::Ck => "ck",
        }
    }
}

impl fmt::Display for UpgradeTarget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.label())
    }
}

// Version discovery arrives with the release backend; the planner still owns
// the update-available state so ordering never depends on that backend's output.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpgradeState {
    NotInstalled,
    Current,
    UpdateAvailable { from: String, to: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpgradeObserved {
    pub platform: PlatformObservation,
    pub targets: BTreeMap<String, UpgradeState>,
    pub releases: BTreeMap<String, ReleaseAvailability>,
}

impl UpgradeObserved {
    pub fn no_updates_on_current_host() -> Self {
        let mut targets = BTreeMap::new();
        let mut releases = BTreeMap::new();
        for target in UpgradeTarget::ORDERED {
            targets.insert(target.label().to_string(), UpgradeState::Current);
            releases.insert(target.label().to_string(), ReleaseAvailability::Available);
        }
        Self {
            platform: PlatformObservation::current(),
            targets,
            releases,
        }
    }

    pub fn target_state(&self, target: UpgradeTarget) -> UpgradeState {
        self.targets
            .get(target.label())
            .cloned()
            .unwrap_or(UpgradeState::NotInstalled)
    }

    pub fn release(&self, target: UpgradeTarget) -> ReleaseAvailability {
        self.releases
            .get(target.label())
            .cloned()
            .unwrap_or(ReleaseAvailability::Available)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UpgradeOperation {
    ObservePlatform,
    DownloadAndVerify { target: UpgradeTarget },
    CreateRollbackCopy { target: UpgradeTarget },
    ReplaceDestination { target: UpgradeTarget },
    WarmExecute { target: UpgradeTarget },
    InitiateModuleRestart { target: UpgradeTarget },
    PollModuleRestartCompletion { target: UpgradeTarget },
    RestartDaemonViaServiceManager,
    PollDaemonServiceReady,
    PostVerify { target: UpgradeTarget },
}

impl UpgradeOperation {
    pub const fn mutates(&self) -> bool {
        !matches!(self, Self::ObservePlatform)
    }
}

impl fmt::Display for UpgradeOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObservePlatform => formatter.write_str("observe alpha platform support"),
            Self::DownloadAndVerify { target } => {
                write!(
                    formatter,
                    "download and verify {target} archive and sidecar"
                )
            }
            Self::CreateRollbackCopy { target } => {
                write!(formatter, "create {target} rollback copy")
            }
            Self::ReplaceDestination { target } => {
                write!(formatter, "replace {target} destination")
            }
            Self::WarmExecute { target } => write!(formatter, "warm-execute {target} destination"),
            Self::InitiateModuleRestart { target } => {
                write!(formatter, "initiate supervised restart for {target}")
            }
            Self::PollModuleRestartCompletion { target } => {
                write!(formatter, "poll supervised restart completion for {target}")
            }
            Self::RestartDaemonViaServiceManager => {
                formatter.write_str("restart ck-subc through the platform service manager")
            }
            Self::PollDaemonServiceReady => {
                formatter.write_str("poll ck-subc service-manager completion")
            }
            Self::PostVerify { target } => write!(formatter, "post-verify {target}"),
        }
    }
}
