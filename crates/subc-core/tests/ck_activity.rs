use std::{
    ffi::OsStr,
    fs,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::{Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

use cortexkit_test_support::{ckdev_binary, dev_command, ScratchDir};
use serde_json::{json, Value};
use subc_client_rs::{
    async_trait, ActivityRecordRequest, HandlerOutcome, ModuleHandler, RequestCtx,
};
use subc_protocol::manifest::ModuleManifest;
use tokio::io::{AsyncBufReadExt, BufReader};

mod common;

const AT: u64 = 1_704_067_200_000;

struct Fixture {
    root: ScratchDir,
    child: Option<Child>,
    _permit: MutexGuard<'static, ()>,
}

impl Fixture {
    fn new() -> Self {
        // Serialize this file's real daemons to avoid competing with the SDK's
        // daemon tests for process startup and authentication deadlines.
        static GATE: OnceLock<Mutex<()>> = OnceLock::new();
        let permit = GATE
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let root = ScratchDir::new("ck-activity");
        for dir in [
            "config/cortexkit",
            "runtime",
            "data/cortexkit/run",
            "handoff",
        ] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        fs::write(
            root.join("config/cortexkit/subc.jsonc"),
            r#"{"version":1,"modules":{}}"#,
        )
        .unwrap();
        Self {
            root,
            child: None,
            _permit: permit,
        }
    }

    fn daemon_command(&self) -> Command {
        let mut command = dev_command(ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")));
        command
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env_remove("CK_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    fn seed(&self, entries: &[Value]) {
        let lines = entries
            .iter()
            .map(|entry| format!("{}\n", serde_json::to_string(entry).unwrap()))
            .collect::<String>();
        fs::write(self.root.join("data/cortexkit/run/activity.jsonl"), lines).unwrap();
    }

    fn connection(&self) -> PathBuf {
        self.root
            .join("runtime")
            .join(subc_transport::CONNECTION_FILE_NAME)
    }

    async fn boot(&mut self) {
        self.child = Some(self.daemon_command().spawn().unwrap());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(
                self.child.as_mut().unwrap().try_wait().unwrap().is_none(),
                "daemon exited during boot"
            );
            if self.connection().exists() {
                if let Ok(consumer) =
                    subc_client_rs::SubcConsumer::connect(&self.connection(), Default::default())
                        .await
                {
                    consumer.close().await;
                    return;
                }
            }
            assert!(
                Instant::now() < deadline,
                "isolated activity daemon did not boot"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    fn ck_command(&self) -> Command {
        self.face_command(false)
    }

    fn face_command(&self, ckni: bool) -> Command {
        let binary = ckdev_binary(env!("CARGO_BIN_EXE_ck-under-test"));
        let program = if ckni {
            let link = self
                .root
                .join(if cfg!(windows) { "ckni.exe" } else { "ckni" });
            #[cfg(unix)]
            std::os::unix::fs::symlink(&binary, &link).unwrap();
            #[cfg(not(unix))]
            fs::copy(&binary, &link).unwrap();
            link
        } else {
            binary
        };
        let mut command = dev_command(program);
        common::kit::isolate_client(&mut command, &self.root.join("client"));
        command
            .arg("--subc")
            .arg(self.connection())
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env_remove("SUBC_MODULE_ID")
            .env_remove("SUBC_LAUNCH_NONCE")
            .env_remove("CK_TEST_ACTIVITY_HANDOFF");
        if ckni {
            common::kit::isolate_client_fully(&mut command, &self.root.join("client"));
        }
        command
    }

    fn ck(&self, args: &[&str]) -> Output {
        self.ck_command().args(args).output().unwrap()
    }

    fn query(&self, filters: &[&str]) -> Value {
        let mut args = vec!["activity", "--json"];
        args.extend(filters);
        let output = self.ck(&args);
        success(&output);
        serde_json::from_slice(&output.stdout).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn success(output: &Output) {
    assert!(
        output.status.success(),
        "status: {:?}, stdout: {}, stderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn entry(seq: u64) -> Value {
    json!({"seq":seq, "at_ms":AT, "daemon_incarnation":"retained", "module":"worker",
        "kind":"worker.changed", "summary":format!("Updated item {seq}"), "subject":{"type":"item","id":format!("item-{seq}")},
        "details":{"item_id":format!("item-{seq}")},
        "actor":{"principal":{"kind":"direct"}, "agent_id":"agent-a", "run_id":"run-a",
        "project_root":"project-a"}})
}

fn sequences(entries: &Value) -> Vec<u64> {
    entries
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["seq"].as_u64().unwrap())
        .collect()
}

#[test]
fn activity_daemon_child_is_isolated() {
    let fixture = Fixture::new();
    let command = fixture.daemon_command();
    let env: Vec<_> = command.get_envs().collect();
    for (key, path) in [
        ("XDG_DATA_HOME", "data"),
        ("XDG_RUNTIME_DIR", "runtime"),
        ("XDG_CONFIG_HOME", "config"),
    ] {
        let expected = fixture.root.join(path);
        assert!(
            env.contains(&(OsStr::new(key), Some(expected.as_os_str()))),
            "child is missing isolated {key}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_query_filters_and_limit_reach_the_daemon() {
    let mut fixture = Fixture::new();
    let mut entries = (1..=8).map(entry).collect::<Vec<_>>();
    entries[1]["actor"]["agent_id"] = json!("agent-b");
    entries[2]["actor"]["project_root"] = json!("project-b");
    entries[3]["module"] = json!("other");
    entries[4]["kind"] = json!("worker.started");
    entries[5]["at_ms"] = json!(AT - 1);
    entries[6]["at_ms"] = json!(AT + 1);
    fixture.seed(&entries);
    fixture.boot().await;
    for (filters, expected) in [
        (vec![], vec![8, 7, 6, 5, 4, 3, 2, 1]),
        (vec!["--agent", "agent-a"], vec![8, 7, 6, 5, 4, 3, 1]),
        (vec!["--project", "project-a"], vec![8, 7, 6, 5, 4, 2, 1]),
        (vec!["--module", "worker"], vec![8, 7, 6, 5, 3, 2, 1]),
        (vec!["--kind", "worker.changed"], vec![8, 7, 6, 4, 3, 2, 1]),
        (
            vec!["--since-ms", "1704067200000"],
            vec![8, 7, 5, 4, 3, 2, 1],
        ),
        (
            vec!["--until-ms", "1704067200000"],
            vec![8, 6, 5, 4, 3, 2, 1],
        ),
        (vec!["--limit", "2"], vec![8, 7]),
        (vec!["--agent", "absent"], vec![]),
        (
            vec![
                "--agent",
                "agent-a",
                "--project",
                "project-a",
                "--module",
                "worker",
                "--kind",
                "worker.changed",
                "--since-ms",
                "1704067200000",
                "--until-ms",
                "1704067200000",
                "--limit",
                "1",
            ],
            vec![8],
        ),
    ] {
        assert_eq!(
            sequences(&fixture.query(&filters)),
            expected,
            "filters: {filters:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_table_has_utc_time_module_actor_and_safe_summary() {
    let mut fixture = Fixture::new();
    let mut first = entry(1);
    first["summary"] = json!("Updated café\n\u{1b}[31m");
    let mut second = entry(2);
    second["actor"] = json!({"principal":{"kind":"reserved", "module_id":"reporter"}});
    fixture.seed(&[first, second]);
    fixture.boot().await;
    let output = fixture.ck(&["activity"]);
    success(&output);
    let text = String::from_utf8(output.stdout).unwrap();
    let rows = text
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>())
        .collect::<Vec<_>>();
    assert_eq!(rows[0], ["time", "(UTC)", "module", "actor", "summary"]);
    assert_eq!(
        rows[1],
        [
            "2024-01-01",
            "00:00:00.000",
            "worker",
            "reserved:reporter",
            "Updated",
            "item",
            "2"
        ]
    );
    assert_eq!(
        rows[2],
        [
            "2024-01-01",
            "00:00:00.000",
            "worker",
            "agent-a",
            "Updated",
            "café\\n\\u{1b}[31m"
        ]
    );
    assert_eq!(rows.len(), 3);
    assert!(!text.contains('\u{1b}'));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_json_preserves_complete_entries_newest_first() {
    let mut fixture = Fixture::new();
    let mut second = entry(2);
    second["summary"] = json!("Updated café\n\u{1b}[31m");
    fixture.seed(&[entry(1), second.clone()]);
    fixture.boot().await;
    let output = fixture.ck(&["--json", "activity"]);
    success(&output);
    let expected = json!([second, entry(1)]);
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap(),
        expected
    );
    assert_eq!(
        output.stdout,
        format!("{}\n", serde_json::to_string_pretty(&expected).unwrap()).as_bytes()
    );
}

struct IdleHandler;
#[async_trait]
impl ModuleHandler for IdleHandler {
    async fn handle(&self, _ctx: RequestCtx, _body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(b"{}".to_vec())
    }
}

async fn reporter(
    connection: &std::path::Path,
    id: &str,
) -> (subc_client_rs::ModuleHandle, tokio::task::JoinHandle<()>) {
    let manifest = ModuleManifest::builder(id, "1.0.0").build();
    let (handle, serving) = subc_client_rs::serve_with_handle(connection, manifest, IdleHandler)
        .await
        .unwrap();
    let task = tokio::spawn(async move {
        serving.await.unwrap();
    });
    (handle, task)
}

async fn record(handle: &subc_client_rs::ModuleHandle, kind: &str, summary: &str) -> u64 {
    handle
        .record_activity(ActivityRecordRequest {
            kind: kind.into(),
            summary: summary.into(),
            subject: None,
            details: None,
            on: None,
        })
        .await
        .unwrap()
        .seq
}

async fn follow_handoff(json_output: bool, empty_page: bool, ckni: bool) {
    let mut fixture = Fixture::new();
    fixture.seed(&[entry(1), entry(2)]);
    fixture.boot().await;
    if ckni {
        let describe = fixture.ck(&["daemon"]);
        success(&describe);
        let describe = String::from_utf8(describe.stdout).unwrap();
        let pid = describe
            .lines()
            .next()
            .unwrap()
            .split(" · ")
            .find_map(|part| part.strip_prefix("pid "))
            .unwrap()
            .parse::<u32>()
            .unwrap();
        assert_eq!(pid, fixture.child.as_ref().unwrap().id());
    }
    let (worker, worker_task) = reporter(&fixture.connection(), "worker").await;
    let (other, other_task) = reporter(&fixture.connection(), "other").await;
    let handoff = fixture.root.join("handoff");
    let mut command = tokio::process::Command::from(fixture.face_command(ckni));
    command
        .args([
            "activity",
            "--follow",
            "--limit",
            "1",
            "--module",
            "worker",
            "--kind",
            "worker.changed",
        ])
        .env("CK_TEST_ACTIVITY_HANDOFF", &handoff)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if json_output {
        command.arg("--json");
    }
    if empty_page {
        command.args(["--since-ms", "1704067200001"]);
    }
    let mut child = command.spawn().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), async {
        while !handoff.join("head.json").exists() {
            assert!(
                child.try_wait().unwrap().is_none(),
                "follow exited before its query head"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("follow must finish its initial query");
    let head: Value =
        serde_json::from_slice(&fs::read(handoff.join("head.json")).unwrap()).unwrap();
    assert_eq!(head["seq"], 2);
    assert_ne!(head["daemon_incarnation"], "retained");

    // These are real module records, durably appended after the query and before
    // subscription. Two fail the filters; the other two must be replayed once.
    let gap_first = record(&worker, "worker.changed", "handoff-first").await;
    record(&worker, "worker.other", "excluded-kind").await;
    record(&other, "other.changed", "excluded-module").await;
    let gap_last = record(&worker, "worker.changed", "handoff-last").await;
    fs::write(handoff.join("resume"), "resume").unwrap();
    let expected_lines = if empty_page { 2 } else { 3 } + usize::from(!json_output);
    let mut output = Vec::new();
    for _ in 0..expected_lines {
        output.push(
            tokio::time::timeout(Duration::from_secs(5), lines.next_line())
                .await
                .expect("handoff entry missing")
                .unwrap()
                .expect("feed ended early"),
        );
    }
    let live = record(&worker, "worker.changed", "live-after-subscribe").await;
    output.push(
        tokio::time::timeout(Duration::from_secs(5), lines.next_line())
            .await
            .expect("live entry missing")
            .unwrap()
            .expect("feed ended early"),
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(200), lines.next_line())
            .await
            .is_err(),
        "entry repeated or filter ignored"
    );
    child.kill().await.unwrap();
    let finished = child.wait_with_output().await.unwrap();
    assert!(
        finished.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&finished.stderr)
    );
    worker_task.abort();
    other_task.abort();
    if json_output {
        if !ckni && !empty_page {
            assert!(output[0].starts_with("{\"seq\":2,\"at_ms\":1704067200000,\"daemon_incarnation\":\"retained\",\"module\":\"worker\""), "piped ck keeps its serialized field order: {}", output[0]);
        }
        let entries = output
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        for entry in &entries {
            if ckni {
                assert_eq!(entry["schema"], "ck.activity/v1");
            } else {
                assert!(entry.get("schema").is_none());
            }
        }
        let expected = if empty_page {
            vec![gap_first, gap_last, live]
        } else {
            vec![2, gap_first, gap_last, live]
        };
        assert_eq!(
            entries
                .iter()
                .map(|entry| entry["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(entries[entries.len() - 3]["summary"], "handoff-first");
        assert_eq!(entries[entries.len() - 2]["summary"], "handoff-last");
        assert_eq!(
            entries[entries.len() - 1]["summary"],
            "live-after-subscribe"
        );
        for entry in &entries[entries.len() - 3..] {
            assert_eq!(entry["daemon_incarnation"], head["daemon_incarnation"]);
        }
    } else {
        assert_eq!(output.len(), 4);
        assert!(
            output[0].contains("time (UTC)")
                && output[0].contains("module")
                && output[0].contains("actor")
                && output[0].contains("summary")
        );
        for (line, summary) in
            output[1..]
                .iter()
                .zip(["handoff-first", "handoff-last", "live-after-subscribe"])
        {
            assert!(line.ends_with(summary), "wrong table entry: {line}");
            assert!(
                line.contains("worker  reserved:worker"),
                "wrong live actor/module: {line}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_json_follow_replays_handoff_once_and_filters_live_entries() {
    follow_handoff(true, false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn activity_table_follow_uses_head_even_when_query_is_empty() {
    follow_handoff(false, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ckni_json_follow_versions_initial_handoff_and_live_entries_one_per_line() {
    follow_handoff(true, false, true).await;
}

#[test]
fn activity_help_and_invalid_filters_do_not_connect() {
    let fixture = Fixture::new();
    for args in [vec!["activity", "--help"], vec!["help", "activity"]] {
        let output = fixture.ck(&args);
        success(&output);
        let help = String::from_utf8(output.stdout).unwrap();
        for flag in [
            "--agent",
            "--project",
            "--module",
            "--kind",
            "--since-ms",
            "--until-ms",
            "--limit",
            "--follow",
            "--json",
        ] {
            assert!(help.contains(flag), "missing help for {flag}");
        }
    }
    for args in [
        vec!["--limit", "0"],
        vec!["--limit", "4294967296"],
        vec!["--limit", "no"],
        vec!["--since-ms", "-1"],
        vec!["--until-ms"],
        vec!["--agent"],
        vec!["--bogus"],
        vec!["--since-ms", "2", "--until-ms", "1"],
    ] {
        let mut full = vec!["activity"];
        full.extend(args);
        let output = fixture.ck(&full);
        assert_eq!(output.status.code(), Some(2), "args: {full:?}");
        assert!(output.stdout.is_empty());
    }
}
