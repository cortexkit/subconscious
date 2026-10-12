//! The ledger is authoritative; bus outages only delay this at-least-once projection.

use crate::bootstrap::Ready;
use async_trait::async_trait;
use cortexkit_bus_naming::AccountNames;
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use subc_client_rs::{ActivityCursor, ActivityEntry};
use tokio::sync::watch;

pub mod cursor;
pub mod feed;
pub mod source;
use cursor::{CursorRead, CursorStore};
pub use feed::{Feed, FeedEnd, Source};

const PAGE_SIZE: u32 = 256;

#[async_trait]
pub trait Publisher: Send + Sync {
    /// Success means the broker stored the entry, not merely queued a socket write.
    async fn publish(&self, entry: &ActivityEntry) -> Result<(), String>;
}

pub trait Journal: Send + Sync {
    fn event(&self, name: &str, fields: Value);
}
struct StderrJournal;
impl Journal for StderrJournal {
    fn event(&self, name: &str, mut fields: Value) {
        fields["event"] = json!(name);
        eprintln!("{fields}");
    }
}

pub struct Mirror {
    source: Arc<dyn Source>,
    publisher: Arc<dyn Publisher>,
    cursors: CursorStore,
    journal: Arc<dyn Journal>,
    retry: Duration,
}
impl Mirror {
    pub fn new(
        source: Arc<dyn Source>,
        publisher: Arc<dyn Publisher>,
        root: &Path,
        retry: Duration,
    ) -> Self {
        Self {
            source,
            publisher,
            cursors: CursorStore::new(root),
            journal: Arc::new(StderrJournal),
            retry,
        }
    }
    async fn checkpoint(&self, cursor: ActivityCursor) -> Result<(), String> {
        let store = self.cursors.clone();
        tokio::task::spawn_blocking(move || store.write(&cursor))
            .await
            .map_err(|err| err.to_string())?
            .map_err(|err| format!("activity checkpoint: {err}"))
    }
    async fn load(&self) -> Result<ActivityCursor, String> {
        let store = self.cursors.clone();
        let saved = tokio::task::spawn_blocking(move || {
            store.remove_stale_tmp();
            store.read()
        })
        .await
        .map_err(|err| err.to_string())?;
        match saved {
            CursorRead::Present(cursor) => Ok(cursor),
            other => {
                if let CursorRead::Damaged(reason) = other {
                    self.journal.event("ckbus.activity.cursor_damaged", json!({"reason": reason, "path": self.cursors.path().display().to_string()}));
                }
                let head = self.source.query(None, 1).await?.head;
                // This baseline deliberately skips history on first installation or damage.
                self.checkpoint(head.clone()).await?;
                Ok(head)
            }
        }
    }
    async fn publish(&self, entry: &ActivityEntry) -> Result<(), String> {
        self.publisher.publish(entry).await?;
        self.checkpoint(ActivityCursor {
            daemon_incarnation: entry.daemon_incarnation.clone(),
            seq: entry.seq,
        })
        .await
    }
    async fn follow(&self, since: ActivityCursor) -> Result<(), FeedEnd> {
        let mut feed = self.source.subscribe(since).await?;
        while let Some(entry) = feed.next().await? {
            self.publish(&entry).await.map_err(FeedEnd::Failed)?;
        }
        Err(FeedEnd::Failed("activity feed ended".into()))
    }
    fn lost(&self, first: u64, last: u64) {
        if first <= last {
            self.journal.event("ckbus.activity.entries_lost", json!({"first_seq": first, "last_seq": last, "reason": "no longer retained by the ledger"}));
        }
    }
    async fn recover(&self, checkpoint: ActivityCursor) -> Result<ActivityCursor, String> {
        let mut page = self.source.query(None, PAGE_SIZE).await?;
        let head = page.head.clone();
        let mut entries = Vec::new();
        loop {
            if page.head.daemon_incarnation != head.daemon_incarnation {
                return Err("daemon restarted during activity recovery".into());
            }
            let done = page.entries.iter().any(|entry| entry.seq <= checkpoint.seq);
            entries.extend(
                page.entries
                    .into_iter()
                    .filter(|entry| entry.seq > checkpoint.seq && entry.seq <= head.seq),
            );
            match page.cursor {
                Some(before) if !done && before > checkpoint.seq => {
                    page = self.source.query(Some(before), PAGE_SIZE).await?;
                    if page.cursor.is_some_and(|next| next >= before) {
                        return Err("activity query continuation did not advance".into());
                    }
                }
                _ => break,
            }
        }
        entries.sort_by_key(|entry| entry.seq);
        let mut next = checkpoint.seq.saturating_add(1);
        for entry in entries {
            if entry.seq > next {
                self.lost(next, entry.seq - 1);
            }
            self.publish(&entry).await?;
            next = entry.seq.saturating_add(1);
        }
        self.lost(next, head.seq);
        // Retained entries can name older incarnations. Only the query head is a
        // valid live cursor after recovery; also advance over logged pruning gaps.
        self.checkpoint(head.clone()).await?;
        Ok(head)
    }
    pub async fn run(self) {
        loop {
            let result = async {
                let mut since = self.load().await?;
                loop {
                    match self.follow(since.clone()).await {
                        Err(FeedEnd::CursorRefused | FeedEnd::Lagged) => {
                            // Read the last successful publish, not the original subscription.
                            since = self.recover(self.load().await?).await?;
                        }
                        Err(FeedEnd::Failed(error)) => return Err(error),
                        Ok(()) => return Ok(()),
                    }
                }
            }
            .await;
            if let Err(error) = result {
                self.journal
                    .event("ckbus.activity.deferred", json!({"error": error}));
            }
            tokio::time::sleep(self.retry).await;
            self.journal.event(
                "ckbus.activity.retrying",
                json!({"checkpoint": self.cursors.path().display().to_string()}),
            );
        }
    }
}

pub fn subject(account: &str) -> Result<String, String> {
    AccountNames::derive(account)
        .and_then(|names| names.event_subject("subc", "activity", 1))
        .map_err(|err| err.to_string())
}

pub struct BusPublisher {
    pub ready: watch::Receiver<Option<Arc<Ready>>>,
    pub timeout: Duration,
}
#[async_trait]
impl Publisher for BusPublisher {
    async fn publish(&self, entry: &ActivityEntry) -> Result<(), String> {
        let ready = self.ready.borrow().clone().ok_or("bus is not ready")?;
        let topic = subject(&ready.account.acct)?;
        let payload = serde_json::to_vec(entry).map_err(|err| err.to_string())?;
        tokio::time::timeout(
            self.timeout,
            ready.box_plane.publish_stored(&topic, payload),
        )
        .await
        .map_err(|_| "activity publish timed out".to_string())?
        .map_err(|err| err.to_string())
    }
}

pub fn wire(
    connection_file: PathBuf,
    root: &Path,
    ready: watch::Receiver<Option<Arc<Ready>>>,
    retry: Duration,
) -> tokio::task::JoinHandle<()> {
    let mirror = Mirror::new(
        Arc::new(source::DaemonSource::new(connection_file)),
        Arc::new(BusPublisher {
            ready,
            timeout: retry,
        }),
        root,
        retry,
    );
    tokio::spawn(mirror.run())
}

#[cfg(test)]
mod identity_tests;
#[cfg(test)]
mod tests;
