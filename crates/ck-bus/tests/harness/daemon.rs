use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_daemon::bootstrap::{run_with_config, BootstrapConfig};
use subc_test_support::TestTempDir;
use tokio::task::JoinHandle;

use super::{
    config::{self, SentinelTiming},
    control,
    stubs::{StubRecorder, CALLOSUM_OPERATIONS, CLAUSTRUM_OPERATIONS},
};

/// Shared bootstrap for every in-process daemon in the acceptance tests. macOS
/// refuses supervised launches without an executable privacy trampoline.
pub fn bootstrap_config(connection_file: &Path) -> BootstrapConfig {
    let config = BootstrapConfig::new(connection_file, 0);
    #[cfg(target_os = "macos")]
    let config = config.with_privacy_trampoline(ckdev_subc());
    config
}

#[cfg(target_os = "macos")]
pub(super) fn ckdev_subc() -> &'static Path {
    static TRAMPOLINE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TRAMPOLINE.get_or_init(|| {
        // Cargo only supplies CARGO_BIN_EXE for the package being tested. Build
        // the workspace's ck-subc explicitly and read its artifact path from
        // Cargo so custom target directories work too. Its main handles the
        // hidden trampoline mode before runtime, logging or daemon startup.
        let output = std::process::Command::new(env!("CARGO"))
            .args([
                "build",
                "--locked",
                "-p",
                "subc-core",
                "--bin",
                "ck-subc",
                "--message-format=json",
            ])
            .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .output()
            .expect("build workspace privacy trampoline");
        assert!(
            output.status.success(),
            "building ck-subc privacy trampoline failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let built = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find_map(|message| {
                (message["reason"] == "compiler-artifact" && message["target"]["name"] == "ck-subc")
                    .then(|| message["executable"].as_str().map(PathBuf::from))
                    .flatten()
            })
            .expect("Cargo must report the ck-subc executable artifact");
        subc_test_support::ckdev_binary(built)
    })
}

pub struct AcceptanceRun {
    pub root: TestTempDir,
    pub connection_file: PathBuf,
    pub config_file: PathBuf,
    pub claustrum: StubRecorder,
    pub callosum: StubRecorder,
    daemon: JoinHandle<Result<(), subc_daemon::bootstrap::BootstrapError>>,
    stub_tasks: Vec<JoinHandle<Result<(), subc_client_rs::SubcModuleError>>>,
}

impl AcceptanceRun {
    pub async fn start(binary: &Path) -> Self {
        let root = TestTempDir::new("ck-bus-acceptance");
        for relative in ["data", "run/logs"] {
            fs::create_dir_all(root.join(relative))
                .expect("observable fixture directory must be creatable");
        }
        let config_file = config::render(&root, binary, SentinelTiming::default());
        let connection_file = root.join("run/subc-connection.json");
        let bootstrap = bootstrap_config(&connection_file)
            .with_terminal_journal_path(root.join("run/terminals.jsonl"))
            .with_capture_logs_dir(root.join("run/logs"))
            .with_daemon_config_path(&config_file)
            .expect("observable fixture daemon config must load");
        let daemon = tokio::spawn(run_with_config(bootstrap));
        control::wait_for_connection(&connection_file, Instant::now() + Duration::from_secs(10))
            .await;

        let claustrum = StubRecorder::refusing("claustrum", CLAUSTRUM_OPERATIONS);
        let callosum = StubRecorder::refusing("callosum", CALLOSUM_OPERATIONS);
        let stub_tasks = vec![
            spawn_stub(&connection_file, claustrum.clone()),
            spawn_stub(&connection_file, callosum.clone()),
        ];
        let run = Self {
            root,
            connection_file,
            config_file,
            claustrum,
            callosum,
            daemon,
            stub_tasks,
        };
        run.wait_for_catalog_id("claustrum").await;
        run.wait_for_catalog_id("callosum").await;
        run
    }

    pub async fn wait_for_catalog_id(&self, module_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let response = control::response(
                &self.connection_file,
                ClientControlRequest::CatalogList {
                    module_id: Some(module_id.to_string()),
                },
            )
            .await;
            let ClientControlResponse::CatalogList { modules, .. } = response else {
                panic!("observable catalog.list must return its matching response variant");
            };
            if modules.iter().any(|module| module.module_id == module_id) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "observable catalog registration {module_id} did not appear"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    pub async fn shutdown(self) {
        for task in self.stub_tasks {
            task.abort();
            let _ = task.await;
        }
        self.daemon.abort();
        let _ = self.daemon.await;
    }
}

fn spawn_stub(
    connection_file: &Path,
    stub: StubRecorder,
) -> JoinHandle<Result<(), subc_client_rs::SubcModuleError>> {
    let connection_file = connection_file.to_path_buf();
    tokio::spawn(async move {
        subc_client_rs::serve_with(&connection_file, stub.manifest(), stub).await
    })
}
