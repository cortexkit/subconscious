use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    process,
    time::Duration,
};

use serde::{de::DeserializeOwned, Serialize};
use subc_transport::{
    authenticate_client, authenticate_server, compute_proof, generate_daemon_id, generate_key,
    read as read_connection_file, write_atomic, AuthError, AuthStage, ClientAuth, ClientHello,
    ConnectionInfo, Endpoint, ServerProof, CLIENT_AUTH_DOMAIN, MAX_AUTH_MESSAGE_LEN, NONCE_LEN,
    SCHEMA_VERSION, SERVER_PROOF_DOMAIN,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time,
};

const TEST_DEADLINE: Duration = Duration::from_millis(300);
const NO_CLIENT_AUTH_TIMEOUT: Duration = Duration::from_millis(150);
const TEST_DAEMON_VER: &str = "subc-auth-test-1";

type TestResult<T = ()> = Result<T, Box<dyn Error + Send + Sync>>;

#[tokio::test(flavor = "multi_thread")]
async fn foreign_server_reused_port_never_receives_client_auth() -> TestResult {
    let (_dir, listener, path, conn) = listener_with_connection_file("foreign").await?;
    let wrong_key = generate_key()?;
    let daemon_id = conn.daemon_id;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept foreign client");
        let hello: ClientHello = read_auth_json(&mut stream).await.expect("client hello");
        let server_nonce = [0x51; NONCE_LEN];
        let bogus_proof = compute_proof(
            &wrong_key,
            SERVER_PROOF_DOMAIN,
            &hello.client_nonce,
            &server_nonce,
            &daemon_id,
        );
        write_auth_json(
            &mut stream,
            &ServerProof {
                daemon_id,
                server_nonce,
                daemon_ver: TEST_DAEMON_VER.to_owned(),
                server_proof: bogus_proof,
            },
        )
        .await
        .expect("write bogus proof");
        assert_no_client_auth(&mut stream).await
    });

    let conn = read_connection_file(&path)?;
    let mut stream = connect_from_info(&conn).await?;
    let err = authenticate_client(&mut stream, &conn, TEST_DEADLINE)
        .await
        .expect_err("foreign server proof must fail");
    assert!(matches!(err, AuthError::InvalidServerProof));
    drop(stream);
    let no_client_auth = server.await?;
    assert!(matches!(
        no_client_auth,
        NoClientAuthObserved::Eof | NoClientAuthObserved::Timeout
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_daemon_id_aborts_without_client_auth() -> TestResult {
    let (_dir, listener, path, conn) = listener_with_connection_file("wrong-daemon").await?;
    let key = conn.key.clone();
    let foreign_daemon_id = generate_daemon_id()?;
    assert_ne!(foreign_daemon_id, conn.daemon_id);
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        let hello: ClientHello = read_auth_json(&mut stream).await.expect("client hello");
        let server_nonce = [0x62; NONCE_LEN];
        let server_proof = compute_proof(
            &key,
            SERVER_PROOF_DOMAIN,
            &hello.client_nonce,
            &server_nonce,
            &foreign_daemon_id,
        );
        write_auth_json(
            &mut stream,
            &ServerProof {
                daemon_id: foreign_daemon_id,
                server_nonce,
                daemon_ver: TEST_DAEMON_VER.to_owned(),
                server_proof,
            },
        )
        .await
        .expect("write valid proof for the wrong daemon id");
        assert_no_client_auth(&mut stream).await
    });

    let conn = read_connection_file(&path)?;
    let mut stream = connect_from_info(&conn).await?;
    let err = authenticate_client(&mut stream, &conn, TEST_DEADLINE)
        .await
        .expect_err("wrong daemon_id must fail");
    assert!(matches!(err, AuthError::DaemonIdMismatch));
    drop(stream);
    let no_client_auth = server.await?;
    assert!(matches!(
        no_client_auth,
        NoClientAuthObserved::Eof | NoClientAuthObserved::Timeout
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn bad_client_key_is_rejected_by_server() -> TestResult {
    let (_dir, listener, _path, conn) = listener_with_connection_file("bad-client-key").await?;
    let server_conn = conn.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        authenticate_server(
            &mut stream,
            &server_conn.key,
            &server_conn.daemon_id,
            &server_conn.daemon_ver,
            TEST_DEADLINE,
        )
        .await
        .expect_err("server should reject wrong client auth")
    });

    let wrong_key = generate_key()?;
    let mut stream = connect_from_info(&conn).await?;
    let client_nonce = [0x73; NONCE_LEN];
    write_auth_json(
        &mut stream,
        &ClientHello {
            client_nonce,
            role: "client".to_owned(),
        },
    )
    .await?;
    let server_proof: ServerProof = read_auth_json(&mut stream).await?;
    let client_auth = compute_proof(
        &wrong_key,
        CLIENT_AUTH_DOMAIN,
        &client_nonce,
        &server_proof.server_nonce,
        &server_proof.daemon_id,
    );
    write_auth_json(&mut stream, &ClientAuth { client_auth }).await?;

    let err = server.await?;
    assert!(matches!(err, AuthError::InvalidClientAuth));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_deadline_closes_idle_client() -> TestResult {
    let (_dir, listener, _path, conn) = listener_with_connection_file("deadline").await?;
    let server_conn = conn.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept idle client");
        authenticate_server(
            &mut stream,
            &server_conn.key,
            &server_conn.daemon_id,
            &server_conn.daemon_ver,
            TEST_DEADLINE,
        )
        .await
        .expect_err("idle client must time out")
    });

    let _stream = connect_from_info(&conn).await?;
    let err = server.await?;
    assert!(matches!(
        err,
        AuthError::Timeout {
            stage: AuthStage::ClientHello,
            ..
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn oversize_pre_auth_message_is_rejected_before_body_allocation() -> TestResult {
    let (_dir, listener, _path, conn) = listener_with_connection_file("oversize").await?;
    let server_conn = conn.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept oversize client");
        authenticate_server(
            &mut stream,
            &server_conn.key,
            &server_conn.daemon_id,
            &server_conn.daemon_ver,
            TEST_DEADLINE,
        )
        .await
        .expect_err("oversize pre-auth message must fail")
    });

    let mut stream = connect_from_info(&conn).await?;
    stream
        .write_all(&(MAX_AUTH_MESSAGE_LEN + 1).to_le_bytes())
        .await?;

    let err = server.await?;
    assert!(matches!(
        err,
        AuthError::MessageTooLarge {
            stage: AuthStage::ClientHello,
            len,
            max: MAX_AUTH_MESSAGE_LEN,
        } if len == MAX_AUTH_MESSAGE_LEN + 1
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_client_hello_returns_json_decode() -> TestResult {
    let (_dir, listener, _path, conn) = listener_with_connection_file("bad-client-hello").await?;
    let server_conn = conn.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept malformed client hello");
        authenticate_server(
            &mut stream,
            &server_conn.key,
            &server_conn.daemon_id,
            &server_conn.daemon_ver,
            TEST_DEADLINE,
        )
        .await
        .expect_err("malformed ClientHello JSON must fail")
    });

    let mut stream = connect_from_info(&conn).await?;
    let body = b"{not valid json";
    stream.write_all(&(body.len() as u32).to_le_bytes()).await?;
    stream.write_all(body).await?;

    let err = server.await?;
    assert!(matches!(
        err,
        AuthError::JsonDecode {
            stage: AuthStage::ClientHello,
            ..
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn truncated_client_hello_returns_unexpected_eof() -> TestResult {
    let (_dir, listener, _path, conn) =
        listener_with_connection_file("truncated-client-hello").await?;
    let server_conn = conn.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener
            .accept()
            .await
            .expect("accept truncated client hello");
        authenticate_server(
            &mut stream,
            &server_conn.key,
            &server_conn.daemon_id,
            &server_conn.daemon_ver,
            TEST_DEADLINE,
        )
        .await
        .expect_err("truncated ClientHello body must fail")
    });

    let mut stream = connect_from_info(&conn).await?;
    stream.write_all(&8u32.to_le_bytes()).await?;
    stream.write_all(b"{\"x").await?;
    drop(stream);

    let err = server.await?;
    assert!(matches!(
        err,
        AuthError::UnexpectedEof {
            stage: AuthStage::ClientHello,
            expected: 8,
            actual: 3,
        }
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn malformed_server_proof_aborts_without_client_auth() -> TestResult {
    let (_dir, listener, _path, conn) = listener_with_connection_file("bad-server-proof").await?;
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.expect("accept client");
        let _hello: ClientHello = read_auth_json(&mut stream).await.expect("client hello");
        let body = b"{malformed server proof";
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .expect("write malformed ServerProof length");
        stream
            .write_all(body)
            .await
            .expect("write malformed ServerProof body");
        assert_no_client_auth(&mut stream).await
    });

    let mut stream = connect_from_info(&conn).await?;
    let err = authenticate_client(&mut stream, &conn, TEST_DEADLINE)
        .await
        .expect_err("malformed ServerProof JSON must fail");
    assert!(matches!(
        err,
        AuthError::JsonDecode {
            stage: AuthStage::ServerProof,
            ..
        }
    ));
    drop(stream);

    let no_client_auth = server.await?;
    assert!(matches!(
        no_client_auth,
        NoClientAuthObserved::Eof | NoClientAuthObserved::Timeout
    ));
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn key_rotation_rejects_old_connection_file_then_accepts_reread_file() -> TestResult {
    let dir = TestDir::new("rotation")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let path = dir.path().join("subc-conn.json");

    let old_conn = make_connection_info(port, generate_key()?, generate_daemon_id()?);
    write_atomic(&path, &old_conn)?;

    let new_conn = make_connection_info(port, generate_key()?, generate_daemon_id()?);
    assert_ne!(old_conn.daemon_id, new_conn.daemon_id);
    assert!(old_conn.key != new_conn.key, "rotated key should differ");
    write_atomic(&path, &new_conn)?;

    let server_conn = new_conn.clone();
    let server = tokio::spawn(async move {
        let mut results = Vec::new();
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().await.expect("accept rotation client");
            results.push(
                authenticate_server(
                    &mut stream,
                    &server_conn.key,
                    &server_conn.daemon_id,
                    &server_conn.daemon_ver,
                    TEST_DEADLINE,
                )
                .await,
            );
        }
        results
    });

    let mut old_stream = connect_from_info(&old_conn).await?;
    let old_err = authenticate_client(&mut old_stream, &old_conn, TEST_DEADLINE)
        .await
        .expect_err("old key must reject the new daemon proof");
    assert!(matches!(old_err, AuthError::InvalidServerProof));
    drop(old_stream);

    let reread_conn = read_connection_file(&path)?;
    assert_eq!(reread_conn.daemon_id, new_conn.daemon_id);
    assert!(
        reread_conn.key == new_conn.key,
        "reread connection file should contain the rotated key"
    );
    let mut new_stream = connect_from_info(&reread_conn).await?;
    authenticate_client(&mut new_stream, &reread_conn, TEST_DEADLINE).await?;

    let results = server.await?;
    assert_eq!(results.len(), 2);
    assert!(matches!(
        &results[0],
        Err(AuthError::UnexpectedEof { .. }) | Err(AuthError::Timeout { .. })
    ));
    assert_eq!(
        results[1]
            .as_ref()
            .expect("new key should authenticate")
            .role,
        "client"
    );
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

async fn read_auth_json<T>(stream: &mut TcpStream) -> TestResult<T>
where
    T: DeserializeOwned,
{
    let mut len_bytes = [0u8; 4];
    stream.read_exact(&mut len_bytes).await?;
    let len = u32::from_le_bytes(len_bytes);
    assert!(
        len <= MAX_AUTH_MESSAGE_LEN,
        "test helper received auth message over cap"
    );
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

async fn write_auth_json<T>(stream: &mut TcpStream, value: &T) -> TestResult
where
    T: Serialize,
{
    let body = serde_json::to_vec(value)?;
    assert!(
        body.len() <= MAX_AUTH_MESSAGE_LEN as usize,
        "test helper auth message over cap"
    );
    stream.write_all(&(body.len() as u32).to_le_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoClientAuthObserved {
    Eof,
    Timeout,
}

async fn assert_no_client_auth(stream: &mut TcpStream) -> NoClientAuthObserved {
    let mut byte = [0u8; 1];
    match time::timeout(NO_CLIENT_AUTH_TIMEOUT, stream.read(&mut byte)).await {
        Err(_) => NoClientAuthObserved::Timeout,
        Ok(Ok(0)) => NoClientAuthObserved::Eof,
        Ok(Ok(read)) => panic!("client sent ClientAuth prelude ({read} byte(s) readable)"),
        Ok(Err(err)) if err.kind() == io::ErrorKind::ConnectionReset => NoClientAuthObserved::Eof,
        Ok(Err(err)) => panic!("failed while checking for absent ClientAuth: {err}"),
    }
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
