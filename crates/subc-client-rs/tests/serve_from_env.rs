#![forbid(unsafe_code)]

//! Exercise process argv/environment in subprocesses, not by mutating the
//! environment shared by parallel tests. Both public entrypoints use the same
//! authenticated stand-in connection and are checked against independent launch
//! inputs, including the inherited secret pipe on Unix.

use std::{
    ffi::OsString,
    path::Path,
    process::{Child, Command, Output, Stdio},
    time::Duration,
};

use cortexkit_test_support::ScratchDir;
use subc_client_rs::{
    async_trait, serve, serve_from_env_with_handle, HandlerOutcome, ModuleHandler, RequestCtx,
    SubcModuleError,
};
use subc_os::launch_nonce::{LAUNCH_NONCE_ENV, LAUNCH_NONCE_FD_ENV};
use subc_protocol::{
    manifest::ModuleManifest, Flags, Frame, FrameType, ModuleHelloAckBody, ModuleHelloBody,
    Priority, PROTOCOL_VERSION, SUBC_MODULE_ID_ENV,
};
use subc_transport::{
    authenticate_server, generate_daemon_id, generate_key, read_frame, write_atomic, write_frame,
    ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{io::AsyncWriteExt, net::TcpListener, time::timeout};

const CHILD_ENTRY: &str = "SUBC_CLIENT_RS_ENV_TEST_ENTRY";
const CHILD_ERROR: &str = "SUBC_CLIENT_RS_ENV_TEST_ERROR";
const CHILD_OK: &str = "launch entrypoint exercised";
const NONCE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

struct Echo;

#[async_trait]
impl ModuleHandler for Echo {
    async fn handle(&self, _ctx: RequestCtx, body: Vec<u8>) -> HandlerOutcome {
        HandlerOutcome::Response(body)
    }
}

// Libtest treats the arguments after `--` as additional test filters, leaving
// them in argv for the SDK. The exact child-test filter still selects this test.
#[test]
fn serve_launch_child() {
    let Ok(entry) = std::env::var(CHILD_ENTRY) else {
        return;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(async {
        let manifest = ModuleManifest::builder("declared-module", "1.0.0").build();
        if entry == "handle" {
            let (handle, serving) = serve_from_env_with_handle(manifest, Echo).await?;
            assert!(!handle.is_closed(), "returned handle must be live");
            serving.await?;
            assert!(handle.is_closed(), "serve completion must close the handle");
            Ok(())
        } else {
            assert_eq!(entry, "serve");
            serve(manifest, Echo).await
        }
    });
    if let Ok(expected) = std::env::var(CHILD_ERROR) {
        let error: SubcModuleError = result.unwrap_err();
        assert!(
            format!("{error:?}").starts_with(&expected),
            "expected {expected}, got {error:?}"
        );
    } else {
        result.unwrap();
    }
    println!("{CHILD_OK}");
}

fn command(entry: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "serve_launch_child", "--nocapture", "--"])
        .env(CHILD_ENTRY, entry)
        .env_remove(CHILD_ERROR)
        .env_remove(SUBC_MODULE_ID_ENV)
        .env_remove(LAUNCH_NONCE_ENV)
        .env_remove(LAUNCH_NONCE_FD_ENV)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn assert_child(output: Output) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "child failed: {stdout}\n{stderr}");
    assert!(
        stdout.contains(CHILD_OK),
        "child entrypoint did not run: {stdout}"
    );
    assert!(
        stdout.contains("1 passed"),
        "child test was not selected: {stdout}"
    );
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn output(mut self) -> Output {
        self.0.take().unwrap().wait_with_output().unwrap()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

enum Secret {
    Absent,
    Environment,
    #[cfg(unix)]
    Pipe,
    #[cfg(unix)]
    BrokenPipe,
}

fn add_subc_arg(command: &mut Command, connection_file: &Path, equals: bool) {
    if equals {
        let mut arg = OsString::from("--subc=");
        arg.push(connection_file);
        command.arg(arg);
    } else {
        command.arg("--subc").arg(connection_file);
    }
}

async fn registered_child(entry: &str, equals: bool, module_id: Option<&str>, secret: Secret) {
    let dir = ScratchDir::new("subc-client-env-entry");
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let connection = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".into(),
            port: listener.local_addr().unwrap().port(),
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "env-entry-test".into(),
    };
    let path = dir.join("subc-conn.json");
    write_atomic(&path, &connection).unwrap();
    let mut command = command(entry);
    add_subc_arg(&mut command, &path, equals);
    if let Some(module_id) = module_id {
        command.env(SUBC_MODULE_ID_ENV, module_id);
    }
    match secret {
        Secret::Absent => {}
        Secret::Environment => {
            command.env(LAUNCH_NONCE_ENV, NONCE);
        }
        #[cfg(unix)]
        Secret::Pipe => {
            let handoff = subc_os::launch_nonce::LaunchNonceHandoff::new(NONCE).unwrap();
            command.env(LAUNCH_NONCE_FD_ENV, handoff.fd_env_value());
            command.env(LAUNCH_NONCE_ENV, "must-not-replace-pipe-secret");
            handoff.install_last(&mut command);
        }
        #[cfg(unix)]
        Secret::BrokenPipe => {
            command.env(LAUNCH_NONCE_FD_ENV, "not-a-descriptor");
            command.env(LAUNCH_NONCE_ENV, NONCE);
            command.env(CHILD_ERROR, "LaunchNonce(");
        }
    }
    let child = ChildGuard(Some(command.spawn().unwrap()));
    timeout(Duration::from_secs(5), async {
        let (mut daemon, _) = listener.accept().await.unwrap();
        authenticate_server(
            &mut daemon,
            &connection.key,
            &connection.daemon_id,
            &connection.daemon_ver,
            Duration::from_secs(2),
        )
        .await
        .unwrap();
        let hello = read_frame(&mut daemon).await.unwrap();
        #[cfg(unix)]
        if matches!(secret, Secret::BrokenPipe) {
            assert!(
                hello.is_none(),
                "invalid secret pipe must not fall back to env or send HELLO"
            );
            return;
        }
        let hello = hello.expect("module must send HELLO");
        assert_eq!(hello.header.ty, FrameType::Hello);
        let hello: ModuleHelloBody = serde_json::from_slice(&hello.body).unwrap();
        assert_eq!(
            hello.manifest.module_id,
            module_id.unwrap_or("declared-module")
        );
        assert_eq!(
            hello.launch_nonce.as_deref(),
            match secret {
                Secret::Absent => None,
                _ => Some(NONCE),
            }
        );
        let ack = ModuleHelloAckBody {
            negotiated_ver: PROTOCOL_VERSION,
            subc_ops: Vec::new(),
            subc_capabilities: Vec::new(),
            storage: None,
            machine_id: None,
        };
        let flags = Flags::new(false, Priority::Passive, false);
        write_frame(
            &mut daemon,
            &Frame::build(
                FrameType::HelloAck,
                flags,
                0,
                0,
                1,
                serde_json::to_vec(&ack).unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        write_frame(
            &mut daemon,
            &Frame::build(FrameType::Goodbye, flags, 0, 0, 0, Vec::new()).unwrap(),
        )
        .await
        .unwrap();
        daemon.flush().await.unwrap();
    })
    .await
    .expect("entrypoint must connect and register promptly");
    assert_child(child.output());
}

#[tokio::test]
async fn env_entrypoints_use_the_same_argv_module_id_and_launch_secret() {
    for entry in ["serve", "handle"] {
        registered_child(entry, false, None, Secret::Absent).await;
        registered_child(
            entry,
            true,
            Some("supervisor-assigned"),
            Secret::Environment,
        )
        .await;
        #[cfg(unix)]
        registered_child(entry, false, Some("pipe-launched"), Secret::Pipe).await;
    }
}

#[tokio::test]
async fn env_entrypoints_preserve_the_same_launch_errors() {
    for entry in ["serve", "handle"] {
        for (args, error) in [
            (vec![], "MissingSubcArg"),
            (vec!["--subc"], "MissingSubcValue"),
            (vec!["--subc="], "MissingSubcValue"),
        ] {
            assert_child(
                command(entry)
                    .args(args)
                    .env(CHILD_ERROR, error)
                    .output()
                    .unwrap(),
            );
        }
        for module_id in ["", "   "] {
            assert_child(
                command(entry)
                    .args(["--subc", "/unused"])
                    .env(SUBC_MODULE_ID_ENV, module_id)
                    .env(CHILD_ERROR, "EmptyModuleIdEnv")
                    .output()
                    .unwrap(),
            );
        }
        let dir = ScratchDir::new("subc-client-env-missing-file");
        assert_child(
            command(entry)
                .arg("--subc")
                .arg(dir.join("missing.json"))
                .env(CHILD_ERROR, "ConnectionFile")
                .output()
                .unwrap(),
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            assert_child(
                command(entry)
                    .args(["--subc", "/unused"])
                    .env(SUBC_MODULE_ID_ENV, OsString::from_vec(vec![0xff]))
                    .env(CHILD_ERROR, "NonUnicodeModuleIdEnv")
                    .output()
                    .unwrap(),
            );
            registered_child(entry, false, None, Secret::BrokenPipe).await;
        }
    }
}
