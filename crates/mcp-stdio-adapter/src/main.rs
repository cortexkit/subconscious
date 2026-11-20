#![forbid(unsafe_code)]

use std::{env, error::Error, ffi::OsString, path::PathBuf, process};

use mcp_stdio_adapter::{
    adapter::{AdapterHandler, ClaustrumCredentialResolver},
    attestation::StartupAttestation,
    settings::{UserSettings, MODULE_ID},
};
use serde_json::json;
use subc_client_rs::serve_with_handle;
use subc_protocol::manifest::{
    Concurrency, IdentityScope, ManagementOperation, ManagementOperationKind, ModuleManifest,
    ObservabilityKind, ObservabilitySurface, ProviderRole,
};

static LOGGER_INSTALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::main]
async fn main() {
    let exit_code = match run().await {
        Ok(()) => 0,
        Err(error) => {
            if LOGGER_INSTALLED.load(std::sync::atomic::Ordering::Relaxed) {
                tracing::error!("adapter failed: {error}");
            } else {
                // Offline probes and attestation refusals have no logger yet.
                eprintln!("ck-mcp-stdio-adapter: {error}");
            }
            1
        }
    };
    process::exit(exit_code);
}

async fn run() -> Result<()> {
    // Emit static discovery data before attestation, config, or daemon setup so
    // offline checks can inspect the binary without runtime side effects.
    if env::args_os()
        .nth(1)
        .is_some_and(|argument| argument == "--manifest")
    {
        print_manifest(manifest())?;
        return Ok(());
    }

    if env::args_os().any(|arg| arg == "--version") {
        println!("ck-mcp-stdio-adapter {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if env::args_os().any(|arg| arg == "--help" || arg == "-h") {
        println!("usage: ck-mcp-stdio-adapter --subc <connection-file>");
        return Ok(());
    }

    // This is intentionally first: no config read or daemon connection may occur
    // before a hand-launched process is refused.
    // The SDK reuses the same validated channel or cached legacy launch nonce.
    StartupAttestation::require()?;
    let _logger = cortexkit_log::init_from_env()?;
    LOGGER_INSTALLED.store(true, std::sync::atomic::Ordering::Relaxed);
    tracing::info!("adapter starting");

    let args = StartupArgs::parse(env::args_os().skip(1))?;
    let connection_file = args.subc_connection_file.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing --subc <connection-file>",
        )
    })?;

    let credential_resolver = ClaustrumCredentialResolver::new(connection_file.clone());
    let handler = AdapterHandler::managed(
        UserSettings::locate()?,
        std::sync::Arc::new(credential_resolver),
    );
    let (_handle, serve_future) = serve_with_handle(&connection_file, manifest(), handler).await?;
    serve_future.await?;
    Ok(())
}

fn manifest_json(manifest: ModuleManifest) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(manifest)?;
    // The adapter executes third-party servers; its identity must never qualify
    // their calls for first-party automatic grants.
    value["hosts_third_party_code"] = serde_json::Value::Bool(true);
    value
        .as_object_mut()
        .expect("serialized module manifest is an object")
        .insert(
            "runtime_computed".to_string(),
            serde_json::Value::Array(Vec::new()),
        );
    Ok(value)
}

fn print_manifest(manifest: ModuleManifest) -> Result<()> {
    println!("{}", serde_json::to_string(&manifest_json(manifest)?)?);
    Ok(())
}

fn manifest() -> ModuleManifest {
    ModuleManifest::builder(MODULE_ID, env!("CARGO_PKG_VERSION"))
        .provides(vec![ProviderRole::ManagementSurface {
        operations: vec![
            ManagementOperation {
                name: "tools/list".to_string(),
                kind: ManagementOperationKind::Query,
                description: Some(
                    "List the MCP tools exposed by the configured child servers and return their names, descriptions, and input schemas."
                        .to_string(),
                ),
            },
            ManagementOperation {
                name: "tools/call".to_string(),
                kind: ManagementOperationKind::Mutate,
                description: Some(
                    "Invoke a named tool on a configured child server and return the child's MCP result."
                        .to_string(),
                ),
            },
        ],
        config_schema: json!({"type": "object"}),
        observability: vec![ObservabilitySurface {
            name: "health".to_string(),
            kind: ObservabilityKind::Snapshot,
        }],
        identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
        concurrency: Concurrency::ModuleManaged,
    }])
    .build()
}

#[derive(Debug, Default)]
struct StartupArgs {
    subc_connection_file: Option<PathBuf>,
}

impl StartupArgs {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut parsed = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--subc") => {
                    parsed.subc_connection_file =
                        Some(PathBuf::from(required_value(&mut arguments, "--subc")?))
                }
                Some(argument) => {
                    if let Some(value) = argument.strip_prefix("--subc=") {
                        if value.is_empty() {
                            return Err("missing value for --subc".into());
                        }
                        parsed.subc_connection_file = Some(PathBuf::from(value));
                    } else {
                        return Err(format!("unknown argument {argument}").into());
                    }
                }
                None => return Err("arguments must be valid Unicode".into()),
            }
        }
        Ok(parsed)
    }
}

fn required_value(arguments: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString> {
    arguments
        .next()
        .ok_or_else(|| format!("missing value for {flag}").into())
}

#[cfg(test)]
mod tests {
    use super::{manifest, StartupArgs, MODULE_ID};
    use subc_protocol::manifest::{Concurrency, ProviderRole};

    #[test]
    fn management_surface_manifest_has_the_adapter_identity_and_health() {
        let manifest = manifest();

        assert_eq!(manifest.module_id, MODULE_ID);
        assert!(matches!(
            &manifest.provides[0],
            ProviderRole::ManagementSurface {
                observability,
                concurrency: Concurrency::ModuleManaged,
                ..
            } if observability.iter().any(|surface| surface.name == "health")
        ));
        let ProviderRole::ManagementSurface { operations, .. } = &manifest.provides[0] else {
            panic!("adapter manifest must expose a management surface");
        };
        assert_eq!(operations[0].description.as_deref(), Some(
            "List the MCP tools exposed by the configured child servers and return their names, descriptions, and input schemas."
        ));
        assert_eq!(operations[1].description.as_deref(), Some(
            "Invoke a named tool on a configured child server and return the child's MCP result."
        ));
        assert_eq!(manifest.provides.len(), 1);
    }

    #[test]
    fn only_the_daemon_connection_path_can_be_overridden() {
        let parsed = StartupArgs::parse(["--subc=/tmp/subc.json".into()]).unwrap();

        assert_eq!(
            parsed.subc_connection_file.unwrap().to_string_lossy(),
            "/tmp/subc.json"
        );
        assert!(StartupArgs::parse(["--config=/tmp/registry.jsonc".into()]).is_err());
    }
}
