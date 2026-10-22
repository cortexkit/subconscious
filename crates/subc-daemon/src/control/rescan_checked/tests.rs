use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use cortexkit_test_support::ScratchDir;
use serde_json::{json, Value};
use subc_control::{
    ActivityActor, ActivityQueryRequest, ActivityRecordRequest, ClientControlRequest,
    ClientControlResponse, RescanExpect, RescanModuleActionKind, SupervisorRescanResult,
    CONFIG_CHANGED_SINCE_PREVIEW, RESCAN_CANDIDATE_MAX_BYTES, RESCAN_CANDIDATE_PREVIEW_CAPABILITY,
};
use subc_protocol::{
    manifest::{CapabilityDeclarations, CapabilityNeed, CapabilityRequirement, ModuleManifest},
    FrameType, ModuleHelloBody, Principal, PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

use super::super::{
    control_flags, parse_client_control_request, ControlHandler, ControlRequestBodyError,
};
use crate::{
    registry::{ConnectionId, Registry},
    router::{FrameSink, OutboundFrame, RouteCtx},
    supervise::{RestartPolicy, Supervisor, SupervisorHandle},
    Frame,
};

/// One in-process daemon control handler that rescans `config`.
struct Rig {
    config: PathBuf,
    handler: ControlHandler,
    ctx: RouteCtx,
    _egress: mpsc::Receiver<OutboundFrame>,
    /// Holds the activity ledger. It is a scratch directory of its own, never
    /// `config`'s parent: some tests pass a relative config path, and a ledger
    /// beside it would be created inside the crate's folder.
    _ledger: ScratchDir,
}

impl Rig {
    /// A daemon whose activity ledger and spawn events would carry
    /// `incarnation`, reading `config`.
    fn new(config: &Path, incarnation: &str) -> Self {
        let registry = Arc::new(Registry::default());
        let handle = SupervisorHandle::new();
        let supervisor = Supervisor::new(registry.clone(), RestartPolicy::new(0, Duration::ZERO))
            .with_handle(handle.clone())
            .with_daemon_incarnation(incarnation.to_string());
        let ledger = ScratchDir::new("rescan-checked-ledger");
        let handler = ControlHandler::new(registry)
            .with_supervisor(handle)
            .with_activity_ledger(ledger.path(), incarnation.to_string())
            .with_supervisor_rescan(supervisor, config, None);
        let (tx, rx) = mpsc::channel(8);
        Self {
            config: config.to_path_buf(),
            handler,
            ctx: RouteCtx {
                connection_id: ConnectionId::new(41),
                egress: FrameSink::new(tx),
            },
            _egress: rx,
            _ledger: ledger,
        }
    }

    async fn send(&self, request: ClientControlRequest) -> Frame {
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
        let mut replies = tokio::time::timeout(
            Duration::from_secs(20),
            self.handler
                .handle_client_control_request(&self.ctx, frame, request, &mut None),
        )
        .await
        .expect("the control request answers")
        .unwrap();
        assert_eq!(replies.len(), 1, "one reply frame: {replies:?}");
        replies.remove(0)
    }

    /// The daemon's answer: the rescan result, or the refusal's code.
    async fn rescan(
        &self,
        request: ClientControlRequest,
    ) -> Result<SupervisorRescanResult, String> {
        let reply = self.send(request).await;
        if reply.header.ty == FrameType::Error {
            let body: Value = serde_json::from_slice(&reply.body).unwrap();
            return Err(body["code"].as_str().unwrap().to_string());
        }
        match serde_json::from_slice(&reply.body).unwrap() {
            ClientControlResponse::SupervisorRescan { result } => Ok(result),
            other => panic!("unexpected reply {other:?}"),
        }
    }

    async fn apply_file(&self) -> SupervisorRescanResult {
        self.rescan(ClientControlRequest::SupervisorRescan { preview: false })
            .await
            .expect("the plain rescan applies")
    }

    async fn preview(&self, candidate: &str) -> Result<SupervisorRescanResult, String> {
        self.rescan(ClientControlRequest::SupervisorRescanChecked {
            preview: true,
            candidate: Some(candidate.to_string()),
            expect: None,
        })
        .await
    }

    async fn apply(&self, expect: RescanExpect) -> Result<SupervisorRescanResult, String> {
        self.rescan(ClientControlRequest::SupervisorRescanChecked {
            preview: false,
            candidate: None,
            expect: Some(expect),
        })
        .await
    }

    fn write(&self, doc: &str) {
        std::fs::write(&self.config, doc).unwrap();
    }

    fn supervised(&self) -> Vec<String> {
        let mut ids = self
            .handler
            .supervisor
            .list()
            .into_iter()
            .map(|module| module.module_id().to_string())
            .collect::<Vec<_>>();
        ids.sort();
        ids
    }

    fn enabled(&self, module_id: &str) -> bool {
        self.handler
            .supervisor
            .get(module_id)
            .expect("module is supervised")
            .status()
            .unwrap()
            .enabled
    }

    fn describe(&self) -> Value {
        let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
        let reply = self.handler.handle_server_describe(frame).unwrap();
        serde_json::from_slice(&reply[0].body).unwrap()
    }

    async fn config_change_count(&self) -> usize {
        let ledger = self.handler.activity.as_ref().unwrap();
        let barrier: ActivityRecordRequest = serde_json::from_value(
            json!({"op":"activity.record", "kind":"barrier.flushed", "summary":"Queue flushed"}),
        )
        .unwrap();
        ledger
            .record(
                "barrier".into(),
                barrier,
                ActivityActor {
                    principal: Principal::Direct,
                    scope_owner: None,
                    agent_id: None,
                    run_id: None,
                    flow_id: None,
                    project_root: None,
                    workdir: None,
                    project_id: None,
                },
                1,
            )
            .await
            .unwrap();
        let query: ActivityQueryRequest =
            serde_json::from_value(json!({"op":"activity.query", "limit":1000})).unwrap();
        ledger
            .query(query)
            .await
            .unwrap()
            .entries
            .iter()
            .filter(|entry| entry.kind == "subc.config_changed")
            .count()
    }
}

fn doc(modules: Value) -> String {
    json!({"version": 1, "modules": modules}).to_string()
}

/// A module entry that starts no process: a disabled module, or an enabled one
/// whose program does not exist, so its spawn fails and it stays supervised
/// and enabled.
fn entry(enabled: bool) -> Value {
    json!({"program": "subc-rescan-checked-test-missing-program", "protocol": "none", "enabled": enabled})
}

#[tokio::test]
async fn a_candidate_preview_changes_nothing_on_disk_or_in_the_running_set() {
    let dir = ScratchDir::new("rescan-checked-noop");
    let rig = Rig::new(&dir.join("subc.jsonc"), "noop");
    let on_disk = doc(json!({"old": entry(false)}));
    rig.write(&on_disk);
    rig.apply_file().await;
    let generation_before = rig
        .handler
        .rescan
        .as_ref()
        .unwrap()
        .generation
        .load(std::sync::atomic::Ordering::SeqCst);

    let preview = rig
        .preview(&doc(json!({"fresh": entry(true)})))
        .await
        .expect("the candidate previews");

    assert!(preview.preview);
    assert_eq!(preview.added, ["fresh"]);
    assert_eq!(preview.removed, ["old"]);
    assert_eq!(std::fs::read_to_string(&rig.config).unwrap(), on_disk);
    assert_eq!(rig.supervised(), ["old"]);
    assert!(!rig.enabled("old"));
    assert_eq!(
        rig.handler
            .rescan
            .as_ref()
            .unwrap()
            .generation
            .load(std::sync::atomic::Ordering::SeqCst),
        generation_before,
        "a preview does not move the staged generation"
    );
}

/// The candidate, not the file the daemon reads, is what a preview describes.
/// A preview that read the file would report the file's module and miss the
/// candidate's.
#[tokio::test]
async fn a_preview_of_the_disk_file_instead_of_the_candidate_fails() {
    let dir = ScratchDir::new("rescan-checked-not-disk");
    let rig = Rig::new(&dir.join("subc.jsonc"), "not-disk");
    rig.write(&doc(json!({"old": entry(false)})));
    rig.apply_file().await;
    // The file moves on without a rescan; the candidate says something else.
    rig.write(&doc(
        json!({"old": entry(false), "on-disk-only": entry(false)}),
    ));

    let preview = rig
        .preview(&doc(json!({"old": entry(false), "fresh": entry(false)})))
        .await
        .expect("the candidate previews");

    assert_eq!(preview.added, ["fresh"]);
    assert!(preview.removed.is_empty());
}

#[tokio::test]
async fn a_candidate_that_adds_env_to_a_module_reports_its_restart() {
    let dir = ScratchDir::new("rescan-checked-env");
    let rig = Rig::new(&dir.join("subc.jsonc"), "env");
    rig.write(&doc(json!({"worker": entry(true), "idle": entry(false)})));
    rig.apply_file().await;

    let mut worker = entry(true);
    worker["env"] = json!({"WORKER_MODE": "fast"});
    worker["drain_timeout_ms"] = json!(1500);
    let preview = rig
        .preview(&doc(json!({"worker": worker, "idle": entry(false)})))
        .await
        .expect("the candidate previews");

    assert_eq!(
        preview.module_actions.len(),
        1,
        "{:?}",
        preview.module_actions
    );
    let action = &preview.module_actions[0];
    assert_eq!(action.module_id, "worker");
    assert_eq!(action.action, RescanModuleActionKind::Restart);
    assert_eq!(action.changed, ["environment"]);
    assert_eq!(action.drain_timeout_ms, Some(1500));
    assert!(preview.earlier_pending.is_empty());
}

#[tokio::test]
async fn enable_edits_report_start_and_stop_and_a_health_edit_reports_reload() {
    let dir = ScratchDir::new("rescan-checked-enable");
    let rig = Rig::new(&dir.join("subc.jsonc"), "enable");
    rig.write(&doc(
        json!({"on": entry(true), "off": entry(false), "probed": entry(true)}),
    ));
    rig.apply_file().await;

    let mut probed = entry(true);
    probed["health"] = json!({"cadence_ms": 60_000});
    let preview = rig
        .preview(&doc(
            json!({"on": entry(false), "off": entry(true), "probed": probed}),
        ))
        .await
        .expect("the candidate previews");

    let actions = preview
        .module_actions
        .iter()
        .map(|action| (action.module_id.as_str(), action.action.clone()))
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        [
            ("off", RescanModuleActionKind::Start),
            ("on", RescanModuleActionKind::Stop),
            ("probed", RescanModuleActionKind::Reload),
        ]
    );
    assert_eq!(preview.module_actions[1].drain_timeout_ms, Some(30_000));
    assert_eq!(preview.module_actions[2].changed, ["health"]);
}

#[tokio::test]
async fn a_candidate_with_a_changed_top_level_section_reports_restart_required() {
    let dir = ScratchDir::new("rescan-checked-section");
    let rig = Rig::new(&dir.join("subc.jsonc"), "section");
    let mut candidate: Value = serde_json::from_str(&doc(json!({}))).unwrap();
    candidate["port"] = json!(48_123);

    let preview = rig
        .preview(&candidate.to_string())
        .await
        .expect("the candidate previews");

    assert_eq!(preview.restart_required, ["port"]);
}

#[tokio::test]
async fn disabling_the_only_provider_of_a_required_capability_reports_a_warning() {
    const CAPABILITY: &str = "credentials-provider/v1";
    let dir = ScratchDir::new("rescan-checked-capability");
    let rig = Rig::new(&dir.join("subc.jsonc"), "capability");
    let declarations = |provides: Vec<String>, requires: Vec<CapabilityRequirement>| {
        Some(CapabilityDeclarations {
            provides,
            requires,
            must_never_reach: Vec::new(),
        })
    };
    let mut consumer = ModuleManifest::builder("consumer", "0.1.0")
        .protocol_ver(PROTOCOL_VERSION)
        .build();
    consumer.capabilities = declarations(
        Vec::new(),
        vec![CapabilityRequirement {
            capability: CAPABILITY.to_string(),
            need: CapabilityNeed::Required,
        }],
    );
    let mut provider = ModuleManifest::builder("provider", "0.1.0")
        .protocol_ver(PROTOCOL_VERSION)
        .build();
    provider.capabilities = declarations(vec![CAPABILITY.to_string()], Vec::new());
    for (connection, manifest) in [(71, consumer), (72, provider)] {
        let hello = Frame::build(
            FrameType::Hello,
            control_flags(),
            0,
            0,
            connection,
            serde_json::to_vec(&ModuleHelloBody {
                protocol_ver: PROTOCOL_VERSION,
                manifest,
                control_ops: None,
                launch_nonce: None,
            })
            .unwrap(),
        )
        .unwrap();
        let reply = rig
            .handler
            .handle_control(ConnectionId::new(connection), hello)
            .expect("module registers");
        assert_eq!(reply[0].header.ty, FrameType::HelloAck);
    }

    let preview = rig
        .preview(&doc(
            json!({"consumer": entry(true), "provider": entry(false)}),
        ))
        .await
        .expect("the candidate previews");

    assert_eq!(
        preview.capability_warnings,
        [format!(
            "disabling provider leaves consumer requires:{CAPABILITY} unprovided"
        )]
    );
}

#[tokio::test]
async fn a_candidate_the_loader_refuses_is_refused_by_name_and_changes_nothing() {
    let dir = ScratchDir::new("rescan-checked-refused");
    let rig = Rig::new(&dir.join("subc.jsonc"), "refused");
    let on_disk = doc(json!({"old": entry(false)}));
    rig.write(&on_disk);
    rig.apply_file().await;

    let refused = rig
        .preview(&json!({"version": 2, "modules": {}}).to_string())
        .await;

    assert_eq!(refused.unwrap_err(), "invalid_daemon_config");
    assert_eq!(std::fs::read_to_string(&rig.config).unwrap(), on_disk);
    assert_eq!(rig.supervised(), ["old"]);
}

#[tokio::test]
async fn an_apply_of_the_previewed_file_applies_it_as_a_rescan_does() {
    let dir = ScratchDir::new("rescan-checked-apply");
    let rig = Rig::new(&dir.join("subc.jsonc"), "apply");
    rig.write(&doc(json!({"old": entry(false)})));
    rig.apply_file().await;

    let candidate = doc(json!({"new": entry(false)}));
    let preview = rig.preview(&candidate).await.unwrap();
    rig.write(&candidate);
    let applied = rig.apply(preview.expect.unwrap()).await.unwrap();

    assert!(!applied.preview);
    assert_eq!(applied.added, ["new"]);
    assert_eq!(applied.removed, ["old"]);
    assert_eq!(rig.supervised(), ["new"]);
}

/// The previewed text is written, then another writer replaces the file before
/// the apply arrives: the apply must not apply the replacement.
#[tokio::test]
async fn another_writer_between_the_write_and_the_apply_is_refused_and_the_module_is_not_stopped() {
    let dir = ScratchDir::new("rescan-checked-writer");
    let rig = Rig::new(&dir.join("subc.jsonc"), "writer");
    rig.write(&doc(json!({"x": entry(true)})));
    rig.apply_file().await;
    assert!(rig.enabled("x"));
    let config_changes_before_preview = rig.config_change_count().await;

    let mut edited = entry(true);
    edited["args"] = json!(["--edited"]);
    let candidate = doc(json!({"x": edited}));
    let preview = rig.preview(&candidate).await.unwrap();
    assert_eq!(
        rig.config_change_count().await,
        config_changes_before_preview
    );
    rig.write(&candidate);
    let other_writer = doc(json!({"x": entry(false)}));
    rig.write(&other_writer);

    let refused = rig.apply(preview.expect.unwrap()).await;

    assert_eq!(refused.unwrap_err(), CONFIG_CHANGED_SINCE_PREVIEW);
    assert!(rig.enabled("x"), "x was not stopped");
    assert_eq!(
        rig.config_change_count().await,
        config_changes_before_preview,
        "a refused checked rescan has no config activity entry"
    );
    // The same file previewed afresh does apply, so the refusal above is what
    // kept x running and not an apply that could never stop it.
    let fresh = rig.preview(&other_writer).await.unwrap();
    rig.apply(fresh.expect.unwrap()).await.unwrap();
    assert!(!rig.enabled("x"));
    assert_eq!(
        rig.config_change_count().await,
        config_changes_before_preview + 1
    );
}

#[tokio::test]
async fn another_rescan_between_the_preview_and_the_apply_is_refused() {
    let dir = ScratchDir::new("rescan-checked-other-rescan");
    let rig = Rig::new(&dir.join("subc.jsonc"), "other-rescan");
    rig.write(&doc(json!({"x": entry(false)})));
    rig.apply_file().await;

    let candidate = doc(json!({"x": entry(false), "mine": entry(false)}));
    let preview = rig.preview(&candidate).await.unwrap();
    // Another client edits the file and rescans before this apply arrives.
    rig.write(&doc(json!({"x": entry(false), "theirs": entry(false)})));
    rig.apply_file().await;
    rig.write(&candidate);

    let refused = rig.apply(preview.expect.unwrap()).await;

    assert_eq!(refused.unwrap_err(), CONFIG_CHANGED_SINCE_PREVIEW);
    assert_eq!(rig.supervised(), ["theirs", "x"]);
    let fresh = rig.preview(&candidate).await.unwrap();
    assert_eq!(fresh.added, ["mine"]);
    assert_eq!(fresh.removed, ["theirs"]);
    rig.apply(fresh.expect.unwrap()).await.unwrap();
    assert_eq!(rig.supervised(), ["mine", "x"]);
}

/// A daemon restarted on the same config file counts its staged generation
/// from zero again, so the counter alone can repeat while the staged specs
/// differ. The incarnation tells the two daemons apart.
#[tokio::test]
async fn a_restarted_daemon_whose_counter_repeats_refuses_the_old_generation() {
    let dir = ScratchDir::new("rescan-checked-restart");
    let config = dir.join("subc.jsonc");
    let first = Rig::new(&config, "first-daemon");
    first.write(&doc(json!({"x": entry(false)})));
    first.apply_file().await;
    let candidate = doc(json!({"x": entry(false), "mine": entry(false)}));
    let stale = first.preview(&candidate).await.unwrap().expect.unwrap();

    let second = Rig::new(&config, "second-daemon");
    second.write(&doc(json!({"x": entry(false), "other": entry(false)})));
    second.apply_file().await;
    let current = second.preview(&candidate).await.unwrap().expect.unwrap();
    assert_eq!(
        current.staged_generation.counter, stale.staged_generation.counter,
        "the counter repeats across the restart"
    );
    second.write(&candidate);

    assert_eq!(
        second.apply(stale).await.unwrap_err(),
        CONFIG_CHANGED_SINCE_PREVIEW
    );
    assert_eq!(second.supervised(), ["other", "x"]);
    second.apply(current).await.unwrap();
    assert_eq!(second.supervised(), ["mine", "x"]);
}

#[tokio::test]
async fn every_other_combination_is_refused_as_an_invalid_body() {
    let dir = ScratchDir::new("rescan-checked-combinations");
    let rig = Rig::new(&dir.join("subc.jsonc"), "combinations");
    rig.write(&doc(json!({})));
    let expect = rig.preview(&doc(json!({}))).await.unwrap().expect.unwrap();
    for (preview, candidate, expect) in [
        (true, None, None),
        (true, Some(doc(json!({}))), Some(expect.clone())),
        (true, None, Some(expect.clone())),
        (false, None, None),
        (false, Some(doc(json!({}))), None),
        (false, Some(doc(json!({}))), Some(expect.clone())),
    ] {
        let refused = rig
            .rescan(ClientControlRequest::SupervisorRescanChecked {
                preview,
                candidate: candidate.clone(),
                expect: expect.clone(),
            })
            .await;
        assert_eq!(
            refused.unwrap_err(),
            "invalid_control_body",
            "preview {preview}, candidate {}, expect {}",
            candidate.is_some(),
            expect.is_some()
        );
    }
    assert!(matches!(
        parse_client_control_request(br#"{"op":"supervisor.rescan_checked"}"#),
        Err((_, ControlRequestBodyError::InvalidBody))
    ));
}

#[tokio::test]
async fn a_candidate_over_the_cap_is_refused() {
    let dir = ScratchDir::new("rescan-checked-cap");
    let rig = Rig::new(&dir.join("subc.jsonc"), "cap");
    let mut candidate = doc(json!({}));
    candidate.push_str(&" ".repeat(RESCAN_CANDIDATE_MAX_BYTES + 1 - candidate.len()));
    assert_eq!(
        rig.preview(&candidate).await.unwrap_err(),
        subc_control::RESCAN_CANDIDATE_TOO_LARGE
    );
    candidate.pop();
    rig.preview(&candidate)
        .await
        .expect("a candidate of exactly the cap previews");
}

#[tokio::test]
async fn describe_advertises_the_op_only_when_it_is_served_and_reports_an_absolute_path() {
    let dir = ScratchDir::new("rescan-checked-describe");
    let config = dir.join("subc.jsonc");
    let served = Rig::new(&config, "describe").describe();
    assert!(served["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability == RESCAN_CANDIDATE_PREVIEW_CAPABILITY));
    assert_eq!(served["config_path"], json!(config.to_str().unwrap()));

    let unserved = ControlHandler::new(Arc::new(Registry::default()));
    let frame = Frame::build(FrameType::Request, control_flags(), 0, 0, 1, Vec::new()).unwrap();
    let body: Value =
        serde_json::from_slice(&unserved.handle_server_describe(frame).unwrap()[0].body).unwrap();
    assert!(!body["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .any(|capability| capability == RESCAN_CANDIDATE_PREVIEW_CAPABILITY));
    assert!(body.get("config_path").is_none());

    let relative = Rig::new(
        Path::new("relative-config/cortexkit/subc.jsonc"),
        "relative",
    );
    assert!(relative.describe().get("config_path").is_none());
}

/// Marks the copy of this test binary that
/// `a_daemon_with_a_relative_config_home_omits_config_path` starts.
const RELATIVE_CONFIG_HOME_CHILD: &str = "SUBC_RESCAN_CHECKED_RELATIVE_CONFIG_HOME_CHILD";

/// Builds the describe reply from `default_config_path()`, the path the daemon
/// binary rescans, in a copy of this test binary started with a relative
/// `XDG_CONFIG_HOME`. The environment is set on that child because this crate
/// forbids the unsafe `set_var` that changing it in-process would need.
#[tokio::test]
async fn a_daemon_with_a_relative_config_home_omits_config_path() {
    const TEST: &str =
        "control::rescan_checked::tests::a_daemon_with_a_relative_config_home_omits_config_path";
    if std::env::var_os(RELATIVE_CONFIG_HOME_CHILD).is_some() {
        let config = crate::daemon_config::default_config_path();
        assert!(config.is_relative(), "{}", config.display());
        let describe = Rig::new(&config, "relative-home").describe();
        assert!(describe.get("config_path").is_none(), "{describe}");
        println!("child described {} without config_path", config.display());
        return;
    }
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(RELATIVE_CONFIG_HOME_CHILD, "1")
        .env("XDG_CONFIG_HOME", "relative-config-home")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "child failed:\n{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("child described relative-config-home"),
        "the child ran the check: {stdout}"
    );
}

/// A module whose running process was started before a launch edit is
/// reported as waiting for a restart, separately from the candidate's own
/// actions.
#[cfg(unix)]
#[tokio::test]
async fn a_module_still_waiting_for_an_earlier_restart_is_listed_separately() {
    let dir = ScratchDir::new("rescan-checked-earlier");
    let rig = Rig::new(&dir.join("subc.jsonc"), "earlier");
    let sleeper = |seconds: &str| {
        json!({
            "program": "/bin/sleep",
            "args": [seconds],
            "protocol": "none",
            "drain_timeout_ms": 0,
            "env": {
                "XDG_DATA_HOME": dir.path(),
                "XDG_RUNTIME_DIR": dir.path(),
                "XDG_CONFIG_HOME": dir.path(),
            },
        })
    };
    rig.write(&doc(json!({"stock": sleeper("600")})));
    rig.apply_file().await;
    let module = rig.handler.supervisor.get("stock").unwrap();
    let started = tokio::time::Instant::now() + Duration::from_secs(10);
    while module.status().unwrap().launch_spec_digest.is_none() {
        assert!(tokio::time::Instant::now() < started, "stock never spawned");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // A launch edit is staged without restarting the running process.
    let staged = doc(json!({"stock": sleeper("601")}));
    rig.write(&staged);
    rig.apply_file().await;

    let preview = rig.preview(&staged).await.unwrap();

    assert!(
        preview.module_actions.is_empty(),
        "{:?}",
        preview.module_actions
    );
    assert_eq!(preview.earlier_pending, ["stock"]);
    module.drain().await.unwrap();
}

/// The well-formed apply changes the supervised modules, so it asks the
/// operator exactly as a `supervisor.rescan` apply does. A preview changes
/// nothing, and a malformed combination is refused before it touches anything,
/// so neither prompts.
#[test]
fn only_the_well_formed_checked_rescan_apply_needs_operator_approval() {
    use crate::privileged_prompt::PrivilegedOperation;
    let expect = RescanExpect {
        config_sha256: "00".repeat(32),
        staged_generation: subc_control::StagedGeneration {
            incarnation: "incarnation".to_string(),
            counter: 0,
        },
    };
    let checked = |preview: bool, candidate: Option<&str>, expect: Option<&RescanExpect>| {
        ClientControlRequest::SupervisorRescanChecked {
            preview,
            candidate: candidate.map(str::to_string),
            expect: expect.cloned(),
        }
    };
    assert_eq!(
        super::super::privileged_operation(&checked(false, None, Some(&expect))),
        Some(PrivilegedOperation::Rescan),
        "the apply"
    );
    assert_eq!(
        super::super::privileged_operation(&ClientControlRequest::SupervisorRescan {
            preview: false
        }),
        Some(PrivilegedOperation::Rescan),
        "the plain apply it matches"
    );
    for (preview, candidate, expect) in [
        (true, Some("{}"), None),
        (true, None, None),
        (true, Some("{}"), Some(&expect)),
        (true, None, Some(&expect)),
        (false, None, None),
        (false, Some("{}"), None),
        (false, Some("{}"), Some(&expect)),
    ] {
        assert_eq!(
            super::super::privileged_operation(&checked(preview, candidate, expect)),
            None,
            "preview {preview}, candidate {}, expect {}",
            candidate.is_some(),
            expect.is_some()
        );
    }
}
