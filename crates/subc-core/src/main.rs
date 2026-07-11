#![forbid(unsafe_code)]

use std::process;

#[tokio::main]
async fn main() {
    // Side-effect-free provenance probe: must be evaluated before tracing,
    // bootstrap, or any runtime state so `ck-subc --version` never touches the
    // start-lock or reports an already-running daemon.
    if std::env::args_os().nth(1).is_some_and(|arg| arg == "--version") {
        println!("ck-subc {}", env!("CARGO_PKG_VERSION"));
        return;
    }

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
