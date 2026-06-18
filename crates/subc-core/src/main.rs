#![forbid(unsafe_code)]

use std::{env, path::PathBuf, process, sync::Arc};

use subc_core::{serve_uds, Router};

#[tokio::main]
async fn main() {
    let Some(path) = env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: subc-core <unix-socket-path>");
        process::exit(2);
    };

    let router = Arc::new(Router::with_default_self_handler());
    if let Err(err) = serve_uds(&path, router).await {
        eprintln!("subc-core: {err}");
        process::exit(1);
    }
}
