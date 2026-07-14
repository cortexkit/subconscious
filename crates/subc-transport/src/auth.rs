use std::{error::Error, fmt, future::Future, io, time::Duration};

use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time,
};

use crate::connection_file::{ConnectionInfo, DAEMON_ID_LEN, MIN_KEY_LEN};

pub const NONCE_LEN: usize = 32;
pub const PROOF_LEN: usize = 32;
pub const MAX_AUTH_MESSAGE_LEN: u32 = 4096;
pub const SERVER_PROOF_DOMAIN: &str = "subc-server-v1";
pub const CLIENT_AUTH_DOMAIN: &str = "subc-client-v1";
pub const DEFAULT_CLIENT_ROLE: &str = "client";
pub const WATCHDOG_CLIENT_ROLE: &str = "watchdog";

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientHello {
    pub client_nonce: [u8; NONCE_LEN],
    pub role: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerProof {
    pub daemon_id: [u8; DAEMON_ID_LEN],
    pub server_nonce: [u8; NONCE_LEN],
    pub daemon_ver: String,
    pub server_proof: [u8; PROOF_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientAuth {
    pub client_auth: [u8; PROOF_LEN],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Authenticated {
    pub role: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStage {
    ClientHello,
    ServerProof,
    ClientAuth,
}

#[derive(Debug)]
pub enum AuthError {
    Io {
        stage: AuthStage,
        source: io::Error,
    },
    Timeout {
        stage: AuthStage,
        deadline: Duration,
    },
    UnexpectedEof {
        stage: AuthStage,
        expected: usize,
        actual: usize,
    },
    MessageTooLarge {
        stage: AuthStage,
        len: u32,
        max: u32,
    },
    JsonEncode {
        stage: AuthStage,
        source: serde_json::Error,
    },
    JsonDecode {
        stage: AuthStage,
        source: serde_json::Error,
    },
    Random(getrandom::Error),
    KeyTooShort {
        len: usize,
        min: usize,
    },
    InvalidServerProof,
    DaemonIdMismatch,
    InvalidClientAuth,
}

pub fn compute_proof(
    key: &[u8],
    domain: &str,
    client_nonce: &[u8; NONCE_LEN],
    server_nonce: &[u8; NONCE_LEN],
    daemon_id: &[u8],
) -> [u8; PROOF_LEN] {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts keys of any length");
    mac.update(domain.as_bytes());
    mac.update(client_nonce);
    mac.update(server_nonce);
    mac.update(daemon_id);
    mac.finalize().into_bytes().into()
}

/// An absolute handshake deadline. Every per-stage read/write recomputes the time
/// remaining until `at`, so the WHOLE handshake (length byte + body, across all
/// stages, plus error teardown) is bounded by a single wall-clock budget. Passing
/// a bare `Duration` to each step instead would let a slow peer spend the full
/// budget on every length read AND every body read — multiplying the real bound.
#[derive(Clone, Copy)]
struct Deadline {
    at: time::Instant,
    total: Duration,
}

impl Deadline {
    fn starting_now(total: Duration) -> Self {
        Self {
            at: time::Instant::now() + total,
            total,
        }
    }

    /// Time left until the deadline, or `Timeout` if it has already elapsed.
    fn remaining(&self, stage: AuthStage) -> Result<Duration, AuthError> {
        let remaining = self.at.saturating_duration_since(time::Instant::now());
        if remaining.is_zero() {
            Err(AuthError::Timeout {
                stage,
                deadline: self.total,
            })
        } else {
            Ok(remaining)
        }
    }

    /// Time left until the deadline, clamped to zero — for best-effort teardown
    /// that must not outlive the handshake budget.
    fn remaining_or_zero(&self) -> Duration {
        self.at.saturating_duration_since(time::Instant::now())
    }
}

pub async fn authenticate_server<S>(
    stream: &mut S,
    key: &[u8],
    daemon_id: &[u8; DAEMON_ID_LEN],
    daemon_ver: &str,
    deadline: Duration,
) -> Result<Authenticated, AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = Deadline::starting_now(deadline);
    let result = authenticate_server_inner(stream, key, daemon_id, daemon_ver, deadline).await;
    if result.is_err() {
        // Bound teardown by the SAME absolute deadline so a failed handshake (and
        // the unauthenticated-handshake slot it holds) is released promptly instead
        // of waiting out another full budget.
        let _ = time::timeout(deadline.remaining_or_zero(), stream.shutdown()).await;
    }
    result
}

async fn authenticate_server_inner<S>(
    stream: &mut S,
    key: &[u8],
    daemon_id: &[u8; DAEMON_ID_LEN],
    daemon_ver: &str,
    deadline: Deadline,
) -> Result<Authenticated, AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    validate_key(key)?;

    let hello: ClientHello = read_message(stream, AuthStage::ClientHello, deadline).await?;
    let server_nonce = random_nonce()?;
    let server_proof = compute_proof(
        key,
        SERVER_PROOF_DOMAIN,
        &hello.client_nonce,
        &server_nonce,
        daemon_id,
    );

    write_message(
        stream,
        AuthStage::ServerProof,
        &ServerProof {
            daemon_id: *daemon_id,
            server_nonce,
            daemon_ver: daemon_ver.to_owned(),
            server_proof,
        },
        deadline,
    )
    .await?;

    let client_auth: ClientAuth = read_message(stream, AuthStage::ClientAuth, deadline).await?;
    let expected_client_auth = compute_proof(
        key,
        CLIENT_AUTH_DOMAIN,
        &hello.client_nonce,
        &server_nonce,
        daemon_id,
    );
    if !constant_time_eq(&expected_client_auth, &client_auth.client_auth) {
        return Err(AuthError::InvalidClientAuth);
    }

    Ok(Authenticated { role: hello.role })
}

pub async fn authenticate_client<S>(
    stream: &mut S,
    conn: &ConnectionInfo,
    deadline: Duration,
) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    authenticate_client_with_role(stream, conn, deadline, DEFAULT_CLIENT_ROLE).await
}

pub async fn authenticate_client_with_role<S>(
    stream: &mut S,
    conn: &ConnectionInfo,
    deadline: Duration,
    role: &str,
) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let deadline = Deadline::starting_now(deadline);
    let result = authenticate_client_inner(stream, conn, deadline, role).await;
    if result.is_err() {
        let _ = time::timeout(deadline.remaining_or_zero(), stream.shutdown()).await;
    }
    result
}

async fn authenticate_client_inner<S>(
    stream: &mut S,
    conn: &ConnectionInfo,
    deadline: Deadline,
    role: &str,
) -> Result<(), AuthError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    validate_key(&conn.key)?;

    let client_nonce = random_nonce()?;
    write_message(
        stream,
        AuthStage::ClientHello,
        &ClientHello {
            client_nonce,
            role: role.to_owned(),
        },
        deadline,
    )
    .await?;

    let server_proof: ServerProof = read_message(stream, AuthStage::ServerProof, deadline).await?;
    let expected_server_proof = compute_proof(
        &conn.key,
        SERVER_PROOF_DOMAIN,
        &client_nonce,
        &server_proof.server_nonce,
        &server_proof.daemon_id,
    );
    if !constant_time_eq(&expected_server_proof, &server_proof.server_proof) {
        return Err(AuthError::InvalidServerProof);
    }
    if server_proof.daemon_id != conn.daemon_id {
        return Err(AuthError::DaemonIdMismatch);
    }

    let client_auth = compute_proof(
        &conn.key,
        CLIENT_AUTH_DOMAIN,
        &client_nonce,
        &server_proof.server_nonce,
        &server_proof.daemon_id,
    );
    write_message(
        stream,
        AuthStage::ClientAuth,
        &ClientAuth { client_auth },
        deadline,
    )
    .await
}

fn validate_key(key: &[u8]) -> Result<(), AuthError> {
    if key.len() < MIN_KEY_LEN {
        return Err(AuthError::KeyTooShort {
            len: key.len(),
            min: MIN_KEY_LEN,
        });
    }
    Ok(())
}

fn random_nonce() -> Result<[u8; NONCE_LEN], AuthError> {
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::getrandom(&mut nonce).map_err(AuthError::Random)?;
    Ok(nonce)
}

fn constant_time_eq(expected: &[u8; PROOF_LEN], actual: &[u8; PROOF_LEN]) -> bool {
    expected.as_slice().ct_eq(actual.as_slice()).into()
}

async fn read_message<S, T>(
    stream: &mut S,
    stage: AuthStage,
    deadline: Deadline,
) -> Result<T, AuthError>
where
    S: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    // Both the length read and the body read recompute the time remaining against
    // the same absolute deadline, so the two together cannot exceed the budget.
    let mut len_bytes = [0u8; 4];
    read_exact_deadline(stream, &mut len_bytes, stage, deadline).await?;
    let len = u32::from_le_bytes(len_bytes);
    if len > MAX_AUTH_MESSAGE_LEN {
        return Err(AuthError::MessageTooLarge {
            stage,
            len,
            max: MAX_AUTH_MESSAGE_LEN,
        });
    }

    let mut json = vec![0u8; len as usize];
    if !json.is_empty() {
        read_exact_deadline(stream, &mut json, stage, deadline).await?;
    }
    serde_json::from_slice(&json).map_err(|source| AuthError::JsonDecode { stage, source })
}

async fn write_message<S, T>(
    stream: &mut S,
    stage: AuthStage,
    value: &T,
    deadline: Deadline,
) -> Result<(), AuthError>
where
    S: AsyncWrite + Unpin,
    T: Serialize,
{
    let json =
        serde_json::to_vec(value).map_err(|source| AuthError::JsonEncode { stage, source })?;
    let len = u32::try_from(json.len()).map_err(|_| AuthError::MessageTooLarge {
        stage,
        len: u32::MAX,
        max: MAX_AUTH_MESSAGE_LEN,
    })?;
    if len > MAX_AUTH_MESSAGE_LEN {
        return Err(AuthError::MessageTooLarge {
            stage,
            len,
            max: MAX_AUTH_MESSAGE_LEN,
        });
    }

    write_all_deadline(stream, &len.to_le_bytes(), stage, deadline).await?;
    write_all_deadline(stream, &json, stage, deadline).await
}

async fn read_exact_deadline<S>(
    stream: &mut S,
    buf: &mut [u8],
    stage: AuthStage,
    deadline: Deadline,
) -> Result<(), AuthError>
where
    S: AsyncRead + Unpin,
{
    let remaining = deadline.remaining(stage)?;
    let expected = buf.len();
    with_timeout(stage, remaining, async {
        let mut actual = 0;
        while actual < expected {
            let read = stream.read(&mut buf[actual..]).await?;
            if read == 0 {
                return Err(ReadExactError::UnexpectedEof { actual });
            }
            actual += read;
        }
        Ok(())
    })
    .await
    .map_err(|err| match err {
        DeadlineIoError::Io(source) => AuthError::Io { stage, source },
        DeadlineIoError::Timeout => AuthError::Timeout {
            stage,
            deadline: deadline.total,
        },
        DeadlineIoError::UnexpectedEof { actual } => AuthError::UnexpectedEof {
            stage,
            expected,
            actual,
        },
    })
}

async fn write_all_deadline<S>(
    stream: &mut S,
    buf: &[u8],
    stage: AuthStage,
    deadline: Deadline,
) -> Result<(), AuthError>
where
    S: AsyncWrite + Unpin,
{
    let remaining = deadline.remaining(stage)?;
    timeout_io(stage, remaining, deadline.total, stream.write_all(buf)).await
}

async fn timeout_io<T, F>(
    stage: AuthStage,
    remaining: Duration,
    total: Duration,
    future: F,
) -> Result<T, AuthError>
where
    F: Future<Output = io::Result<T>>,
{
    match time::timeout(remaining, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(source)) => Err(AuthError::Io { stage, source }),
        Err(_) => Err(AuthError::Timeout {
            stage,
            deadline: total,
        }),
    }
}

async fn with_timeout<F>(
    _stage: AuthStage,
    deadline: Duration,
    future: F,
) -> Result<(), DeadlineIoError>
where
    F: Future<Output = Result<(), ReadExactError>>,
{
    match time::timeout(deadline, future).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(ReadExactError::Io(source))) => Err(DeadlineIoError::Io(source)),
        Ok(Err(ReadExactError::UnexpectedEof { actual })) => {
            Err(DeadlineIoError::UnexpectedEof { actual })
        }
        Err(_) => Err(DeadlineIoError::Timeout),
    }
}

#[derive(Debug)]
enum ReadExactError {
    Io(io::Error),
    UnexpectedEof { actual: usize },
}

impl From<io::Error> for ReadExactError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

#[derive(Debug)]
enum DeadlineIoError {
    Io(io::Error),
    Timeout,
    UnexpectedEof { actual: usize },
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { stage, source } => write!(f, "auth {stage:?} I/O error: {source}"),
            Self::Timeout { stage, deadline } => {
                write!(f, "auth {stage:?} timed out after {deadline:?}")
            }
            Self::UnexpectedEof {
                stage,
                expected,
                actual,
            } => write!(
                f,
                "auth {stage:?} ended early: expected {expected} bytes, got {actual}"
            ),
            Self::MessageTooLarge { stage, len, max } => write!(
                f,
                "auth {stage:?} message length {len} exceeds hard cap {max}"
            ),
            Self::JsonEncode { stage, source } => {
                write!(f, "auth {stage:?} JSON encode error: {source}")
            }
            Self::JsonDecode { stage, source } => {
                write!(f, "auth {stage:?} JSON decode error: {source}")
            }
            Self::Random(source) => write!(f, "auth random generation failed: {source}"),
            Self::KeyTooShort { len, min } => {
                write!(f, "auth key is too short: {len} bytes, need at least {min}")
            }
            Self::InvalidServerProof => write!(f, "invalid server auth proof"),
            Self::DaemonIdMismatch => write!(f, "server daemon_id did not match connection file"),
            Self::InvalidClientAuth => write!(f, "invalid client auth proof"),
        }
    }
}

impl Error for AuthError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::JsonEncode { source, .. } | Self::JsonDecode { source, .. } => Some(source),
            Self::Random(_) => None,
            Self::Timeout { .. }
            | Self::UnexpectedEof { .. }
            | Self::MessageTooLarge { .. }
            | Self::KeyTooShort { .. }
            | Self::InvalidServerProof
            | Self::DaemonIdMismatch
            | Self::InvalidClientAuth => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Endpoint;
    use tokio::{
        io::{duplex, AsyncReadExt, AsyncWriteExt, DuplexStream},
        task::yield_now,
        time::advance,
    };

    const TEST_DAEMON_VER: &str = "subc-auth-test-1";

    #[tokio::test(start_paused = true)]
    async fn authenticate_server_deadline_is_absolute_across_handshake() {
        let key = vec![0x5a; MIN_KEY_LEN];
        let daemon_id = [0x6b; DAEMON_ID_LEN];
        let deadline = Duration::from_millis(100);
        let stage_delay = Duration::from_millis(60);
        let (mut client, mut server) = duplex(4096);

        let server_task = tokio::spawn(async move {
            authenticate_server(&mut server, &key, &daemon_id, TEST_DAEMON_VER, deadline).await
        });

        yield_now().await;
        assert!(!server_task.is_finished());

        advance(stage_delay).await;
        write_auth_json(
            &mut client,
            &ClientHello {
                client_nonce: [0x11; NONCE_LEN],
                role: DEFAULT_CLIENT_ROLE.to_owned(),
            },
        )
        .await;
        yield_now().await;

        let server_proof: ServerProof = read_auth_json(&mut client).await;
        assert_eq!(server_proof.daemon_id, daemon_id);
        assert_eq!(server_proof.daemon_ver, TEST_DAEMON_VER);
        assert!(!server_task.is_finished());

        advance(stage_delay).await;
        yield_now().await;
        assert!(server_task.is_finished());

        let err = server_task
            .await
            .expect("server task should join")
            .expect_err("server handshake should time out once the total deadline elapses");
        assert!(matches!(
            err,
            AuthError::Timeout {
                stage: AuthStage::ClientAuth,
                ..
            }
        ));
    }

    /// Write only the 4-byte length prefix of an auth message, withholding the
    /// body — used to stall a peer mid-message so the within-stage deadline can be
    /// exercised (the length read and body read must share one absolute budget).
    async fn write_auth_len_only<T>(stream: &mut DuplexStream, value: &T)
    where
        T: Serialize,
    {
        let body = serde_json::to_vec(value).expect("encode auth json");
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .expect("write auth length");
    }

    #[tokio::test(start_paused = true)]
    async fn server_deadline_spans_length_and_body_within_one_stage() {
        // The bug this guards: applying the timeout independently to the length
        // read and the body read lets a single stage consume ~2x the budget. Here
        // the client sends the ClientHello length prefix late, then withholds the
        // body until the absolute deadline has passed. With one shared deadline the
        // server times out at the deadline; with per-read deadlines the body read
        // would get a fresh full window and not time out yet.
        let key = vec![0x5a; MIN_KEY_LEN];
        let daemon_id = [0x6b; DAEMON_ID_LEN];
        let deadline = Duration::from_millis(100);
        let (mut client, mut server) = duplex(4096);

        let server_task = tokio::spawn(async move {
            authenticate_server(&mut server, &key, &daemon_id, TEST_DAEMON_VER, deadline).await
        });

        yield_now().await;
        // Burn most of the budget, then send only the length prefix.
        advance(Duration::from_millis(60)).await;
        write_auth_len_only(
            &mut client,
            &ClientHello {
                client_nonce: [0x11; NONCE_LEN],
                role: DEFAULT_CLIENT_ROLE.to_owned(),
            },
        )
        .await;
        yield_now().await;
        assert!(!server_task.is_finished());

        // Cross the absolute deadline (60 + 50 > 100) without sending the body.
        advance(Duration::from_millis(50)).await;
        yield_now().await;
        assert!(
            server_task.is_finished(),
            "body read must share the handshake deadline, not get a fresh window"
        );
        let err = server_task
            .await
            .expect("join")
            .expect_err("must time out at ClientHello body");
        assert!(matches!(
            err,
            AuthError::Timeout {
                stage: AuthStage::ClientHello,
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn client_deadline_is_absolute() {
        // The client previously passed the full deadline to each stage with no
        // absolute bound. Here the server stalls the ServerProof body past the
        // deadline; the client must time out at the budget, not wait a fresh window.
        let key = vec![0x5a; MIN_KEY_LEN];
        let daemon_id = [0x6b; DAEMON_ID_LEN];
        let deadline = Duration::from_millis(100);
        let conn = ConnectionInfo {
            schema: 1,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "127.0.0.1".to_owned(),
                port: 1,
            }],
            key: key.clone(),
            daemon_id,
            pid: 1,
            daemon_ver: TEST_DAEMON_VER.to_owned(),
        };

        let (mut server, mut client) = duplex(4096);
        let client_task =
            tokio::spawn(async move { authenticate_client(&mut client, &conn, deadline).await });

        // Read the client's ClientHello so its write completes.
        let _hello: ClientHello = read_auth_json(&mut server).await;
        yield_now().await;
        assert!(!client_task.is_finished());

        // Send only the ServerProof length prefix, then stall the body past the
        // absolute deadline.
        advance(Duration::from_millis(60)).await;
        let server_nonce = [0x22; NONCE_LEN];
        let server_proof = compute_proof(
            &key,
            SERVER_PROOF_DOMAIN,
            &[0u8; NONCE_LEN],
            &server_nonce,
            &daemon_id,
        );
        write_auth_len_only(
            &mut server,
            &ServerProof {
                daemon_id,
                server_nonce,
                daemon_ver: TEST_DAEMON_VER.to_owned(),
                server_proof,
            },
        )
        .await;
        yield_now().await;
        advance(Duration::from_millis(50)).await;
        yield_now().await;
        assert!(
            client_task.is_finished(),
            "client must bound the whole handshake by one absolute deadline"
        );
        let err = client_task
            .await
            .expect("join")
            .expect_err("client must time out");
        assert!(matches!(err, AuthError::Timeout { .. }));
    }

    async fn read_auth_json<T>(stream: &mut DuplexStream) -> T
    where
        T: DeserializeOwned,
    {
        let mut len_bytes = [0u8; 4];
        stream
            .read_exact(&mut len_bytes)
            .await
            .expect("read auth length");
        let len = u32::from_le_bytes(len_bytes);
        assert!(
            len <= MAX_AUTH_MESSAGE_LEN,
            "test helper received auth message over cap"
        );
        let mut body = vec![0u8; len as usize];
        stream.read_exact(&mut body).await.expect("read auth body");
        serde_json::from_slice(&body).expect("decode auth json")
    }

    async fn write_auth_json<T>(stream: &mut DuplexStream, value: &T)
    where
        T: Serialize,
    {
        let body = serde_json::to_vec(value).expect("encode auth json");
        assert!(
            body.len() <= MAX_AUTH_MESSAGE_LEN as usize,
            "test helper auth message over cap"
        );
        stream
            .write_all(&(body.len() as u32).to_le_bytes())
            .await
            .expect("write auth length");
        stream.write_all(&body).await.expect("write auth body");
    }
}
