//! ck-bus-specific observers over the shared private-bus test kit.
#![allow(dead_code)]

use super::{control, signer::nats::unix_now};
pub use cortexkit_bus_testkit::LOOPBACK;
use nkeys::KeyPair;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use subc_control::{ClientControlRequest, ClientControlResponse};

/// Compatibility adapter for ck-bus's externally owned, restartable fixture trees.
/// All broker configuration and process management live in the published kit.
pub struct BusServer(cortexkit_bus_testkit::BusServer);
impl std::ops::Deref for BusServer {
    type Target = cortexkit_bus_testkit::BusServer;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for BusServer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl BusServer {
    pub async fn start(bin: &Path, dir: &Path, trust: &TrustChain, listen_host: &str) -> Self {
        assert_eq!(
            listen_host, LOOPBACK,
            "private brokers only listen on loopback"
        );
        Self(
            cortexkit_bus_testkit::BusServer::start(cortexkit_bus_testkit::BusOptions {
                binary: Some(bin.to_path_buf()),
                directory: Some(dir.to_path_buf()),
                trust: Some(trust.clone()),
                ..Default::default()
            })
            .await
            .expect("private NATS server"),
        )
    }
    pub async fn stop(self) {
        self.0.stop().await;
    }
}
pub use cortexkit_bus_testkit::trust::*;

/// Connects with `jwt`, answering the nonce with `user`.
pub async fn connect(
    url: &str,
    jwt: String,
    user: KeyPair,
) -> Result<async_nats::Client, async_nats::ConnectError> {
    let user = std::sync::Arc::new(user);
    async_nats::ConnectOptions::with_jwt(jwt, move |nonce| {
        let user = user.clone();
        async move { user.sign(&nonce).map_err(async_nats::AuthError::new) }
    })
    .connection_timeout(Duration::from_secs(5))
    .connect(url)
    .await
}

/// A harness client in the box account.
pub async fn box_client(
    trust: &TrustChain,
    server: &BusServer,
    account_public: &str,
) -> async_nats::Client {
    let user = KeyPair::new_user();
    let jwt = trust.user_jwt(&box_root_id(), account_public, &user, unix_now() - 60);
    connect(&server.url, jwt, user)
        .await
        .unwrap_or_else(|error| {
            panic!("harness box client refused: {error}\n{}", server.log_text())
        })
}

/// A harness client in the system account.
pub async fn system_client(trust: &TrustChain, server: &BusServer) -> async_nats::Client {
    let user = KeyPair::new_user();
    let jwt = trust.user_jwt(
        &system_root_id(),
        &trust.system_account,
        &user,
        unix_now() - 60,
    );
    connect(&server.url, jwt, user)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "harness system client refused: {error}\n{}",
                server.log_text()
            )
        })
}

/// The stored account JWT for `account_public`, read through the claims lookup.
pub async fn lookup(system: &async_nats::Client, account_public: &str) -> Option<String> {
    let reply = system
        .request(
            format!("$SYS.REQ.ACCOUNT.{account_public}.CLAIMS.LOOKUP"),
            Vec::new().into(),
        )
        .await
        .expect("claims lookup answers");
    (!reply.payload.is_empty()).then(|| String::from_utf8(reply.payload.to_vec()).unwrap())
}

/// Every structured ck-bus log line named `event` in the run's capture logs, oldest
/// first within each file.
pub fn events(run_root: &Path, event: &str) -> Vec<Value> {
    let mut found = Vec::new();
    let mut stack = vec![run_root.join("run/logs")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut paths: Vec<_> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        paths.sort();
        for path in paths {
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            for line in text.lines() {
                let Some(start) = line.find('{') else {
                    continue;
                };
                if let Ok(value) = serde_json::from_str::<Value>(&line[start..]) {
                    if value["event"] == event {
                        found.push(value);
                    }
                }
            }
        }
    }
    found
}

/// Waits until at least `count` `event` lines exist and returns the last one.
pub async fn wait_event(run_root: &Path, event: &str, count: usize, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    loop {
        let found = events(run_root, event);
        if found.len() >= count {
            return found.last().cloned().expect("at least one event");
        }
        assert!(
            Instant::now() < deadline,
            "no {count}th {event} line within {limit:?}; last bootstrap.down line: {:?}",
            events(run_root, "ckbus.bootstrap.down").last()
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// `supervisor.health_probe` for `ckbus`: status, detail and metrics.
pub async fn health(connection_file: &Path) -> (String, Option<String>, Value) {
    try_health(connection_file)
        .await
        .unwrap_or_else(|refusal| panic!("supervisor.health_probe ckbus refused: {refusal}"))
}

/// `health`, with a refused probe (the module is between processes) as an error.
pub async fn try_health(connection_file: &Path) -> Result<(String, Option<String>, Value), String> {
    let response = match control::rpc(
        connection_file,
        ClientControlRequest::SupervisorHealthProbe {
            module_id: "ckbus".to_string(),
        },
    )
    .await
    {
        control::ControlReply::Response(response) => response,
        control::ControlReply::Error(error) => {
            return Err(format!("{} {}", error.code, error.message))
        }
    };
    let ClientControlResponse::SupervisorHealthProbe {
        status,
        detail,
        metrics,
        ..
    } = response
    else {
        panic!("supervisor.health_probe must return its matching response variant");
    };
    Ok((
        format!("{status:?}"),
        detail,
        metrics.unwrap_or(Value::Null),
    ))
}

/// Waits until the health probe's `metrics.cause` equals `cause`.
pub async fn wait_health_cause(connection_file: &Path, cause: &str, limit: Duration) -> Value {
    let deadline = Instant::now() + limit;
    loop {
        let last = try_health(connection_file).await;
        if let Ok((_, _, metrics)) = &last {
            if metrics["cause"] == cause {
                return metrics.clone();
            }
        }
        assert!(
            Instant::now() < deadline,
            "health never named {cause} within {limit:?}; last probe {last:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// A real supervised restart of `ckbus` through `supervisor.restart`.
pub async fn restart_ckbus(connection_file: &Path) {
    let reply = control::rpc(
        connection_file,
        ClientControlRequest::SupervisorRestart {
            module_id: "ckbus".to_string(),
            drain_timeout_ms: None,
        },
    )
    .await;
    if let control::ControlReply::Error(error) = reply {
        panic!(
            "supervisor.restart ckbus refused: {} {}",
            error.code, error.message
        );
    }
}

/// `account.json` in the run's ckbus store.
pub fn account_json(run_root: &Path) -> PathBuf {
    run_root.join("data/cortexkit/ckbus/account.json")
}

pub fn own_users_json(run_root: &Path) -> PathBuf {
    run_root.join("data/cortexkit/ckbus/own_users.json")
}

pub fn read_json(path: &Path) -> Value {
    serde_json::from_slice(
        &std::fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display())),
    )
    .expect("store file is JSON")
}
