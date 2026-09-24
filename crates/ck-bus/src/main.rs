// Each runtime capability is installed independently; until then its placeholder returns an explicit not-implemented error.
#[allow(dead_code)]
mod runtime;
// Generated grants. Bootstrap asks the grant seam for ck-bus's own users; the
// participant grant waits for issuance.
#[allow(dead_code)]
mod grants;
// Vault roots, in-memory user keys and user JWTs. Bootstrap signs ck-bus's own users and
// the box account through it; issuance, when it lands, is the next caller.
#[allow(dead_code)]
mod credentials;
// The machine id, the box account, ck-bus's own users, the census bucket and the five
// streams.
#[allow(dead_code)]
mod bootstrap;
// The offline `install-plan` and `install-apply` commands `ck setup` drives.
mod install;
// `ckbus.credential` and `ckbus.nonce_sign`: participant credentials, the census write
// and the epoch high-water mark.
#[allow(dead_code)]
mod issuance;
// Revocation: the operator-signed revocation list, the census delete and the kick, with
// durable progress; superseded users are found from the census.
#[allow(dead_code)]
mod revocation;
// The spawn-stream consumer: revokes the credential of every process that exits, and
// reconciles the census against the supervisor's spawn snapshot.
#[allow(dead_code)]
mod spawn_consumer;
// The sentinel probe and the health answer: bootstrap's cause until it is ready, then
// the verdict of a real round trip through the server.
#[allow(dead_code)]
mod sentinel;

use std::{
    env,
    error::Error,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use serde_json::json;
use subc_client_rs::{HandlerOutcome, ModuleHandler, RequestCtx};
use subc_protocol::{
    manifest::ModuleManifest,
    session::{HealthReport, HealthStatus},
    SUBC_MODULE_ID_ENV,
};

const MODULE_ID: &str = "ckbus";
const SENTINEL_PERIOD_MS: u64 = 10_000;
const SENTINEL_TIMEOUT_MS: u64 = 2_000;

struct BusHandler {
    runtime: Arc<runtime::Runtime>,
}

#[async_trait]
impl ModuleHandler for BusHandler {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Error {
            code: "ckbus_area_not_yet_landed".to_string(),
            message: "ck-bus data routes refuse until their owning runtime area lands".to_string(),
        }
    }

    async fn health(&self) -> HealthReport {
        match self.runtime.sentinel_health.report_health().await {
            Ok(report) => report,
            Err(refusal) => HealthReport {
                status: HealthStatus::Failing,
                detail: Some("bus.health.down".to_string()),
                metrics: Some(json!({
                    "class": "Unavailable",
                    "runtime_refusal": refusal.to_string(),
                })),
            },
        }
    }
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // The install commands are offline: evaluated before anything supervised, they need
    // no SUBC_MODULE_ID and reach neither the daemon nor the vault.
    let args: Vec<String> = env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    if let Some(status) = install::run(&args) {
        std::process::exit(status);
    }
    let module_id = env::var(SUBC_MODULE_ID_ENV)
        .map_err(|_| format!("{SUBC_MODULE_ID_ENV} is required; ck-bus only runs supervised"))?;
    let store_root = runtime::resolve_store_root()?;
    let runtime =
        runtime::Runtime::refusing_defaults(store_root).with_grants(Arc::new(grants::GrantSeam));

    let credentials = Arc::new(credentials::Credentials::supervised()?);

    let sentinel_period_ms = positive_env_ms("CKBUS_SENTINEL_PERIOD_MS", SENTINEL_PERIOD_MS)?;
    let sentinel_timeout_ms = positive_env_ms("CKBUS_SENTINEL_TIMEOUT_MS", SENTINEL_TIMEOUT_MS)?;
    let incarnation = process_incarnation();
    let bootstrap = bootstrap::Bootstrap::new(
        bootstrap::BootDeps {
            credentials: credentials.clone(),
            grants: runtime.grants.clone(),
            store: bootstrap::store::Store::new(runtime.store_root().clone()),
            incarnation: incarnation.clone(),
            own_spawn: Arc::new(bootstrap::SnapshotOwnSpawn {
                connection_file: credentials::vault::subc_arg(env::args_os()).unwrap_or_default(),
                module_id: module_id.clone(),
            }),
        },
        std::time::Duration::from_millis(sentinel_period_ms),
    );
    let store_root = runtime.store_root().clone();
    let runtime = Arc::new(runtime.with_sentinel_health(sentinel::wire(
        bootstrap.health(),
        bootstrap.ready(),
        &store_root,
        module_id.clone(),
        incarnation.clone(),
        sentinel::Timing {
            period: std::time::Duration::from_millis(sentinel_period_ms),
            timeout: std::time::Duration::from_millis(sentinel_timeout_ms),
        },
    )));
    eprintln!(
        "{}",
        json!({
            "event": "ckbus.runtime.started",
            "module_id": module_id,
            "process_incarnation": incarnation,
            "sentinel_period_ms": sentinel_period_ms,
            "sentinel_timeout_ms": sentinel_timeout_ms,
        })
    );

    let wired = issuance::handler::wire(
        ModuleManifest::builder(&module_id, env!("CARGO_PKG_VERSION")),
        BusHandler { runtime },
        credentials.clone(),
        &store_root,
        bootstrap.ready(),
    )?;
    let wired = revocation::handler::wire(
        wired,
        credentials,
        &store_root,
        bootstrap.ready(),
        std::time::Duration::from_millis(sentinel_period_ms),
    );
    spawn_consumer::wire(
        credentials::vault::subc_arg(env::args_os())
            .ok_or("ck-bus needs --subc <connection file> to follow the spawn stream")?,
        wired.handler.area().revoker().clone(),
        &store_root,
        bootstrap.ready(),
        module_id.clone(),
        std::time::Duration::from_millis(sentinel_period_ms),
    );
    bootstrap.serve(wired.manifest, wired.handler).await?;
    Ok(())
}

fn positive_env_ms(name: &str, default: u64) -> Result<u64, Box<dyn Error + Send + Sync>> {
    let Some(raw) = env::var_os(name) else {
        return Ok(default);
    };
    let raw = raw
        .into_string()
        .map_err(|_| format!("{name} must be valid UTF-8"))?;
    let value = raw
        .parse::<u64>()
        .map_err(|_| format!("{name} must be a positive integer"))?;
    if value == 0 {
        return Err(format!("{name} must be greater than zero").into());
    }
    Ok(value)
}

fn process_incarnation() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{}-{nanos}", std::process::id())
}
