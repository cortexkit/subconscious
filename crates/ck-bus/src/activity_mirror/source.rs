//! Operator-authorized ledger reads over ck-bus's separate consumer connection.

use super::{Feed, FeedEnd, Source};
use async_trait::async_trait;
use std::{path::PathBuf, sync::Once};
use subc_client_rs::{
    consumer::{ConsumerOptions, SubcConsumer},
    ActivityCursor, ActivityEntry, ActivityQueryRequest, ActivityQueryResponse,
    ActivityStreamError, ActivitySubscription,
};
use tokio::sync::OnceCell;

pub fn classify(code: Option<&str>, message: String) -> FeedEnd {
    match code {
        Some(
            subc_client_rs::ACTIVITY_CURSOR_INCARNATION_MISMATCH
            | subc_client_rs::ACTIVITY_CURSOR_TOO_OLD,
        ) => FeedEnd::CursorRefused,
        Some(subc_client_rs::ACTIVITY_SUBSCRIBER_LAGGED) => FeedEnd::Lagged,
        _ => FeedEnd::Failed(message),
    }
}

pub struct DaemonSource {
    connection_file: PathBuf,
    consumer: OnceCell<SubcConsumer>,
    unverified_warning: Once,
}
impl DaemonSource {
    pub fn new(connection_file: PathBuf) -> Self {
        Self {
            connection_file,
            consumer: OnceCell::new(),
            unverified_warning: Once::new(),
        }
    }
    fn warn_unverified(
        &self,
        identity: Option<&subc_client_rs::ConsumerIdentity>,
        emit: impl FnOnce(),
    ) {
        if identity.is_none() {
            self.unverified_warning.call_once(emit);
        }
    }
    async fn consumer(&self) -> Result<&SubcConsumer, String> {
        self.consumer
            .get_or_try_init(|| async {
                // query_activity and subscribe_activity add the launch identity
                // themselves. Read the SDK's cached nonce first: missing identity
                // gets a warning, while a nonce-read error stops the connection
                // instead of omitting identity and reading as a local caller (Direct).
                let identity = crate::credentials::identity::supervised_identity()?;
                self.warn_unverified(identity.as_ref(), || {
                    eprintln!("{}", serde_json::json!({
                        "event": "ckbus.activity.unverified_local_caller",
                        "level": "warn",
                        "message": "activity mirror reads as an unverified local caller: no supervised launch identity is available",
                    }));
                });
                SubcConsumer::connect(&self.connection_file, ConsumerOptions::default())
                    .await.map_err(|err| err.to_string())
            })
            .await
    }
}
#[async_trait]
impl Source for DaemonSource {
    async fn query(
        &self,
        before: Option<u64>,
        limit: u32,
    ) -> Result<ActivityQueryResponse, String> {
        self.consumer()
            .await?
            .query_activity(ActivityQueryRequest {
                agent: None,
                project: None,
                module: None,
                kind: None,
                since_ms: None,
                until_ms: None,
                limit,
                cursor: before,
            })
            .await
            .map_err(|err| err.to_string())
    }
    async fn subscribe(&self, since: ActivityCursor) -> Result<Box<dyn Feed>, FeedEnd> {
        let consumer = self.consumer().await.map_err(FeedEnd::Failed)?;
        let subscription = consumer
            .subscribe_activity(Some(since))
            .await
            .map_err(|err| classify(err.code(), err.to_string()))?;
        Ok(Box::new(DaemonFeed(subscription)))
    }
}
struct DaemonFeed(ActivitySubscription);
#[async_trait]
impl Feed for DaemonFeed {
    async fn next(&mut self) -> Result<Option<ActivityEntry>, FeedEnd> {
        self.0
            .next()
            .await
            .map_err(|err: ActivityStreamError| classify(err.code(), err.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::DaemonSource;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn unverified_activity_reader_warns_once_across_connection_retries() {
        let source = DaemonSource::new("unused-connection.json".into());
        let warnings = AtomicUsize::new(0);
        let verified = subc_client_rs::ConsumerIdentity {
            module_id: "ckbus".into(),
            launch_nonce: "nonce-id".into(),
        };
        source.warn_unverified(Some(&verified), || {
            warnings.fetch_add(1, Ordering::SeqCst);
        });
        assert_eq!(warnings.load(Ordering::SeqCst), 0);
        for _ in 0..3 {
            source.warn_unverified(None, || {
                warnings.fetch_add(1, Ordering::SeqCst);
            });
        }
        assert_eq!(warnings.load(Ordering::SeqCst), 1);
    }
}
