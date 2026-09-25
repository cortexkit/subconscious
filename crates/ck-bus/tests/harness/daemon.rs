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
        let bootstrap = BootstrapConfig::new(&connection_file, 0)
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
