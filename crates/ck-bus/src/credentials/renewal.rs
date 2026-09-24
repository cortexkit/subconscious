//! Renewing the JWTs of ck-bus's own users (R16).
//!
//! ck-bus's own connections (its system-account user and its box-account user) present
//! whatever JWT `OwnJwts` holds for their user at each connect. A renewal task per user
//! re-signs the same key with a fresh `iat` and `exp` once the current JWT is
//! `JwtLifetime::renew_delay` old and stores it there. The live connection keeps its
//! old JWT until nats-server ends it at that JWT's `exp`; async-nats then reconnects,
//! and the reconnect presents the renewed JWT. Nothing is revoked and nothing changes
//! key.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use serde_json::json;
use tokio::task::JoinHandle;

use super::{
    issue::{sign_user_jwt, UserJwtRequest},
    Credentials,
};
use crate::grants::Grant;

/// The JWT each of ck-bus's own users presents on its next connect.
#[derive(Debug, Default)]
pub struct OwnJwts {
    jwts: Mutex<HashMap<String, String>>,
}

impl OwnJwts {
    pub fn set(&self, user_public: &str, jwt: &str) {
        self.lock().insert(user_public.to_string(), jwt.to_string());
    }

    pub fn get(&self, user_public: &str) -> Option<String> {
        self.lock().get(user_public).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        // Every mutation is one insert, so a poisoned map is still whole.
        self.jwts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Everything one of ck-bus's own users' JWTs is signed from, apart from its times.
#[derive(Debug, Clone)]
pub struct OwnRenewal {
    pub root_credential_id: String,
    pub user_public: String,
    pub issuer_account: String,
    pub name: String,
    pub grant: Grant,
}

/// A running renewal task, stopped when dropped: a boot attempt that fails after
/// connecting a user drops its renewal with it.
#[derive(Debug)]
pub struct RenewalTask(JoinHandle<()>);

impl Drop for RenewalTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Starts renewing `renewal`'s user, whose current JWT was issued at `issued_at`
/// (seconds since the Unix epoch).
pub fn spawn(credentials: Arc<Credentials>, renewal: OwnRenewal, issued_at: i64) -> RenewalTask {
    RenewalTask(tokio::spawn(run(credentials, renewal, issued_at)))
}

async fn run(credentials: Arc<Credentials>, renewal: OwnRenewal, mut issued_at: i64) {
    let lifetime = credentials.lifetime;
    // A failed renewal is retried well inside the window between renewal and expiry.
    let retry = (lifetime.lifetime.saturating_sub(lifetime.renew_after) / 10)
        .clamp(Duration::from_millis(100), Duration::from_secs(5));
    loop {
        tokio::time::sleep(until(issued_at, lifetime.renew_delay())).await;
        loop {
            let now = unix_now();
            let signed = sign_user_jwt(
                credentials.vault.as_ref(),
                &credentials.key_ids,
                &UserJwtRequest {
                    root_credential_id: &renewal.root_credential_id,
                    user_public: &renewal.user_public,
                    issuer_account: Some(&renewal.issuer_account),
                    name: &renewal.name,
                    issued_at: now,
                    expires_at: lifetime.expires_at(now),
                    grant: &renewal.grant,
                },
            )
            .await;
            match signed {
                Ok(signed) => {
                    credentials.own_jwts.set(&renewal.user_public, &signed.jwt);
                    log_event(
                        "ckbus.credentials.own_renewed",
                        json!({
                            "name": renewal.name,
                            "user_public": renewal.user_public,
                            "user_jwt_id": signed.jti,
                            "exp": signed.exp,
                        }),
                    );
                    issued_at = now;
                    break;
                }
                Err(error) => {
                    log_event(
                        "ckbus.credentials.own_renewal_failed",
                        json!({
                            "name": renewal.name,
                            "user_public": renewal.user_public,
                            "reason": error.to_string(),
                            "retry_ms": retry.as_millis() as u64,
                        }),
                    );
                    tokio::time::sleep(retry).await;
                }
            }
        }
    }
}

/// How long from now until `delay` after `issued_at`; zero once that has passed.
fn until(issued_at: i64, delay: Duration) -> Duration {
    let issued = UNIX_EPOCH + Duration::from_secs(issued_at.max(0) as u64);
    (issued + delay)
        .duration_since(SystemTime::now())
        .unwrap_or_default()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or_default()
}

fn log_event(event: &str, fields: serde_json::Value) {
    let at_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let mut line = json!({ "event": event, "at_ms": at_ms });
    if let (Some(line), serde_json::Value::Object(fields)) = (line.as_object_mut(), fields) {
        line.extend(fields);
    }
    eprintln!("{line}");
}
