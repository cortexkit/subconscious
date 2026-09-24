//! The daemon side of the spawn consumer: `supervisor.spawn_snapshot` and
//! `supervisor.spawn_subscribe` over ck-bus's own client connection.

use std::path::PathBuf;

use async_trait::async_trait;
use subc_client_rs::consumer::{
    ConsumerOptions, SpawnCursor, SpawnEvent, SpawnSnapshot, SpawnStreamError, SpawnSubscription,
    SubcConsumer,
};
use tokio::sync::OnceCell;

use super::{FeedEnd, SpawnFeed, SpawnSource};

/// How a stream error reads to the consumer. Classified by the daemon's code, so a
/// refusal whose detail did not parse is still a refusal.
pub fn classify(error: &SpawnStreamError) -> FeedEnd {
    match error.code() {
        Some(subc_client_rs::SPAWN_CURSOR_INCARNATION_MISMATCH)
        | Some(subc_client_rs::SPAWN_CURSOR_TOO_OLD) => FeedEnd::CursorRefused {
            code: error.code().unwrap_or_default().to_string(),
        },
        Some(subc_client_rs::SPAWN_SUBSCRIBER_LAGGED) => FeedEnd::Lagged,
        _ => FeedEnd::Failed(error.to_string()),
    }
}

pub struct DaemonSource {
    connection_file: PathBuf,
    consumer: OnceCell<SubcConsumer>,
}

impl DaemonSource {
    pub fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            consumer: OnceCell::new(),
        }
    }

    async fn consumer(&self) -> Result<&SubcConsumer, String> {
        self.consumer
            .get_or_try_init(|| async {
                SubcConsumer::connect(&self.connection_file, ConsumerOptions::default()).await
            })
            .await
            .map_err(|error| format!("cannot reach the daemon: {error}"))
    }
}

#[async_trait]
impl SpawnSource for DaemonSource {
    async fn snapshot(&self) -> Result<SpawnSnapshot, String> {
        self.consumer()
            .await?
            .spawn_snapshot()
            .await
            .map_err(|error| format!("supervisor.spawn_snapshot: {error}"))
    }

    async fn subscribe(&self, since: SpawnCursor) -> Result<Box<dyn SpawnFeed>, FeedEnd> {
        let consumer = self.consumer().await.map_err(FeedEnd::Failed)?;
        let subscription = consumer
            .spawn_subscribe(Some(since))
            .await
            .map_err(|error| FeedEnd::Failed(format!("supervisor.spawn_subscribe: {error}")))?;
        Ok(Box::new(DaemonFeed(subscription)))
    }
}

struct DaemonFeed(SpawnSubscription);

#[async_trait]
impl SpawnFeed for DaemonFeed {
    async fn next(&mut self) -> Result<Option<SpawnEvent>, FeedEnd> {
        self.0.next().await.map_err(|error| classify(&error))
    }
}
