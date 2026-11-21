use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use subc_client_rs::config::{
    ConfigAppliedReply, ConfigAppliedRequest, ConfigChanged, ConfigGetReply, ConfigGetRequest,
};
use subc_control::config::{ConfigControlRequest, ConfigControlResponse, ConfigWatchResponse};
use subc_protocol::ErrorBody;

fn round_trip<T: DeserializeOwned + Serialize>(name: &str, body: &Value) {
    let decoded: T = serde_json::from_value(body.clone()).unwrap_or_else(|error| {
        panic!(
            "{name}: failed to decode as {}: {error}",
            std::any::type_name::<T>()
        )
    });
    assert_eq!(serde_json::to_value(decoded).unwrap(), *body, "{name}");
}

macro_rules! project_vector {
    ($name:literal) => {
        serde_json::from_str::<Value>(include_str!(concat!(
            "../../../vectors/config-settings-v1/project-",
            $name,
            ".json"
        )))
        .unwrap()
    };
}

macro_rules! exchange {
    ($request:ty, $reply:ty; $($name:literal),+ $(,)?) => {
        $(
            let sample = project_vector!($name);
            round_trip::<$request>(concat!("project-", $name, ".json request"), &sample["request"]);
            round_trip::<$reply>(concat!("project-", $name, ".json reply"), &sample["reply"]);
        )+
    };
}

#[test]
fn config_sdk_project_delivery_vectors_round_trip() {
    round_trip::<ConfigChanged>("project-changed.json", &project_vector!("changed"));
    exchange!(ConfigGetRequest, ConfigGetReply; "get-fallback", "get-pending", "get-unstamped");
    exchange!(ConfigAppliedRequest, ConfigAppliedReply; "applied");
}

#[test]
fn config_sdk_project_settings_control_vectors_round_trip() {
    exchange!(ConfigControlRequest, ConfigControlResponse;
        "apply-unstamped",
        "plan-apply-kind",
        "plan-capture",
        "plan-credentials",
        "plan-edit",
        "plan-root-refusals",
        "show",
        "show-default",
        "show-user",
        "show-workspace",
        "status-root-io",
    );
    exchange!(ConfigControlRequest, ErrorBody;
        "plan-config-unavailable",
        "plan-edit-spans-files",
        "plan-registry-unavailable",
        "plan-unknown-workspace",
        "plan-workspace-mismatch",
        "show-unknown-project",
    );
}

#[test]
fn config_sdk_project_settings_watch_vectors_round_trip() {
    round_trip::<ConfigControlRequest>(
        "project-watch.request.json",
        &project_vector!("watch.request"),
    );
    round_trip::<ConfigWatchResponse>(
        "project-watch.change.json",
        &project_vector!("watch.change"),
    );
    round_trip::<ConfigWatchResponse>(
        "project-watch.resync.json",
        &project_vector!("watch.resync"),
    );
    round_trip::<ConfigWatchResponse>(
        "project-watch.drop.resync.json",
        &project_vector!("watch.drop.resync"),
    );
}
