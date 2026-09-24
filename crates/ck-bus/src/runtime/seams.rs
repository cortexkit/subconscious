use std::{collections::BTreeSet, error::Error, fmt, sync::Arc};

use async_trait::async_trait;
use subc_protocol::session::HealthReport;

/// A typed error returned when a runtime capability has not yet been implemented.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AreaNotLanded {
    area: &'static str,
}

impl AreaNotLanded {
    pub const fn new(area: &'static str) -> Self {
        Self { area }
    }

    pub const fn area(&self) -> &'static str {
        self.area
    }
}

impl fmt::Display for AreaNotLanded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ck-bus runtime area '{}' not yet landed", self.area)
    }
}

impl Error for AreaNotLanded {}

pub type SeamResult<T> = Result<T, AreaNotLanded>;

#[async_trait]
pub trait CredentialMinting: Send + Sync {
    async fn reconcile_credentials(&self) -> SeamResult<()>;
}

#[async_trait]
pub trait Census: Send + Sync {
    async fn read_census(&self) -> SeamResult<()>;
    async fn write_census(&self) -> SeamResult<()>;
}

#[async_trait]
pub trait Revocation: Send + Sync {
    async fn resume_revocations(&self) -> SeamResult<()>;
}

/// Which of ck-bus's own users a grant is generated for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnUser {
    /// ck-bus's user in the box account.
    BusModule,
    /// ck-bus's user in the system account.
    SystemAccount,
}

/// A generated permission set: the allow lists a user JWT carries. There is no deny
/// list; absence from the allow lists is the denial.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GeneratedGrant {
    pub publish: BTreeSet<String>,
    pub subscribe: BTreeSet<String>,
}

/// Why the grant seam produced no grant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantSeamError {
    NotLanded(AreaNotLanded),
    /// The generator refused an input or its own output; the message names why.
    Refused(String),
}

impl fmt::Display for GrantSeamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotLanded(area) => area.fmt(f),
            Self::Refused(reason) => write!(f, "grant refused: {reason}"),
        }
    }
}

/// Generates the grant for one of ck-bus's own users, for the box account `account`
/// and the user's public key `user_public` (which names its inbox).
pub trait GrantGeneration: Send + Sync {
    fn own_user_grant(
        &self,
        user: OwnUser,
        account: &cortexkit_bus_naming::AccountNames,
        user_public: &str,
    ) -> Result<GeneratedGrant, GrantSeamError>;
}

#[async_trait]
pub trait SpawnStream: Send + Sync {
    async fn consume_spawn_stream(&self) -> SeamResult<()>;
}

#[async_trait]
pub trait LeafConfiguration: Send + Sync {
    async fn refresh_leaf_configuration(&self) -> SeamResult<()>;
}

#[async_trait]
pub trait SentinelHealth: Send + Sync {
    async fn report_health(&self) -> SeamResult<HealthReport>;
}

#[derive(Debug)]
struct RefusingArea {
    area: &'static str,
}

impl RefusingArea {
    const fn new(area: &'static str) -> Self {
        Self { area }
    }

    const fn refusal(&self) -> AreaNotLanded {
        AreaNotLanded::new(self.area)
    }
}

#[async_trait]
impl CredentialMinting for RefusingArea {
    async fn reconcile_credentials(&self) -> SeamResult<()> {
        Err(self.refusal())
    }
}

#[async_trait]
impl Census for RefusingArea {
    async fn read_census(&self) -> SeamResult<()> {
        Err(self.refusal())
    }

    async fn write_census(&self) -> SeamResult<()> {
        Err(self.refusal())
    }
}

#[async_trait]
impl Revocation for RefusingArea {
    async fn resume_revocations(&self) -> SeamResult<()> {
        Err(self.refusal())
    }
}

impl GrantGeneration for RefusingArea {
    fn own_user_grant(
        &self,
        _user: OwnUser,
        _account: &cortexkit_bus_naming::AccountNames,
        _user_public: &str,
    ) -> Result<GeneratedGrant, GrantSeamError> {
        Err(GrantSeamError::NotLanded(self.refusal()))
    }
}

#[async_trait]
impl SpawnStream for RefusingArea {
    async fn consume_spawn_stream(&self) -> SeamResult<()> {
        Err(self.refusal())
    }
}

#[async_trait]
impl LeafConfiguration for RefusingArea {
    async fn refresh_leaf_configuration(&self) -> SeamResult<()> {
        Err(self.refusal())
    }
}

#[async_trait]
impl SentinelHealth for RefusingArea {
    async fn report_health(&self) -> SeamResult<HealthReport> {
        Err(self.refusal())
    }
}

pub fn refusing_credential_minting() -> Arc<dyn CredentialMinting> {
    Arc::new(RefusingArea::new("credential-minting"))
}

pub fn refusing_census() -> Arc<dyn Census> {
    Arc::new(RefusingArea::new("census"))
}

pub fn refusing_revocation() -> Arc<dyn Revocation> {
    Arc::new(RefusingArea::new("revocation"))
}

pub fn refusing_grant_generation() -> Arc<dyn GrantGeneration> {
    Arc::new(RefusingArea::new("grant-generation"))
}

pub fn refusing_spawn_stream() -> Arc<dyn SpawnStream> {
    Arc::new(RefusingArea::new("spawn-stream"))
}

pub fn refusing_leaf_configuration() -> Arc<dyn LeafConfiguration> {
    Arc::new(RefusingArea::new("leaf-configuration"))
}

pub fn refusing_sentinel_health() -> Arc<dyn SentinelHealth> {
    Arc::new(RefusingArea::new("sentinel-health"))
}
