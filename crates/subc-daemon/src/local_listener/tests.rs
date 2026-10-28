//! Tests for the daemon's local endpoint: where it lives, who may connect, and
//! what a connection may do once it is there.
//!
//! The cases run the real endpoint and the real authentication server over it,
//! so an assertion about the handshake is an assertion about the bytes the
//! daemon actually exchanges.

use std::{sync::Arc, time::Duration};

#[cfg(unix)]
use cortexkit_test_support::ScratchDir;
use subc_protocol::{
    manifest::ModuleManifest, ErrorBody, Flags, Frame, FrameType, ModuleHelloBody, Priority,
    PROTOCOL_VERSION,
};
use subc_transport::{
    authenticate_client, generate_daemon_id, generate_key, ConnectionInfo, Endpoint, LocalEndpoint,
    SCHEMA_VERSION,
};
#[cfg(unix)]
use subc_transport::{read_with_local_endpoint, write_atomic, write_atomic_with_local_endpoint};
#[cfg(unix)]
use tokio::net::TcpListener;

#[cfg(unix)]
use crate::server::serve_listener;
use crate::{
    control::ControlHandler,
    local_listener::{self, LocalListenerConfig},
    read_frame,
    registry::Registry,
    server::{serve_local_listener, ServerAuth},
    write_frame, Router,
};

fn test_connection_info(port: u16) -> ConnectionInfo {
    ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: Some(PROTOCOL_VERSION),
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_owned(),
            port,
        }],
        key: generate_key().unwrap(),
        daemon_id: generate_daemon_id().unwrap(),
        pid: std::process::id(),
        daemon_ver: "test-subc".to_owned(),
    }
}

/// A router over a registry the test keeps, so a registration that must not
/// happen can be looked for afterwards.
fn router_with_registry() -> (Arc<Router>, Arc<Registry>) {
    let registry = Arc::new(Registry::default());
    let control = Arc::new(ControlHandler::new(Arc::clone(&registry)));
    (Arc::new(Router::with_control_handler(control)), registry)
}

fn hello_frame(module_id: &str, launch_nonce: Option<&str>) -> Frame {
    let manifest = ModuleManifest::builder(module_id, "0.0.1").build();
    let hello = ModuleHelloBody {
        protocol_ver: PROTOCOL_VERSION,
        manifest,
        launch_nonce: launch_nonce.map(str::to_owned),
        control_ops: None,
    };
    Frame::build(
        FrameType::Hello,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        7,
        serde_json::to_vec(&hello).unwrap(),
    )
    .unwrap()
}

/// A one-letter socket directory inside the scratch directory: a Unix socket
/// path is limited to just over a hundred bytes, and a test harness's temporary
/// directory uses nearly all of that.
#[cfg(unix)]
fn short_config(dir: &ScratchDir, tag: &str) -> LocalListenerConfig {
    let tag: String = tag.chars().take(1).collect();
    // The socket directory is a short one under the system temporary directory,
    // not inside `dir`: a test harness's own directories (and the lockdown
    // sandbox's `/private/.../aft-w/...` prefix) can leave no room inside the
    // Unix socket path limit, and these tests are about the endpoint, not the
    // harness's path length.
    let _ = dir;
    LocalListenerConfig::in_socket_dir(
        std::env::temp_dir().join(format!("subc-lt-{}-{tag}", std::process::id())),
    )
}

/// A connection file under the system temporary directory. The socket's name
/// does not come from it, but each test still gets its own, so a test that
/// reads a published endpoint back cannot see another test's file.
#[cfg(unix)]
fn short_connection_file(_dir: &ScratchDir) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    std::env::temp_dir().join(format!(
        "subc-lt-c-{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ))
}

fn ping_frame(corr: u64) -> Frame {
    Frame::build(
        FrameType::Ping,
        Flags::new(false, Priority::Interactive, false),
        0,
        0,
        corr,
        Vec::new(),
    )
    .unwrap()
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_socket_is_owner_only_inside_an_owner_only_directory() {
        let dir = ScratchDir::new("tm");
        let bound =
            local_listener::bind(&short_config(&dir, "om"), &short_connection_file(&dir)).unwrap();
        let LocalEndpoint::Unix { path } = bound.endpoint() else {
            panic!("a Unix listener publishes a socket path");
        };

        assert_eq!(
            local_listener::unix_socket_mode(&path).unwrap(),
            0o600,
            "another user must not be able to connect to the socket file"
        );
        let socket_dir = path.parent().unwrap();
        let dir_mode = std::fs::metadata(socket_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700, "the socket directory is owner-only");

        local_listener::release(&bound);
        assert!(!path.exists(), "releasing the listener removes the socket");
    }

    /// A directory the daemon did not create keeps the mode its owner set, even
    /// when that mode lets other users reach the socket: the daemon refuses to
    /// serve there rather than changing someone else's directory.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_world_readable_socket_directory_is_refused_and_left_alone() {
        let dir = ScratchDir::new("ts");
        let socket_dir = std::env::temp_dir().join(format!("subc-lt-wr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&socket_dir);
        std::fs::create_dir(&socket_dir).unwrap();
        std::fs::set_permissions(&socket_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        let error = local_listener::bind(
            &LocalListenerConfig::in_socket_dir(&socket_dir),
            &short_connection_file(&dir),
        )
        .expect_err("a directory other users can reach cannot hold the socket");
        assert!(
            matches!(error, crate::ServerError::LocalAccept { .. }),
            "{error}"
        );
        let mode = std::fs::metadata(&socket_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "the daemon does not change a directory it did not create"
        );
        assert_eq!(
            std::fs::read_dir(&socket_dir).unwrap().count(),
            0,
            "a refused bind creates no socket file"
        );
    }

    /// A directory the daemon creates is owner-only from the start.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_created_socket_directory_is_owner_only() {
        let dir = ScratchDir::new("tc");
        let bound =
            local_listener::bind(&short_config(&dir, "c"), &short_connection_file(&dir)).unwrap();
        let LocalEndpoint::Unix { path } = bound.endpoint() else {
            panic!("a Unix listener publishes a socket path");
        };
        let mode = std::fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o700);
        local_listener::release(&bound);
    }

    /// A daemon that died without unwinding leaves its socket file behind, and
    /// the next start must replace it rather than refuse to serve.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_leftover_socket_from_a_dead_daemon_is_replaced() {
        let dir = ScratchDir::new("ml");
        // One connection file, used by both starts: the socket's name comes from
        // it, so the second start looks for exactly the socket the first left.
        let connection_file = short_connection_file(&dir);
        let first = local_listener::bind(&short_config(&dir, "le"), &connection_file).unwrap();
        let LocalEndpoint::Unix { path } = first.endpoint() else {
            panic!("a Unix listener publishes a socket path");
        };
        // The first daemon's socket is left behind with its listener still open,
        // which is his start before the operating system closes it: a crash
        // leaves the file and no listener that answers.
        std::mem::forget(first);
        assert!(path.exists());
        // A listener that is gone leaves the file behind for the next start.
        std::fs::remove_file(&path).unwrap();
        std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stale = std::os::unix::net::UnixListener::bind(&path);
        assert!(stale.is_err(), "a live listener owns its path");

        let second = local_listener::bind(&short_config(&dir, "le"), &connection_file)
            .expect("a stale socket file is replaced, not refused");
        local_listener::release(&second);
        assert!(!path.exists());
    }

    /// Only another user can own the socket directory in production, which a
    /// test cannot arrange without root. This pins the answer the check relies
    /// on: the directory a test creates belongs to the process running it.
    #[test]
    fn the_socket_directory_belongs_to_this_user() {
        let dir = ScratchDir::new("tw");
        let bound =
            local_listener::bind(&short_config(&dir, "wn"), &short_connection_file(&dir)).unwrap();
        let LocalEndpoint::Unix { path } = bound.endpoint() else {
            panic!("a Unix listener publishes a socket path");
        };
        assert_eq!(
            std::fs::metadata(path.parent().unwrap()).unwrap().uid(),
            rustix::process::geteuid().as_raw()
        );
        local_listener::release(&bound);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tcp_client_and_a_socket_client_send_and_receive_identical_bytes() {
        let dir = ScratchDir::new("th");
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info = test_connection_info(tcp.local_addr().unwrap().port());
        let bound = local_listener::bind(&short_config(&dir, "by"), &short_connection_file(&dir))
            .expect("bind the local endpoint");
        let LocalEndpoint::Unix { path } = bound.endpoint() else {
            panic!("a Unix listener publishes a socket path");
        };

        let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
        let (router, _registry) = router_with_registry();
        let tcp_server = tokio::spawn(serve_listener(tcp, Arc::clone(&router), auth.clone()));
        let local_server = tokio::spawn(serve_local_listener(bound, Arc::clone(&router), auth));

        let mut over_tcp = tokio::net::TcpStream::connect(("127.0.0.1", info.endpoints[0].port))
            .await
            .unwrap();
        let mut over_socket = tokio::net::UnixStream::connect(&path).await.unwrap();
        // The handshake is the same exchange on both: the client's proof and the
        // daemon's are computed over the same file's key and daemon id.
        authenticate_client(&mut over_tcp, &info, Duration::from_secs(5))
            .await
            .expect("the TCP handshake completes");
        authenticate_client(&mut over_socket, &info, Duration::from_secs(5))
            .await
            .expect("the local handshake completes");

        write_frame(&mut over_tcp, &ping_frame(11)).await.unwrap();
        write_frame(&mut over_socket, &ping_frame(11))
            .await
            .unwrap();
        let tcp_reply = read_frame(&mut over_tcp).await.unwrap().unwrap();
        let local_reply = read_frame(&mut over_socket).await.unwrap().unwrap();

        assert_eq!(
            tcp_reply, local_reply,
            "both endpoints answer the same request with the same bytes"
        );
        assert_eq!(local_reply.header.ty, FrameType::Pong);

        tcp_server.abort();
        local_server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_module_hello_on_the_socket_is_refused_and_leaves_no_registration() {
        for (label, nonce) in [
            ("without-a-launch-nonce", None),
            ("with-a-launch-nonce", Some("a-test-nonce")),
        ] {
            let dir = ScratchDir::new("hg");
            let info = test_connection_info(0);
            let bound =
                local_listener::bind(&short_config(&dir, "hl"), &short_connection_file(&dir))
                    .unwrap();
            let LocalEndpoint::Unix { path } = bound.endpoint() else {
                panic!("a Unix listener publishes a socket path");
            };
            let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
            let (router, registry) = router_with_registry();
            let server = tokio::spawn(serve_local_listener(
                bound,
                Arc::clone(&router),
                auth.clone(),
            ));

            let mut client = tokio::net::UnixStream::connect(&path).await.unwrap();
            authenticate_client(&mut client, &info, Duration::from_secs(5))
                .await
                .unwrap();
            write_frame(&mut client, &hello_frame("socket-module", nonce))
                .await
                .unwrap();
            let reply = read_frame(&mut client).await.unwrap().unwrap();

            let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
            assert_eq!(
                error.code,
                local_listener::LOCAL_ENDPOINT_CLIENTS_ONLY,
                "{label}: a module HELLO over the local endpoint is refused by name"
            );
            assert_eq!(error.code, "local_endpoint_clients_only");
            assert!(
                registry.get_module("socket-module").unwrap().is_none(),
                "{label}: the refused HELLO registers nothing"
            );
            assert_eq!(
                registry.list_modules().unwrap().1.len(),
                0,
                "{label}: the catalog is unchanged"
            );
            server.abort();
        }
    }

    /// A TCP client keeps the right to register, so the refusal above comes from
    /// the endpoint and not from the HELLO.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_same_hello_registers_over_tcp() {
        let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let info = test_connection_info(tcp.local_addr().unwrap().port());
        let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
        let (router, registry) = router_with_registry();
        let server = tokio::spawn(serve_listener(tcp, router, auth));

        let mut client = tokio::net::TcpStream::connect(("127.0.0.1", info.endpoints[0].port))
            .await
            .unwrap();
        authenticate_client(&mut client, &info, Duration::from_secs(5))
            .await
            .unwrap();
        write_frame(&mut client, &hello_frame("tcp-module", None))
            .await
            .unwrap();
        let reply = read_frame(&mut client).await.unwrap().unwrap();
        assert_eq!(reply.header.ty, FrameType::HelloAck);
        assert!(registry.get_module("tcp-module").unwrap().is_some());
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_connection_file_publishes_the_socket_path_the_listener_serves() {
        let dir = ScratchDir::new("tp");
        let connection_path = dir.join("connection.json");
        let info = test_connection_info(0);
        let bound =
            local_listener::bind(&short_config(&dir, "pb"), &short_connection_file(&dir)).unwrap();
        let endpoint = bound.endpoint();
        write_atomic_with_local_endpoint(&connection_path, &info, Some(&endpoint)).unwrap();

        let read = read_with_local_endpoint(&connection_path).unwrap();
        assert_eq!(read.local_endpoint, Some(endpoint));
        assert_eq!(read.info, info);

        // A daemon with no endpoint writes a file that reads as absent, and
        // servers that only know the old format still read a file that has one.
        let without = dir.join("without-endpoint.json");
        write_atomic(&without, &info).unwrap();
        let no_endpoint = read_with_local_endpoint(&without).unwrap();
        assert!(no_endpoint.local_endpoint.is_none());
        assert_eq!(subc_transport::read(&connection_path).unwrap(), info);
    }
}

#[cfg(windows)]
mod windows {
    use super::*;

    fn bound_pipe() -> (local_listener::BoundLocalListener, String) {
        let bound = local_listener::bind(
            &LocalListenerConfig { socket_dir: None },
            std::path::Path::new("unused"),
        )
        .unwrap();
        let LocalEndpoint::Pipe { name } = bound.endpoint() else {
            panic!("a Windows listener publishes a pipe name");
        };
        (bound, name)
    }

    /// A restarted daemon must never find the previous start's pipe, which a
    /// fixed name would allow.
    // Creating a tokio named-pipe server needs a runtime to register with.
    #[tokio::test]
    async fn every_start_mints_a_fresh_pipe_name() {
        let (first, first_name) = bound_pipe();
        let (second, second_name) = bound_pipe();
        assert!(first_name.starts_with(r"\\.\pipe\"), "{first_name}");
        assert!(first_name.is_ascii(), "the pipe name is ASCII");
        assert_ne!(first_name, second_name);
        drop((first, second));
    }

    /// The SID a pipe's DACL is built from, and that a daemon can bind a pipe
    /// with it. Refusing another user is the DACL's job and is checked on a real
    /// machine; a test cannot create a second user's token.
    // Creating a tokio named-pipe server needs a runtime to register with.
    #[tokio::test]
    async fn the_pipe_is_built_from_the_current_users_sid() {
        let sid = subc_os::named_pipe::current_user_sid_string().unwrap();
        assert!(
            sid.starts_with("S-1-5-21-") || sid.starts_with("S-1-"),
            "{sid}"
        );
        let (bound, name) = bound_pipe();
        assert!(name.contains("subc-"));
        drop(bound);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_local_pipe_client_completes_the_handshake() {
        let info = test_connection_info(0);
        let (bound, name) = bound_pipe();
        let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
        let (router, _registry) = router_with_registry();
        let server = tokio::spawn(serve_local_listener(bound, router, auth));

        let mut client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .expect("open the daemon's pipe");
        authenticate_client(&mut client, &info, Duration::from_secs(5))
            .await
            .expect("the pipe handshake completes");
        // The reply after the handshake is the exchange a synchronous pipe
        // handle stalled: the daemon's write waited behind the reader's pending
        // read, and the client waited for the reply it would only send after.
        write_frame(&mut client, &ping_frame(3)).await.unwrap();
        let reply = tokio::time::timeout(Duration::from_secs(5), read_frame(&mut client))
            .await
            .expect("the daemon answers a ping over the pipe")
            .unwrap()
            .unwrap();
        assert_eq!(reply.header.ty, FrameType::Pong);
        assert_eq!(reply.header.corr, 3);
        drop(client);
        server.abort();
    }

    /// Two clients at once is what a daemon with more than one local caller
    /// looks like. Each gets its own end of the pipe name, completes its own
    /// handshake, and then gets its own reply: a client waiting behind another
    /// client's read would time out here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_pipe_clients_each_get_their_reply_after_handshaking() {
        let info = test_connection_info(0);
        let (bound, name) = bound_pipe();
        let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
        let (router, _registry) = router_with_registry();
        let server = tokio::spawn(serve_local_listener(bound, router, auth));

        let mut first = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .expect("open the daemon's pipe");
        authenticate_client(&mut first, &info, Duration::from_secs(5))
            .await
            .expect("the first client's handshake completes");
        let mut second = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .expect("a second client opens the same pipe while the first is connected");
        authenticate_client(&mut second, &info, Duration::from_secs(5))
            .await
            .expect("the second client's handshake completes");

        // Both requests are in flight before either reply is read, so neither
        // client's read can be what unblocks the other's.
        write_frame(&mut first, &ping_frame(4)).await.unwrap();
        write_frame(&mut second, &ping_frame(5)).await.unwrap();
        let (first_reply, second_reply) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(read_frame(&mut first), read_frame(&mut second))
        })
        .await
        .expect("both clients get their reply over the pipe");
        assert_eq!(first_reply.unwrap().unwrap().header.corr, 4);
        assert_eq!(second_reply.unwrap().unwrap().header.corr, 5);
        drop((first, second));
        server.abort();
    }

    /// A pipe name that another process already owns cannot be taken over: the
    /// bind fails instead of serving beside a daemon that is already there. The
    /// daemon mints a fresh name per start, so this uses a name of its own.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn binding_a_pipe_name_another_process_holds_fails() {
        let name = format!(r"\\.\pipe\subc-held-{}", std::process::id());
        let owner = subc_os::named_pipe::create_end(std::ffi::OsStr::new(&name), true)
            .expect("take the name first");
        let error = subc_os::named_pipe::create_end(std::ffi::OsStr::new(&name), true)
            .expect_err("the name is already held");
        // Win32 reports a refused first instance as access denied.
        assert_eq!(error.raw_os_error(), Some(5), "{error}");
        drop(owner);
        // The same name binds once the holder releases it, so the refusal comes
        // from the name being held and nothing else.
        local_listener::bind_named_pipe(&name).expect("the name is free again");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_module_hello_on_the_pipe_is_refused_and_leaves_no_registration() {
        let info = test_connection_info(0);
        let (bound, name) = bound_pipe();
        let auth = ServerAuth::new(info.key.clone(), info.daemon_id, info.daemon_ver.clone());
        let (router, registry) = router_with_registry();
        let server = tokio::spawn(serve_local_listener(bound, router, auth));

        let mut client = tokio::net::windows::named_pipe::ClientOptions::new()
            .open(&name)
            .expect("open the daemon's pipe");
        authenticate_client(&mut client, &info, Duration::from_secs(5))
            .await
            .unwrap();
        write_frame(
            &mut client,
            &hello_frame("pipe-module", Some("a-test-nonce")),
        )
        .await
        .unwrap();
        let reply = read_frame(&mut client).await.unwrap().unwrap();
        let error: ErrorBody = serde_json::from_slice(&reply.body).unwrap();
        assert_eq!(error.code, "local_endpoint_clients_only");
        assert!(registry.get_module("pipe-module").unwrap().is_none());
        server.abort();
    }
}
