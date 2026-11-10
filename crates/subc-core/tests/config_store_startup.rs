//! The service argument, never ambient home variables, selects config authority.

mod common;

use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Stdio},
    time::{Duration, Instant},
};
use subc_protocol::{Flags, Frame, FrameType, Priority};
use tokio::io::{AsyncRead, AsyncWrite};

struct Daemon {
    root: PathBuf,
    child: Child,
}

impl Daemon {
    fn start(root: PathBuf, store: Option<&Path>) -> Self {
        let mut command = cortexkit_test_support::dev_command(
            cortexkit_test_support::ckdev_binary(env!("CARGO_BIN_EXE_ck-subc")),
        );
        command
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("HOME", root.join("home"))
            .env("USERPROFILE", root.join("home"))
            .env("LOCALAPPDATA", root.join("data"))
            .env(
                "SUBC_CONNECTION_FILE",
                root.join("runtime")
                    .join(subc_transport::CONNECTION_FILE_NAME),
            )
            .env("SUBC_PORT", "0")
            .env("SUBC_CGROUP_PLACEMENT", "disabled")
            .env_remove(subc_protocol::SUBC_MODULE_ID_ENV)
            .env_remove(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
            .env_remove("CK_LOG")
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(
                fs::File::create(root.join("stderr.log")).unwrap(),
            ));
        if let Some(store) = store {
            command.arg("--config-store-root").arg(store);
        }
        let child = command.spawn().unwrap();
        Self { root, child }
    }

    async fn ready(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                panic!(
                    "isolated daemon exited during startup: {status}; {}",
                    fs::read_to_string(self.root.join("stderr.log")).unwrap()
                );
            }
            if self
                .root
                .join("runtime")
                .join(subc_transport::CONNECTION_FILE_NAME)
                .exists()
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "isolated daemon did not publish its connection file"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn status(&self) -> (FrameType, Value) {
        tokio::time::timeout(Duration::from_secs(5), async {
            let read = subc_transport::read_for_client_with_local_endpoint(
                self.root
                    .join("runtime")
                    .join(subc_transport::CONNECTION_FILE_NAME),
            )
            .unwrap();
            match read.local_endpoint.unwrap() {
                #[cfg(unix)]
                subc_transport::LocalEndpoint::Unix { path } => {
                    status_on(
                        &mut tokio::net::UnixStream::connect(path).await.unwrap(),
                        &read.info,
                    )
                    .await
                }
                #[cfg(windows)]
                subc_transport::LocalEndpoint::Pipe { name } => {
                    status_on(
                        &mut tokio::net::windows::named_pipe::ClientOptions::new()
                            .open(name)
                            .unwrap(),
                        &read.info,
                    )
                    .await
                }
                _ => panic!("daemon published an unsupported local endpoint"),
            }
        })
        .await
        .unwrap()
    }

    fn stop(&mut self) {
        // Child::kill targets only this process's retained PID/handle. It never
        // searches by executable name or touches a separately running daemon.
        self.child.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if self.child.try_wait().unwrap().is_some() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "owned daemon did not exit after stop"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

async fn status_on<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    info: &subc_transport::ConnectionInfo,
) -> (FrameType, Value) {
    subc_transport::authenticate_client(stream, info, Duration::from_secs(2))
        .await
        .unwrap();
    let request = Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        1,
        serde_json::to_vec(&json!({"op":"config.status"})).unwrap(),
    )
    .unwrap();
    subc_transport::write_frame(stream, &request).await.unwrap();
    let response = subc_transport::read_frame(stream).await.unwrap().unwrap();
    assert_eq!(response.header.corr, 1);
    (
        response.header.ty,
        serde_json::from_slice(&response.body).unwrap(),
    )
}

struct Temp(common::kit::ShortRoot);
impl Temp {
    fn new() -> Self {
        let path = common::kit::ShortRoot::create_short_socket_root();
        for name in ["data", "runtime", "config/cortexkit", "home"] {
            fs::create_dir_all(path.join(name)).unwrap();
        }
        fs::write(
            path.join("config/cortexkit/subc.jsonc"),
            br#"{"version":1,"modules":{}}"#,
        )
        .unwrap();
        Self(path)
    }
}

fn marker(root: &Path) -> Vec<u8> {
    use std::io::Write;
    fs::create_dir_all(root).unwrap();
    let bytes =
        serde_json::to_vec(&json!({"format_version":8675309,"isolated_marker":root})).unwrap();
    let path = root.join("approved.json");
    #[cfg(unix)]
    let mut file = {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap()
    };
    #[cfg(windows)]
    let mut file = subc_os::windows_acl::create_private_file(&path).unwrap();
    file.write_all(&bytes).unwrap();
    bytes
}

fn config_artifacts(root: &Path, paths: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            config_artifacts(&entry.path(), paths);
        } else if entry
            .file_name()
            .to_string_lossy()
            .contains("approved.json")
            || matches!(
                entry.file_name().to_str(),
                Some("config.lock" | "cortexkit.lock" | "service-store.lock")
            )
        {
            paths.push(entry.path());
        }
    }
}

#[tokio::test]
/// Only Linux runs the real-store positive case: macOS and Windows can reach
/// the user's credential store despite HOME/XDG isolation. Isolated owner
/// fixtures cover commits without opening the user's credential store.
#[cfg(target_os = "linux")]
async fn config_store_startup_explicit_root_reads_only_its_unique_marker() {
    let temp = Temp::new();
    let root = temp.0.join("service-store");
    let bytes = marker(&root);
    let mut daemon = Daemon::start(temp.0.path().to_path_buf(), Some(&root));
    daemon.ready().await;
    let (kind, body) = daemon.status().await;
    assert_eq!(kind, FrameType::Response, "{body}");
    assert_eq!(body["status"]["state"], "store_unverified", "{body}");
    assert!(
        body["status"]["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap()
                .contains("unsupported store format")),
        "{body}"
    );
    daemon.stop();
    assert_eq!(fs::read(root.join("approved.json")).unwrap(), bytes);
    let mut artifacts = Vec::new();
    config_artifacts(temp.0.path(), &mut artifacts);
    artifacts.sort();
    let mut expected = vec![
        root.join("approved.json"),
        temp.0.join("service-store.lock"),
    ];
    expected.sort();
    assert_eq!(artifacts, expected);
}

#[tokio::test]
async fn config_store_startup_without_argument_ignores_all_fallback_markers() {
    let temp = Temp::new();
    let roots = [
        "data/cortexkit/config",
        "config/cortexkit/config",
        "config/cortexkit",
        "home/.local/share/cortexkit/config",
        "home/.config/cortexkit/config",
    ];
    let markers: Vec<_> = roots
        .iter()
        .map(|path| {
            let root = temp.0.join(path);
            let bytes = marker(&root);
            (root.join("approved.json"), bytes)
        })
        .collect();
    let mut daemon = Daemon::start(temp.0.path().to_path_buf(), None);
    daemon.ready().await;
    let (kind, body) = daemon.status().await;
    assert_eq!(kind, FrameType::Error, "{body}");
    assert_eq!(body["code"], "config_gate_inactive", "{body}");
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("no config store configured"));
    daemon.stop();
    for (path, bytes) in &markers {
        assert_eq!(fs::read(path).unwrap(), *bytes);
    }
    let mut artifacts = Vec::new();
    config_artifacts(temp.0.path(), &mut artifacts);
    artifacts.sort();
    let mut expected: Vec<_> = markers.into_iter().map(|(path, _)| path).collect();
    expected.sort();
    assert_eq!(artifacts, expected);
}
