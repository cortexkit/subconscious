use std::{collections::BTreeMap, fmt};

/// The independently selectable pieces of an alpha CortexKit installation.
///
/// Core owns the daemon and MCP bridge. AFT and MC are optional so adding one
/// later can leave the other component's known-good state untouched.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Component {
    Core,
    Aft,
    Mc,
}

impl Component {
    pub const ALL: [Self; 3] = [Self::Core, Self::Aft, Self::Mc];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Core => "core",
            Self::Aft => "aft",
            Self::Mc => "mc",
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

// The platform-only observer cannot yet fetch release metadata, but the
// planner keeps this state explicit so missing assets cannot be collapsed into
// an unsupported-platform result when a release observer is connected.
#[allow(dead_code)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReleaseAvailability {
    Available,
    Incomplete {
        missing_asset: String,
    },
    /// MC is wiring-only in alpha and must never acquire an archive.
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
    pub detections: BTreeMap<Component, DetectionOutcome>,
}

impl SetupObserved {
    /// A safe host snapshot for the command surface before the installation
    /// backend supplies manifest and filesystem probes. It reads only compile-time
    /// host facts and deliberately assumes no managed state rather than inferring
    /// ownership from user configuration or stores.
    pub fn unconfigured_current_host() -> Self {
        let mut components = BTreeMap::new();
        let mut releases = BTreeMap::new();
        for component in Component::ALL {
            components.insert(component, ComponentState::Missing);
            releases.insert(
                component,
                if component == Component::Mc {
                    ReleaseAvailability::NotRequired
                } else {
                    ReleaseAvailability::Available
                },
            );
        }
        let mut detections = BTreeMap::new();
        detections.insert(
            Component::Aft,
            DetectionOutcome::OwnerGated {
                reason: "automatic AFT detection is disabled until its owner supplies a detector contract"
                    .to_string(),
            },
        );
        detections.insert(Component::Mc, DetectionOutcome::None);
        Self {
            platform: PlatformObservation::current(),
            components,
            releases,
            runtime: RuntimeState::Missing,
            configuration: ConfigurationState::Additive,
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
}

impl SetupRequest {
    pub fn install(optional_components: Vec<Component>) -> Self {
        Self {
            optional_components,
            uninstall: false,
            dry_run: false,
            convert: None,
            conversion_confirmed: false,
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
        missing_asset: String,
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
                missing_asset,
            } => write!(
                formatter,
                "release-incomplete: {component} is missing {missing_asset}"
            ),
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
    ConfirmConversion { component: Component },
    InstallComponent { component: Component },
    ConfigureComponent { component: Component },
    RegisterRuntime,
    StartRuntime,
    Validate { instrument: &'static str },
    DeregisterRuntime,
    RemoveManagedComponent { component: Component },
    RetainUserData,
}

impl SetupOperation {
    pub const fn mutates(&self) -> bool {
        matches!(
            self,
            Self::InstallComponent { .. }
                | Self::ConfigureComponent { .. }
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
                formatter.write_str("offer optional components: aft, mc")
            }
            Self::ConfirmConversion { component } => {
                write!(formatter, "confirm explicit {component} conversion")
            }
            Self::InstallComponent { component } => write!(formatter, "install {component}"),
            Self::ConfigureComponent { component } => write!(formatter, "configure {component}"),
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
