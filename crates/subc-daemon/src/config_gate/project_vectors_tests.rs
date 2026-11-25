use super::super::stamped_delivery_tests::{report, Peer};
use super::*;
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use subc_protocol::config::ConfigGetReply;

type Sample = (&'static str, Value);

pub(super) fn assert_samples(samples: impl IntoIterator<Item = Sample>) {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../vectors/config-settings-v1");
    let manifest = directory.join("SHA256SUMS");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let saved = std::fs::read_to_string(&manifest).unwrap();
    let mut checksums: BTreeMap<_, _> = saved
        .lines()
        .map(|line| {
            let (sum, name) = line.split_once("  ").unwrap();
            (name, sum.to_owned())
        })
        .collect();
    for (name, sample) in samples {
        let canonical = subc_config::canonical_json(&sample).unwrap();
        let value: Value = serde_json::from_slice(&canonical).unwrap();
        let mut bytes = serde_json::to_vec_pretty(&value).unwrap();
        bytes.push(b'\n');
        let sum = format!("{:x}", Sha256::digest(&bytes));
        let path = directory.join(name);
        if update {
            std::fs::write(&path, &bytes).unwrap();
            checksums.insert(name, sum.clone());
        }
        assert_eq!(
            std::fs::read(&path).unwrap_or_else(|err| panic!("{name}: {err}")),
            bytes,
            "{name}; regenerate with UPDATE_GOLDEN=1"
        );
        assert_eq!(checksums.get(name), Some(&sum), "{name} checksum");
    }
    if update {
        let bytes: String = checksums
            .into_iter()
            .map(|(name, sum)| format!("{sum}  {name}\n"))
            .collect();
        std::fs::write(manifest, bytes).unwrap();
    }
}

struct Client {
    handler: ControlHandler,
    ctx: RouteCtx,
    _rx: tokio::sync::mpsc::Receiver<crate::router::OutboundFrame>,
}

impl Client {
    fn new(rig: &CaptureRig) -> Self {
        let handler = ControlHandler::new(Arc::new(crate::Registry::default()))
            .with_config_runtime(rig.owner.clone());
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let ctx = RouteCtx {
            connection_id: ConnectionId::new(80),
            egress: FrameSink::new(tx),
        };
        owner_connection(&handler, ctx.connection_id);
        Self {
            handler,
            ctx,
            _rx: rx,
        }
    }

    async fn call(&self, body: &Value, kind: FrameType) -> Value {
        let frames = request(&self.handler, &self.ctx, 91, body.clone()).await;
        assert_eq!(frames.len(), 1);
        let reply: Value = serde_json::from_slice(&frames[0].body).unwrap();
        assert_eq!(frames[0].header.ty, kind, "{body} -> {reply}");
        reply
    }

    async fn refusal(&self, rig: &CaptureRig, body: Value, code: &str) -> Value {
        let reply = self.call(&body, FrameType::Error).await;
        assert_eq!(reply["code"], code);
        normalize(json!({"request":body,"reply":reply}), rig, false)
    }
}

fn normalize(value: Value, rig: &CaptureRig, fingerprints: bool) -> Value {
    // Captures include both the configured paths and resolved program paths.
    let user_root = rig.rig.path.canonicalize().unwrap();
    let project_root = rig.project_scratch.canonicalize().unwrap();
    let path = |text: &str| {
        text.replace(rig.rig.path.to_str().unwrap(), "/home/example")
            .replace(user_root.to_str().unwrap(), "/home/example")
            .replace(rig.project_scratch.to_str().unwrap(), "/home/example/repos")
            .replace(project_root.to_str().unwrap(), "/home/example/repos")
    };
    normalize_value(value, &path, fingerprints)
}

fn normalize_value(value: Value, path: &impl Fn(&str) -> String, fingerprints: bool) -> Value {
    match value {
        Value::Object(fields) => {
            let file_id = fields.get("platform").and_then(Value::as_str) == Some("unix");
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| {
                        let fingerprint = fingerprints
                            || matches!(
                                key.as_str(),
                                "input_vector"
                                    | "reviewed_programs"
                                    | "registry_revision"
                                    | "edit_registry"
                                    | "project_approval_digests"
                            )
                            || (key == "state" && value.is_object());
                        let value = if file_id && matches!(key.as_str(), "device" | "inode") {
                            json!("1")
                        } else {
                            normalize_value(value, path, fingerprint)
                        };
                        (path(&key), value)
                    })
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| normalize_value(item, path, fingerprints))
                .collect(),
        ),
        Value::String(text) if fingerprints && text.starts_with("sha256:") => {
            json!(format!("sha256:{}", "a".repeat(64)))
        }
        Value::String(text) => Value::String(path(&text)),
        other => other,
    }
}

fn plan_sample(rig: &CaptureRig, request: Value, reply: Value) -> Value {
    let mut reply = normalize(reply, rig, false);
    reply["view"]["plan"]["daemon_incarnation"] = json!("example-daemon");
    let plan: subc_config::Plan = serde_json::from_value(reply["view"]["plan"].clone()).unwrap();
    reply["view"]["digest"] = json!(plan.digest().unwrap());
    json!({"request":request,"reply":reply})
}

fn edit(layer: &str, project: Option<&str>, workspace: Option<&str>) -> Value {
    let mut edit = json!({"module_id":"a","layer":layer,"path":"/limit","set":8});
    if let Some(id) = project {
        edit["project_id"] = json!(id);
    }
    if let Some(id) = workspace {
        edit["workspace_id"] = json!(id);
    }
    edit
}

fn edits(base: u64, edits: Vec<Value>) -> Value {
    json!({"op":"config.plan","base_generations":{"a":base},"edits":edits})
}

async fn edit_samples() -> Vec<Sample> {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let project = root(&rig.project_scratch, "project");
    let unavailable = root(&rig.project_scratch, "unavailable");
    rig.set_projects(vec![
        member("A", Some("W"), &[&project]),
        member("unavailable", None, &[&unavailable]),
    ]);
    std::fs::remove_dir(&unavailable).unwrap();
    let workspace = write_layer(&rig.workspace, 4);
    let client = Client::new(&rig);
    let workspace_edit = edit("workspace", None, Some("W"));
    let request = edits(0, vec![workspace_edit.clone()]);
    let reply = client.call(&request, FrameType::Response).await;
    assert!(reply["view"].get("refusals").is_none());
    let success = plan_sample(&rig, request, reply.clone());
    let apply = json!({"op":"config.apply","digest":reply["view"]["digest"]});
    let applied = client.call(&apply, FrameType::Response).await;
    assert_eq!(std::fs::read(&workspace).unwrap(), br#"{"limit":8}"#);

    let mut samples = vec![
        ("project-plan-edit.json", success.clone()),
        (
            "project-apply-unstamped.json",
            json!({"request":{"op":"config.apply","digest":success["reply"]["view"]["digest"]},"reply":applied}),
        ),
    ];
    for (name, request, code) in [
        (
            "project-plan-unknown-workspace.json",
            edits(0, vec![edit("workspace", None, Some("missing"))]),
            "unknown_workspace",
        ),
        (
            "project-plan-workspace-mismatch.json",
            edits(0, vec![edit("workspace", Some("A"), Some("other"))]),
            "workspace_mismatch",
        ),
        (
            "project-plan-edit-spans-files.json",
            edits(0, vec![workspace_edit, edit("project", Some("A"), None)]),
            "edit_spans_files",
        ),
        (
            "project-plan-config-unavailable.json",
            edits(0, vec![edit("project", Some("unavailable"), None)]),
            "config_unavailable",
        ),
    ] {
        samples.push((name, client.refusal(&rig, request, code).await));
    }
    *rig.registry.reply.lock().unwrap() = None;
    samples.push((
        "project-plan-registry-unavailable.json",
        client
            .refusal(
                &rig,
                edits(0, vec![edit("project", Some("A"), None)]),
                "project_registry_unavailable",
            )
            .await,
    ));
    samples
}

async fn root_refusals() -> Sample {
    let rig = CaptureRig::new();
    let new_root = |name| root(&rig.project_scratch, name);
    let link = new_root("symlink");
    let directory = new_root("directory");
    let hardlink = new_root("hardlink");
    let large = new_root("large");
    let ambiguous = new_root("ambiguous");
    let other = new_root("other");
    let link_path = write_layer(&link, 5);
    let directory_path = write_layer(&directory, 5);
    let hardlink_path = write_layer(&hardlink, 5);
    let large_path = write_layer(&large, 5);
    std::fs::remove_file(&link_path).unwrap();
    std::os::unix::fs::symlink(&hardlink_path, &link_path).unwrap();
    std::fs::remove_file(&directory_path).unwrap();
    std::fs::create_dir(&directory_path).unwrap();
    std::fs::hard_link(&hardlink_path, hardlink.join("another-link")).unwrap();
    std::fs::File::options()
        .write(true)
        .open(large_path)
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    let cases: [(&str, &[&Path], &str); 6] = [
        ("symlink", &[&link], "layer_symlink"),
        ("directory", &[&directory], "layer_not_regular"),
        ("hardlink", &[&hardlink], "layer_hardlinked"),
        ("large", &[&large], "layer_too_large"),
        ("ambiguous", &[&ambiguous, &other], "layer_root_ambiguous"),
        ("rootless", &[], "layer_root_invalid"),
    ];
    rig.set_projects(
        cases
            .iter()
            .map(|(id, roots, _)| member(id, Some("W"), roots))
            .collect(),
    );
    let request = edits(
        rig.stamp,
        cases
            .iter()
            .map(|(id, _, _)| edit("project", Some(id), Some("W")))
            .collect(),
    );
    let reply = Client::new(&rig).call(&request, FrameType::Response).await;
    let refusals = reply["view"]["refusals"].as_array().unwrap();
    assert_eq!(refusals.len(), cases.len());
    for (id, _, code) in cases {
        assert!(refusals.iter().any(|row| row["project_id"] == id
            && row["workspace_id"] == "W"
            && row["code"] == code));
    }
    (
        "project-plan-root-refusals.json",
        plan_sample(&rig, request, reply),
    )
}

async fn field_refusals() -> Vec<Sample> {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    let project = root(&rig.project_scratch, "project");
    rig.set_projects(vec![member("A", Some("W"), &[&project])]);
    let mut declaration = rig.rig.declaration.clone();
    declaration.schema["properties"]["limit"]["x-ck-apply"] = json!("route_rebind");
    declaration.schema["properties"]["account"] = json!({
        "type":"object", "unevaluatedProperties":false, "properties": {
            "credential": {"type":"object", "description":"Vault reference", "x-ck-kind":"credential_ref", "x-ck-layers":["user"]},
            "enabled": {"type":"boolean", "description":"Enabled", "default":false}
        }
    });
    rig.owner
        .declare(ConnectionId::new(11), "a", declaration, true)
        .unwrap();
    let client = Client::new(&rig);
    let mut credential_edit = edit("workspace", Some("A"), Some("W"));
    credential_edit["path"] = json!("/account/enabled");
    credential_edit["set"] = json!(true);
    let mut samples = Vec::new();
    for (name, edit, code) in [
        (
            "project-plan-apply-kind.json",
            edit("project", Some("A"), None),
            "project_apply_kind_unsupported",
        ),
        (
            "project-plan-credentials.json",
            credential_edit,
            "credential_layer_unsupported",
        ),
    ] {
        let request = edits(0, vec![edit]);
        let reply = client.call(&request, FrameType::Response).await;
        assert_eq!(reply["view"]["unstamped"], true);
        assert_eq!(reply["view"]["refusals"][0]["code"], code);
        assert_eq!(reply["view"]["refusals"][0]["project_id"], "A");
        samples.push((name, plan_sample(&rig, request, reply)));
    }
    samples
}

async fn capture_and_delivery_samples() -> Vec<Sample> {
    let rig = CaptureRig::new();
    std::fs::write(&rig.rig.declaration.user_file, br#"{"limit":2}"#).unwrap();
    rig.set_projects(vec![]);
    let user = finish(&rig.owner, rig.prepare().await.unwrap());
    let user_generation = rig.apply(&user).await.unwrap();
    rig.owner
        .lock()
        .unwrap()
        .store
        .record_delivery(&tuple(None), user_generation, true)
        .unwrap();

    let project = root(&rig.project_scratch, "project");
    let missing = root(&rig.project_scratch, "missing");
    rig.set_projects(vec![
        member("A", Some("W"), &[&project]),
        member("B", None, &[&missing]),
        member("C", None, &[]),
    ]);
    std::fs::remove_dir(&missing).unwrap();
    write_layer(&rig.workspace, 4);
    write_layer(&project, 9);
    let mut client = Client::new(&rig);
    let prepared = rig.prepare().await.unwrap();
    let (tuple, review) = prepared.project_reviews().next().unwrap();
    assert_eq!(tuple.project.as_deref(), Some("A"));
    assert_eq!(prepared.project_reviews().count(), 1);
    client.handler.config_review_test_reply = Some(reply(review));
    let request = json!({"op":"config.plan"});
    let payload = client.call(&request, FrameType::Response).await;
    assert_eq!(payload["view"]["unstamped"], false);
    assert!(payload["view"].get("refusals").is_none());
    assert_eq!(payload["view"]["skipped"][0]["project_id"], "B");
    assert_eq!(payload["view"]["skipped"][0]["code"], "config_unavailable");
    let plan = serde_json::from_value(payload["view"].clone()).unwrap();
    let mut samples = vec![(
        "project-plan-capture.json",
        plan_sample(&rig, request, payload),
    )];
    let generation = rig.apply(&plan).await.unwrap();
    for (name, cutoff, expected) in [
        ("project-show.json", None, 9),
        ("project-show-default.json", Some("default"), 1),
        ("project-show-user.json", Some("user"), 2),
        ("project-show-workspace.json", Some("workspace"), 4),
    ] {
        let mut request = json!({"op":"config.show","module_id":"a","project_id":"A"});
        if let Some(cutoff) = cutoff {
            request["through"] = json!(cutoff);
        }
        let reply = client.call(&request, FrameType::Response).await;
        assert_eq!(reply["project_id"], "A");
        assert_eq!(reply["effective"]["limit"], expected);
        samples.push((name, json!({"request":request,"reply":reply})));
    }
    samples.push((
        "project-show-unknown-project.json",
        client
            .refusal(
                &rig,
                json!({"op":"config.show","module_id":"a","project_id":"unknown"}),
                "unknown_project",
            )
            .await,
    ));

    let mut peer = Peer::new(rig.owner.clone(), ConnectionId::new(11));
    peer.rpc(&rig.rig.declaration, false).await;
    let worker = peer.handler.spawn_config_delivery_worker().unwrap();
    let hint: Value = serde_json::from_slice(&peer.next_hint_frame().await.body).unwrap();
    assert_eq!(hint["project_id"], "A");
    assert_eq!(hint["generation"], generation);
    samples.push(("project-changed.json", hint));
    let request = json!({"op":"config.get","project_id":"A"});
    let frame = peer.rpc(&request, true).await;
    let payload: Value = serde_json::from_slice(&frame.body).unwrap();
    let view = serde_json::from_slice::<ConfigGetReply>(&frame.body)
        .unwrap()
        .pending
        .unwrap();
    assert_eq!(view.project_id.as_deref(), Some("A"));
    assert_eq!(view.generation, generation);
    samples.push((
        "project-get-pending.json",
        normalize(json!({"request":request,"reply":payload}), &rig, false),
    ));
    let ack = report(&view, true);
    let applied: Value = serde_json::from_slice(&peer.rpc(&ack, false).await.body).unwrap();
    samples.push((
        "project-applied.json",
        json!({"request":ack,"reply":applied}),
    ));
    let (name, fallback) = project_get(&mut peer, "C", "project-get-fallback.json").await;
    assert_eq!(fallback["reply"]["active"]["status"], "active");
    samples.push((name, normalize(fallback, &rig, false)));
    worker.abort();
    let _ = worker.await;
    samples
}

async fn project_get(peer: &mut Peer, id: &str, name: &'static str) -> Sample {
    let request = json!({"op":"config.get","project_id":id});
    let payload: Value = serde_json::from_slice(&peer.rpc(&request, true).await.body).unwrap();
    assert_eq!(payload["active"]["project_id"], id);
    assert!(payload["active"].get("attempt").is_none());
    assert!(payload.get("pending").is_none());
    (name, json!({"request":request,"reply":payload}))
}

async fn unstamped_delivery_sample() -> Sample {
    let rig = CaptureRig::with_state(true, RootIoPool::new().unwrap(), false);
    write_layer(&rig.workspace, 4);
    rig.set_projects(vec![member("A", Some("W"), &[])]);
    let mut peer = Peer::new(rig.owner.clone(), ConnectionId::new(11));
    peer.rpc(&rig.rig.declaration, false).await;
    let sample = project_get(&mut peer, "A", "project-get-unstamped.json").await;
    assert_eq!(sample.1["reply"]["active"]["status"], "unstamped");
    assert_eq!(sample.1["reply"]["active"]["generation"], 0);
    (sample.0, normalize(sample.1, &rig, false))
}

async fn root_io_health_sample() -> Sample {
    let pool = RootIoPool::with_timeout(Duration::from_millis(300)).unwrap();
    let rig = CaptureRig::with_pool(true, pool);
    rig.set_projects(vec![]);
    let client = Client::new(&rig);
    let request = json!({"op":"config.status"});
    let idle = client.call(&request, FrameType::Response).await;
    assert!(idle["status"].get("root_io").is_none());
    let (release, wait) = std::sync::mpsc::channel();
    let (entered, signal) = tokio::sync::oneshot::channel();
    let job = rig
        .registry
        .pool
        .submit(rig.workspace.clone(), move || {
            entered.send(()).unwrap();
            let _ = wait.recv();
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), signal)
        .await
        .unwrap()
        .unwrap();
    assert!(job.wait().await.is_err());
    let busy = client.call(&request, FrameType::Response).await;
    assert_eq!(
        busy["status"]["root_io"],
        json!({"stuck_roots":[rig.workspace],"charged_workers":1})
    );
    release.send(()).unwrap();
    ("project-status-root-io.json", normalize_status(busy, &rig))
}

pub(super) fn assert_status_file_errors_sample(name: &'static str, reply: Value, rig: &CaptureRig) {
    assert_samples([(name, normalize_status(reply, rig))]);
}

pub(super) fn assert_watch_samples(samples: impl IntoIterator<Item = Sample>, rig: &CaptureRig) {
    assert_samples(
        samples
            .into_iter()
            .map(|(name, sample)| (name, normalize(sample, rig, false))),
    );
}

fn normalize_status(mut reply: Value, rig: &CaptureRig) -> Value {
    // The test runner can itself be launched with startup environment overrides.
    reply["status"]["ungated_inputs"] = json!([]);
    // A build with the `dev-desktop-approval` feature lists that switch and can
    // approve on the desktop. The sample shows a release daemon either way.
    if cfg!(feature = "dev-desktop-approval") {
        reply["status"]["development_switches"] = json!([]);
        reply["status"]["missing_approval_surfaces"]
            .as_array_mut()
            .unwrap()
            .insert(0, json!("desktop (required for stamp)"));
    }
    normalize(
        json!({"request":{"op":"config.status"},"reply":reply}),
        rig,
        false,
    )
}

#[tokio::test]
async fn project_settings_vectors_match_real_handler_bytes() {
    let mut samples = edit_samples().await;
    samples.push(root_refusals().await);
    samples.extend(field_refusals().await);
    samples.extend(capture_and_delivery_samples().await);
    samples.push(unstamped_delivery_sample().await);
    samples.push(root_io_health_sample().await);
    assert_samples(samples);
}
