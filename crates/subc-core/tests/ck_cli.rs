use std::{
    collections::BTreeMap,
    fs,
    ops::Deref,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::Arc,
    time::Duration,
};

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use subc_control::{
    ClientControlRequest, ClientControlResponse, SupervisorEntry, SupervisorHealthStatus,
};
use subc_core::{
    read_frame, test_support::TestTempDir as TempDir, write_frame, Frame, HealthConfig, ModuleSpec,
    RestartPolicy, SupervisedModule, Supervisor, SupervisorHandle, SupervisorProcessLiveness,
};
use subc_protocol::{Flags, FrameType, Priority, PROTOCOL_VERSION};
use subc_transport::{
    generate_daemon_id, generate_key, write_atomic, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    time::{sleep, timeout, Instant},
};

mod common;
use common::{
    connect_authed_client, start_test_daemon_with_process_liveness_and_supervisor, TestDaemon,
};

const READ_TIMEOUT: Duration = Duration::from_secs(2);
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

struct TestServer {
    daemon: TestDaemon,
    process_liveness: Arc<SupervisorProcessLiveness>,
    supervisor_handle: SupervisorHandle,
}

impl TestServer {
    async fn start() -> Self {
        let process_liveness = Arc::new(SupervisorProcessLiveness::new());
        let supervisor_handle = SupervisorHandle::new();
        let daemon = start_test_daemon_with_process_liveness_and_supervisor(
            "ck-cli-server",
            process_liveness.clone(),
            supervisor_handle.clone(),
        )
        .await;
        Self {
            daemon,
            process_liveness,
            supervisor_handle,
        }
    }
}

impl Deref for TestServer {
    type Target = TestDaemon;

    fn deref(&self) -> &Self::Target {
        &self.daemon
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_reports_and_renders_route_counters() {
    let server = TestServer::start().await;

    let response = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["daemon", "--json"],
    ));
    assert_eq!(response["counters"]["module_frames_dropped_no_route"], 0);
    assert_eq!(response["counters"]["route_release_stale_skipped"], 0);

    let output = ck_with_subc(&server.connection_file_path, ["daemon"]);
    assert_exit(&output, 0);
    let uptime = connection_file_elapsed(&server.connection_file_path);
    assert_eq!(
        text(&output.stdout),
        format!(
            "daemon test-subc · pid {} · up {uptime} · 1 clients · no frame drops in the last 10 minutes\n",
            std::process::id()
        )
    );

    let verbose = ck_with_subc(&server.connection_file_path, ["daemon", "--verbose"]);
    assert_exit(&verbose, 0);
    let stdout = text(&verbose.stdout);
    assert!(stdout.contains("counter"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("module_frames_dropped_no_route"),
        "stdout:\n{stdout}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_ck_renders_live_dashboard_and_commands() {
    let server = TestServer::start().await;
    let output = ck_with_subc(&server.connection_file_path, []);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with(&format!(
            "ck {} · daemon running (pid {}, up ",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        )),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("modules: none"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("updates: unknown (could not reach cortexkit.io)"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("commands:"), "stdout:\n{stdout}");
    assert!(
        stdout.ends_with("run `ck <command>` for its verbs, `ck --help` for everything\n"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn bare_json_keeps_the_captured_master_help_bytes() {
    let empty_path = TempDir::new("ck-bare-json-path");
    let output = ck_command()
        .arg("--json")
        .env("PATH", empty_path.path())
        .output()
        .unwrap();
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        "ck — CortexKit operator CLI\n\nusage:\n  ck [--subc <connection-file>] [--json] <domain> [<verb>] [<args>]\n\ndomains:\n  setup     plan and apply the managed CortexKit installation\n  upgrade   plan managed component upgrades\n  module    supervised modules: list, status, stderr, terminals, restart, stop, start, rescan, release\n  routes    live consumers for one module or the whole daemon\n  provenance daemon-attested and module-declared build/process facts\n  health    one-line health for every supervised module\n  quota     AI-provider quota and usage windows\n  daemon    daemon version, uptime, connection info, offline triage, and CI lint\n\nflags:\n  --subc <file>   use a specific connection file (default: auto-discover)\n  --json          raw JSON output instead of tables\n\nrun 'ck <domain>' with no verb to see that domain's commands\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bare_ck_renders_degraded_module_and_no_updates_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "insula",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
            ("FAKE_AFT_HEALTH_DETAIL", "1 provider failing"),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "insula",
        SupervisorHealthStatus::Degraded,
    )
    .await;

    let cache_dir = TempDir::new("ck-dashboard-current-cache");
    let cache = cache_dir.path().join("update-metadata.json");
    fs::write(
        &cache,
        format!(
            r#"{{"format_version":3,"checked_at_unix_secs":{},"targets":{{}}}}"#,
            now_ms() / 1_000
        ),
    )
    .unwrap();
    let output = ck_command()
        .args(["--subc"])
        .arg(&server.connection_file_path)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .unwrap();
    assert_exit(&output, 0);
    let uptime = connection_file_elapsed(&server.connection_file_path);
    assert_eq!(
        text(&output.stdout),
        format!(
            "ck {} · daemon running (pid {}, up {uptime}, 2 clients)\nmodules: insula degraded (1 provider failing)\nupdates: none\n\ncommands: setup · upgrade · module · health · routes · quota · daemon\nrun `ck <command>` for its verbs, `ck --help` for everything\n",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        )
    );

    module.stop().await.unwrap();
}

#[test]
fn bare_ck_degrades_to_domains_when_daemon_is_unreachable() {
    let missing = unique_temp_dir("ck-bare-missing").join("subc-connection.json");
    let output = ck_command()
        .args(["--subc"])
        .arg(&missing)
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    let stdout = text(&output.stdout);
    assert!(
        stdout.starts_with(&format!(
            "ck {} · daemon stopped",
            env!("CARGO_PKG_VERSION")
        )),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains(&missing.display().to_string()),
        "stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("updates: unknown (could not reach cortexkit.io)"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("commands:"), "stdout:\n{stdout}");
    assert!(stdout.contains("module"), "stdout:\n{stdout}");
    assert!(stdout.contains("next:"), "stdout:\n{stdout}");
}

#[test]
fn bare_ck_hanging_release_source_uses_stale_cache_within_the_refresh_budget() {
    let temp = TempDir::new("ck-update-timeout");
    let cache_path = temp.path().join("update-metadata.json");
    fs::write(
        &cache_path,
        r#"{"format_version":3,"checked_at_unix_secs":0,"targets":{}}"#,
    )
    .unwrap();

    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let source_base = format!("http://{}", listener.local_addr().unwrap());
    let (release_done_tx, release_done_rx) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 1024];
        let _ = std::io::Read::read(&mut stream, &mut request);
        let _ = release_done_rx.recv_timeout(Duration::from_secs(10));
    });

    let missing = temp.path().join("subc-connection.json");
    let started = std::time::Instant::now();
    let output = ck_command()
        .args(["--subc"])
        .arg(&missing)
        .env("CK_UPDATE_CACHE_PATH", &cache_path)
        .env("CK_RELEASE_INDEX_URL", format!("{source_base}/index.json"))
        .output()
        .unwrap();
    let elapsed = started.elapsed();
    release_done_tx.send(()).unwrap();
    server.join().unwrap();

    assert_exit(&output, 0);
    // Discriminating margin, not a latency SLO: a ck that BLOCKS on the hanging
    // source cannot return before the server's 10s hold releases, while an
    // unblocked ck finishes in well under 5s even on a loaded runner. The gap
    // between the two bounds is what keeps blocked-vs-slow distinguishable;
    // an absolute tight envelope here flaked under parallel suite load.
    assert!(
        elapsed < Duration::from_secs(5),
        "bare ck exceeded its bounded refresh envelope: {elapsed:?}"
    );
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("updates: unknown (could not reach cortexkit.io)"),
        "stdout:\n{stdout}"
    );
}

fn serve_index(body: &[u8], signature_header: Option<&str>) -> String {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let addr = listener.local_addr().unwrap();
    let body = body.to_vec();
    let signature_header = signature_header.map(str::to_string);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut request = [0_u8; 2048];
            let _ = std::io::Read::read(&mut stream, &mut request);
            let mut header = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            if let Some(signature) = &signature_header {
                header.push_str(&format!("X-CortexKit-Signature-Ed25519: {signature}\r\n"));
            }
            header.push_str("\r\n");
            let _ = std::io::Write::write_all(&mut stream, header.as_bytes());
            let _ = std::io::Write::write_all(&mut stream, &body);
        }
    });
    format!("http://{addr}/index.json")
}

fn fixture_index_body() -> Vec<u8> {
    format!(
        r#"{{"schema":1,"channel":"alpha","generated_at_ms":{},"components":{{"core":{{"release":"subc-core-v{}","version":"{}","assets":{{}}}}}}}}"#,
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
        env!("CARGO_PKG_VERSION"),
        env!("CARGO_PKG_VERSION"),
    )
    .into_bytes()
}

/// RFC 8032 test-key signature over `{{"schema":1}}` — valid Ed25519, wrong key.
const FOREIGN_SIGNATURE: &str =
    "fjYZ87Tka7M+yJ+lmjD7vjSjflypCGi2KIvmSktgssO79FN8/mntGhobmTCwYDeQRAEAu7oDdv7zrAkI9N9uDA==";

#[test]
fn setup_dry_run_refuses_when_the_signature_header_is_stripped() {
    let url = serve_index(&fixture_index_body(), None);
    let output = ck_command()
        .args(["setup", "--dry-run"])
        .env("CK_RELEASE_INDEX_URL", &url)
        .output()
        .unwrap();
    assert_ne!(output.status.code(), Some(0), "stripped header must refuse");
    let combined = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(
        combined.contains("index_signature_invalid"),
        "stdout+stderr:\n{combined}"
    );
    assert!(
        combined.contains("generation 1"),
        "stdout+stderr:\n{combined}"
    );
    assert!(
        !combined.contains("setup plan:"),
        "stripped header must plan nothing:\n{combined}"
    );
}

#[test]
fn setup_dry_run_refuses_a_test_key_signature_against_the_embedded_key() {
    let url = serve_index(&fixture_index_body(), Some(FOREIGN_SIGNATURE));
    let output = ck_command()
        .args(["setup", "--dry-run"])
        .env("CK_RELEASE_INDEX_URL", &url)
        .output()
        .unwrap();
    assert_ne!(
        output.status.code(),
        Some(0),
        "foreign signature must refuse"
    );
    let combined = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(
        combined.contains("index_signature_invalid"),
        "stdout+stderr:\n{combined}"
    );
    assert!(
        combined.contains("generation 1"),
        "stdout+stderr:\n{combined}"
    );
}

struct SetupFixture {
    _root: TempDir,
    home: PathBuf,
    data_home: PathBuf,
    config_home: PathBuf,
    tools: PathBuf,
}

impl SetupFixture {
    fn installed(name: &str) -> Self {
        Self::new(name, true)
    }

    fn fresh(name: &str) -> Self {
        Self::new(name, false)
    }

    fn new(name: &str, installed: bool) -> Self {
        let root = TempDir::new(name);
        let home = root.path().join("home");
        let data_home = home.join(".local").join("share");
        let config_home = home.join(".config");
        let cortexkit = data_home.join("cortexkit");
        let bin = cortexkit.join("bin");
        let config = config_home.join("cortexkit").join("subc.jsonc");
        let tools = root.path().join("tools");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&tools).unwrap();
        write_executable(
            &tools.join(service_manager_program()),
            service_manager_script(),
        );

        if installed {
            let binaries = [
                ("core", "ck-subc"),
                ("core", "ck-subc-mcp"),
                ("aft", "ck-aft"),
                ("claustrum", "ck-claustrum"),
                ("claustrum", "ck-auth"),
                ("insula", "ck-insula"),
            ];
            let mut mutations = binaries
                .iter()
                .map(|(component, binary)| {
                    let path = bin.join(platform_binary(binary));
                    fs::write(&path, binary).unwrap();
                    json!({
                        "kind": "managed-binary",
                        "path": path,
                        "component": component,
                    })
                })
                .collect::<Vec<_>>();
            mutations.push(json!({
                "kind": "runtime-definition",
                "path": runtime_definition(&home),
            }));
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::write(
                &config,
                serde_json::to_string_pretty(&json!({
                    "version": 1,
                    "storage": {"backend": "sqlite"},
                    "modules": {
                        "aft": {"program": bin.join(platform_binary("ck-aft"))},
                        "claustrum": {
                            "program": bin.join(platform_binary("ck-claustrum")),
                            "reserved": true,
                        },
                        "insula": {"program": bin.join(platform_binary("ck-insula"))},
                    },
                }))
                .unwrap(),
            )
            .unwrap();
            fs::create_dir_all(&cortexkit).unwrap();
            fs::write(
                cortexkit.join("installer-manifest.json"),
                serde_json::to_vec_pretty(&json!({
                    "schema_version": 1,
                    "platform": host_target(),
                    "mutations": mutations,
                }))
                .unwrap(),
            )
            .unwrap();
        }

        Self {
            _root: root,
            home,
            data_home,
            config_home,
            tools,
        }
    }

    fn command(&self, index: &SignedIndex, args: &[&str]) -> Command {
        let mut command = ck_command();
        let path = std::env::join_paths(std::iter::once(self.tools.clone()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .unwrap();
        command
            .args(args)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", &self.data_home)
            .env("XDG_DATA_HOME", &self.data_home)
            .env("XDG_CONFIG_HOME", &self.config_home)
            .env("PATH", path)
            .env("CK_RELEASE_INDEX_URL", &index.url)
            .env("CK_TEST_RELEASE_INDEX_PUBKEY", &index.public_key)
            .env("CK_TEST_SETUP_CONTROL_OK", "1")
            .env("CK_TEST_SETUP_MODULES", "aft,claustrum,insula");
        command
    }
}

struct SignedIndex {
    url: String,
    public_key: String,
}

fn serve_signed_index(
    build: impl FnOnce(&str) -> (Value, BTreeMap<String, Vec<u8>>),
) -> SignedIndex {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let (document, assets) = build(&base);
    let body = serde_json::to_vec(&document).unwrap();
    let signing = SigningKey::from_bytes(&[0x42; 32]);
    let signature =
        base64::engine::general_purpose::STANDARD.encode(signing.sign(&body).to_bytes());
    let public_key = base64::engine::general_purpose::STANDARD.encode(signing.verifying_key());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else {
                continue;
            };
            let mut request = [0_u8; 4096];
            let read = std::io::Read::read(&mut stream, &mut request).unwrap_or(0);
            let request = String::from_utf8_lossy(&request[..read]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/");
            let (response, signature_header) = if path == "/index.json" {
                (&body, Some(signature.as_str()))
            } else if let Some(asset) = assets.get(path) {
                (asset, None)
            } else {
                let response = b"not found".to_vec();
                let header = format!(
                    "HTTP/1.1 404 Not Found\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    response.len()
                );
                let _ = std::io::Write::write_all(&mut stream, header.as_bytes());
                let _ = std::io::Write::write_all(&mut stream, &response);
                continue;
            };
            let mut header = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n",
                response.len()
            );
            if let Some(signature) = signature_header {
                header.push_str(&format!("X-CortexKit-Signature-Ed25519: {signature}\r\n"));
            }
            header.push_str("\r\n");
            let _ = std::io::Write::write_all(&mut stream, header.as_bytes());
            let _ = std::io::Write::write_all(&mut stream, response);
        }
    });
    SignedIndex {
        url: format!("{base}/index.json"),
        public_key,
    }
}

fn setup_index(base: &str, mc_archive: Option<&[u8]>) -> (Value, BTreeMap<String, Vec<u8>>) {
    let target = host_target();
    let dummy = fixture_asset(format!("{base}/unused.zip"), "00".repeat(32), 1, None);
    let mut components = serde_json::Map::new();
    components.insert(
        "core".to_string(),
        index_component(
            "subc-core-v0.17.9",
            Some("0.17.9"),
            &target,
            [("ck-subc", dummy.clone()), ("ck-subc-mcp", dummy.clone())],
        ),
    );
    components.insert(
        "synapse".to_string(),
        json!({"release": "v0.1.0", "version": "0.1.0", "assets": {}}),
    );
    let mut assets = BTreeMap::new();
    if let Some(archive) = mc_archive {
        let path = "/ck-mc.zip";
        components.insert(
            "mc".to_string(),
            index_component(
                "ck-mc-alpha.22464bf2",
                None,
                &target,
                [(
                    "ck-mc",
                    fixture_asset(
                        format!("{base}{path}"),
                        sha256_hex(archive),
                        archive.len() as u64,
                        Some("ck-mc-alpha.22464bf2"),
                    ),
                )],
            ),
        );
        assets.insert(path.to_string(), archive.to_vec());
    }
    (
        json!({
            "schema": 1,
            "channel": "alpha",
            "generated_at_ms": now_ms(),
            "components": components,
        }),
        assets,
    )
}

fn index_component<const N: usize>(
    release: &str,
    version: Option<&str>,
    target: &str,
    assets: [(&str, Value); N],
) -> Value {
    let binaries = assets
        .into_iter()
        .map(|(name, asset)| (name.to_string(), asset))
        .collect::<serde_json::Map<_, _>>();
    let mut targets = serde_json::Map::new();
    targets.insert(target.to_string(), Value::Object(binaries));
    json!({
        "release": release,
        "version": version,
        "assets": targets,
    })
}

fn fixture_asset(url: String, sha256: String, bytes: u64, reports: Option<&str>) -> Value {
    json!({"url": url, "sha256": sha256, "bytes": bytes, "reports": reports})
}

fn fixture_archive(_root: &Path, binary: &str, version_line: &str, filler_bytes: u64) -> Vec<u8> {
    let script = format!("#!/bin/sh\necho '{version_line}'\n").into_bytes();
    let filler = vec![0; filler_bytes.try_into().unwrap()];
    let archived_binary = platform_binary(binary);
    stored_zip(&[
        (archived_binary.as_str(), script.as_slice()),
        ("fixture-padding", filler.as_slice()),
    ])
}

fn stored_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut archive = Vec::new();
    let mut central = Vec::new();
    for (name, bytes) in entries {
        let offset = u32::try_from(archive.len()).unwrap();
        let size = u32::try_from(bytes.len()).unwrap();
        let crc = crc32(bytes);
        push_u32(&mut archive, 0x0403_4b50);
        push_u16(&mut archive, 20);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u16(&mut archive, 0);
        push_u32(&mut archive, crc);
        push_u32(&mut archive, size);
        push_u32(&mut archive, size);
        push_u16(&mut archive, u16::try_from(name.len()).unwrap());
        push_u16(&mut archive, 0);
        archive.extend_from_slice(name.as_bytes());
        archive.extend_from_slice(bytes);

        push_u32(&mut central, 0x0201_4b50);
        push_u16(&mut central, 20);
        push_u16(&mut central, 20);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, crc);
        push_u32(&mut central, size);
        push_u32(&mut central, size);
        push_u16(&mut central, u16::try_from(name.len()).unwrap());
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u16(&mut central, 0);
        push_u32(&mut central, 0);
        push_u32(&mut central, offset);
        central.extend_from_slice(name.as_bytes());
    }
    let central_offset = u32::try_from(archive.len()).unwrap();
    let central_size = u32::try_from(central.len()).unwrap();
    archive.extend_from_slice(&central);
    push_u32(&mut archive, 0x0605_4b50);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, 0);
    push_u16(&mut archive, u16::try_from(entries.len()).unwrap());
    push_u16(&mut archive, u16::try_from(entries.len()).unwrap());
    push_u32(&mut archive, central_size);
    push_u32(&mut archive, central_offset);
    push_u16(&mut archive, 0);
    archive
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

fn push_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn push_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn service_manager_program() -> &'static str {
    if cfg!(target_os = "macos") {
        "launchctl"
    } else if cfg!(windows) {
        "schtasks.exe"
    } else {
        "systemctl"
    }
}

fn service_manager_script() -> &'static str {
    "#!/bin/sh\necho 'state = running'\nexit 0\n"
}

fn runtime_definition(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/LaunchAgents/cortexkit.subc.plist")
    } else if cfg!(windows) {
        home.join("cortexkit-subc-daemon.xml")
    } else {
        home.join(".config/systemd/user/cortexkit-subc.service")
    }
}

fn platform_binary(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

fn host_target() -> String {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "darwin-arm64".to_string(),
        ("linux", "x86_64") => "linux-x64".to_string(),
        ("linux", "aarch64") => "linux-arm64".to_string(),
        ("windows", "x86_64") => "windows-x64".to_string(),
        ("windows", "aarch64") => "windows-arm64".to_string(),
        (os, arch) => format!("{os}-{arch}"),
    }
}

fn single_row_table(headers: [&str; 3], row: [&str; 3]) -> String {
    let widths = [
        headers[0].len().max(row[0].len()),
        headers[1].len().max(row[1].len()),
        headers[2].len().max(row[2].len()),
    ];
    let render = |cells: [&str; 3]| {
        format!(
            "{:<w0$}  {:<w1$}  {:<w2$}\n",
            cells[0],
            cells[1],
            cells[2],
            w0 = widths[0],
            w1 = widths[1],
            w2 = widths[2]
        )
    };
    format!("{}{}", render(headers), render(row))
}

fn home_relative(path: &str) -> String {
    std::env::var("HOME")
        .ok()
        .and_then(|home| {
            path.strip_prefix(&home)
                .filter(|tail| tail.starts_with(std::path::MAIN_SEPARATOR))
                .map(|tail| format!("~{tail}"))
        })
        .unwrap_or_else(|| path.to_string())
}

fn connection_file_elapsed(path: &Path) -> String {
    let seconds = fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .and_then(|modified| modified.elapsed().map_err(std::io::Error::other))
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn age_from_ms(timestamp_ms: u64) -> String {
    let now = now_ms();
    if timestamp_ms > now {
        let seconds = (timestamp_ms - now) / 1_000;
        return if seconds == 0 {
            "just now".to_string()
        } else {
            format!("in {}", compact_duration(seconds))
        };
    }
    let seconds = (now - timestamp_ms) / 1_000;
    if seconds == 0 {
        "just now".to_string()
    } else {
        format!("{} ago", compact_duration(seconds))
    }
}

fn compact_duration(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m", seconds / 60)
    } else if seconds < 86_400 {
        format!("{}h", seconds / 3_600)
    } else {
        format!("{}d", seconds / 86_400)
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn setup_nothing_to_do_never_renders_outcome_no_op() {
    let fixture = SetupFixture::installed("ck-setup-current");
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture.command(&index, &["setup"]).output().unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "CortexKit is set up: daemon running, aft · claustrum · insula ok.\nOptional modules not installed: mc, synapse — run `ck setup mc` or `ck setup synapse`.\n"
    );
}

#[test]
fn setup_mc_apply_reports_each_completed_change_without_a_plan() {
    let fixture = SetupFixture::installed("ck-setup-mc-apply");
    let archive = fixture_archive(
        fixture._root.path(),
        "ck-mc",
        "ck-mc ck-mc-alpha.22464bf2",
        12 * 1024 * 1024 + 102 * 1024,
    );
    let index = serve_signed_index(|base| setup_index(base, Some(&archive)));
    let output = fixture.command(&index, &["setup", "mc"]).output().unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "Installing magic-context (ck-mc-alpha.22464bf2, {})\n  downloaded and verified ck-mc (12.1 MiB)\n  placed ~/.local/share/cortexkit/bin/{}\n  configured magic-context in ~/.config/cortexkit/subc.jsonc\n  registered with the daemon; magic-context ok\nDone.\n",
            host_target(),
            platform_binary("ck-mc")
        )
    );
    let stdout = text(&output.stdout);
    assert!(!stdout.contains("setup plan:"), "stdout:\n{stdout}");
    assert!(
        !stdout.contains("proposed configuration diff:"),
        "stdout:\n{stdout}"
    );
}

#[test]
fn setup_mc_dry_run_has_future_steps_and_the_config_diff() {
    let fixture = SetupFixture::installed("ck-setup-mc-dry-run");
    let archive = fixture_archive(
        fixture._root.path(),
        "ck-mc",
        "ck-mc ck-mc-alpha.22464bf2",
        12 * 1024 * 1024 + 102 * 1024,
    );
    let index = serve_signed_index(|base| setup_index(base, Some(&archive)));
    let output = fixture
        .command(&index, &["setup", "mc", "--dry-run"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    for line in [
        "  1. would download and verify ck-mc (12.1 MiB)".to_string(),
        format!(
            "  2. would place ~/.local/share/cortexkit/bin/{}",
            platform_binary("ck-mc")
        ),
        "  3. would configure magic-context in ~/.config/cortexkit/subc.jsonc".to_string(),
        "  4. would register magic-context with the daemon and check its health".to_string(),
        "proposed configuration diff:".to_string(),
        "+    \"magic-context\": {".to_string(),
    ] {
        assert!(
            stdout.contains(&line),
            "missing {line:?} in stdout:\n{stdout}"
        );
    }
    assert!(
        !stdout.contains("observe alpha platform support"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("validate with ck daemon triage"),
        "{stdout}"
    );
}

#[test]
fn setup_synapse_without_a_host_asset_is_one_complete_refusal_line() {
    let fixture = SetupFixture::installed("ck-setup-synapse-unavailable");
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture
        .command(&index, &["setup", "synapse"])
        .output()
        .unwrap();

    assert_exit(&output, 1);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        format!(
            "synapse has no {} release yet; nothing was installed.\n",
            host_target()
        )
    );
}

#[test]
fn setup_verbose_keeps_every_preexisting_outcome_line() {
    let fixture = SetupFixture::installed("ck-setup-verbose");
    let index = serve_signed_index(|base| setup_index(base, None));
    let output = fixture
        .command(&index, &["setup", "--verbose"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    for outcome in [
        "  outcome: no-op: core is already correct",
        "  outcome: no-op: per-user runtime is already registered and live",
    ] {
        assert!(stdout.contains(outcome), "missing {outcome:?}:\n{stdout}");
    }
}

#[test]
fn fresh_setup_prints_the_pasteable_claude_code_command() {
    let fixture = SetupFixture::fresh("ck-setup-fresh-next");
    let subc = fixture_archive(fixture._root.path(), "ck-subc", "ck-subc 0.17.10", 0);
    let mcp = fixture_archive(
        fixture._root.path(),
        "ck-subc-mcp",
        "ck-subc-mcp 0.17.10",
        0,
    );
    let index = serve_signed_index(|base| {
        let target = host_target();
        let mut assets = BTreeMap::new();
        assets.insert("/ck-subc.zip".to_string(), subc.clone());
        assets.insert("/ck-subc-mcp.zip".to_string(), mcp.clone());
        (
            json!({
                "schema": 1,
                "channel": "alpha",
                "generated_at_ms": now_ms(),
                "components": {
                    "core": index_component(
                        "subc-core-v0.17.10",
                        Some("0.17.10"),
                        &target,
                        [
                            ("ck-subc", fixture_asset(format!("{base}/ck-subc.zip"), sha256_hex(&subc), subc.len() as u64, Some("0.17.10"))),
                            ("ck-subc-mcp", fixture_asset(format!("{base}/ck-subc-mcp.zip"), sha256_hex(&mcp), mcp.len() as u64, Some("0.17.10"))),
                        ],
                    ),
                },
            }),
            assets,
        )
    });
    let output = fixture.command(&index, &["setup"]).output().unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.ends_with(
            "next: connect your agent — Claude Code: claude mcp add ck -- ck-subc-mcp shim --harness claude-code\n      other harnesses: https://github.com/cortexkit/subconscious#readme\n"
        ),
        "stdout:\n{stdout}"
    );
}

struct UpgradeFixture {
    _root: TempDir,
    home: PathBuf,
    connection_file: PathBuf,
}

impl UpgradeFixture {
    fn new(name: &str) -> Self {
        let root = TempDir::new(name);
        let home = root.path().join("home");
        let cortexkit = home.join(".local/share/cortexkit");
        let bin = cortexkit.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let aft = bin.join(platform_binary("ck-aft"));
        let daemon = bin.join(platform_binary("ck-subc"));
        let mcp = bin.join(platform_binary("ck-subc-mcp"));
        write_executable(&aft, "#!/bin/sh\necho 'ck-aft 0.55.1'\n");
        write_executable(&daemon, "#!/bin/sh\necho 'ck-subc 0.17.9'\n");
        write_executable(&mcp, "#!/bin/sh\necho 'ck-subc-mcp 0.17.9'\n");
        let ck = fs::canonicalize(env!("CARGO_BIN_EXE_ck")).unwrap();
        let mutations = [
            (ck, "44".repeat(32)),
            (daemon, "33".repeat(32)),
            (mcp, "11".repeat(32)),
            (aft, "22".repeat(32)),
        ]
        .into_iter()
        .map(|(path, archive_sha256)| {
            json!({
                "kind": "managed-binary",
                "path": path,
                "archive_sha256": archive_sha256,
            })
        })
        .collect::<Vec<_>>();
        fs::write(
            cortexkit.join("installer-manifest.json"),
            serde_json::to_vec_pretty(&json!({
                "schema_version": 1,
                "platform": host_target(),
                "mutations": mutations,
            }))
            .unwrap(),
        )
        .unwrap();

        let connection_file = root.path().join("subc-connection.json");
        write_atomic(
            &connection_file,
            &ConnectionInfo {
                schema: SCHEMA_VERSION,
                wire_version: Some(PROTOCOL_VERSION),
                endpoints: vec![Endpoint {
                    host: "127.0.0.1".to_string(),
                    port: 9,
                }],
                key: generate_key().unwrap(),
                daemon_id: generate_daemon_id().unwrap(),
                pid: std::process::id(),
                daemon_ver: "0.17.9".to_string(),
            },
        )
        .unwrap();
        Self {
            _root: root,
            home,
            connection_file,
        }
    }

    fn command(&self, index: &SignedIndex, args: &[&str]) -> Command {
        let mut command = ck_command();
        command
            .args(args)
            .arg("--subc")
            .arg(&self.connection_file)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("LOCALAPPDATA", self.home.join(".local/share"))
            .env_remove("XDG_DATA_HOME")
            .env("CK_RELEASE_INDEX_URL", &index.url)
            .env("CK_TEST_RELEASE_INDEX_PUBKEY", &index.public_key)
            .env("CK_TEST_CK_VERSION", "0.17.9")
            .env("CK_TEST_SUBC_MCP_VERSION", "0.17.9")
            .env("CK_TEST_AFT_VERSION", "0.55.1")
            .env(
                "CK_UPDATE_CACHE_PATH",
                self._root.path().join("update-metadata.json"),
            );
        command
    }
}

fn upgrade_index(
    base: &str,
    aft_version: &str,
    aft_digest: &str,
    ck_version: &str,
    ck_digest: &str,
) -> (Value, BTreeMap<String, Vec<u8>>) {
    let target = host_target();
    (
        json!({
            "schema": 1,
            "channel": "alpha",
            "generated_at_ms": now_ms(),
            "components": {
                "core": index_component(
                    "subc-core-v0.17.10",
                    Some(ck_version),
                    &target,
                    [
                        ("ck-subc-mcp", fixture_asset(format!("{base}/mcp.zip"), "11".repeat(32), 1, Some("0.17.9"))),
                        ("ck-subc", fixture_asset(format!("{base}/daemon.zip"), "33".repeat(32), 1, Some("0.17.9"))),
                        ("ck", fixture_asset(format!("{base}/ck.zip"), ck_digest.to_string(), 1, Some(ck_version))),
                    ],
                ),
                "aft": index_component(
                    "v0.55.2",
                    Some(aft_version),
                    &target,
                    [(
                        "ck-aft",
                        fixture_asset(format!("{base}/aft.zip"), aft_digest.to_string(), 1, Some(aft_version)),
                    )],
                ),
            },
        }),
        BTreeMap::new(),
    )
}

#[test]
fn upgrade_and_check_say_everything_is_current_in_one_line() {
    let fixture = UpgradeFixture::new("ck-upgrade-current");
    let index = serve_signed_index(|base| {
        upgrade_index(base, "0.55.1", &"22".repeat(32), "0.17.9", &"44".repeat(32))
    });
    let expected =
        "Everything is up to date (ck 0.17.9 · ck-subc 0.17.9 · ck-subc-mcp · ck-aft 0.55.1).\n";

    for args in [&["upgrade"][..], &["upgrade", "--check"][..]] {
        let output = fixture.command(&index, args).output().unwrap();
        assert_exit(&output, 0);
        assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
        assert_eq!(text(&output.stdout), expected);
    }
}

#[test]
fn upgrade_check_names_the_one_available_update_and_command() {
    let fixture = UpgradeFixture::new("ck-upgrade-check");
    let index = serve_signed_index(|base| {
        upgrade_index(base, "0.55.2", &"aa".repeat(32), "0.17.9", &"44".repeat(32))
    });
    let output = fixture
        .command(&index, &["upgrade", "--check"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "ck-aft 0.55.1 → 0.55.2. Run ck upgrade.\n"
    );
}

#[test]
fn upgrade_apply_reports_completed_targets_then_done() {
    let fixture = UpgradeFixture::new("ck-upgrade-apply");
    let index = serve_signed_index(|base| {
        upgrade_index(
            base,
            "0.55.2",
            &"aa".repeat(32),
            "0.17.10",
            &"bb".repeat(32),
        )
    });
    let output = fixture
        .command(&index, &["upgrade"])
        .env("CK_TEST_UPGRADE_APPLY_OK", "1")
        .output()
        .unwrap();

    assert_exit(&output, 0);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert_eq!(
        text(&output.stdout),
        "upgraded ck-aft 0.55.1 → 0.55.2, restarted\nupgraded ck 0.17.9 → 0.17.10\nDone.\n"
    );
}

#[test]
fn upgrade_verbose_keeps_every_preexisting_outcome_line() {
    let fixture = UpgradeFixture::new("ck-upgrade-verbose");
    let index = serve_signed_index(|base| {
        upgrade_index(base, "0.55.1", &"22".repeat(32), "0.17.9", &"44".repeat(32))
    });
    let output = fixture
        .command(&index, &["upgrade", "--verbose"])
        .output()
        .unwrap();

    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    for outcome in [
        "  outcome: no-op: ck-subc-mcp is already current",
        "  outcome: no-op: ck-aft is already current",
        "  outcome: no-op: ck-subc is already current",
        "  outcome: no-op: ck is already current",
    ] {
        assert!(stdout.contains(outcome), "missing {outcome:?}:\n{stdout}");
    }
}

#[test]
fn daemon_lint_uses_its_explicit_config_without_a_daemon_connection() {
    let temp = TempDir::new("ck-daemon-lint");
    let config = temp.path().join("subc.jsonc");
    fs::write(&config, r#"{"version":1,"modules":{}}"#).unwrap();

    let output = ck_command()
        .args(["daemon", "lint"])
        .arg(&config)
        .output()
        .unwrap();

    assert_exit(&output, 2);
    assert!(output.stderr.is_empty(), "stderr: {}", text(&output.stderr));
    assert!(
        text(&output.stdout).contains("checked 0 of 0 configured modules"),
        "stdout: {}",
        text(&output.stdout)
    );
}

#[cfg(unix)]
#[test]
fn external_domains_opt_in_dispatch_and_cache_their_probe() {
    use std::os::unix::fs::PermissionsExt;

    let temp = TempDir::new("ck-domain-probes");
    let bin = temp.path().join("bin");
    fs::create_dir_all(&bin).expect("create fake PATH");
    let count = temp.path().join("yes-probe-count");
    let cache = temp.path().join("update-metadata.json");
    let write_program = |name: &str, body: &str| {
        let path = bin.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write fake domain");
        let mut permissions = fs::metadata(&path).expect("metadata").permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(path, permissions).expect("mark fake domain executable");
    };
    write_program(
        "ck-yes",
        "if [ \"$1\" = \"--ck-domain\" ]; then printf x >> \"$CK_DOMAIN_PROBE_COUNT\"; echo \"helpful fake domain\"; exit 0; fi\necho dispatched-yes",
    );
    write_program("ck-no", "exit 1");
    write_program("ck-hang", "sleep 3");
    write_program("ck-aft", "exit 1");
    write_program("ck-mc", "exit 1");

    let started = std::time::Instant::now();
    let help = ck_command()
        .arg("--help")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("run help");
    assert_exit(&help, 0);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "a hanging probe must stop at its two-second deadline"
    );
    let help_text = text(&help.stdout);
    assert!(
        help_text.contains("yes       helpful fake domain"),
        "approved domain is listed with its headline:\n{help_text}"
    );
    assert!(
        !help_text.contains("\n  no "),
        "refusing binary leaked into help:\n{help_text}"
    );
    assert!(
        !help_text.contains("\n  hang "),
        "hanging binary leaked into help:\n{help_text}"
    );
    assert!(
        !help_text.contains("\n  aft "),
        "module binary leaked into help:\n{help_text}"
    );
    assert_eq!(fs::read_to_string(&count).expect("probe count"), "x");

    let second_help = ck_command()
        .arg("--help")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("run cached help");
    assert_exit(&second_help, 0);
    assert_eq!(
        fs::read_to_string(&count).expect("cached probe count"),
        "x",
        "a matching executable stamp must skip the probe"
    );

    let refusing = ck_command()
        .arg("no")
        .env("PATH", &bin)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject unapproved domain");
    assert_exit(&refusing, 1);
    assert!(
        refusing.stdout.is_empty(),
        "unapproved domain was dispatched"
    );

    let dispatched = ck_command()
        .arg("yes")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("dispatch approved domain");
    assert_exit(&dispatched, 0);
    assert_eq!(text(&dispatched.stdout).trim(), "dispatched-yes");

    fs::write(
        bin.join("ck-yes"),
        "#!/bin/sh\nif [ \"$1\" = \"--ck-domain\" ]; then printf x >> \"$CK_DOMAIN_PROBE_COUNT\"; echo \"helpful fake domain\"; exit 0; fi\necho dispatched-yes\n# changed\n",
    )
    .expect("change fake domain");
    let refreshed = ck_command()
        .arg("--help")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("run changed help");
    assert_exit(&refreshed, 0);
    assert_eq!(
        fs::read_to_string(&count).expect("changed probe count"),
        "xx",
        "a changed binary must be probed again"
    );

    let module = ck_command()
        .arg("aft")
        .env("PATH", &bin)
        .env("CK_DOMAIN_PROBE_COUNT", &count)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject module binary");
    assert_exit(&module, 1);
    assert_eq!(
        text(&module.stderr).trim(),
        "'aft' is a module, not a command. Try: ck module status aft"
    );

    let mc = ck_command()
        .arg("mc")
        .env("PATH", &bin)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject magic-context module binary");
    assert_exit(&mc, 1);
    assert_eq!(
        text(&mc.stderr).trim(),
        "'mc' is a module, not a command. Try: ck module status magic-context"
    );

    let unknown = ck_command()
        .arg("nosuch")
        .env("PATH", &bin)
        .env("CK_UPDATE_CACHE_PATH", &cache)
        .output()
        .expect("reject unknown command");
    assert_exit(&unknown, 1);
    assert_eq!(
        text(&unknown.stderr).trim(),
        "unknown command 'nosuch'. Run ck --help."
    );
    assert!(!text(&unknown.stderr).contains("usage:"));
}

#[test]
fn mc_hint_names_the_daemon_module_id() {
    let output = ck_command().arg("mc").output().unwrap();
    assert_exit(&output, 1);
    assert_eq!(
        text(&output.stderr),
        "'mc' is a module, not a command. Try: ck module status magic-context\n"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_json_uses_subc_override_and_shows_stub() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ck-list-stub";
    let module = spawn_stub(&server, &supervisor, module_id).await;
    let hidden_runtime = TempDir::new("ck-hidden-runtime");
    let hidden_home = TempDir::new("ck-hidden-home");
    let hidden_tmp = TempDir::new("ck-hidden-tmp");

    let output = ck_command()
        .args(["module", "list", "--json", "--subc"])
        .arg(&server.connection_file_path)
        .env("XDG_RUNTIME_DIR", hidden_runtime.path())
        .env("HOME", hidden_home.path())
        .env("TMPDIR", hidden_tmp.path())
        .env("TMP", hidden_tmp.path())
        .env("TEMP", hidden_tmp.path())
        .output()
        .unwrap();
    let json_stdout = text(&output.stdout);
    let value = assert_json_success(output);
    let modules = value["modules"].as_array().unwrap();
    assert!(
        modules
            .iter()
            .any(|module| module["module_id"] == module_id),
        "ck module list --json should include the supervised stub: {value}"
    );

    let text_output = ck_with_subc(&server.connection_file_path, ["module", "list"]);
    assert_exit(&text_output, 0);
    let text_stdout = text(&text_output.stdout);
    assert_eq!(
        text_stdout,
        "module        status   health \nck-list-stub  running  unknown\n"
    );
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer: {json_stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_renders_status_words_not_wire_booleans() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "insula",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "insula",
        SupervisorHealthStatus::Degraded,
    )
    .await;

    let output = ck_with_subc(&server.connection_file_path, ["module", "list"]);
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        "module  status   health  \ninsula  running  degraded\n"
    );
    assert!(!text(&output.stdout).contains("true"));
    assert!(!text(&output.stdout).contains("false"));

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_status_renders_key_value_block_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "aft",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "aft",
        SupervisorHealthStatus::Degraded,
    )
    .await;

    let status_json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "aft", "--json"],
    ));
    assert_eq!(status_json["module"]["module_id"], "aft");
    assert_eq!(status_json["health"]["status"], "degraded");

    let provenance = control_rpc_value_on_stream_within(
        &mut wait_for_client(&server.connection_file_path).await,
        92,
        ClientControlRequest::SupervisorProvenance {
            module_id: Some("aft".to_string()),
        },
        Duration::from_secs(120),
    )
    .await;
    let observed = &provenance["modules"][0]["daemon_observed"];
    let pid = observed["pid"].as_u64().unwrap();
    let started = age_from_ms(observed["spawned_at_ms"].as_u64().unwrap());
    let binary = home_relative(observed["spawned_from"].as_str().unwrap());
    let image = match observed["running_image"]["status"].as_str() {
        Some("match") => "running image matches".to_string(),
        Some("mismatch") => "running image differs: running vs disk".to_string(),
        Some("unavailable") => format!(
            "running image differs: {}",
            observed["running_image"]["reason"]
                .as_str()
                .unwrap_or("none")
        ),
        status => format!("running image status {}", status.unwrap_or("unknown")),
    };

    let output = ck_with_subc(&server.connection_file_path, ["module", "status", "aft"]);
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        format!(
            "aft — running, degraded\n  pid {pid} · started {started} · restarts 0 of 1\n  last exit: none\n  binary: {binary} ({image})\nmetrics: run `ck health aft`\n"
        )
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_terminals_renders_empty_history_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module = spawn_stub(&server, &supervisor, "aft").await;
    let json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "terminals", "aft", "--json"],
    ));
    let started = age_from_ms(json["daemon_started_at_ms"].as_u64().unwrap());

    let output = ck_with_subc(&server.connection_file_path, ["module", "terminals", "aft"]);
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        format!("no exits recorded since the daemon started ({started})\n")
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_terminals_renders_one_record_byte_for_byte() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_restart_limit(&server, 0);
    let module = supervisor
        .spawn(stub_spec_with_env("aft", vec![("FAKE_AFT_EXIT_CODE", "7")]))
        .unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, "aft", |entry| {
        entry.state == "failed" && !entry.live
    })
    .await;
    let json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "terminals", "aft", "--json"],
    ));
    let entry = &json["entries"][0];
    assert_eq!(json["entries"].as_array().unwrap().len(), 1);
    let when = age_from_ms(entry["at_ms"].as_u64().unwrap());
    let disposition = entry["disposition"]
        .as_str()
        .unwrap()
        .replace(['_', '-'], " ");

    let output = ck_with_subc(&server.connection_file_path, ["module", "terminals", "aft"]);
    assert_exit(&output, 0);
    assert_eq!(
        text(&output.stdout),
        single_row_table(
            ["when", "exit", "disposition"],
            [&when, "exit 7", &disposition]
        )
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_restart_stop_start_json_drive_supervisor() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "ck-control-stub";
    let module = spawn_stub(&server, &supervisor, module_id).await;

    let restart = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "restart", module_id, "--json"],
    ));
    assert_eq!(restart["module_id"], module_id);
    assert_eq!(restart["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;

    let stop = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "stop", module_id, "--json"],
    ));
    assert_eq!(stop["module_id"], module_id);
    assert_eq!(stop["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "disabled" && !entry.enabled && !entry.live
    })
    .await;

    let start = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["module", "start", module_id, "--json"],
    ));
    assert_eq!(start["module_id"], module_id);
    assert_eq!(start["applied"], true);
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_list_empty_result_has_a_next_step_and_json_has_no_footer() {
    let server = TestServer::start().await;

    let output = ck_with_subc(&server.connection_file_path, ["module", "list"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("no supervised modules"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("next:"), "stdout:\n{stdout}");
    assert!(stdout.contains("ck module rescan"), "stdout:\n{stdout}");

    let json_output = ck_with_subc(&server.connection_file_path, ["module", "list", "--json"]);
    let json_stdout = text(&json_output.stdout);
    let _ = assert_json_success(json_output);
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn routes_empty_result_is_one_line_and_json_has_no_footer() {
    let server = TestServer::start().await;

    let output = ck_with_subc(&server.connection_file_path, ["routes"]);
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "no live routes\n");

    let json_output = ck_with_subc(&server.connection_file_path, ["routes", "--json"]);
    let json_stdout = text(&json_output.stdout);
    let _ = assert_json_success(json_output);
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer"
    );
}

#[test]
fn provenance_requires_exactly_one_module_id_without_connecting() {
    for args in [
        vec!["provenance"],
        vec!["provenance", "--help"],
        vec!["provenance", "aft", "extra"],
    ] {
        let output = ck_command().args(args).output().unwrap();
        assert_exit(&output, 0);
        let stdout = text(&output.stdout);
        assert!(stdout.contains("ck provenance"), "stdout:\n{stdout}");
        assert!(stdout.contains("usage:"), "stdout:\n{stdout}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_json_preserves_the_complete_daemon_response_without_a_footer() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "aft",
        vec![
            ("FAKE_AFT_BUILD_COMMIT", "declared-commit"),
            ("FAKE_AFT_BUILD_LOCK_DIGEST", "declared-lock"),
            ("FAKE_AFT_WIRE_CRATE_VERSION", "declared-wire"),
            ("FAKE_AFT_STORE_SCHEMA_VERSION", "declared-schema"),
        ],
    )
    .await;

    // First provenance evaluation pays the linux double-hash of the stub
    // binary; see control_rpc_value_on_stream_within for why 10s is not enough
    // on a contended CI disk.
    let expected = control_rpc_value_on_stream_within(
        &mut wait_for_client(&server.connection_file_path).await,
        91,
        ClientControlRequest::SupervisorProvenance {
            module_id: Some("aft".to_string()),
        },
        Duration::from_secs(120),
    )
    .await;
    let output = ck_with_subc(
        &server.connection_file_path,
        ["--json", "provenance", "aft"],
    );
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let actual: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(actual, expected);
    assert!(stdout.contains("\"module_declared\""), "stdout:\n{stdout}");
    assert!(stdout.contains("\"daemon_observed\""), "stdout:\n{stdout}");
    assert!(!stdout.contains("next:"), "stdout:\n{stdout}");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_human_output_keeps_declared_values_under_the_declared_label() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "aft",
        vec![
            ("FAKE_AFT_BUILD_COMMIT", "declared-dirty-dirty"),
            ("FAKE_AFT_BUILD_LOCK_DIGEST", "declared-lock"),
            ("FAKE_AFT_WIRE_CRATE_VERSION", "declared-wire"),
            ("FAKE_AFT_STORE_SCHEMA_VERSION", "declared-schema"),
        ],
    )
    .await;

    let output = ck_with_subc(&server.connection_file_path, ["provenance", "aft"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    let declared_at = stdout
        .find("Module declared")
        .unwrap_or_else(|| panic!("stdout:\n{stdout}"));
    let observed_at = stdout
        .rfind("Daemon observed")
        .unwrap_or_else(|| panic!("stdout:\n{stdout}"));
    assert!(declared_at < observed_at, "stdout:\n{stdout}");
    let module_declared = &stdout[declared_at..observed_at];
    let module_observed = &stdout[observed_at..];
    assert!(
        module_observed.starts_with("Daemon observed\n  pid:"),
        "module-level observed section boundary was not verified:\n{module_observed}"
    );
    for declared in [
        "declared-dirty-dirty",
        "declared-lock",
        "declared-wire",
        "declared-schema",
    ] {
        let value_at = module_declared
            .find(declared)
            .unwrap_or_else(|| panic!("missing {declared:?} in:\n{stdout}"));
        assert!(
            value_at < module_declared.len(),
            "declared value {declared:?} escaped its source section:\n{stdout}"
        );
        assert!(
            !module_observed.contains(declared),
            "declared value {declared:?} leaked into module-level observed section:\n{module_observed}"
        );
    }
    assert!(stdout.contains("Daemon build"), "stdout:\n{stdout}");
    assert_eq!(
        stdout.matches("Daemon build").count(),
        1,
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("Module: aft"), "stdout:\n{stdout}");
    assert!(stdout.contains("pid:"), "stdout:\n{stdout}");
    assert!(stdout.contains("started "), "stdout:\n{stdout}");
    assert!(stdout.contains("spawned from:"), "stdout:\n{stdout}");
    assert!(stdout.contains("running image "), "stdout:\n{stdout}");
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    assert!(
        stdout.contains("running image matches the file it was spawned from"),
        "stdout:\n{stdout}"
    );
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    assert!(
        stdout.contains("running image could not be compared"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("commit match only"), "stdout:\n{stdout}");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_hostile_declared_values_are_refused_before_rendering() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let _module = supervisor
        .spawn(stub_spec_with_env(
            "aft",
            vec![
                ("FAKE_AFT_BUILD_COMMIT", "\u{1b}]52;c;AAAA\u{07}"),
                ("FAKE_AFT_BUILD_LOCK_DIGEST", "\u{1b}[2J"),
            ],
        ))
        .unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, "aft", |entry| {
        entry.state == "failed" && !entry.live
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_renders_unverifiable_and_lock_only_declarations() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let unverifiable = spawn_stub(&server, &supervisor, "unverifiable").await;
    let lock_only = spawn_stub_with_env(
        &server,
        &supervisor,
        "lock-only",
        vec![("FAKE_AFT_BUILD_LOCK_DIGEST", "declared-lock-only")],
    )
    .await;

    let absent = ck_with_subc(&server.connection_file_path, ["provenance", "unverifiable"]);
    assert_exit(&absent, 0);
    assert!(
        text(&absent.stdout).contains("unverifiable"),
        "stdout:\n{}",
        text(&absent.stdout)
    );

    let lock_only_output = ck_with_subc(&server.connection_file_path, ["provenance", "lock-only"]);
    assert_exit(&lock_only_output, 0);
    assert!(
        text(&lock_only_output.stdout).contains("change-detectable; commit identity unavailable"),
        "stdout:\n{}",
        text(&lock_only_output.stdout)
    );

    unverifiable.stop().await.unwrap();
    lock_only.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn provenance_unknown_module_preserves_the_daemon_error_and_exit_code() {
    let server = TestServer::start().await;

    let output = ck_with_subc(
        &server.connection_file_path,
        ["provenance", "missing-module"],
    );
    assert_exit(&output, 1);
    let stderr = text(&output.stderr);
    assert!(stderr.contains("unknown_module"), "stderr:\n{stderr}");
    assert!(stderr.contains("missing-module"), "stderr:\n{stderr}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn module_status_and_health_unknown_module_errors_are_one_line() {
    let server = TestServer::start().await;

    let output = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "missing-module"],
    );
    assert_exit(&output, 1);
    assert_eq!(
        text(&output.stderr),
        "no module named 'missing-module'. Run ck module list.\n"
    );

    let health = ck_with_subc(&server.connection_file_path, ["health", "missing-module"]);
    assert_exit(&health, 1);
    assert_eq!(
        text(&health.stderr),
        "no module named 'missing-module'. Run ck module list.\n"
    );

    let json_output = ck_with_subc(
        &server.connection_file_path,
        ["module", "status", "missing-module", "--json"],
    );
    let json_stderr = text(&json_output.stderr);
    assert_exit(&json_output, 1);
    assert!(
        !json_stderr.contains("next:"),
        "JSON error gained footer: {json_stderr}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn health_summarizes_provider_state_and_headline_metrics() {
    let server = TestServer::start().await;
    let supervisor = supervisor_with_fast_health(&server);
    let unconfigured = (0..36)
        .map(|index| format!("provider-{index}"))
        .collect::<Vec<_>>();
    let metrics = serde_json::json!({
        "degraded": ["antigravity"],
        "fetchBlackout": false,
        "lastTickAgeSecs": 2,
        "unconfigured": unconfigured,
    })
    .to_string();
    let module = spawn_stub_with_env(
        &server,
        &supervisor,
        "insula",
        vec![
            ("FAKE_AFT_ADVERTISE_HEALTH", "1"),
            ("FAKE_AFT_HEALTH_STATUS", "degraded"),
            ("FAKE_AFT_HEALTH_METRICS", metrics.as_str()),
        ],
    )
    .await;
    wait_for_health_status(
        &server.connection_file_path,
        "insula",
        SupervisorHealthStatus::Degraded,
    )
    .await;

    let overview_json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["health", "--json"],
    ));
    assert_eq!(overview_json["modules"][0]["module_id"], "insula");
    let detail_json = assert_json_success(ck_with_subc(
        &server.connection_file_path,
        ["health", "insula", "--json"],
    ));
    assert_eq!(detail_json["metrics"]["lastTickAgeSecs"], 2);

    let overview = ck_with_subc(&server.connection_file_path, ["health"]);
    assert_exit(&overview, 0);
    assert_eq!(
        text(&overview.stdout),
        "insula  degraded  1 provider degraded (antigravity); 36 not configured\n"
    );

    let detail = ck_with_subc(&server.connection_file_path, ["health", "insula"]);
    assert_exit(&detail, 0);
    assert_eq!(
        text(&detail.stdout),
        "insula: degraded\n  1 provider degraded (antigravity); 36 not configured\n  fetchBlackout: disabled\n  lastTickAgeSecs: 2s ago\n"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_empty_result_has_a_next_step_and_json_has_no_footer() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let fixture = serde_json::json!([]);
    let module = spawn_quota_stub(&server, &supervisor, "insula", &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(
        stdout.contains("no providers reported"),
        "stdout:\n{stdout}"
    );
    assert!(stdout.contains("next:"), "stdout:\n{stdout}");
    assert!(
        stdout.contains("ck module status <module-id> --subc <connection-file>"),
        "stdout:\n{stdout}"
    );

    let json_output = ck_with_subc(&server.connection_file_path, ["quota", "--json"]);
    let json_stdout = text(&json_output.stdout);
    let _ = assert_json_success(json_output);
    assert!(
        !json_stdout.contains("next:"),
        "JSON output must not gain human footer"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_reports_local_provider_not_running_in_one_line() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let fixture = serde_json::json!([{
        "provider": "antigravity",
        "account": "local",
        "errorClass": "local_source_unavailable",
        "error": "no Antigravity language server or agy CLI process running"
    }]);
    let module = spawn_quota_stub(&server, &supervisor, "insula", &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota"]);
    assert_exit(&output, 0);
    assert_eq!(text(&output.stdout), "Antigravity — not running locally\n");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_table_renders_providers_and_used_percent() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("Anthropic"), "stdout:\n{stdout}");
    assert!(stdout.contains("Openai"), "stdout:\n{stdout}");
    assert!(stdout.contains("work"), "account label, stdout:\n{stdout}");
    // The breakdown layout renders utilization as "42% used" in the window
    // detail string beside the progress bar.
    assert!(
        stdout.contains("42% used"),
        "expected usedPercent 42.0 in window details, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("Bonus credits"),
        "extraRateWindows title should appear, stdout:\n{stdout}"
    );
    // The default view lists connected providers only; entries without a
    // usage object collapse into the summary line.
    assert!(
        stdout.contains("1 providers not configured"),
        "not-connected summary should appear, stdout:\n{stdout}"
    );
    assert!(
        !stdout.contains("cookie jar unreadable"),
        "error-only entry must stay out of the default view, stdout:\n{stdout}"
    );

    let verbose = ck_with_subc(&server.connection_file_path, ["quota", "--verbose"]);
    assert_exit(&verbose, 0);
    let verbose_stdout = text(&verbose.stdout);
    assert!(
        verbose_stdout.contains("rate limit probe failed for secondary account"),
        "degraded error should appear under --verbose, stdout:\n{verbose_stdout}"
    );
    assert!(
        verbose_stdout.contains("cookie jar unreadable"),
        "error-only entry should appear under --verbose, stdout:\n{verbose_stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_filters_by_provider_id() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota", "anthropic"]);
    assert_exit(&output, 0);
    let stdout = text(&output.stdout);
    assert!(stdout.contains("Anthropic"), "stdout:\n{stdout}");
    // Provider sections are headed by the display name; the filtered view
    // must not contain the other provider's section at all.
    assert!(
        !stdout.contains("Openai"),
        "filtered view leaked openai section, stdout:\n{stdout}"
    );
    assert!(
        stdout.contains("42% used"),
        "expected usedPercent 42.0, stdout:\n{stdout}"
    );

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_unknown_provider_lists_valid_ids_and_exits_nonzero() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(
        &server.connection_file_path,
        ["quota", "unknown-id", "--verbose"],
    );
    assert_exit(&output, 1);
    let stderr = text(&output.stderr);
    assert!(
        stderr.contains("unknown provider 'unknown-id'"),
        "stderr:\n{stderr}"
    );
    assert!(stderr.contains("anthropic"), "stderr:\n{stderr}");
    assert!(stderr.contains("openai"), "stderr:\n{stderr}");
    assert!(stderr.contains("grok"), "stderr:\n{stderr}");
    assert!(stderr.contains("next:"), "stderr:\n{stderr}");
    assert!(stderr.contains("ck quota --verbose"), "stderr:\n{stderr}");

    module.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn quota_json_emits_wrapped_reply_verbatim() {
    let server = TestServer::start().await;
    let supervisor = supervisor(&server);
    let module_id = "insula";
    let fixture = quota_wire_fixture();
    let module = spawn_quota_stub(&server, &supervisor, module_id, &fixture).await;

    let output = ck_with_subc(&server.connection_file_path, ["quota", "--json"]);
    let value = assert_json_success(output);
    let result = value["result"].as_array().expect("result array");
    assert_eq!(result.len(), 3);
    assert_eq!(result[0]["provider"], "anthropic");
    assert!(result[0]["usage"]["primary"]["usedPercent"]
        .as_f64()
        .is_some());
    assert_eq!(result[2]["error"].as_str(), Some("cookie jar unreadable"));

    module.stop().await.unwrap();
}

#[test]
fn discovery_failure_lists_tried_paths_and_exits_2() {
    let runtime = TempDir::new("ck-empty-runtime");
    let home = TempDir::new("ck-empty-home");
    let tmp = TempDir::new("ck-empty-tmp");

    let output = ck_command()
        .args(["module", "list", "--json"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("HOME", home.path())
        .env("TMPDIR", tmp.path())
        .env("TMP", tmp.path())
        .env("TEMP", tmp.path())
        .output()
        .unwrap();

    assert_exit(&output, 2);
    assert!(
        output.stdout.is_empty(),
        "discovery failures must not write data to stdout: {}",
        text(&output.stdout)
    );
    let stderr = text(&output.stderr);
    assert_eq!(
        stderr.lines().count(),
        1,
        "stderr should be one line: {stderr}"
    );
    assert!(stderr.contains("no usable subc connection file found"));
    assert!(stderr.contains(
        &runtime
            .path()
            .join("subc-connection.json")
            .display()
            .to_string()
    ));
    assert!(stderr.contains(&home_connection_file(home.path()).display().to_string()));
    assert!(stderr.contains(&tmp.path().display().to_string()));

    let text_output = ck_command()
        .args(["module", "list"])
        .env("XDG_RUNTIME_DIR", runtime.path())
        .env("HOME", home.path())
        .env("TMPDIR", tmp.path())
        .env("TMP", tmp.path())
        .env("TEMP", tmp.path())
        .output()
        .unwrap();
    assert_exit(&text_output, 2);
    let text_stderr = text(&text_output.stderr);
    assert!(text_stderr.contains("next:"), "stderr:\n{text_stderr}");
    assert!(
        text_stderr.contains("ck daemon --subc <connection-file>"),
        "stderr:\n{text_stderr}"
    );
}

fn ck_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_ck"));
    // Every CLI test gets an isolated update cache and a closed local endpoint.
    // This proves dashboard output without reaching public release infrastructure.
    command
        .env(
            "CK_UPDATE_CACHE_PATH",
            unique_temp_dir("ck-update-cache").join("update-metadata.json"),
        )
        .env("CK_RELEASE_INDEX_URL", "http://127.0.0.1:0/index.json");
    command
}

fn ck_with_subc<const N: usize>(connection_file: &Path, args: [&str; N]) -> Output {
    let mut command = ck_command();
    command.args(args).arg("--subc").arg(connection_file);
    command.output().unwrap()
}

fn assert_json_success(output: Output) -> Value {
    assert_exit(&output, 0);
    assert!(
        output.stderr.is_empty(),
        "JSON command wrote stderr: {}",
        text(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout was not JSON: {error}: {}", text(&output.stdout)));
    let expected = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
    assert_eq!(
        text(&output.stdout),
        expected,
        "--json bytes changed from the captured pretty-JSON renderer"
    );
    value
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "unexpected ck exit status\nstdout:\n{}\nstderr:\n{}",
        text(&output.stdout),
        text(&output.stderr)
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn home_connection_file(home: &Path) -> PathBuf {
    home.join(".local")
        .join("share")
        .join("cortexkit")
        .join("run")
        .join("subc-connection.json")
}

fn supervisor(server: &TestServer) -> Supervisor {
    supervisor_with_restart_limit(server, 1)
}

fn supervisor_with_restart_limit(server: &TestServer, max_restarts: u32) -> Supervisor {
    Supervisor::new(
        Arc::clone(&server.registry),
        RestartPolicy::new(max_restarts, Duration::from_millis(10)),
    )
    .with_process_liveness(Arc::clone(&server.process_liveness))
    .with_forwarding(Arc::clone(&server.forwarding))
    .with_handle(server.supervisor_handle.clone())
    .with_drain_timeout(Duration::from_millis(25))
    .with_connection_file_path(server.connection_file_path.clone())
}

fn supervisor_with_fast_health(server: &TestServer) -> Supervisor {
    supervisor(server).with_health_config(HealthConfig {
        cadence: Duration::from_millis(10),
        deadline: Duration::from_secs(1),
        ..HealthConfig::default()
    })
}

async fn spawn_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
) -> SupervisedModule {
    let module = supervisor.spawn(stub_spec(module_id)).unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

async fn spawn_stub_with_env(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    env: Vec<(&str, &str)>,
) -> SupervisedModule {
    let mut spec = stub_spec(module_id);
    spec.env.extend(
        env.into_iter()
            .map(|(key, value)| (key.to_string(), value.to_string())),
    );
    let module = supervisor.spawn(spec).unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

fn stub_spec(module_id: &str) -> ModuleSpec {
    stub_spec_with_env(module_id, Vec::new())
}

fn stub_spec_with_env(module_id: &str, env: Vec<(&str, &str)>) -> ModuleSpec {
    ModuleSpec {
        module_id: module_id.to_string(),
        program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
        args: Vec::new(),
        env: std::iter::once(("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()))
            .chain(
                env.into_iter()
                    .map(|(key, value)| (key.to_string(), value.to_string())),
            )
            .collect(),
        reserved: false,
        reserved_prefixes: Vec::new(),
    }
}

fn quota_wire_fixture() -> Value {
    serde_json::json!([
        {
            "provider": "anthropic",
            "account": "work",
            "source": "vault",
            "usage": {
                "primary": {
                    "usedPercent": 42.0,
                    "resetsAt": "2099-06-15T14:30:00Z",
                    "windowMinutes": 300
                },
                "secondary": {
                    "usedPercent": 17.5,
                    "resetsAt": "2099-06-22T08:00:00+00:00",
                    "windowMinutes": 10080
                },
                "extraRateWindows": [
                    {
                        "title": "Bonus credits",
                        "id": "bonus-1",
                        "window": {
                            "usedPercent": 5.0,
                            "resetsAt": "2099-07-01T00:00:00Z",
                            "windowMinutes": 43200
                        }
                    }
                ]
            }
        },
        {
            "provider": "openai",
            "account": "personal",
            "error": "rate limit probe failed for secondary account",
            "usage": {
                "primary": {
                    "usedPercent": 88.0,
                    "resetsAt": "2099-06-15T18:00:00Z",
                    "windowMinutes": 300
                }
            }
        },
        {
            "provider": "grok",
            "error": "cookie jar unreadable"
        }
    ])
}

async fn spawn_quota_stub(
    server: &TestServer,
    supervisor: &Supervisor,
    module_id: &str,
    fixture: &Value,
) -> SupervisedModule {
    let fixture_json = serde_json::to_string(fixture).unwrap();
    let module = supervisor
        .spawn(ModuleSpec {
            module_id: module_id.to_string(),
            program: PathBuf::from(env!("CARGO_BIN_EXE_fake-aft-stub")),
            args: Vec::new(),
            env: vec![
                ("FAKE_AFT_MODULE_ID".to_string(), module_id.to_string()),
                (
                    "FAKE_AFT_ROLE".to_string(),
                    "management_surface".to_string(),
                ),
                ("FAKE_AFT_USAGE_GET_FIXTURE".to_string(), fixture_json),
            ],
            reserved: false,
            reserved_prefixes: Vec::new(),
        })
        .unwrap();
    wait_for_supervisor_entry(&server.connection_file_path, module_id, |entry| {
        entry.state == "running" && entry.enabled && entry.live
    })
    .await;
    module
}

async fn wait_for_supervisor_entry(
    path: &Path,
    module_id: &str,
    predicate: impl Fn(&SupervisorEntry) -> bool,
) -> SupervisorEntry {
    let deadline = Instant::now() + SETUP_TIMEOUT;
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
            panic!("module {module_id} did not reach expected supervisor state within {SETUP_TIMEOUT:?}; modules: {modules:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_health_status(path: &Path, module_id: &str, expected: SupervisorHealthStatus) {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    let mut corr = 30_000;
    loop {
        let mut client = wait_for_client(path).await;
        let response =
            control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorHealth {})
                .await;
        if let ClientControlResponse::SupervisorHealth { modules, .. } = response {
            if modules
                .iter()
                .any(|entry| entry.module_id == module_id && entry.status == expected)
            {
                return;
            }
        }
        if Instant::now() >= deadline {
            panic!("module {module_id} did not report {expected:?} within {SETUP_TIMEOUT:?}");
        }
        corr += 1;
        sleep(Duration::from_millis(20)).await;
    }
}

async fn supervisor_modules(path: &Path, corr: u64) -> Vec<SupervisorEntry> {
    let mut client = wait_for_client(path).await;
    match control_rpc_on_stream(&mut client, corr, ClientControlRequest::SupervisorList {}).await {
        ClientControlResponse::SupervisorList { modules, .. } => modules,
        other => panic!("unexpected supervisor.list response: {other:?}"),
    }
}

async fn wait_for_client(path: &Path) -> tokio::net::TcpStream {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        match connect_authed_client(path).await {
            Ok(client) => return client,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                sleep(Duration::from_millis(20)).await;
            }
            Err(err) => {
                panic!("daemon did not accept authenticated client within {SETUP_TIMEOUT:?}: {err}")
            }
        }
    }
}

async fn control_rpc_on_stream<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
) -> ClientControlResponse
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
        FrameType::Response => serde_json::from_slice(&frame.body).unwrap(),
        FrameType::Error => panic!(
            "control RPC returned error: {:?}",
            serde_json::from_slice::<Value>(&frame.body).unwrap()
        ),
        ty => panic!("unexpected control RPC frame type: {ty:?}"),
    }
}

/// Control RPC helper with a caller-sized reply window. The provenance op is the
/// motivating caller: on linux its first evaluation sha256-hashes the running
/// module executable twice (via /proc and from disk), and a debug-profile stub
/// on a cold, contended CI disk legitimately exceeds the default 10s — the
/// ubuntu leg failed 2 of 3 runs on exactly that. The window is sized against
/// the slowest acceptable progress of the operation, not the median.
async fn control_rpc_value_on_stream_within<S>(
    stream: &mut S,
    corr: u64,
    request: ClientControlRequest,
    reply_window: Duration,
) -> Value
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    write_frame(stream, &control_request_frame(corr, request))
        .await
        .unwrap();
    stream.flush().await.unwrap();
    let frame = timeout(reply_window, async {
        read_frame(stream)
            .await
            .unwrap()
            .expect("connection should stay open")
    })
    .await
    .expect("timed out waiting for control RPC reply");
    assert_eq!(frame.header.channel, 0);
    assert_eq!(frame.header.corr, corr);
    assert_eq!(frame.header.ty, FrameType::Response);
    serde_json::from_slice(&frame.body).unwrap()
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

fn unique_temp_dir(name: &str) -> TempDir {
    TempDir::new(name)
}
