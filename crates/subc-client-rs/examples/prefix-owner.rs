#![forbid(unsafe_code)]

use std::{error::Error, path::PathBuf};
use subc_client_rs::{async_trait, serve_with_handle, HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::manifest::ModuleManifest;

struct Echo;

#[async_trait]
impl ModuleHandler for Echo {
    async fn handle(&self, _: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let path = std::env::args_os()
        .skip_while(|arg| arg != "--subc")
        .nth(1)
        .map(PathBuf::from)
        .ok_or("missing --subc connection file")?;
    let (_owner, serving) = serve_with_handle(
        &path,
        ModuleManifest::builder("test-owner", "1.0.0")
            .registration_channel(true)
            .build(),
        Echo,
    )
    .await?;
    tokio::spawn(serving);
    let (_child, serving) = serve_with_handle(
        &path,
        ModuleManifest::builder("test-owner:child", "1.0.0").build(),
        Echo,
    )
    .await?;
    serving.await?;
    Ok(())
}
