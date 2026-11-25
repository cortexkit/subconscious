//! Fault injection for the dedicated daemon fixture executable.

use super::{Hooks, PathBuf};
use std::{io, path::Path, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

impl Hooks {
    pub(crate) fn for_test_binary() -> Self {
        let freeze_gate = std::env::var_os("SUBC_TEST_UPGRADE_FREEZE_GATE").map(PathBuf::from);
        Self {
            #[cfg(target_os = "macos")]
            enabled: true,
            #[cfg(target_os = "macos")]
            team: std::env::var("SUBC_TEST_UPGRADE_TEAM_ID").ok(),
            exec_path: std::env::var_os("SUBC_TEST_UPGRADE_EXEC_PATH").map(PathBuf::from),
            write_failure: std::env::var_os("SUBC_TEST_UPGRADE_HANDOVER_WRITE_FAILURE")
                .is_some()
                .into(),
            corrupt_hmac: std::env::var_os("SUBC_TEST_UPGRADE_CORRUPT_HMAC").is_some(),
            stall_receipt: std::env::var_os("SUBC_TEST_UPGRADE_RECEIPT_WAIT_MARKER")
                .map(PathBuf::from),
            notice_failure: std::env::var("SUBC_TEST_UPGRADE_NOTICE_FAILURE").ok(),
            refusal_writer_gate: freeze_gate.clone().map(Arc::new),
            freeze_gate,
            pause_local_listener: std::env::var_os("SUBC_TEST_RESUME_PAUSE_LOCAL_LISTENER")
                .is_some(),
            boot_hash_gate: std::env::var_os("SUBC_TEST_UPGRADE_BOOT_HASH_GATE").map(PathBuf::from),
        }
    }
}

pub(crate) async fn wait_at_gate(path: &Path, phase: u8) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(20), async {
        let mut stream = tokio::net::UnixStream::connect(path).await?;
        stream.write_u8(phase).await?;
        stream.read_u8().await?;
        Ok(())
    })
    .await?
}
