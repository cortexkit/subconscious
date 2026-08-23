#![forbid(unsafe_code)]

use std::{env, error::Error, ffi::OsString, path::PathBuf, process};

use mcp_stdio_adapter::{
    adapter::{AdapterHandler, ClaustrumCredentialResolver, LifecycleSettings},
    attestation::StartupAttestation,
    registry::{default_config_path, load},
};
use serde_json::json;
use subc_client_rs::{serve_with_handle, ConsumerIdentity};
use subc_protocol::manifest::ModuleManifest;
use subc_protocol::{
    manifest::{
        Bindings, Concurrency, IdentityBinding, IdentityScope, ManagementOperation,
        ManagementOperationKind, ObservabilityKind, ObservabilitySurface, ProviderRole,
        StorageBinding, StorageKind, StorageScope, TrustTier,
    },
    PROTOCOL_VERSION,
};

const MODULE_ID: &str = "mcp-stdio-adapter";

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::main]
async fn main() {
    let exit_code = match run().await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("ck-mcp-stdio-adapter: {error}");
            1
        }
    };
    process::exit(exit_code);
}

async fn run() -> Result<()> {
    // This is intentionally first: no config read or daemon connection may occur
    // before a hand-launched process is refused.
    let attestation = StartupAttestation::require_and_scrub()?;
    subc_client_rs::retain_launch_nonce_for_hello(attestation.launch_nonce().to_string())
        .map_err(std::io::Error::other)?;

    let args = StartupArgs::parse(env::args_os().skip(1))?;
    let config_path = args.config.unwrap_or(default_config_path()?);
    let (registry, warnings) = load(&config_path)?;
    for warning in warnings {
        eprintln!("ck-mcp-stdio-adapter: warning: {warning}");
    }
    let connection_file = args.subc_connection_file.ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "missing --subc <connection-file>",
        )
    })?;

    let credential_resolver = ClaustrumCredentialResolver::new(
        connection_file.clone(),
        ConsumerIdentity {
            module_id: attestation.module_id().to_string(),
            launch_nonce: attestation.launch_nonce().to_string(),
        },
    );
    let handler = AdapterHandler::with_resolver(
        registry,
        std::sync::Arc::new(credential_resolver),
        LifecycleSettings::default(),
    );
    let (_handle, serve_future) = serve_with_handle(&connection_file, manifest(), handler).await?;
    serve_future.await?;
    Ok(())
}

fn manifest() -> ModuleManifest {
    ModuleManifest {
        module_id: MODULE_ID.to_string(),
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ManagementSurface {
            operations: vec![
                ManagementOperation {
                    name: "tools/list".to_string(),
                    kind: ManagementOperationKind::Query,
                },
                ManagementOperation {
                    name: "tools/call".to_string(),
                    kind: ManagementOperationKind::Mutate,
                },
            ],
            config_schema: json!({"type": "object"}),
            observability: vec![ObservabilitySurface {
                name: "health".to_string(),
                kind: ObservabilityKind::Snapshot,
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::ModuleManaged,
        }],
        consumes: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
    }
}

#[derive(Debug, Default)]
struct StartupArgs {
    config: Option<PathBuf>,
    subc_connection_file: Option<PathBuf>,
}

impl StartupArgs {
    fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Self> {
        let mut parsed = Self::default();
        let mut arguments = arguments.into_iter();
        while let Some(argument) = arguments.next() {
            match argument.to_str() {
                Some("--config") => {
                    parsed.config = Some(PathBuf::from(required_value(&mut arguments, "--config")?))
                }
                Some("--subc") => {
                    parsed.subc_connection_file =
                        Some(PathBuf::from(required_value(&mut arguments, "--subc")?))
                }
                Some(argument) => {
                    if let Some(value) = argument.strip_prefix("--config=") {
                        if value.is_empty() {
                            return Err("missing value for --config".into());
                        }
                        parsed.config = Some(PathBuf::from(value));
                    } else if let Some(value) = argument.strip_prefix("--subc=") {
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
        assert_eq!(manifest.provides.len(), 1);
    }

    #[test]
    fn config_override_and_subc_path_are_parsed() {
        let parsed = StartupArgs::parse([
            "--config".into(),
            "/tmp/registry.jsonc".into(),
            "--subc=/tmp/subc.json".into(),
        ])
        .unwrap();

        assert_eq!(
            parsed.config.unwrap().to_string_lossy(),
            "/tmp/registry.jsonc"
        );
        assert_eq!(
            parsed.subc_connection_file.unwrap().to_string_lossy(),
            "/tmp/subc.json"
        );
    }
}
