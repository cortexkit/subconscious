//! ck-bus-specific vault and golden-fixture helpers.
#![allow(dead_code)]

pub mod claustrum;
pub mod nats;
pub mod run;
pub mod seeds;

pub use cortexkit_bus_testkit::signer::*;
use serde_json::Value;
use std::path::PathBuf;

pub fn golden_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/harness/signer/golden/credential_pair.json")
}

pub fn golden() -> Value {
    let path = golden_path();
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text).expect("the golden pair is JSON")
}
