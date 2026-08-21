//! Live integration probe: PolicyResolver against the real prefrontal-core.
//! Arms: (1) unknown gate on the approval domain resolves to the closed
//! default (Denied) — safe-by-construction; (2) a Fault is distinguishable
//! when the target is wrong (bounded by the helper's hard timeout).
use std::path::Path;
use std::time::Duration;
use subc_client_rs::{PolicyResolver, PolicyResolverConfig, ProjectRef, SubcConsumer, Subject};

#[tokio::main]
async fn main() {
    let cf = std::env::var("SUBC_CONNECTION_FILE").unwrap_or_else(|_| {
        format!(
            "{}/.local/share/cortexkit/run/subc-connection.json",
            std::env::var("HOME").unwrap()
        )
    });
    let consumer = SubcConsumer::connect(Path::new(&cf), Default::default())
        .await
        .expect("connect");
    let resolver = PolicyResolver::with_resolver_target(
        consumer,
        "prefrontal-core",
        PolicyResolverConfig {
            hard_timeout: Duration::from_secs(2),
            ttl_floor_ms: 1000,
        },
    );
    let verdict = resolver
        .resolve(
            "approval",
            "subc.integration_probe_gate",
            Subject::SessionToResolve("ses_12a4fa38dffe81Fz7Y2AsWb5Cg".into()),
            ProjectRef::Root("/tmp".to_string()),
        )
        .await;
    println!("arm1 unknown-gate closed-default: {verdict:?}");
    let started = std::time::Instant::now();
    let consumer2 = SubcConsumer::connect(Path::new(&cf), Default::default())
        .await
        .expect("connect2");
    let bad = PolicyResolver::with_resolver_target(
        consumer2,
        "no-such-module",
        PolicyResolverConfig {
            hard_timeout: Duration::from_secs(2),
            ttl_floor_ms: 1000,
        },
    );
    let fault = bad
        .resolve(
            "approval",
            "subc.integration_probe_gate",
            Subject::SessionToResolve("ses_12a4fa38dffe81Fz7Y2AsWb5Cg".into()),
            ProjectRef::Root("/tmp".to_string()),
        )
        .await;
    println!(
        "arm2 fault-distinct: {fault:?} elapsed={:?}",
        started.elapsed()
    );
    let again = resolver
        .resolve(
            "approval",
            "subc.integration_probe_gate",
            Subject::SessionToResolve("ses_12a4fa38dffe81Fz7Y2AsWb5Cg".into()),
            ProjectRef::Root("/tmp".to_string()),
        )
        .await;
    println!("arm3 cached-repeat: {again:?}");
}
