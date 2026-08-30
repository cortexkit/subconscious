use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{self, Command},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use serde_json::{json, Value};
use subc_control::{
    ClientControlRequest, ClientControlResponse, ConsumerIdentity, SupervisorEntry,
    SupervisorRescanResult,
};
use subc_core::{
    bootstrap::{run_with_config, run_with_daemon_config_path, BootstrapConfig},
    read_frame, write_frame, Frame,
};
use subc_protocol::{
    manifest::{
        Bindings, IdentityBinding, IdentityScope, ModuleManifest, StorageBinding, StorageKind,
        StorageScope, TrustTier,
    },
    BindIdentity, ErrorBody, Flags, FrameType, ModuleHelloBody, Priority, RouteTarget,
    PROTOCOL_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    task::JoinHandle,
    time::{sleep, timeout, Instant},
};

mod common;
use common::connect_authed_client;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const START_TIMEOUT: Duration = Duration::from_secs(10);
const STATE_TIMEOUT: Duration = Duration::from_secs(10);

struct RunningDaemon {
    connection_file_path: PathBuf,
    config_path: PathBuf,
    temp_dir: PathBuf,
    task: JoinHandle<Result<(), subc_core::bootstrap::BootstrapError>>,
}

impl RunningDaemon {
    async fn start(name: &str, config_doc: Option<String>) -> Self {
        let temp_dir = unique_temp_dir(name);
        fs::create_dir_all(&temp_dir).unwrap();
        let connection_file_path = temp_dir.join("subc-conn.json");
        let config_path = temp_dir.join("config").join("cortexkit").join("subc.jsonc");
        if let Some(config_doc) = config_doc {
            fs::create_dir_all(config_path.parent().unwrap()).unwrap();
            fs::write(&config_path, config_doc).unwrap();
        }

        let config = BootstrapConfig::new(&connection_file_path, 0)
            .with_daemon_config_path(&config_path)
            .unwrap();
        let task = tokio::spawn(run_with_config(config));
        let mut client = wait_for_client(&connection_file_path, START_TIMEOUT).await;
        let _ =
            control_rpc_on_stream(&mut client, 1, ClientControlRequest::ServerDescribe {}).await;

        Self {
            connection_file_path,
            config_path,
            temp_dir,
            task,
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_dir_all(&self.temp_dir);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn config_spawns_supervises_registers_and_routes_stub_module() {
    let module_id = "daemon-aft";
    let daemon = RunningDaemon::start(
        "daemon-config-spawn",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;

    let entry = wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "running" && entry.enabled && entry.live,
        STATE_TIMEOUT,
    )
    .await;
    assert_eq!(entry.module_id, module_id);
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let route_channel = open_route(&mut client, module_id, 100).await;
    write_frame(
        &mut client,
        &data_request(
            route_channel,
            101,
            br#"{ "name": "echo", "arguments": { "value": 1 } }"#,
        ),
    )
    .await
    .unwrap();
    client.flush().await.unwrap();

    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel.channel);
    assert_eq!(response.header.corr, 101);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    let text = body["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("fake-aft tool echo called"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_configured_module_is_listed_then_enable_spawns_and_registers() {
    let module_id = "disabled-aft";
    let daemon = RunningDaemon::start(
        "daemon-config-disabled",
        Some(config_doc([stub_module(module_id, false, [])])),
    )
    .await;

    let entry = wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "disabled" && !entry.enabled && !entry.live,
        STATE_TIMEOUT,
    )
    .await;
    assert_eq!(entry.module_id, module_id);
    assert_catalog_modules(&daemon.connection_file_path, Some(module_id), 10, 0).await;

    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    match control_rpc_on_stream(
        &mut client,
        11,
        ClientControlRequest::SupervisorSetEnabled {
            module_id: module_id.to_string(),
            enabled: true,
        },
    )
    .await
    {
        ClientControlResponse::SupervisorAck {
            module_id: actual,
            applied,
        } => {
            assert_eq!(actual, module_id);
            assert!(applied);
        }
        other => panic!("unexpected supervisor.set_enabled response: {other:?}"),
    }

    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "running" && entry.enabled && entry.live,
        STATE_TIMEOUT,
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_spawn_is_visible_and_good_modules_still_start() {
    let good_module_id = "good-aft";
    let bad_module_id = "missing-aft";
    let missing_program = unique_temp_dir("missing-program").join("definitely-not-a-module");
    let daemon = RunningDaemon::start(
        "daemon-config-failed-spawn",
        Some(config_doc([
            stub_module(good_module_id, true, []),
            module_doc(bad_module_id, &missing_program, true, BTreeMap::new()),
        ])),
    )
    .await;

    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        bad_module_id,
        |entry| entry.state == "failed" && entry.enabled && !entry.live,
        STATE_TIMEOUT,
    )
    .await;
    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        good_module_id,
        |entry| entry.state == "running" && entry.enabled && entry.live,
        STATE_TIMEOUT,
    )
    .await;

    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let route_channel = open_route(&mut client, good_module_id, 20).await;
    write_frame(&mut client, &data_request(route_channel, 21, b"ping"))
        .await
        .unwrap();
    client.flush().await.unwrap();
    let response = read_frame_timeout(&mut client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route_channel.channel);
    assert_eq!(response.header.corr, 21);
    assert_eq!(response.body, b"ping");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_adds_reserved_module_and_attests_its_launch_nonce() {
    let existing_id = "rescan-existing";
    let added_id = "rescan-added";
    let existing = stub_module(existing_id, true, []);
    let daemon =
        RunningDaemon::start("daemon-rescan-add", Some(config_doc([existing.clone()]))).await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;

    let mut added = stub_module(added_id, true, []);
    added["reserved"] = json!(true);
    fs::write(&daemon.config_path, config_doc([existing, added])).unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 300).await;
    assert_eq!(result.added, [added_id]);
    assert!(result.removed.is_empty());
    assert!(result.changed_pending_reload.is_empty());
    assert_eq!(result.unchanged, 1);

    wait_for_catalog_module(&daemon.connection_file_path, added_id, STATE_TIMEOUT).await;
    let mut added_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let added_route = open_route(&mut added_client, added_id, 301).await;
    let nonce = call_tool(&mut added_client, added_route, 302, "_test.launch_nonce").await;
    assert!(
        !nonce.is_empty(),
        "rescan-added module must receive a launch nonce"
    );

    let mut consumer_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let attested_route = open_route_with_consumer_identity(
        &mut consumer_client,
        existing_id,
        303,
        ConsumerIdentity {
            module_id: added_id.to_string(),
            launch_nonce: nonce,
        },
    )
    .await
    .expect("launch nonce from rescan-added module should attest its consumer identity");
    assert!(attested_route.channel > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_removes_module_and_leaves_other_open_route_undisturbed() {
    let kept_id = "rescan-kept";
    let removed_id = "rescan-removed";
    let kept = stub_module(kept_id, true, []);
    let removed = stub_module(removed_id, true, []);
    let daemon = RunningDaemon::start(
        "daemon-rescan-remove",
        Some(config_doc([kept.clone(), removed])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, kept_id, STATE_TIMEOUT).await;
    wait_for_catalog_module(&daemon.connection_file_path, removed_id, STATE_TIMEOUT).await;

    let mut kept_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let kept_route = open_route(&mut kept_client, kept_id, 400).await;
    let mut removed_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let removed_route = open_route(&mut removed_client, removed_id, 401).await;

    fs::write(&daemon.config_path, config_doc([kept])).unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 402).await;
    assert_eq!(result.removed, [removed_id]);
    assert_eq!(result.unchanged, 1);
    wait_for_supervisor_absent(&daemon.connection_file_path, removed_id, STATE_TIMEOUT).await;
    wait_for_catalog_absent(&daemon.connection_file_path, removed_id, STATE_TIMEOUT).await;

    let closing = read_frame_timeout(&mut removed_client).await;
    assert_eq!(closing.header.ty, FrameType::Push);
    assert_eq!(closing.header.channel, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&closing.body).unwrap(),
        json!({"op": "route.closing", "module_id": removed_id, "reason": "disable"})
    );
    let closed = read_frame_timeout(&mut removed_client).await;
    assert_eq!(closed.header.ty, FrameType::Push);
    assert_eq!(closed.header.channel, 0);
    assert_eq!(
        serde_json::from_slice::<Value>(&closed.body).unwrap(),
        json!({
            "op": "route.closed",
            "module_id": removed_id,
            "reason": "disable",
            "drained": true,
            "abandoned": 0,
            "terminal": true,
        })
    );
    let goodbye = read_frame_timeout(&mut removed_client).await;
    assert_eq!(goodbye.header.ty, FrameType::Goodbye);
    assert_eq!(goodbye.header.channel, removed_route.channel);

    write_frame(
        &mut kept_client,
        &data_request(kept_route, 403, b"still-open"),
    )
    .await
    .unwrap();
    kept_client.flush().await.unwrap();
    let response = read_frame_timeout(&mut kept_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.body, b"still-open");

    let mut unknown_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let error = open_route_with_identity(&mut unknown_client, removed_id, 404, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "module_removed");
    assert!(error.message.contains(removed_id));
    assert!(error.message.contains("ago"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn route_open_unknown_id_without_tombstone_stays_unknown_module() {
    let daemon = RunningDaemon::start("daemon-unknown-no-tombstone", Some(config_doc([]))).await;
    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let error = open_route_with_identity(&mut client, "never-configured", 405, None)
        .await
        .unwrap_err();
    assert_eq!(error.code, "unknown_module");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_readding_a_module_clears_its_route_open_removal_tombstone() {
    let module_id = "rescan-readd";
    let configured = stub_module(module_id, true, []);
    let daemon = RunningDaemon::start(
        "daemon-rescan-readd",
        Some(config_doc([configured.clone()])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    fs::write(&daemon.config_path, config_doc([])).unwrap();
    let removed = supervisor_rescan(&daemon.connection_file_path, 410).await;
    assert_eq!(removed.removed, [module_id]);
    wait_for_catalog_absent(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut removed_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let tombstone = open_route_with_identity(&mut removed_client, module_id, 411, None)
        .await
        .unwrap_err();
    assert_eq!(tombstone.code, "module_removed");

    fs::write(&daemon.config_path, config_doc([configured])).unwrap();
    let added = supervisor_rescan(&daemon.connection_file_path, 412).await;
    assert_eq!(added.added, [module_id]);
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut readded_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let route = open_route(&mut readded_client, module_id, 413).await;
    assert!(
        route.channel > 0,
        "re-added module must open a normal route"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retained_reserved_gate_refuses_hello_until_explicit_release_after_rescan_removal() {
    let module_id = "reserved-after-removal";
    let mut reserved = stub_module(module_id, false, []);
    reserved["reserved"] = json!(true);
    let daemon =
        RunningDaemon::start("daemon-reserved-release", Some(config_doc([reserved]))).await;
    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "disabled" && !entry.enabled,
        STATE_TIMEOUT,
    )
    .await;

    let mut configured_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let configured_error = control_rpc_result_on_stream(
        &mut configured_client,
        420,
        ClientControlRequest::SupervisorReleaseReserved {
            module_id: module_id.to_string(),
        },
    )
    .await
    .expect_err("release must refuse while the module remains configured");
    assert_eq!(configured_error.code, "reserved_module_configured");

    fs::write(&daemon.config_path, config_doc([])).unwrap();
    let removed = supervisor_rescan(&daemon.connection_file_path, 421).await;
    assert_eq!(removed.removed, [module_id]);
    wait_for_supervisor_absent(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    let mut forged_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let rejection =
        hello_error_on_stream(&mut forged_client, untrusted_hello_frame(module_id, 422)).await;
    assert_eq!(rejection.code, "reserved_module");
    assert_catalog_modules(&daemon.connection_file_path, Some(module_id), 423, 0).await;

    let mut release_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let released = control_rpc_on_stream(
        &mut release_client,
        424,
        ClientControlRequest::SupervisorReleaseReserved {
            module_id: module_id.to_string(),
        },
    )
    .await;
    assert!(matches!(
        released,
        ClientControlResponse::SupervisorAck { applied: true, .. }
    ));

    let mut released_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let accepted =
        hello_on_stream(&mut released_client, untrusted_hello_frame(module_id, 425)).await;
    assert_eq!(accepted.header.ty, FrameType::HelloAck);
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_changed_spec_is_pending_until_reload_uses_it() {
    let module_id = "rescan-changed";
    let original = stub_module(
        module_id,
        true,
        [("FAKE_AFT_TOOLCALL_RESULT", "before-rescan")],
    );
    let daemon = RunningDaemon::start("daemon-rescan-changed", Some(config_doc([original]))).await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut old_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let old_route = open_route(&mut old_client, module_id, 501).await;
    let before_pid = call_tool(&mut old_client, old_route, 502, "_test.pid").await;

    let changed = stub_module(
        module_id,
        true,
        [("FAKE_AFT_TOOLCALL_RESULT", "after-reload")],
    );
    fs::write(&daemon.config_path, config_doc([changed])).unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 503).await;
    assert_eq!(result.changed_pending_reload, [module_id]);
    assert_eq!(
        call_tool(&mut old_client, old_route, 504, "_test.pid").await,
        before_pid
    );
    assert_eq!(
        call_tool(&mut old_client, old_route, 505, "echo").await,
        "before-rescan"
    );

    let mut control = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let response = control_rpc_on_stream(
        &mut control,
        506,
        ClientControlRequest::SupervisorReload {
            module_id: module_id.to_string(),
        },
    )
    .await;
    assert!(matches!(
        response,
        ClientControlResponse::SupervisorAck { applied: true, .. }
    ));

    let mut new_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let new_route = open_route(&mut new_client, module_id, 507).await;
    assert_ne!(
        call_tool(&mut new_client, new_route, 508, "_test.pid").await,
        before_pid
    );
    assert_eq!(
        call_tool(&mut new_client, new_route, 509, "echo").await,
        "after-reload"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_add_does_not_interrupt_existing_in_flight_request() {
    let existing_id = "rescan-in-flight";
    let added_id = "rescan-in-flight-added";
    let existing = stub_module(existing_id, true, [("FAKE_AFT_TOOLCALL_DELAY_MS", "250")]);
    let daemon = RunningDaemon::start(
        "daemon-rescan-in-flight",
        Some(config_doc([existing.clone()])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;

    let mut existing_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let existing_route = open_route(&mut existing_client, existing_id, 600).await;
    write_frame(
        &mut existing_client,
        &data_request(
            existing_route,
            601,
            br#"{"name":"echo","arguments":{"value":"in-flight"}}"#,
        ),
    )
    .await
    .unwrap();
    existing_client.flush().await.unwrap();

    fs::write(
        &daemon.config_path,
        config_doc([existing, stub_module(added_id, true, [])]),
    )
    .unwrap();
    let result = supervisor_rescan(&daemon.connection_file_path, 602).await;
    assert_eq!(result.added, [added_id]);
    assert_eq!(result.unchanged, 1);

    let response = read_frame_timeout(&mut existing_client).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, existing_route.channel);
    assert_eq!(response.header.corr, 601);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    assert!(body["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("in-flight"));

    assert_eq!(
        call_tool(&mut existing_client, existing_route, 603, "echo").await,
        "fake-aft tool echo called with {}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_invalid_config_is_rejected_without_mutating_modules() {
    let module_id = "rescan-fail-loud";
    let original = stub_module(module_id, true, []);
    let daemon = RunningDaemon::start(
        "daemon-rescan-fail-loud",
        Some(config_doc([original.clone()])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut module_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let module_route = open_route(&mut module_client, module_id, 700).await;
    let before_pid = call_tool(&mut module_client, module_route, 701, "_test.pid").await;

    fs::write(&daemon.config_path, "{ invalid jsonc").unwrap();
    let corrupt_error = supervisor_rescan_error(&daemon.connection_file_path, 702).await;
    assert_eq!(corrupt_error.code, "invalid_daemon_config");
    let after_corrupt = supervisor_modules(&daemon.connection_file_path, 703).await;
    assert_eq!(after_corrupt.len(), 1);
    assert_eq!(
        call_tool(&mut module_client, module_route, 704, "_test.pid").await,
        before_pid
    );

    let mut owner = original;
    owner["reserved"] = json!(true);
    owner["reserved_prefixes"] = json!(["owned:"]);
    let overlapping = stub_module("owned:child", true, []);
    fs::write(&daemon.config_path, config_doc([owner, overlapping])).unwrap();
    let overlap_error = supervisor_rescan_error(&daemon.connection_file_path, 705).await;
    assert_eq!(overlap_error.code, "invalid_daemon_config");
    let after_overlap = supervisor_modules(&daemon.connection_file_path, 706).await;
    assert_eq!(after_overlap.len(), 1);
    assert_eq!(after_overlap[0].module_id, module_id);
    assert_eq!(
        call_tool(&mut module_client, module_route, 707, "_test.pid").await,
        before_pid
    );
}

/// A corrupt config is refused loudly and was already covered. An ABSENT one
/// took the quieter path: `load` reports a missing file as Ok(None), which is
/// right at boot and wrong here, because rescan reads "not in the config" as
/// "remove it". An empty module list is therefore an instruction to retire the
/// entire running fleet, and any editor writing via write-new-then-rename opens
/// a window where that is what rescan sees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_absent_config_is_refused_and_retires_nothing() {
    let module_id = "rescan-absent-config";
    let daemon = RunningDaemon::start(
        "daemon-rescan-absent",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut module_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let module_route = open_route(&mut module_client, module_id, 760).await;
    let before_pid = call_tool(&mut module_client, module_route, 761, "_test.pid").await;

    fs::remove_file(&daemon.config_path).unwrap();
    let error = supervisor_rescan_error(&daemon.connection_file_path, 762).await;
    assert_eq!(error.code, "invalid_daemon_config");

    // The module is not merely still listed: it is the same process, still
    // serving on the route opened before the rescan.
    let after = supervisor_modules(&daemon.connection_file_path, 763).await;
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].module_id, module_id);
    assert_eq!(
        call_tool(&mut module_client, module_route, 764, "_test.pid").await,
        before_pid
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_preview_reports_the_removal_it_would_make_and_makes_none() {
    // The preview exists so an operator can see that a rescan is about to stop a
    // live process BEFORE it stops one. So the assertion that matters is not that
    // the diff is right -- it is that the process named in the diff is still
    // serving afterwards. Asserting only the returned lists would pass on an
    // implementation that reported correctly and retired anyway.
    let module_id = "rescan-preview";
    let daemon = RunningDaemon::start(
        "daemon-rescan-preview",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut module_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let module_route = open_route(&mut module_client, module_id, 900).await;
    let before_pid = call_tool(&mut module_client, module_route, 901, "_test.pid").await;

    // Remove the module from the config, then preview.
    fs::write(&daemon.config_path, config_doc([])).unwrap();
    let preview = supervisor_rescan_with(&daemon.connection_file_path, 902, true).await;

    assert_eq!(preview.removed, vec![module_id.to_string()]);
    assert!(preview.added.is_empty());
    assert!(
        preview.preview,
        "the result must carry the preview flag, or a reader meeting this output \
         later cannot tell a preview from an execution"
    );

    // The effect assertion: same process, same route, still answering.
    let after = supervisor_modules(&daemon.connection_file_path, 903).await;
    assert_eq!(after.len(), 1, "preview must not retire the module");
    assert_eq!(after[0].module_id, module_id);
    assert_eq!(
        call_tool(&mut module_client, module_route, 904, "_test.pid").await,
        before_pid,
        "the route opened before the preview must still be served by the same process"
    );

    // And the same call without preview DOES apply it, so the test cannot pass on
    // an implementation that simply never retires anything.
    let applied = supervisor_rescan_with(&daemon.connection_file_path, 905, false).await;
    assert_eq!(applied.removed, vec![module_id.to_string()]);
    assert!(!applied.preview);
    assert!(
        supervisor_modules(&daemon.connection_file_path, 906)
            .await
            .is_empty(),
        "a non-preview rescan must retire the module the preview only reported"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_reports_config_sections_it_cannot_apply() {
    // A section outside `modules` changes, rescan cannot apply it, and until now
    // the only evidence was a journal warning. Reported by an outside contributor
    // whose module crash-looped through four respawns because a new top-level
    // `storage` section was silently not applied: rescan said `added: <module>`,
    // which was true, and the module could not run.
    //
    // Asserted on BOTH the preview and the executed rescan, because the preview
    // is where a careful operator looks first and is exactly where a missing
    // warning costs the most.
    let module_id = "rescan-restart-required";
    let daemon = RunningDaemon::start(
        "daemon-rescan-restart-required",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    // With only the `modules` section touched, `restart_required` must stay
    // empty. Without this the test would pass on an implementation that reports a
    // restart requirement for EVERY rescan, including the ones where no such
    // section changed -- an alarm that is always on carries no information.
    let unchanged_sections = supervisor_rescan_with(&daemon.connection_file_path, 940, true).await;
    assert!(
        unchanged_sections.restart_required.is_empty(),
        "no non-modules section changed, so nothing should be reported: {:?}",
        unchanged_sections.restart_required
    );

    // Add a top-level storage section, leaving the modules section identical.
    let mut doc: Value =
        serde_json::from_str(&config_doc([stub_module(module_id, true, [])])).unwrap();
    doc["storage"] = json!({
        "backend": "sqlite",
        "data_home": daemon.config_path.parent().unwrap().join("store"),
    });
    fs::write(
        &daemon.config_path,
        serde_json::to_string_pretty(&doc).unwrap(),
    )
    .unwrap();

    let preview = supervisor_rescan_with(&daemon.connection_file_path, 941, true).await;
    assert_eq!(
        preview.restart_required,
        vec!["storage".to_string()],
        "the preview must name the section it cannot apply"
    );
    // The module section is genuinely unchanged, so the operator would otherwise
    // read a completely quiet result for a config edit that does not take effect.
    assert!(preview.added.is_empty());
    assert!(preview.changed_pending_reload.is_empty());

    let applied = supervisor_rescan_with(&daemon.connection_file_path, 942, false).await;
    assert_eq!(
        applied.restart_required,
        vec!["storage".to_string()],
        "an executed rescan must report it too -- it is the run that leaves the \
         daemon serving stale config"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_preview_reports_an_enabled_flip_it_would_apply() {
    // Rescan calls set_enabled for an enabled-flip, so a preview that omits them
    // under-reports a mutation class it performs. The bucket arithmetic is what
    // exposes it: a module changing only its enabled flag is deliberately not
    // counted as unchanged, so before this field existed the buckets summed to one
    // less than the configured module count and nothing else in the output moved.
    //
    // Asserting the sum is what makes this fail if a FUTURE mutation class is added
    // without a bucket; naming the field alone would only cover this one.
    let module_id = "rescan-preview-enabled";
    let daemon = RunningDaemon::start(
        "daemon-rescan-preview-enabled",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    fs::write(
        &daemon.config_path,
        config_doc([stub_module(module_id, false, [])]),
    )
    .unwrap();
    let preview = supervisor_rescan_with(&daemon.connection_file_path, 940, true).await;

    assert_eq!(
        preview.enabled_changes,
        vec![module_id.to_string()],
        "an enabled flip must appear in the preview, or the operator is told a rescan \
         will change nothing while it is about to stop a live module"
    );
    assert!(preview.added.is_empty());
    assert!(preview.removed.is_empty());
    assert!(preview.changed_pending_reload.is_empty());
    assert_eq!(
        preview.added.len()
            + preview.removed.len()
            + preview.changed_pending_reload.len()
            + preview.enabled_changes.len()
            + preview.unchanged as usize,
        1,
        "every configured module must land in exactly one bucket; a shortfall means the \
         preview performs a mutation class it does not report"
    );

    // The effect assertion: the preview reported the flip and must not have applied
    // it, so the module is still enabled and live.
    let after = supervisor_modules(&daemon.connection_file_path, 941).await;
    assert_eq!(after.len(), 1);
    assert!(
        after[0].enabled && after[0].live,
        "preview must not apply the enabled flip it reported"
    );

    // And the applying call reports the same bucket, so a reader cannot conclude the
    // field is preview-only decoration.
    let applied = supervisor_rescan_with(&daemon.connection_file_path, 942, false).await;
    assert_eq!(applied.enabled_changes, vec![module_id.to_string()]);
    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "disabled" && !entry.enabled && !entry.live,
        STATE_TIMEOUT,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_applies_enabled_flips_without_daemon_restart() {
    let module_id = "rescan-enabled";
    let daemon = RunningDaemon::start(
        "daemon-rescan-enabled",
        Some(config_doc([stub_module(module_id, true, [])])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut before_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let before_route = open_route(&mut before_client, module_id, 800).await;
    let before_pid = call_tool(&mut before_client, before_route, 801, "_test.pid").await;

    fs::write(
        &daemon.config_path,
        config_doc([stub_module(module_id, false, [])]),
    )
    .unwrap();
    supervisor_rescan(&daemon.connection_file_path, 802).await;
    wait_for_supervisor_entry(
        &daemon.connection_file_path,
        module_id,
        |entry| entry.state == "disabled" && !entry.enabled && !entry.live,
        STATE_TIMEOUT,
    )
    .await;
    wait_for_catalog_absent(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;

    fs::write(
        &daemon.config_path,
        config_doc([stub_module(module_id, true, [])]),
    )
    .unwrap();
    supervisor_rescan(&daemon.connection_file_path, 803).await;
    wait_for_catalog_module(&daemon.connection_file_path, module_id, STATE_TIMEOUT).await;
    let mut enabled_client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let enabled_route = open_route(&mut enabled_client, module_id, 804).await;
    assert_ne!(
        call_tool(&mut enabled_client, enabled_route, 805, "_test.pid").await,
        before_pid
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_rescans_queue_and_reconcile_once() {
    let existing_id = "rescan-concurrent-existing";
    let added_id = "rescan-concurrent-added";
    let existing = stub_module(existing_id, true, []);
    let daemon = RunningDaemon::start(
        "daemon-rescan-concurrent",
        Some(config_doc([existing.clone()])),
    )
    .await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;
    fs::write(
        &daemon.config_path,
        config_doc([existing, stub_module(added_id, true, [])]),
    )
    .unwrap();

    let (first, second) = tokio::join!(
        supervisor_rescan(&daemon.connection_file_path, 900),
        supervisor_rescan(&daemon.connection_file_path, 901),
    );
    let reports = [first, second];
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.added == [added_id])
            .count(),
        1
    );
    assert_eq!(
        reports
            .iter()
            .filter(|report| report.added.is_empty() && report.unchanged == 2)
            .count(),
        1
    );
    wait_for_catalog_module(&daemon.connection_file_path, added_id, STATE_TIMEOUT).await;
    let modules = supervisor_modules(&daemon.connection_file_path, 902).await;
    assert_eq!(modules.len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ck_module_rescan_prints_reconcile_table() {
    let existing_id = "ck-rescan-existing";
    let added_id = "ck-rescan-added";
    let existing = stub_module(existing_id, true, []);
    let daemon =
        RunningDaemon::start("daemon-ck-rescan", Some(config_doc([existing.clone()]))).await;
    wait_for_catalog_module(&daemon.connection_file_path, existing_id, STATE_TIMEOUT).await;
    fs::write(
        &daemon.config_path,
        config_doc([existing, stub_module(added_id, true, [])]),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_ck"))
        .args(["module", "rescan", "--subc"])
        .arg(&daemon.connection_file_path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ck stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("change"));
    assert!(stdout.contains("changed-pending-reload"));
    assert!(stdout.contains(added_id));
    assert!(stdout.contains("unchanged"));
    wait_for_catalog_module(&daemon.connection_file_path, added_id, STATE_TIMEOUT).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn required_capability_absence_surfaces_in_health_and_server_describe() {
    let consumer = stub_module(
        "capability-consumer",
        true,
        [
            (
                "FAKE_AFT_CAPABILITIES",
                r#"{"requires":[{"capability":"credentials-provider/v1","need":"required"}]}"#,
            ),
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
        ],
    );
    let disabled_provider = stub_module(
        "capability-provider",
        false,
        [(
            "FAKE_AFT_CAPABILITIES",
            r#"{"provides":["credentials-provider/v1"]}"#,
        )],
    );
    let daemon = RunningDaemon::start(
        "capability-surface",
        Some(config_doc([consumer, disabled_provider])),
    )
    .await;
    wait_for_catalog_module(
        &daemon.connection_file_path,
        "capability-consumer",
        STATE_TIMEOUT,
    )
    .await;

    let mut client = wait_for_client(&daemon.connection_file_path, START_TIMEOUT).await;
    let describe =
        control_rpc_on_stream(&mut client, 950, ClientControlRequest::ServerDescribe {}).await;
    let ClientControlResponse::ServerDescribe {
        capability_requirements,
        ..
    } = describe
    else {
        panic!("server.describe response expected");
    };
    assert!(capability_requirements.iter().any(|status| {
        status.consumer == "capability-consumer"
            && status.capability == "credentials-provider/v1"
            && status.verdict == "never_provided"
            && status.detail.contains("credentials-provider/v1")
    }));

    let health = supervisor_health_entries(&daemon.connection_file_path, 951).await;
    let consumer_health = health
        .iter()
        .find(|entry| entry.module_id == "capability-consumer")
        .expect("consumer health row");
    assert!(
        consumer_health
            .detail
            .as_deref()
            .unwrap_or_default()
            .contains("requires:credentials-provider/v1 unprovided"),
        "capability absence must render in ck health/module status detail"
    );

    for args in [
        vec![
            "health",
            "--subc",
            daemon.connection_file_path.to_str().unwrap(),
        ],
        vec![
            "health",
            "capability-consumer",
            "--subc",
            daemon.connection_file_path.to_str().unwrap(),
        ],
        vec![
            "module",
            "status",
            "capability-consumer",
            "--subc",
            daemon.connection_file_path.to_str().unwrap(),
        ],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ck"))
            .args(args)
            .output()
            .expect("ck launches");
        assert!(
            output.status.success(),
            "ck stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("requires:credentials-provider/v1 unprovided"),
            "ck must render the daemon-owned capability detail"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rescan_preview_reports_removal_that_strands_a_capability_consumer() {
    let consumer = stub_module(
        "preview-consumer",
        true,
        [(
            "FAKE_AFT_CAPABILITIES",
            r#"{"requires":[{"capability":"credentials-provider/v1","need":"required"}]}"#,
        )],
    );
    let provider = stub_module(
        "preview-provider",
        true,
        [(
            "FAKE_AFT_CAPABILITIES",
            r#"{"provides":["credentials-provider/v1"]}"#,
        )],
    );
    let daemon = RunningDaemon::start(
        "capability-preview",
        Some(config_doc([consumer.clone(), provider])),
    )
    .await;
    wait_for_catalog_module(
        &daemon.connection_file_path,
        "preview-consumer",
        STATE_TIMEOUT,
    )
    .await;
    wait_for_catalog_module(
        &daemon.connection_file_path,
        "preview-provider",
        STATE_TIMEOUT,
    )
    .await;

    fs::write(&daemon.config_path, config_doc([consumer])).unwrap();
    let preview = supervisor_rescan_with(&daemon.connection_file_path, 952, true).await;
    assert_eq!(
        preview.capability_warnings,
        vec![
            "removing preview-provider leaves preview-consumer requires:credentials-provider/v1 unprovided"
                .to_string()
        ]
    );
    assert!(
        catalog_modules(&daemon.connection_file_path, Some("preview-provider"), 953)
            .await
            .len()
            == 1,
        "preview must not remove the provider it warns about"
    );
    let output = Command::new(env!("CARGO_BIN_EXE_ck"))
        .args(["module", "rescan", "--dry-run", "--subc"])
        .arg(&daemon.connection_file_path)
        .output()
        .expect("ck launches");
    assert!(
        output.status.success(),
        "ck stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("removing preview-provider leaves preview-consumer requires:credentials-provider/v1 unprovided")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn absent_config_starts_bare_daemon_with_no_supervised_modules() {
    let daemon = RunningDaemon::start("daemon-config-absent", None).await;
    assert!(!daemon.config_path.exists());
    let modules = supervisor_modules(&daemon.connection_file_path, 30).await;
    assert!(modules.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn present_invalid_config_fails_loud_before_daemon_starts() {
    let temp_dir = unique_temp_dir("daemon-config-invalid");
    fs::create_dir_all(&temp_dir).unwrap();
    let connection_file_path = temp_dir.join("subc-conn.json");
    let config_path = temp_dir.join("subc.jsonc");
    fs::write(
        &config_path,
        r#"{ "version": 1, "modules": { "aft": "wrong" } }"#,
    )
    .unwrap();

    let err =
        run_with_daemon_config_path(BootstrapConfig::new(&connection_file_path, 0), &config_path)
            .await
            .unwrap_err();
    let message = err.to_string();
    assert!(message.contains(&config_path.display().to_string()));
    assert!(message.contains("invalid daemon config"));
    assert!(!connection_file_path.exists());

    let _ = fs::remove_dir_all(temp_dir);
}

fn config_doc<const N: usize>(modules: [Value; N]) -> String {
    let mut module_map = serde_json::Map::new();
    for module in modules {
        let module_id = module["module_id"].as_str().unwrap().to_string();
        let mut module = module.as_object().unwrap().clone();
        module.remove("module_id");
        module_map.insert(module_id, Value::Object(module));
    }
    serde_json::to_string_pretty(&json!({
        "version": 1,
        "modules": module_map,
    }))
    .unwrap()
}

fn stub_module<const N: usize>(
    module_id: &str,
    enabled: bool,
    extra_env: [(&str, &str); N],
) -> Value {
    let mut env = BTreeMap::from([("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string())]);
    for (key, value) in extra_env {
        env.insert(key.to_string(), value.to_string());
    }
    module_doc(
        module_id,
        Path::new(env!("CARGO_BIN_EXE_fake-aft-stub")),
        enabled,
        env,
    )
}

fn module_doc(
    module_id: &str,
    program: &Path,
    enabled: bool,
    env: BTreeMap<String, String>,
) -> Value {
    json!({
        "module_id": module_id,
        "program": program.to_string_lossy(),
        "args": [],
        "env": env,
        "enabled": enabled,
    })
}

async fn wait_for_client(path: &Path, wait: Duration) -> TcpStream {
    let deadline = Instant::now() + wait;
    loop {
        match connect_authed_client(path).await {
            Ok(client) => return client,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(20)).await;
            }
            Err(err) => panic!("daemon did not accept authenticated client within {wait:?}: {err}"),
        }
    }
}

async fn wait_for_supervisor_entry(
    path: &Path,
    module_id: &str,
    predicate: impl Fn(&SupervisorEntry) -> bool,
    wait: Duration,
) -> SupervisorEntry {
    let deadline = Instant::now() + wait;
    let mut corr = 1_000;
    loop {
        let modules = supervisor_modules(path, corr).await;
        if let Some(entry) = modules
            .into_iter()
            .find(|entry| entry.module_id == module_id && predicate(entry))
        {
            return entry;
        }
        if Instant::now() >= deadline {
            let modules = supervisor_modules(path, corr + 10_000).await;
            panic!("module {module_id} did not reach expected supervisor state within {wait:?}; modules: {modules:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn supervisor_modules(path: &Path, corr: u64) -> Vec<SupervisorEntry> {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorList {}).await {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
}

async fn wait_for_catalog_module(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 2_000;
    loop {
        if catalog_modules(path, Some(module_id), corr).await.len() == 1 {
            return;
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not register in catalog within {wait:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

/// Assert the catalog holds `expected` modules matching `module_id`.
///
/// WHAT THIS DOES NOT PROVE: that the daemon's `SUBC_MODULE_ID` injection
/// reached the module. The stub reads its id from its OWN variable
/// (`FAKE_AFT_MODULE_ID`), so these assertions establish that the stub honours
/// its own config and are silent on daemon injection.
///
/// NOR IS THAT PROVEN ELSEWHERE, and this comment first claimed it was. The
/// spawn-attestation tests in `control.rs` assert the daemon REFUSES a bad
/// consumer identity and STAMPS `Reserved` for a good one -- they seed the
/// nonce directly via `set_spawn_nonce` and never spawn a process, so they
/// prove the guard's logic and are equally silent on whether a spawned child
/// receives the variable. The two suites fail in the same direction, which is
/// why reading either one leaves the impression the other covers it.
///
/// What is actually established: the daemon SETS the variable at the spawn site
/// (`supervise.rs`, `command.env(SUBC_MODULE_ID_ENV, ..)`), and the real client
/// library CONSUMES it (`subc-client-rs::serve` overrides `manifest.module_id`).
/// Both are single-line source facts; neither is under test end to end.
/// Proving it needs a supervised module spawned WITHOUT the variable, asserted
/// to register under its compiled fallback -- a configuration production never
/// uses, which is why it does not exist yet. Recorded as a gap rather than left
/// to be inferred as coverage.
///
/// The pointer is here because this is where a reader forms the wrong
/// conclusion: someone auditing "is id delivery tested" finds module-id
/// assertions in the catalog tests and answers yes for the wrong reason.
/// Coverage that lives somewhere other than where a reader looks for it will
/// eventually be re-asserted wrongly (CKE2E's clause, from the same exchange
/// that produced the vacuity note on `DEFAULT_MODULE_ID`).
async fn assert_catalog_modules(path: &Path, module_id: Option<&str>, corr: u64, expected: usize) {
    let modules = catalog_modules(path, module_id, corr).await;
    assert_eq!(modules.len(), expected, "catalog modules: {modules:?}");
}

async fn catalog_modules(
    path: &Path,
    module_id: Option<&str>,
    corr: u64,
) -> Vec<subc_control::CatalogEntry> {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    match control_rpc_on_stream(
        &mut client,
        corr,
        ClientControlRequest::CatalogList {
            module_id: module_id.map(ToOwned::to_owned),
        },
    )
    .await
    {
        ClientControlResponse::CatalogList { modules, .. } => modules,
        other => panic!("unexpected catalog.list response: {other:?}"),
    }
}

async fn supervisor_health_entries(
    path: &Path,
    corr: u64,
) -> Vec<subc_control::SupervisorHealthEntry> {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorHealth {}).await
    {
        ClientControlResponse::SupervisorHealth { modules, .. } => modules,
        other => panic!("unexpected supervisor.health response: {other:?}"),
    }
}

async fn supervisor_rescan(path: &Path, corr: u64) -> SupervisorRescanResult {
    supervisor_rescan_with(path, corr, false).await
}

async fn supervisor_rescan_with(path: &Path, corr: u64, preview: bool) -> SupervisorRescanResult {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    match control_rpc_on_stream(
        &mut client,
        corr,
        ClientControlRequest::SupervisorRescan { preview },
    )
    .await
    {
        ClientControlResponse::SupervisorRescan { result } => result,
        other => panic!("unexpected supervisor.rescan response: {other:?}"),
    }
}

async fn supervisor_rescan_error(path: &Path, corr: u64) -> ErrorBody {
    let mut client = wait_for_client(path, START_TIMEOUT).await;
    control_rpc_result_on_stream(
        &mut client,
        corr,
        ClientControlRequest::SupervisorRescan { preview: false },
    )
    .await
    .expect_err("supervisor.rescan should return a typed error")
}

async fn wait_for_supervisor_absent(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 3_000;
    loop {
        if supervisor_modules(path, corr)
            .await
            .iter()
            .all(|entry| entry.module_id != module_id)
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} remained in supervisor.list after {wait:?}"
        );
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_catalog_absent(path: &Path, module_id: &str, wait: Duration) {
    let deadline = Instant::now() + wait;
    let mut corr = 4_000;
    loop {
        if catalog_modules(path, Some(module_id), corr)
            .await
            .is_empty()
        {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "module {module_id} remained in catalog after {wait:?}"
        );
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

#[derive(Debug, Clone, Copy)]
struct RouteHandle {
    channel: u16,
    epoch: u32,
}

async fn open_route<S>(stream: &mut S, module_id: &str, corr: u64) -> RouteHandle
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    open_route_with_identity(stream, module_id, corr, None)
        .await
        .unwrap_or_else(|error| panic!("route.open returned error: {error:?}"))
}

async fn open_route_with_consumer_identity<S>(
    stream: &mut S,
    module_id: &str,
    corr: u64,
    consumer_identity: ConsumerIdentity,
) -> Result<RouteHandle, ErrorBody>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    open_route_with_identity(stream, module_id, corr, Some(consumer_identity)).await
}

async fn open_route_with_identity<S>(
    stream: &mut S,
    module_id: &str,
    corr: u64,
    consumer_identity: Option<ConsumerIdentity>,
) -> Result<RouteHandle, ErrorBody>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let project_root = unique_temp_dir("daemon-config-project");
    fs::create_dir_all(&project_root).unwrap();
    match control_rpc_result_on_stream(
        stream,
        corr,
        ClientControlRequest::RouteOpen {
            target: RouteTarget::ToolProvider {
                module_id: module_id.to_string(),
            },
            identity: BindIdentity {
                project_root,
                harness: "daemon-config-test".to_string(),
                session: format!("session-{corr}"),
            },
            consumer_identity,
            consumer_capabilities: None,
            admission_facts: None,
        },
    )
    .await?
    {
        ClientControlResponse::RouteOpen {
            route_channel,
            route_epoch,
        } => Ok(RouteHandle {
            channel: route_channel,
            epoch: route_epoch,
        }),
        other => panic!("unexpected route.open response: {other:?}"),
    }
}

async fn call_tool<S>(stream: &mut S, route: RouteHandle, corr: u64, name: &str) -> String
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let body = serde_json::to_vec(&json!({ "name": name, "arguments": {} })).unwrap();
    write_frame(stream, &data_request(route, corr, &body))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let response = read_frame_timeout(stream).await;
    assert_eq!(response.header.ty, FrameType::Response);
    assert_eq!(response.header.channel, route.channel);
    assert_eq!(response.header.corr, corr);
    let body: Value = serde_json::from_slice(&response.body).unwrap();
    body["content"][0]["text"]
        .as_str()
        .expect("stub tool response should contain text")
        .to_string()
}

async fn control_rpc_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> ClientControlResponse
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    control_rpc_result_on_stream(stream, corr, request)
        .await
        .unwrap_or_else(|error| panic!("control RPC returned error: {error:?}"))
}

async fn control_rpc_result_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> Result<ClientControlResponse, ErrorBody>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &control_request_frame(corr, request))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let frame = read_frame_timeout(stream).await;
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    match frame.header.ty {
        FrameType::Response => Ok(serde_json::from_slice(&frame.body).unwrap()),
        FrameType::Error => Err(serde_json::from_slice(&frame.body).unwrap()),
        ty => panic!("unexpected control RPC frame type: {ty:?}"),
    }
}

fn control_request_frame(corr: u64, request: ClientControlRequest) -> Frame {
    let body = serde_json::to_vec(&request).unwrap();
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

fn untrusted_hello_frame(module_id: &str, corr: u64) -> Frame {
    let manifest = ModuleManifest::builder(
        module_id,
        "0.0.0-test",
        TrustTier::FirstParty,
        Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: Vec::new(),
                optional: vec![IdentityScope::Project],
            },
        },
    )
    .build();
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest,
        protocol_ver: PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    })
    .unwrap();
    Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Passive, false),
        0,
        0,
        corr,
        body,
    )
    .unwrap()
}

async fn hello_on_stream<S>(stream: &mut S, hello: Frame) -> Frame
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &hello).await.unwrap();
    stream.flush().await.unwrap();
    read_frame_timeout(stream).await
}

async fn hello_error_on_stream<S>(stream: &mut S, hello: Frame) -> ErrorBody
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let frame = hello_on_stream(stream, hello).await;
    assert_eq!(frame.header.ty, FrameType::Error);
    serde_json::from_slice(&frame.body).unwrap()
}

fn data_request(route: RouteHandle, corr: u64, body: &[u8]) -> Frame {
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        route.channel,
        route.epoch,
        corr,
        body.to_vec(),
    )
    .unwrap()
}

async fn read_frame_timeout<S>(stream: &mut S) -> Frame
where
    S: AsyncRead + Unpin,
{
    timeout(READ_TIMEOUT, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for frame")
}

fn unique_temp_dir(name: &str) -> PathBuf {
    let nonce = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("subc-core-{name}-{}-{nonce}", process::id()))
}
