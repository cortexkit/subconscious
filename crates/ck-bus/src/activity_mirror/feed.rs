//! Activity query and subscription interfaces used by the daemon source and mirror.
//! Keeping them separate lets the supervised reader fixture exercise the production
//! source without also starting a broker.

use async_trait::async_trait;
use subc_client_rs::{ActivityCursor, ActivityEntry, ActivityQueryResponse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedEnd {
    CursorRefused,
    Lagged,
    Failed(String),
}

#[async_trait]
pub trait Source: Send + Sync {
    async fn query(&self, before: Option<u64>, limit: u32)
        -> Result<ActivityQueryResponse, String>;
    async fn subscribe(&self, since: ActivityCursor) -> Result<Box<dyn Feed>, FeedEnd>;
}

#[async_trait]
pub trait Feed: Send {
    async fn next(&mut self) -> Result<Option<ActivityEntry>, FeedEnd>;
}
