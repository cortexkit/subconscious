#![forbid(unsafe_code)]

use std::process;

#[tokio::main]
async fn main() {
    init_tracing();

    if let Err(err) = subc_core::bootstrap::run().await {
        tracing::error!(error = %err, "subc-core failed");
        eprintln!("subc-core: {err}");
        process::exit(1);
    }
}

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .init();
}
