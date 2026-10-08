//! End-to-end confirmation tests: all prompts come from the daemon's scripted
//! provider, never the OS UI. Only the explicitly non-ckdev refusal test selects
//! the OS provider, and it sends no confirmation on platforms with real dialogs.
use super::*;

const FIRST: &str = "operator-confirm-module-1";
const DIRECT: &str = "operator-confirm-direct";
const WAIT: Duration = Duration::from_secs(10);

fn binaries() -> (PathBuf, PathBuf) {
    static BINARIES: OnceLock<(PathBuf, PathBuf)> = OnceLock::new();
    BINARIES
        .get_or_init(|| {
            let workspace = workspace_root();
            let daemon = ensure_binary(
                &workspace,
                binary_path(&workspace, "ck-subc-under-test"),
                &[
                    "build",
                    "-p",
                    "subc-core",
                    "--features",
                    "test-support",
                    "--bin",
                    "ck-subc-under-test",
                ],
            );
            let module = ensure_binary(
                &workspace,
                example_path(&workspace, "operator-confirm-module"),
                &[
                    "build",
                    "-p",
                    "subc-client-rs",
                    "--example",
                    "operator-confirm-module",
                ],
            );
            (daemon, module)
        })
        .clone()
}

struct Harness {
    _serial: tokio::sync::OwnedMutexGuard<()>,
    daemon: LiveDaemon,
    _temp: TestTempDir,
    root: PathBuf,
    events: PathBuf,
    module_bin: PathBuf,
}

impl Harness {
    async fn start(script: &str) -> Self {
        Self::clocks(script, 150_000, 120_000, 10_000, true).await
    }

    async fn clocks(script: &str, queue: u64, prompt: u64, stuck: u64, ckdev: bool) -> Self {
        static SERIAL: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
        let serial = SERIAL
            .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
            .lock_owned()
            .await;
        let (daemon_bin, module_bin) = binaries();
        let temp = unique_temp_dir("operator-confirm");
        let root = temp.to_path_buf();
        let runtime = root.join("runtime");
        let config = root.join("config");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(config.join("cortexkit")).unwrap();
        let mut modules = serde_json::Map::new();
        for n in 1..=6 {
            modules.insert(
                format!("operator-confirm-module-{n}"),
                json!({
                    "program": module_bin, "args":[], "enabled":true,
                    "env":{"OPERATOR_MODULE_EVENTS":root.join(format!("module-{n}.jsonl"))},
                }),
            );
        }
        fs::write(
            config.join("cortexkit/subc.jsonc"),
            json!({"version":1, "log":{"level":"debug"}, "modules":modules}).to_string(),
        )
        .unwrap();
        let events = root.join("operator.jsonl");
        let script_path = root.join("script");
        // Gate paths are made inside this harness, even for scripts assembled
        // before the scratch directory exists.
        fs::write(
            &script_path,
            script.replace("@GATE@", root.join("gate").to_str().unwrap()),
        )
        .unwrap();
        let placed = runtime.join(format!(
            "{}{}",
            if ckdev {
                "ckdev-subc"
            } else {
                "ck-subc-under-test"
            },
            std::env::consts::EXE_SUFFIX
        ));
        if fs::hard_link(&daemon_bin, &placed).is_err() {
            fs::copy(&daemon_bin, &placed).unwrap();
        }
        let child = Command::new(&placed)
            .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
            .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
            .env_remove(subc_client_rs::launch_nonce::LAUNCH_NONCE_FD_ENV)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("XDG_CONFIG_HOME", &config)
            .env("XDG_DATA_HOME", root.join("data"))
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env("SUBC_TEST_OPERATOR_SCRIPT", &script_path)
            .env("SUBC_TEST_OPERATOR_EVENTS", &events)
            .env("SUBC_TEST_OPERATOR_QUEUE_WAIT_MS", queue.to_string())
            .env("SUBC_TEST_OPERATOR_TIMEOUT_MS", prompt.to_string())
            .env("SUBC_TEST_OPERATOR_STUCK_MS", stuck.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let daemon = LiveDaemon {
            child,
            runtime_dir: runtime.clone(),
            config_dir: config,
            connection_file: runtime.join("subc-connection.json"),
        };
        let harness = Self {
            _serial: serial,
            daemon,
            _temp: temp,
            root,
            events,
            module_bin,
        };
        wait_for_connection_file(&harness.daemon.connection_file, WAIT).await;
        for n in 1..=6 {
            wait_for_catalog_module(
                &harness.daemon.connection_file,
                &format!("operator-confirm-module-{n}"),
                WAIT,
            )
            .await;
        }
        harness
    }

    async fn raw(&self) -> TcpStream {
        connect_authed_client(&self.daemon.connection_file)
            .await
            .unwrap()
    }

    async fn request(&self, module: &str, body: Value) -> Pending {
        let mut stream = self.raw().await;
        let (channel, epoch) = open_route(&mut stream, module, 1).await;
        send(
            &mut stream,
            data_request(channel, epoch, 2, &serde_json::to_vec(&body).unwrap()),
        )
        .await;
        Pending {
            stream,
            channel,
            epoch,
        }
    }
    async fn call(&self, module: &str, body: Value) -> Value {
        self.request(module, body).await.reply().await
    }
    async fn confirm(&self, summary: &str) -> Value {
        self.call(FIRST, json!({"summary":summary})).await
    }
    fn events(&self) -> Vec<Value> {
        read_events(&self.events)
    }
    fn gate(&self) {
        fs::write(self.root.join("gate"), "approved").unwrap();
    }
    async fn shown(&self, count: usize) {
        let until = Instant::now() + WAIT;
        while self
            .events()
            .iter()
            .filter(|e| e["event"] == "prompt_shown")
            .count()
            < count
        {
            assert!(
                Instant::now() < until,
                "prompt {count} missing: {:?}",
                self.events()
            );
            sleep(Duration::from_millis(10)).await;
        }
    }
    async fn event(&self, event: &str, reason: &str) -> Value {
        wait_for_event(&self.events, WAIT, |e| {
            e["event"] == event && (reason.is_empty() || e["reason"] == reason)
        })
        .await
    }
    fn logs(&self) -> String {
        fs::read_dir(self.root.join("data/cortexkit/run/logs"))
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("subc."))
            .filter_map(|e| fs::read_to_string(e.path()).ok())
            .collect::<Vec<_>>()
            .join("\n")
    }
    async fn audit(
        &self,
        outcome: &str,
        reason: &str,
        summary: Option<&str>,
    ) -> BTreeMap<String, String> {
        let until = Instant::now() + WAIT;
        loop {
            for line in self
                .logs()
                .lines()
                .filter(|l| l.contains("operator_confirm_audit"))
            {
                let fields = audit_fields(line);
                if fields.get("outcome").is_some_and(|v| v == outcome)
                    && fields.get("reason").is_some_and(|v| v == reason)
                    && summary.is_none_or(|s| {
                        fields
                            .get("summary")
                            .is_some_and(|v| v == &format!("{s:?}"))
                    })
                {
                    return fields;
                }
            }
            assert!(
                Instant::now() < until,
                "missing audit {outcome}/{reason}: {}",
                self.logs()
            );
            sleep(Duration::from_millis(20)).await;
        }
    }
    async fn direct(&self) -> ProviderProcess {
        let child = Command::new(&self.module_bin)
            .arg("--subc")
            .arg(&self.daemon.connection_file)
            .env(subc_protocol::SUBC_MODULE_ID_ENV, DIRECT)
            .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
            .env_remove(subc_client_rs::launch_nonce::LAUNCH_NONCE_FD_ENV)
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_RUNTIME_DIR", &self.daemon.runtime_dir)
            .env("XDG_CONFIG_HOME", &self.daemon.config_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let process = ProviderProcess { child };
        wait_for_catalog_module(&self.daemon.connection_file, DIRECT, WAIT).await;
        process
    }
    // Probe until the first request really occupies its module slot. This is an
    // admission barrier, not a guessed sleep, so FIFO tests cannot pass by chance.
    async fn admitted(&self, module: &str) {
        let summary = format!("admission probe for {module}");
        assert_eq!(
            self.call(module, json!({"summary":summary})).await["result"],
            "unavailable"
        );
        self.audit(
            "operator_presence_unavailable",
            "module_limit",
            Some(&summary),
        )
        .await;
    }
}

struct Pending {
    stream: TcpStream,
    channel: u16,
    epoch: u32,
}
impl Pending {
    async fn reply(&mut self) -> Value {
        let frame = timeout(WAIT, read_frame(&mut self.stream))
            .await
            .expect("module answer timed out")
            .unwrap()
            .expect("consumer connection closed");
        assert_eq!(
            frame.header.ty,
            FrameType::Response,
            "{}",
            String::from_utf8_lossy(&frame.body)
        );
        assert_eq!(frame.header.corr, 2);
        json_body(&frame.body)
    }
    async fn close(&mut self) {
        send(&mut self.stream, goodbye_frame(self.channel, self.epoch, 3)).await;
    }
    async fn cancel(&mut self) {
        send(&mut self.stream, cancel_frame(self.channel, self.epoch, 2)).await;
    }
}
async fn send(stream: &mut TcpStream, frame: Frame) {
    write_frame(stream, &frame).await.unwrap();
    stream.flush().await.unwrap();
}

// tracing's text formatter uses Rust Debug strings, not JSON strings (notably
// \\u{202e}). Scan quoted tokens, including their escapes, rather than splitting
// on spaces or accepting a subset of fields. This also rejects unescaped lines.
fn audit_fields(line: &str) -> BTreeMap<String, String> {
    let text = line.split_once("operator_confirm_audit").unwrap().1.trim();
    let mut fields = BTreeMap::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        let start = at;
        while at < bytes.len() && bytes[at] != b'=' {
            at += 1;
        }
        assert!(at < bytes.len(), "unparsed audit fragment: {line}");
        let key = &text[start..at];
        at += 1;
        let value_start = at;
        let quoted = at < bytes.len() && bytes[at] == b'"';
        if quoted {
            at += 1;
            loop {
                assert!(at < bytes.len(), "unterminated audit value: {line}");
                match bytes[at] {
                    b'\\' => at += 2,
                    b'"' => {
                        at += 1;
                        break;
                    }
                    _ => at += 1,
                }
            }
        } else {
            while at < bytes.len() && !bytes[at].is_ascii_whitespace() {
                at += 1;
            }
        }
        let raw = &text[value_start..at];
        // Preserve summary's Debug spelling so rejected format characters are
        // checked against the original scalars, not a lossy JSON decoder.
        let value = if key == "summary" {
            serde_json::from_str::<String>(raw).expect("summary must be an escaped Debug string")
        } else {
            raw.trim_matches('"').to_string()
        };
        assert!(
            fields.insert(key.into(), value).is_none(),
            "duplicate audit field"
        );
    }
    assert_eq!(
        fields.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            "module_id",
            "outcome",
            "principal",
            "prompt_ms",
            "prompt_shown",
            "reason",
            "summary",
            "wait_ms"
        ]
    );
    assert!(fields["wait_ms"].parse::<u64>().is_ok());
    assert!(fields["prompt_ms"].parse::<u64>().is_ok());
    assert!(fields["prompt_shown"].parse::<bool>().is_ok());
    fields
}

async fn basic(script: &str, result: &str, code: &str, reason: &str) {
    let h = Harness::start(script).await;
    assert_eq!(h.confirm("write the record").await["result"], result);
    let audit = h.audit(code, reason, Some("write the record")).await;
    assert_eq!(audit["principal"], "direct");
    assert_eq!(audit["prompt_shown"], "true");
}
#[tokio::test]
async fn confirmed() {
    basic("approve", "confirmed", "confirmed", "").await;
}
#[tokio::test]
async fn declined_by_person() {
    basic("decline", "declined", "operator_declined", "person").await;
}
#[tokio::test]
async fn provider_error_is_unavailable() {
    basic(
        "unavailable",
        "unavailable",
        "operator_presence_unavailable",
        "provider_error",
    )
    .await;
}
#[tokio::test]
async fn exhausted_script_is_unavailable() {
    basic(
        "",
        "unavailable",
        "operator_presence_unavailable",
        "provider_error",
    )
    .await;
}

async fn invalid(summary: &str) {
    let h = Harness::start("approve").await;
    assert_eq!(h.confirm(summary).await["result"], "summary_invalid");
    let audit = h.audit("operator_summary_invalid", "", Some(summary)).await;
    assert_eq!(audit["prompt_shown"], "false");
    assert!(h.events().is_empty());
    assert_eq!(
        h.logs()
            .lines()
            .filter(|l| l.contains("operator_confirm_audit"))
            .count(),
        1
    );
}
macro_rules! invalid_tests {
    ($($name:ident => $summary:expr),* $(,)?) => { $(#[tokio::test] async fn $name() { invalid($summary).await; })* };
}
invalid_tests! {
    summary_empty => "", summary_201_scalars => &"é".repeat(201),
    summary_cr => "write\rrecord", summary_lf => "write\nrecord", summary_tab => "write\trecord",
    summary_u2028 => "write\u{2028}record", summary_u202e => "write\u{202e}record",
    summary_u200b => "write\u{200b}record", summary_u2029 => "write\u{2029}record",
    summary_u2060 => "write\u{2060}record", summary_ufeff => "write\u{feff}record",
    summary_leading_whitespace => " write", summary_trailing_whitespace => "write\u{2003}",
}
#[tokio::test]
async fn summary_200_multibyte_scalars() {
    let h = Harness::start("approve").await;
    assert_eq!(h.confirm(&"é".repeat(200)).await["result"], "confirmed");
}
#[tokio::test]
async fn summary_combining_character() {
    let h = Harness::start("approve").await;
    assert_eq!(h.confirm("e\u{301}crire").await["result"], "confirmed");
}

#[tokio::test]
async fn malformed_registered_body() {
    let h = Harness::start("approve").await;
    let mut module = h.raw().await;
    let hello = subc_protocol::ModuleHelloBody {
        manifest: inline_module_manifest("operator-raw", &["operator"]),
        protocol_ver: subc_protocol::PROTOCOL_VERSION,
        control_ops: None,
        launch_nonce: None,
    };
    send(
        &mut module,
        Frame::build(
            FrameType::Hello,
            Flags::new(false, Priority::Interactive, false),
            0,
            0,
            1,
            serde_json::to_vec(&hello).unwrap(),
        )
        .unwrap(),
    )
    .await;
    assert_eq!(
        read_frame_timeout(&mut module).await.header.ty,
        FrameType::HelloAck
    );
    send(
        &mut module,
        control_request_frame(2, json!({"op":"operator.confirm"}).to_string().into_bytes()),
    )
    .await;
    let reply = read_frame_timeout(&mut module).await;
    assert_eq!(reply.header.ty, FrameType::Error);
    assert_eq!(json_body(&reply.body)["code"], "invalid_control_body");
    h.audit("invalid_control_body", "", None).await;
    assert!(h.events().is_empty());
}
#[tokio::test]
async fn raw_client_not_registered_before_permission() {
    let h = Harness::start("approve").await;
    let mut client = h.raw().await;
    send(
        &mut client,
        control_request_frame(
            2,
            json!({"op":"operator.confirm", "summary":"", "route_channel":65535, "route_epoch":0})
                .to_string()
                .into_bytes(),
        ),
    )
    .await;
    let reply = read_frame_timeout(&mut client).await;
    assert_eq!(json_body(&reply.body)["code"], "not_registered");
    h.audit("not_registered", "", None).await;
    assert!(h.events().is_empty());
}
#[tokio::test]
async fn non_nonce_module_not_permitted() {
    let h = Harness::start("approve").await;
    let _direct = h.direct().await;
    assert_eq!(
        h.call(DIRECT, json!({"summary":"write"})).await["result"],
        "not_permitted"
    );
    h.audit("operator_request_not_permitted", "", Some("write"))
        .await;
    assert!(h.events().is_empty());
}
#[tokio::test]
async fn permission_before_invalid_summary() {
    let h = Harness::start("approve").await;
    let _direct = h.direct().await;
    assert_eq!(
        h.call(DIRECT, json!({"summary":""})).await["result"],
        "not_permitted"
    );
    h.audit("operator_request_not_permitted", "", Some(""))
        .await;
    assert!(h.events().is_empty());
}
#[tokio::test]
async fn stale_route_not_permitted() {
    let h = Harness::start("approve").await;
    let mut first = h.request(FIRST, json!({"mode":"echo"})).await;
    first.reply().await;
    first.close().await;
    // A control round trip after GOODBYE fences its processing on this socket.
    control_rpc_on_stream(&mut first.stream, 4, json!({"op":"catalog.list"})).await;
    assert_eq!(
        h.call(FIRST, json!({"mode":"stale_route", "summary":"write"}))
            .await["result"],
        "not_permitted"
    );
    h.audit("operator_request_not_permitted", "", Some("write"))
        .await;
    assert!(h.events().is_empty());
}
#[tokio::test]
async fn summary_before_backoff() {
    let h = Harness::start("decline").await;
    assert_eq!(h.confirm("first").await["result"], "declined");
    assert_eq!(h.confirm("").await["result"], "summary_invalid");
    h.audit("operator_summary_invalid", "", Some("")).await;
}
#[tokio::test]
async fn backoff_on_new_client_route() {
    let h = Harness::start("decline\napprove").await;
    assert_eq!(h.confirm("first").await["result"], "declined");
    assert_eq!(h.confirm("again").await["result"], "declined");
    h.audit("operator_declined", "backoff", Some("again")).await;
    assert_eq!(
        h.events()
            .iter()
            .filter(|e| e["event"] == "prompt_shown")
            .count(),
        1
    );
}
#[tokio::test]
async fn module_limit() {
    let h = Harness::start("hang").await;
    let _pending = h.request(FIRST, json!({"summary":"first"})).await;
    h.shown(1).await;
    assert_eq!(h.confirm("second").await["result"], "unavailable");
    h.audit(
        "operator_presence_unavailable",
        "module_limit",
        Some("second"),
    )
    .await;
    assert_eq!(h.events().len(), 1);
}

async fn fill(h: &Harness) -> Vec<Pending> {
    let mut requests = vec![h.request(FIRST, json!({"summary":"first"})).await];
    h.shown(1).await;
    for n in 2..=5 {
        let module = format!("operator-confirm-module-{n}");
        requests.push(
            h.request(&module, json!({"summary":format!("waiting-{n}")}))
                .await,
        );
        wait_for_event(&h.root.join(format!("module-{n}.jsonl")), WAIT, |e| {
            e["event"] == "confirm_waiting" && e["summary"] == format!("waiting-{n}")
        })
        .await;
        h.admitted(&module).await;
    }
    requests
}
#[tokio::test]
async fn fifo_four_waiters() {
    let h = Harness::start("approve when @GATE@\napprove\napprove\napprove\napprove").await;
    let mut requests = fill(&h).await;
    h.gate();
    for request in &mut requests {
        assert_eq!(request.reply().await["result"], "confirmed");
    }
    let shown: Vec<_> = h
        .events()
        .into_iter()
        .filter(|e| e["event"] == "prompt_shown")
        .map(|e| e["text"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(shown.len(), 5);
    for (n, text) in shown.iter().enumerate() {
        assert!(text.starts_with(&format!("operator-confirm-module-{} asks:", n + 1)));
    }
}
#[tokio::test]
async fn queue_full() {
    let h = Harness::start("hang").await;
    let _requests = fill(&h).await;
    assert_eq!(
        h.call("operator-confirm-module-6", json!({"summary":"overflow"}))
            .await["result"],
        "unavailable"
    );
    h.audit(
        "operator_presence_unavailable",
        "queue_full",
        Some("overflow"),
    )
    .await;
}
#[tokio::test]
async fn module_limit_before_queue_full() {
    let h = Harness::start("hang").await;
    let _requests = fill(&h).await;
    assert_eq!(h.confirm("duplicate").await["result"], "unavailable");
    h.audit(
        "operator_presence_unavailable",
        "module_limit",
        Some("duplicate"),
    )
    .await;
}
#[tokio::test]
async fn queue_wait_expires_and_leaves_queue() {
    let h = Harness::clocks("hang\napprove", 300, 5000, 1000, true).await;
    let mut first = h.request(FIRST, json!({"summary":"first"})).await;
    h.shown(1).await;
    let started = Instant::now();
    let mut second = h
        .request("operator-confirm-module-2", json!({"summary":"waiting"}))
        .await;
    assert_eq!(second.reply().await["result"], "unavailable");
    let audit = h
        .audit(
            "operator_presence_unavailable",
            "queue_wait",
            Some("waiting"),
        )
        .await;
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert_eq!(audit["prompt_shown"], "false");
    first.cancel().await;
    h.event("provider_returned", "").await;
    assert_eq!(
        h.call("operator-confirm-module-2", json!({"summary":"new"}))
            .await["result"],
        "confirmed"
    );
}
#[tokio::test]
async fn withdraw_on_route_close() {
    let h = Harness::start("hang").await;
    let mut pending = h.request(FIRST, json!({"summary":"closing"})).await;
    h.shown(1).await;
    pending.close().await;
    h.event("withdraw", "route_closed").await;
    h.event("provider_returned", "").await;
    h.audit("operator_declined", "route_closed", Some("closing"))
        .await;
    sleep(Duration::from_millis(100)).await;
    assert_eq!(
        h.logs()
            .lines()
            .filter(|l| l.contains("operator_confirm_audit"))
            .count(),
        1
    );
    assert_eq!(
        h.events()
            .iter()
            .filter(|e| e["event"] == "withdraw")
            .count(),
        1
    );
}
#[tokio::test]
async fn withdraw_on_module_disconnect() {
    let h = Harness::start("hang").await;
    let _pending = h
        .request(
            FIRST,
            json!({"summary":"disconnect", "mode":"exit_when_shown", "events":h.events}),
        )
        .await;
    h.event("withdraw", "module_closed").await;
    h.audit("operator_declined", "module_closed", Some("disconnect"))
        .await;
    assert!(!h.events().iter().any(|e| e["reason"] == "route_closed"));
}
#[tokio::test]
async fn helper_drop_cancel_prompts_next_request() {
    let h = Harness::start("hang\napprove").await;
    let mut first = h.request(FIRST, json!({"summary":"cancel"})).await;
    h.shown(1).await;
    let mut next = h
        .request("operator-confirm-module-2", json!({"summary":"next"}))
        .await;
    first.cancel().await;
    h.event("withdraw", "caller_cancelled").await;
    h.audit("operator_declined", "caller_cancelled", Some("cancel"))
        .await;
    assert_eq!(next.reply().await["result"], "confirmed");
    h.shown(2).await;
    provider_return_fences_next(&h);
}
#[tokio::test]
async fn timeout_withdraws_and_answers() {
    let h = Harness::clocks("hang", 5000, 300, 1000, true).await;
    assert_eq!(h.confirm("timeout").await["result"], "unavailable");
    h.event("withdraw", "timeout").await;
    h.audit("operator_presence_unavailable", "timeout", Some("timeout"))
        .await;
}

fn provider_return_fences_next(h: &Harness) {
    let mut showing = false;
    for event in h.events() {
        if event["event"] == "prompt_shown" {
            assert!(!showing, "next prompt preceded provider return");
            showing = true;
        }
        if event["event"] == "provider_returned" {
            assert!(showing);
            showing = false;
        }
    }
}
#[tokio::test]
async fn confirm_before_close_has_no_withdraw() {
    let h = Harness::start("approve when @GATE@\napprove").await;
    let mut first = h.request(FIRST, json!({"summary":"race"})).await;
    h.shown(1).await;
    let mut next = h
        .request("operator-confirm-module-2", json!({"summary":"next"}))
        .await;
    h.gate();
    assert_eq!(first.reply().await["result"], "confirmed");
    first.close().await;
    assert_eq!(next.reply().await["result"], "confirmed");
    h.audit("confirmed", "", Some("race")).await;
    assert!(!h.events().iter().any(|e| e["event"] == "withdraw"));
    provider_return_fences_next(&h);
}
#[tokio::test]
async fn close_before_confirm_discards_late_success() {
    let h = Harness::start("approve when @GATE@\napprove").await;
    let mut first = h.request(FIRST, json!({"summary":"race"})).await;
    h.shown(1).await;
    let mut next = h
        .request("operator-confirm-module-2", json!({"summary":"next"}))
        .await;
    first.close().await;
    h.event("withdraw", "route_closed").await;
    h.audit("operator_declined", "route_closed", Some("race"))
        .await;
    assert_eq!(
        h.events()
            .iter()
            .filter(|e| e["event"] == "prompt_shown")
            .count(),
        1
    );
    h.gate();
    assert_eq!(next.reply().await["result"], "confirmed");
    provider_return_fences_next(&h);
    let until = Instant::now() + WAIT;
    while !h.logs().contains("late operator provider result discarded") {
        assert!(
            Instant::now() < until,
            "late success not logged: {}",
            h.logs()
        );
        sleep(Duration::from_millis(20)).await;
    }
}
#[tokio::test]
async fn timeout_before_confirm_discards_late_success() {
    let h = Harness::clocks("approve when @GATE@\napprove", 5000, 400, 3000, true).await;
    let mut first = h.request(FIRST, json!({"summary":"race"})).await;
    h.shown(1).await;
    let mut next = h
        .request("operator-confirm-module-2", json!({"summary":"next"}))
        .await;
    assert_eq!(first.reply().await["result"], "unavailable");
    h.event("withdraw", "timeout").await;
    h.audit("operator_presence_unavailable", "timeout", Some("race"))
        .await;
    assert_eq!(
        h.events()
            .iter()
            .filter(|e| e["event"] == "prompt_shown")
            .count(),
        1
    );
    h.gate();
    assert_eq!(next.reply().await["result"], "confirmed");
    provider_return_fences_next(&h);
}
#[tokio::test]
async fn stuck_provider_refuses_until_provider_returns() {
    let h = Harness::clocks("unavailable when @GATE@\napprove", 5000, 400, 500, true).await;
    let mut first = h.request(FIRST, json!({"summary":"hung"})).await;
    h.shown(1).await;
    let mut queued = h
        .request("operator-confirm-module-2", json!({"summary":"queued"}))
        .await;
    assert_eq!(first.reply().await["result"], "unavailable");
    h.audit("operator_presence_unavailable", "timeout", Some("hung"))
        .await;
    assert_eq!(
        h.events()
            .iter()
            .filter(|e| e["event"] == "prompt_shown")
            .count(),
        1
    );
    assert_eq!(queued.reply().await["result"], "unavailable");
    h.audit(
        "operator_presence_unavailable",
        "provider_stuck",
        Some("queued"),
    )
    .await;
    assert_eq!(
        h.call("operator-confirm-module-3", json!({"summary":"new"}))
            .await["result"],
        "unavailable"
    );
    h.audit(
        "operator_presence_unavailable",
        "provider_stuck",
        Some("new"),
    )
    .await;
    assert_eq!(
        h.events()
            .iter()
            .filter(|e| e["event"] == "prompt_shown")
            .count(),
        1
    );
    h.gate();
    h.event("provider_returned", "").await;
    assert_eq!(h.confirm("recovered").await["result"], "confirmed");
    provider_return_fences_next(&h);
}
#[tokio::test]
async fn backoff_before_provider_stuck() {
    let h = Harness::clocks("decline\nunavailable when @GATE@", 5000, 400, 200, true).await;
    assert_eq!(h.confirm("decline").await["result"], "declined");
    let mut hung = h
        .request("operator-confirm-module-2", json!({"summary":"hung"}))
        .await;
    h.shown(2).await;
    let mut queued = h
        .request("operator-confirm-module-3", json!({"summary":"queued"}))
        .await;
    hung.reply().await;
    queued.reply().await;
    h.audit(
        "operator_presence_unavailable",
        "provider_stuck",
        Some("queued"),
    )
    .await;
    assert_eq!(h.confirm("backoff").await["result"], "declined");
    h.audit("operator_declined", "backoff", Some("backoff"))
        .await;
    h.gate();
}
#[tokio::test]
async fn consumer_300_second_timeout_accepts_approval_after_35_seconds() {
    let h = Harness::start("approve when @GATE@").await;
    let consumer = SubcConsumer::connect(&h.daemon.connection_file, fast_consumer_options())
        .await
        .unwrap();
    let call = consumer.call(
        tool_target(FIRST),
        consumer_identity("slow"),
        json!({"summary":"slow approval"}).to_string().into_bytes(),
        CallOptions {
            timeout: Duration::from_secs(300),
            ..fast_call_options()
        },
    );
    tokio::pin!(call);
    tokio::select! { result = &mut call => panic!("answered before approval: {result:?}"), _ = h.shown(1) => {} }
    tokio::select! { result = &mut call => panic!("answered at default timeout: {result:?}"), _ = sleep(Duration::from_secs(35)) => {} }
    h.gate();
    let reply = timeout(WAIT, call).await.unwrap().unwrap();
    assert_eq!(json_body(&reply)["result"], "confirmed");
    consumer.close().await;
}
#[tokio::test]
async fn routing_forwarding_and_scope_describe_work_during_prompt() {
    let h = Harness::start("hang").await;
    let _hung = h.request(FIRST, json!({"summary":"hung"})).await;
    h.shown(1).await;
    assert_eq!(
        h.call(
            "operator-confirm-module-2",
            json!({"mode":"echo", "summary":"routing"})
        )
        .await["summary"],
        "routing"
    );
    assert_eq!(h.call("operator-confirm-module-3", json!({"mode":"forward", "peer":"operator-confirm-module-4", "peer_mode":"echo", "summary":"forwarding"})).await["summary"], "forwarding");
    assert_eq!(
        h.call("operator-confirm-module-5", json!({"mode":"describe"}))
            .await["ok"],
        true
    );
    assert_eq!(h.events().len(), 1);
}
#[tokio::test]
async fn direct_prompt_text_and_exact_audit() {
    let h = Harness::start("approve").await;
    assert_eq!(h.confirm("write this record").await["result"], "confirmed");
    assert_eq!(
        h.event("prompt_shown", "").await["text"],
        "operator-confirm-module-1 asks: write this record (requested by a local program)"
    );
    let audit = h.audit("confirmed", "", Some("write this record")).await;
    assert_eq!(audit["principal"], "direct");
}
#[tokio::test]
async fn reserved_prompt_text() {
    let h = Harness::start("approve").await;
    assert_eq!(h.call(FIRST, json!({"mode":"forward", "peer":"operator-confirm-module-2", "summary":"write this record"})).await["result"], "confirmed");
    assert_eq!(h.event("prompt_shown", "").await["text"], "operator-confirm-module-2 asks: write this record (requested by operator-confirm-module-1)");
    assert_eq!(
        h.audit("confirmed", "", Some("write this record")).await["principal"],
        "reserved:operator-confirm-module-1"
    );
}
#[tokio::test]
async fn consecutive_confirmed_calls_prompt_twice() {
    let h = Harness::start("approve\napprove").await;
    assert_eq!(h.confirm("first").await["result"], "confirmed");
    assert_eq!(h.confirm("second").await["result"], "confirmed");
    h.shown(2).await;
    assert_eq!(
        h.events()
            .iter()
            .filter(|e| e["event"] == "prompt_shown")
            .count(),
        2
    );
}
#[tokio::test]
async fn hello_ack_advertises_operator_confirm() {
    let h = Harness::start("approve").await;
    let ack = wait_for_event(&h.root.join("module-1.jsonl"), WAIT, |e| {
        e["event"] == "hello_ack"
    })
    .await;
    assert!(ack["subc_ops"]
        .as_array()
        .unwrap()
        .iter()
        .any(|op| op == "operator.confirm"));
}
#[tokio::test]
async fn non_ckdev_refuses_test_provider() {
    let h = Harness::clocks("approve", 5000, 5000, 1000, false).await;
    let until = Instant::now() + WAIT;
    while !h
        .logs()
        .contains("operator confirm test provider refused: executable name must start with ckdev-")
    {
        assert!(Instant::now() < until, "refusal log missing: {}", h.logs());
        sleep(Duration::from_millis(20)).await;
    }
    assert!(h.events().is_empty());
}
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_unsupported_platform_without_prompt() {
    let h = Harness::clocks("approve", 5000, 5000, 1000, false).await;
    assert!(h.logs().contains("operator confirm test provider refused"));
    assert_eq!(h.confirm("linux write").await["result"], "unavailable");
    assert_eq!(
        h.audit(
            "operator_presence_unavailable",
            "unsupported_platform",
            Some("linux write")
        )
        .await["prompt_shown"],
        "false"
    );
    assert!(h.events().is_empty());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn macos_supervised_confirmation_crosses_privacy_trampoline() {
    let h = Harness::start("approve").await;
    assert_eq!(h.confirm("trampoline write").await["result"], "confirmed");
    assert!(h
        .logs()
        .contains("module spawned with own privacy identity (responsibility disclaimed)"));
}
