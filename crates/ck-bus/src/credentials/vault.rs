//! The credentials area's seam: signing with, and reading the public half of, a vault
//! root. Production reaches Claustrum over a subc route; the acceptance harness serves
//! the same wire from throwaway keys.

use std::{env, ffi::OsString, fmt, path::PathBuf};

use async_trait::async_trait;
use subc_client_rs::{
    consumer::{CallError, CallOptions, ConsumerOptions, SubcConsumer},
    ConsumerIdentity,
};
use subc_protocol::{BindIdentity, RouteTarget, SUBC_MODULE_ID_ENV};
use tokio::sync::OnceCell;

use super::wire::{self, ReplyError, VaultPublicKey, VaultSignature};

/// The vault's module id; acceptance registers its harness modules under the same id.
pub const CLAUSTRUM_MODULE_ID: &str = "claustrum";

/// The recorded condition name for a root the vault answers `not_found` for.
pub const ROOT_KEY_UNREACHABLE: &str = "root-key-unreachable";

const LAUNCH_NONCE_ENV: &str = "SUBC_LAUNCH_NONCE";

/// Whether a failed call is worth retrying on the next sentinel period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Retry {
    Retryable,
    Terminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VaultError {
    /// The vault answered `not_found`. That is byte-identical for a missing key, a
    /// missing grant, and a route that arrived without ConsumerIdentity (as `Direct`),
    /// so the message names every cause.
    RootKeyUnreachable { credential_id: String },
    /// Any other vault refusal, with its code and error class.
    Refused { code: String, class: Option<String> },
    /// The route to the vault failed or was refused before the vault answered.
    Route {
        code: Option<String>,
        retry: Retry,
        message: String,
    },
    /// The vault answered outside the recorded wire shape.
    Malformed(String),
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RootKeyUnreachable { credential_id } => write!(
                f,
                "{ROOT_KEY_UNREACHABLE}: the vault answered not_found for {credential_id}: \
                 the key or ck-bus's exact grant on it is missing, or the route was opened \
                 without ConsumerIdentity and arrived as Direct, which the vault answers the \
                 same way"
            ),
            Self::Refused { code, class } => match class {
                Some(class) => write!(f, "vault refused: {code} ({class})"),
                None => write!(f, "vault refused: {code}"),
            },
            Self::Route {
                code,
                retry,
                message,
            } => write!(
                f,
                "vault route failed ({}, {retry:?}): {message}",
                code.as_deref().unwrap_or("no code")
            ),
            Self::Malformed(detail) => write!(f, "vault reply malformed: {detail}"),
        }
    }
}

impl VaultError {
    fn from_reply(credential_id: &str, error: ReplyError) -> Self {
        match error {
            ReplyError::Refused { code, .. } if code == "not_found" => Self::RootKeyUnreachable {
                credential_id: credential_id.to_string(),
            },
            ReplyError::Refused { code, class } => Self::Refused { code, class },
            ReplyError::Malformed(detail) => Self::Malformed(detail),
        }
    }
}

/// Signing and public-key reads against vault roots, by credential id.
#[async_trait]
pub trait VaultSigning: Send + Sync {
    async fn sign(&self, credential_id: &str, payload: &[u8])
        -> Result<VaultSignature, VaultError>;
    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError>;
}

/// `credential.sign` and `credential.public_key` over a subc route to `claustrum`.
///
/// Every route carries `ConsumerIdentity { module_id, launch_nonce }` when both are
/// known, which is what makes the daemon stamp `Principal::Reserved`. The consumer
/// connects on first use, so constructing this does no I/O.
pub struct ClaustrumRoute {
    connection_file: PathBuf,
    consumer_identity: Option<ConsumerIdentity>,
    bind: BindIdentity,
    consumer: OnceCell<SubcConsumer>,
}

impl ClaustrumRoute {
    pub fn new(connection_file: PathBuf, consumer_identity: Option<ConsumerIdentity>) -> Self {
        let bind = BindIdentity::new(
            connection_file
                .parent()
                .map(PathBuf::from)
                .unwrap_or_default(),
            "ck-bus",
            "credentials",
        );
        Self {
            connection_file,
            consumer_identity,
            bind,
            consumer: OnceCell::new(),
        }
    }

    /// The route a supervised ck-bus uses: the daemon's connection file from `--subc`,
    /// and the identity from `SUBC_MODULE_ID` and `SUBC_LAUNCH_NONCE` exactly as the
    /// daemon injected them. A missing nonce is not synthesised; the route then carries
    /// no identity and the vault's `not_found` names that cause.
    pub fn supervised() -> Result<Self, String> {
        let connection_file = subc_arg(env::args_os()).ok_or_else(|| {
            "ck-bus needs --subc <connection file> to reach the vault".to_string()
        })?;
        let identity = match (env::var(SUBC_MODULE_ID_ENV), env::var(LAUNCH_NONCE_ENV)) {
            (Ok(module_id), Ok(launch_nonce))
                if !module_id.is_empty() && !launch_nonce.is_empty() =>
            {
                Some(ConsumerIdentity {
                    module_id,
                    launch_nonce,
                })
            }
            _ => None,
        };
        Ok(Self::new(connection_file, identity))
    }

    async fn call(&self, body: Vec<u8>) -> Result<Vec<u8>, VaultError> {
        let consumer = self
            .consumer
            .get_or_try_init(|| async {
                SubcConsumer::connect(&self.connection_file, ConsumerOptions::default()).await
            })
            .await
            .map_err(|error| VaultError::Route {
                code: None,
                retry: Retry::Retryable,
                message: format!("cannot reach the daemon: {error}"),
            })?;
        let options = CallOptions {
            consumer_identity: self.consumer_identity.clone(),
            ..CallOptions::default()
        };
        consumer
            .call(
                // Claustrum registers its read surface as a management surface
                // (`credentials-module/src/main.rs::manifest` at 57a501b), not a tool
                // provider, and the daemon routes by role.
                RouteTarget::ManagementSurface {
                    module_id: CLAUSTRUM_MODULE_ID.to_string(),
                },
                self.bind.clone(),
                body,
                options,
            )
            .await
            .map_err(classify_call_error)
    }
}

/// `target_unavailable` and `module_warming` clear on their own and are retried on the
/// sentinel period; `module_no_protocol` and anything else unnamed are terminal.
fn classify_call_error(error: CallError) -> VaultError {
    let code = error
        .route_open_refusal()
        .map(|body| body.code.clone())
        .or_else(|| error.code().map(str::to_string));
    let retry = match code.as_deref() {
        Some("target_unavailable" | "module_warming") => Retry::Retryable,
        Some(_) => Retry::Terminal,
        None if matches!(error, CallError::NotSent(_)) => Retry::Retryable,
        None => Retry::Terminal,
    };
    VaultError::Route {
        code,
        retry,
        message: error.to_string(),
    }
}

/// The daemon connection file from `--subc <path>` or `--subc=<path>`.
pub fn subc_arg(args: impl IntoIterator<Item = OsString>) -> Option<PathBuf> {
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--subc" {
            return args.next().map(PathBuf::from);
        }
        if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--subc=")) {
            return Some(PathBuf::from(value));
        }
    }
    None
}

#[async_trait]
impl VaultSigning for ClaustrumRoute {
    async fn sign(
        &self,
        credential_id: &str,
        payload: &[u8],
    ) -> Result<VaultSignature, VaultError> {
        let body = wire::sign_request(credential_id, payload)
            .map_err(|error| VaultError::from_reply(credential_id, error))?;
        let reply = self.call(body).await?;
        wire::parse_sign_reply(&reply).map_err(|error| VaultError::from_reply(credential_id, error))
    }

    async fn public_key(&self, credential_id: &str) -> Result<VaultPublicKey, VaultError> {
        let reply = self.call(wire::public_key_request(credential_id)).await?;
        wire::parse_public_key_reply(&reply)
            .map_err(|error| VaultError::from_reply(credential_id, error))
    }
}
