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
    let result = authenticate_server_inner(stream, key, daemon_id, daemon_ver, deadline).await;
    if result.is_err() {
        let _ = time::timeout(deadline, stream.shutdown()).await;
    }
    result
}

async fn authenticate_server_inner<S>(
    stream: &mut S,
    key: &[u8],
    daemon_id: &[u8; DAEMON_ID_LEN],
    daemon_ver: &str,
    deadline: Duration,
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
    let result = authenticate_client_inner(stream, conn, deadline).await;
    if result.is_err() {
        let _ = time::timeout(deadline, stream.shutdown()).await;
    }
    result
}

async fn authenticate_client_inner<S>(
    stream: &mut S,
    conn: &ConnectionInfo,
    deadline: Duration,
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
            role: DEFAULT_CLIENT_ROLE.to_owned(),
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
    deadline: Duration,
) -> Result<T, AuthError>
where
    S: AsyncRead + Unpin,
    T: DeserializeOwned,
{
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
    deadline: Duration,
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
    deadline: Duration,
) -> Result<(), AuthError>
where
    S: AsyncRead + Unpin,
{
    let expected = buf.len();
    with_timeout(stage, deadline, async {
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
        DeadlineIoError::Timeout => AuthError::Timeout { stage, deadline },
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
    deadline: Duration,
) -> Result<(), AuthError>
where
    S: AsyncWrite + Unpin,
{
    timeout_io(stage, deadline, stream.write_all(buf)).await
}

async fn timeout_io<T, F>(stage: AuthStage, deadline: Duration, future: F) -> Result<T, AuthError>
where
    F: Future<Output = io::Result<T>>,
{
    match time::timeout(deadline, future).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(source)) => Err(AuthError::Io { stage, source }),
        Err(_) => Err(AuthError::Timeout { stage, deadline }),
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
