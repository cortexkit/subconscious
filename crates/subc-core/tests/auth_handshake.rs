use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use subc_core::{read_frame, write_frame, Frame};
use subc_protocol::{Flags, FrameType, Priority};
use subc_transport::{
    authenticate_client, authenticate_server, generate_daemon_id, generate_key,
    read_for_client as read_connection_file, write_atomic, ConnectionInfo, Endpoint,
    SCHEMA_VERSION,
};
use tokio::net::{TcpListener, TcpStream};

const TEST_DEADLINE: Duration = Duration::from_millis(300);
const TEST_DAEMON_VER: &str = "subc-auth-test-1";

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread")]
async fn happy_path_authenticates_then_round_trips_envelope_frame() -> TestResult {
    let (_dir, listener, path, conn) = listener_with_connection_file("happy").await?;
    let server_conn = conn.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let authenticated = authenticate_server(
            &mut stream,
            &server_conn.key,
            &server_conn.daemon_id,
            &server_conn.daemon_ver,
            TEST_DEADLINE,
        )
        .await?;
        assert_eq!(authenticated.role, "client");

        let frame = read_frame(&mut stream)
            .await?
            .expect("client should write a post-auth frame");
        write_frame(&mut stream, &frame).await?;
        TestResult::Ok(frame)
    });

    let conn = read_connection_file(&path)?;
    let mut stream = connect_from_info(&conn).await?;
    authenticate_client(&mut stream, &conn, TEST_DEADLINE).await?;

    let frame = Frame::build(
        FrameType::Request,
        Flags::new(true, Priority::Interactive, true),
        7,
        0,
        42,
        b"opaque post-auth envelope body".to_vec(),
    )?;
    write_frame(&mut stream, &frame).await?;
    let echoed = read_frame(&mut stream)
        .await?
        .expect("server should echo the post-auth frame");
    assert_eq!(echoed, frame);
    assert_eq!(server.await??, frame);
    Ok(())
}

async fn listener_with_connection_file(
    name: &str,
) -> TestResult<(TestDir, TcpListener, PathBuf, ConnectionInfo)> {
    let dir = TestDir::new(name)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let conn = make_connection_info(port, generate_key()?, generate_daemon_id()?);
    let path = dir.path().join("subc-conn.json");
    write_atomic(&path, &conn)?;
    Ok((dir, listener, path, conn))
}

fn make_connection_info(port: u16, key: Vec<u8>, daemon_id: [u8; 16]) -> ConnectionInfo {
    ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: "127.0.0.1".to_owned(),
            port,
        }],
        key,
        daemon_id,
        pid: process::id(),
        daemon_ver: TEST_DAEMON_VER.to_owned(),
    }
}

async fn connect_from_info(conn: &ConnectionInfo) -> io::Result<TcpStream> {
    let endpoint = conn
        .endpoints
        .first()
        .expect("test connection file should have an endpoint");
    TcpStream::connect((endpoint.host.as_str(), endpoint.port)).await
}

struct TestDir {
    path: PathBuf,
}

impl TestDir {
    fn new(name: &str) -> TestResult<Self> {
        let suffix = generate_daemon_id()?;
        let path = std::env::temp_dir().join(format!(
            "subc-auth-handshake-{name}-{}-{}",
            process::id(),
            hex(&suffix)
        ));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}
