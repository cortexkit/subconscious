//! An acceptance daemon whose `claustrum` is the harness signer or a real claustrum
//! binary, instead of the shape stub. `callosum` stays the shape stub and `ckbus` is the
//! supervised module under test, exactly as in the fixture declaration.
//!
//! The fixture file is never edited: this is the per-test renderer registration. It
//! renders the shared template and then, for the real binary only, replaces the
//! `claustrum` block and adds a fixture `storage` section so the vault lives inside the
//! run's tree.

use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use serde_json::{json, Value};
use subc_control::{ClientControlRequest, ClientControlResponse};
use subc_daemon::{
    bootstrap::{run_with_config, BootstrapConfig},
    test_support::TestTempDir,
};
use tokio::task::JoinHandle;

use super::{claustrum::RealClaustrum, HarnessSigner};
use crate::harness::{
    config::{self, SentinelTiming},
    control, data_home,
    stubs::{StubRecorder, CALLOSUM_OPERATIONS, CLAUSTRUM_OPERATIONS},
};

/// What answers `claustrum` in a run.
pub enum ClaustrumSide<'a> {
    Signer(HarnessSigner),
    /// The slice-0 shape stub, which answers no signature: the refusing-signer control.
    Stub,
    /// A real claustrum, with a vault the ceremony already prepared at
    /// `RealClaustrum::vault_dir` under this run's data home.
    Binary(&'a RealClaustrum),
}

pub struct SignerRun {
    pub root: TestTempDir,
    /// The operator's real ckbus store, fingerprinted before the run; `shutdown` fails
    /// the run if anything under it changed.
    operator_dir: Option<PathBuf>,
    operator_before: data_home::TreeFingerprint,
    pub connection_file: PathBuf,
    pub config_file: PathBuf,
    pub callosum: StubRecorder,
    daemon: JoinHandle<Result<(), subc_daemon::bootstrap::BootstrapError>>,
    module_tasks: Vec<JoinHandle<Result<(), subc_client_rs::SubcModuleError>>>,
}

impl SignerRun {
    /// Creates the run's tree without starting anything, so a ceremony can prepare the
    /// fixture vault under `data_home` first.
    pub fn tree() -> TestTempDir {
        let root = TestTempDir::new("ck-bus-signer");
        for relative in ["data", "run/logs"] {
            fs::create_dir_all(root.join(relative)).expect("fixture directory must be creatable");
        }
        root
    }

    pub fn data_home(root: &TestTempDir) -> PathBuf {
        root.join("data")
    }

    pub async fn start(root: TestTempDir, ck_bus: &Path, side: ClaustrumSide<'_>) -> Self {
        Self::start_with(root, ck_bus, side, RunOptions::default()).await
    }

    /// `start`, with extra `ckbus` environment and, when given, a machine id the daemon
    /// serves on HELLO_ACK from a file inside the run's tree.
    pub async fn start_with(
        root: TestTempDir,
        ck_bus: &Path,
        side: ClaustrumSide<'_>,
        options: RunOptions,
    ) -> Self {
        let operator_dir = data_home::operator_module_dir();
        if let Some(dir) = &operator_dir {
            // Evidence only while the operator directory lies outside the fixture tree.
            assert!(
                !dir.starts_with(root.path()),
                "operator data home {} must lie outside the fixture tree",
                dir.display()
            );
        }
        let operator_before = data_home::fingerprint(operator_dir.as_deref());
        let config_file = config::render(&root, ck_bus, SentinelTiming::default());
        if let ClaustrumSide::Binary(real) = &side {
            register_claustrum_binary(&config_file, &Self::data_home(&root), real);
        }
        if !options.ckbus_env.is_empty() {
            add_ckbus_env(&config_file, &options.ckbus_env);
        }
        let connection_file = root.join("run/subc-connection.json");
        // A second daemon on the same tree must not be mistaken for the first one's
        // connection file.
        let _ = fs::remove_file(&connection_file);
        let mut bootstrap = BootstrapConfig::new(&connection_file, 0)
            .with_terminal_journal_path(root.join("run/terminals.jsonl"))
            .with_capture_logs_dir(root.join("run/logs"));
        if let Some(machine_id) = &options.machine_id {
            let path = root.join("run/machine-id");
            fs::write(&path, format!("{machine_id}\n")).expect("fixture machine id must write");
            bootstrap = bootstrap.with_machine_id_path(path);
        }
        let bootstrap = bootstrap
            .with_daemon_config_path(&config_file)
            .expect("fixture daemon config must load");
        let daemon = tokio::spawn(run_with_config(bootstrap));
        control::wait_for_connection(&connection_file, Instant::now() + Duration::from_secs(10))
            .await;

        let callosum = StubRecorder::refusing("callosum", CALLOSUM_OPERATIONS);
        let mut module_tasks = vec![spawn_module(
            &connection_file,
            callosum.manifest(),
            callosum.clone(),
        )];
        match side {
            ClaustrumSide::Signer(signer) => {
                module_tasks.push(spawn_module(&connection_file, signer.manifest(), signer));
            }
            ClaustrumSide::Stub => {
                let stub = StubRecorder::refusing("claustrum", CLAUSTRUM_OPERATIONS);
                module_tasks.push(spawn_module(&connection_file, stub.manifest(), stub));
            }
            ClaustrumSide::Binary(_) => {}
        }
        let run = Self {
            root,
            operator_dir,
            operator_before,
            connection_file,
            config_file,
            callosum,
            daemon,
            module_tasks,
        };
        for module_id in ["claustrum", "callosum", "ckbus"] {
            run.wait_for_catalog_id(module_id).await;
        }
        run
    }

    pub async fn wait_for_catalog_id(&self, module_id: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let response = control::response(
                &self.connection_file,
                ClientControlRequest::CatalogList {
                    module_id: Some(module_id.to_string()),
                },
            )
            .await;
            let ClientControlResponse::CatalogList { modules, .. } = response else {
                panic!("catalog.list must return its matching response variant");
            };
            if modules.iter().any(|module| module.module_id == module_id) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "catalog registration {module_id} did not appear"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// The supervised pid from `supervisor.provenance` narrowed to `module_id`, at
    /// `daemon_observed.pid`: an absent pid is re-read every 200 ms for up to 2 s.
    pub async fn supervised_pid(&self, module_id: &str) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let response = control::response(
                &self.connection_file,
                ClientControlRequest::SupervisorProvenance {
                    module_id: Some(module_id.to_string()),
                },
            )
            .await;
            let ClientControlResponse::SupervisorProvenance { modules, .. } = response else {
                panic!("supervisor.provenance must return its matching response variant");
            };
            let matching: Vec<_> = modules
                .into_iter()
                .filter(|entry| entry.module_id == module_id)
                .collect();
            assert_eq!(
                matching.len(),
                1,
                "narrowed supervisor.provenance must hold exactly one {module_id} entry"
            );
            if let Some(pid) = matching[0].daemon_observed.pid {
                return pid;
            }
            assert!(
                Instant::now() < deadline,
                "supervisor.provenance {module_id} daemon_observed.pid stayed absent for 2 s: \
                 {matching:?}"
            );
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn shutdown(self) -> TestTempDir {
        for task in self.module_tasks {
            task.abort();
            let _ = task.await;
        }
        self.daemon.abort();
        let _ = self.daemon.await;
        assert_eq!(
            data_home::fingerprint(self.operator_dir.as_deref()),
            self.operator_before,
            "the operator's real ckbus data home must be unchanged by the run"
        );
        self.root
    }
}

/// Options for `SignerRun::start_with`.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    pub ckbus_env: Vec<(String, String)>,
    pub machine_id: Option<String>,
}

fn add_ckbus_env(config_file: &Path, env: &[(String, String)]) {
    let mut value: Value =
        serde_json::from_slice(&fs::read(config_file).expect("rendered config must be readable"))
            .expect("rendered config must be JSON");
    for (key, entry) in env {
        value["modules"]["ckbus"]["env"][key] = Value::String(entry.clone());
    }
    fs::write(
        config_file,
        serde_json::to_vec_pretty(&value).expect("config encodes"),
    )
    .expect("rendered config must be writable");
}

fn spawn_module<H>(
    connection_file: &Path,
    manifest: subc_protocol::manifest::ModuleManifest,
    handler: H,
) -> JoinHandle<Result<(), subc_client_rs::SubcModuleError>>
where
    H: subc_client_rs::ModuleHandler + 'static,
{
    let connection_file = connection_file.to_path_buf();
    tokio::spawn(
        async move { subc_client_rs::serve_with(&connection_file, manifest, handler).await },
    )
}

/// Declares the real claustrum: supervised, reserved like the production vault, its
/// master key from the fixture key file, and its sqlite store under the run's data home
/// (the daemon derives `<data_home>/cortexkit/claustrum/store.db`).
fn register_claustrum_binary(config_file: &Path, data_home: &Path, real: &RealClaustrum) {
    let mut value: Value =
        serde_json::from_slice(&fs::read(config_file).expect("rendered config must be readable"))
            .expect("rendered config must be JSON");
    value["modules"]["claustrum"] = json!({
        "program": real.claustrum_bin.display().to_string(),
        "args": [],
        "env": {
            "CK_MASTER_KEY_PATH": real.master_key_path.display().to_string(),
            "XDG_DATA_HOME": data_home.display().to_string(),
        },
        "enabled": true,
        "reserved": true,
        "protocol": "subc",
    });
    value["storage"] = json!({
        "backend": "sqlite",
        "data_home": data_home.display().to_string(),
    });
    fs::write(
        config_file,
        serde_json::to_vec_pretty(&value).expect("config encodes"),
    )
    .expect("rendered config must be writable");
}
