pub mod bus;
pub mod config;
pub mod control;
pub mod daemon;
pub mod data_home;
pub mod issuance;
pub mod report;
pub mod signer;
pub mod stubs;

use std::sync::Once;

static ACCEPTANCE_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static TRACING: Once = Once::new();

pub async fn acceptance_gate() -> tokio::sync::MutexGuard<'static, ()> {
    ACCEPTANCE_GATE.lock().await
}

pub fn install_tracing() {
    TRACING.call_once(|| {
        use tracing_subscriber::prelude::*;
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_test_writer()
                .with_target(true)
                .with_filter(tracing_subscriber::EnvFilter::new("warn")),
        );
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}
