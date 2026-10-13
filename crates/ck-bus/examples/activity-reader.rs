//! The daemon launches this reader as ckbus and supplies its random launch proof
//! (the nonce). It exercises the production daemon source, so the SDK must include
//! that proof in activity requests rather than using a test-created identity.

#[path = "../src/activity_mirror/feed.rs"]
mod feed;
#[path = "../src/credentials/identity.rs"]
pub mod identity;
mod credentials {
    pub use crate::identity;
}
#[path = "../src/activity_mirror/source.rs"]
mod source;
use feed::{Feed, FeedEnd, Source};
use std::{env, path::PathBuf, time::Duration};

#[tokio::main]
async fn main() -> Result<(), String> {
    let connection = PathBuf::from(
        env::var_os("CKBUS_ACTIVITY_CONNECTION").ok_or("connection path is required")?,
    );
    let result =
        PathBuf::from(env::var_os("CKBUS_ACTIVITY_RESULT").ok_or("result path is required")?);
    tokio::time::timeout(Duration::from_secs(20), async {
        while !connection.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| "test transport did not start")?;
    let source = source::DaemonSource::new(connection);
    let page = source.query(None, 10).await?;
    let mut feed = source
        .subscribe(page.head)
        .await
        .map_err(|err| format!("{err:?}"))?;
    let entry = tokio::time::timeout(Duration::from_secs(20), feed.next())
        .await
        .map_err(|_| "activity append did not arrive")?
        .map_err(|err| format!("{err:?}"))?
        .ok_or("activity feed ended")?;
    let bytes =
        serde_json::to_vec(&serde_json::json!({"seq": entry.seq, "summary": entry.summary}))
            .map_err(|err| err.to_string())?;
    let pending = result.with_extension("tmp");
    std::fs::write(&pending, bytes).map_err(|err| err.to_string())?;
    cortexkit_lease::durable_replace(&pending, &result).map_err(|err| err.to_string())
}
