// Linux prerequisites, from the workspace root (all children use isolated homes):
// mkdir -p target/scratch/config-sdk
// cargo build --locked -p subc-core --bins
// cp target/debug/ck-subc target/scratch/config-sdk/default-daemon
// cp target/debug/ck target/scratch/config-sdk/ck
// cargo build --locked -p subc-core --bin ck-subc --features subc-core/test-support,subc-daemon/test-support,subc-daemon/dev-desktop-approval
// cp target/debug/ck-subc target/scratch/config-sdk/dev-daemon
// cargo build --locked --target-dir target --manifest-path crates/subc-core/tests/config_status_sdk/doc-sample/Cargo.toml
// export CONFIG_SDK_DEFAULT_DAEMON="$PWD/target/scratch/config-sdk/default-daemon"
// export CONFIG_SDK_DEV_DAEMON="$PWD/target/scratch/config-sdk/dev-daemon"
// export CONFIG_SDK_CK="$PWD/target/scratch/config-sdk/ck"
// export CONFIG_SDK_RUST_SAMPLE="$PWD/target/debug/config-sdk-doc-sample"
// export CONFIG_SDK_BUN="$(command -v bun)"
// cargo test --locked -p subc-core --features test-support --test config_status_sdk config_status_sdk -- --include-ignored
#![cfg(all(target_os = "linux", feature = "test-support"))]

#[path = "config_status_sdk/mod.rs"]
mod config_status_sdk;

use config_status_sdk::*;

#[test]
#[ignore = "requires CONFIG_SDK_DEV_DAEMON and CONFIG_SDK_CK built by the header commands"]
fn config_status_sdk_rust_real_daemon() {
    isolated("config_status_sdk_rust_real_daemon", "rust");
}

#[test]
#[ignore = "requires CONFIG_SDK_DEV_DAEMON, CONFIG_SDK_CK and CONFIG_SDK_BUN built by the header commands"]
fn config_status_sdk_typescript_real_daemon() {
    isolated("config_status_sdk_typescript_real_daemon", "typescript");
}

#[test]
fn config_status_sdk_module() {
    if let Some(root) = std::env::var_os("CONFIG_SDK_MODULE_ROOT") {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(rust_module(root.into()));
    }
}

#[test]
#[ignore = "requires CONFIG_SDK_DEV_DAEMON and CONFIG_SDK_DEFAULT_DAEMON built by the header commands"]
fn config_status_sdk_seam_guards_and_release_binary() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(seam_guards());
}

#[test]
#[ignore = "requires CONFIG_SDK_DEV_DAEMON and CONFIG_SDK_CK built by the header commands"]
fn config_status_sdk_approved_daemon_values() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(approved_status());
}

#[test]
#[ignore = "requires CONFIG_SDK_DEV_DAEMON and CONFIG_SDK_RUST_SAMPLE built by the header commands"]
fn config_status_sdk_published_samples_rust() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(published_sample("rust"));
}

#[test]
#[ignore = "requires CONFIG_SDK_DEV_DAEMON and CONFIG_SDK_BUN built by the header commands"]
fn config_status_sdk_published_samples_typescript() {
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(published_sample("typescript"));
}

#[test]
fn config_status_sdk_consent_goldens() {
    use subc_protocol::consent::{ConsentDecision, ConsentRequest, ConsentResponse};
    let request: ConsentRequest = serde_json::from_str(include_str!(
        "../../subc-protocol/tests/golden/config_gate/consent_v1_request.json"
    ))
    .unwrap();
    assert!(request.is_consistent());
    let requester = ConsentRequest::new(request.plan.clone()).unwrap();
    assert_eq!(requester, request);
    let approved: ConsentResponse = serde_json::from_str(include_str!(
        "../../subc-protocol/tests/golden/config_gate/consent_v1_response_approved.json"
    ))
    .unwrap();
    let declined: ConsentResponse = serde_json::from_str(include_str!(
        "../../subc-protocol/tests/golden/config_gate/consent_v1_response_declined.json"
    ))
    .unwrap();
    assert_eq!(
        ConsentResponse::new(&request, ConsentDecision::Approved),
        approved
    );
    assert!(approved.approves(&requester));
    assert!(!declined.approves(&requester));
    let mut stale = requester.clone();
    stale.base_revision += 1;
    assert!(!approved.approves(&stale));
}
