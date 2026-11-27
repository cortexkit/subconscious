use std::path::Path;

use serde_json::{json, Value};

pub const BUS_VERSION: &str = "0.3.0";
pub const VAULT_VERSION: &str = "0.8.0";
pub const SIGNER_ID: &str = "signing:ck-bus-operator-signer:1";
pub const SYSACCOUNT_ID: &str = "signing:ck-bus-sysaccount:1";
pub const ACCOUNT_ID: &str = "signing:ck-bus-account:1";

pub fn public_half(id: &str) -> String {
    match id {
        SIGNER_ID => "12",
        SYSACCOUNT_ID => "34",
        ACCOUNT_ID => "56",
        _ => panic!("unexpected fixture key {id}"),
    }
    .repeat(32)
}

pub fn bus_env(nats: &Path) -> Value {
    json!({
        "CKBUS_NATS_URL": "nats://127.0.0.1:14222",
        "CKBUS_OPERATOR_JWT": nats.join("operator.jwt"),
        "CKBUS_SYSTEM_ACCOUNT": "fixture-system-account",
    })
}
