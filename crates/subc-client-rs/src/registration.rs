//! One process owner for registration and claim minting. No synchronous mutex is
//! held over I/O. The async pool lock serializes consumer creation, not minting
//! or the registration reader, so a HELLO/claim reply can always make progress.

use super::*;
use consumer::{ConsumerError, ConsumerOptions, SubcConsumer};
use std::sync::{OnceLock, Weak};
use subc_os::registration_channel as os;
use subc_protocol::claim::{
    Claim, ClaimMintReply, ClaimMintRequest, ClaimPresentReply, ClaimPresentRequest, ClaimPurpose,
    ClaimRateLimitedDetail,
};

static OWNER: OnceLock<Weak<ModuleHandleShared>> = OnceLock::new();
static HELPER_BOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SUPERVISED: OnceLock<bool> = OnceLock::new();

#[derive(Default)]
pub(crate) struct Session {
    active: std::sync::atomic::AtomicBool,
    pool: tokio::sync::Mutex<Vec<PoolEntry>>,
}

struct PoolEntry {
    path: PathBuf,
    options: ConsumerOptions,
    local: bool,
    consumer: Weak<SubcConsumer>,
}

/// Claim failures never permit continuing with an unbound connection. Debug and
/// Display omit daemon-authored messages and bodies, which can contain secrets.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClaimError {
    Unavailable,
    Timeout,
    Protocol,
    Refused { code: String },
    Handoff,
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("claim authentication failed")
    }
}
impl Error for ClaimError {}

pub(crate) fn supervised() -> bool {
    SUPERVISED.get().copied().unwrap_or(false)
}

pub(crate) fn adopt() -> bool {
    *SUPERVISED.get_or_init(|| {
        let supervised = os::supervised();
        if let Some(reason) = os::registration_refusal() {
            tracing::warn!(%reason, "registration channel not adopted");
        }
        supervised
    })
}

pub(crate) fn take_stream() -> Result<Option<Box<dyn ModuleStream>>, SubcModuleError> {
    if !supervised() {
        return Ok(None);
    }
    let Some(channel) = os::take_registration_channel()
        .map_err(|error| SubcModuleError::RegistrationChannel(io::Error::other(error)))?
    else {
        return Ok(None);
    };
    #[cfg(unix)]
    let stream = {
        let stream = channel.into_unix_stream();
        stream
            .set_nonblocking(true)
            .map_err(SubcModuleError::RegistrationChannel)?;
        tokio::net::UnixStream::from_std(stream).map_err(SubcModuleError::RegistrationChannel)?
    };
    #[cfg(windows)]
    let stream = channel
        .into_tokio_pipe_client()
        .map_err(SubcModuleError::RegistrationChannel)?;
    #[cfg(any(unix, windows))]
    {
        Ok(Some(Box::new(stream)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = channel;
        Err(SubcModuleError::RegistrationChannel(io::Error::new(
            io::ErrorKind::Unsupported,
            "registration transport unsupported",
        )))
    }
}

pub(crate) fn install_owner(handle: &ModuleHandle) -> Result<(), SubcModuleError> {
    OWNER.set(Arc::downgrade(&handle.shared)).map_err(|_| {
        SubcModuleError::RegistrationChannel(io::Error::other(
            "registration owner already installed",
        ))
    })?;
    handle
        .shared
        .registration
        .active
        .store(true, Ordering::Release);
    Ok(())
}

impl ModuleHandle {
    /// Open or reuse a self-bound TCP consumer. The pool belongs to this spawn's
    /// registration binding; keys compare the exact path and every option. It
    /// holds only weak references: dropping the last Arc closes the connection.
    /// Await or spawn the serve future before calling this method.
    pub async fn consumer(
        &self,
        path: &Path,
        options: ConsumerOptions,
    ) -> Result<Arc<SubcConsumer>, ConsumerError> {
        self.pooled_consumer(path, options, false).await
    }

    /// Like [`Self::consumer`], but local-only. Local and TCP connections never
    /// share a pool entry, even when their connection file and options match.
    pub async fn consumer_local(
        &self,
        path: &Path,
        options: ConsumerOptions,
    ) -> Result<Arc<SubcConsumer>, ConsumerError> {
        self.pooled_consumer(path, options, true).await
    }

    async fn pooled_consumer(
        &self,
        path: &Path,
        options: ConsumerOptions,
        local: bool,
    ) -> Result<Arc<SubcConsumer>, ConsumerError> {
        if !self.shared.registration.active.load(Ordering::Acquire)
            || self.shared.close_token.is_cancelled()
        {
            return Err(ConsumerError::claim(ClaimError::Unavailable));
        }
        let mut pool = self.shared.registration.pool.lock().await;
        pool.retain(|entry| {
            entry
                .consumer
                .upgrade()
                .is_some_and(|consumer| consumer.pool_reusable())
        });
        if let Some(consumer) = pool
            .iter()
            .find(|entry| entry.path == path && entry.local == local && entry.options == options)
            .and_then(|entry| entry.consumer.upgrade())
            .filter(|consumer| consumer.pool_reusable())
        {
            return Ok(consumer);
        }
        let consumer = Arc::new(if local {
            SubcConsumer::connect_local(path, options.clone()).await?
        } else {
            SubcConsumer::connect(path, options.clone()).await?
        });
        pool.push(PoolEntry {
            path: path.into(),
            options,
            local,
            consumer: Arc::downgrade(&consumer),
        });
        Ok(consumer)
    }

    /// Pass a helper-purpose claim through subc-os's one-read pipe. The helper
    /// acts for the module, but its calls are attested as delegated, not its own.
    /// No self claim or launch nonce is ever put into the child's environment or
    /// argv. On Unix, install this handoff last on the helper's Command. On
    /// Windows, configure the Command before spawning, then serve the handoff
    /// with the live child's PID on a blocking thread, retaining the child.
    pub async fn helper_claim_handoff(&self) -> Result<os::HelperClaimHandoff, ClaimError> {
        let claim = self.mint_claim(ClaimPurpose::Helper).await?;
        os::HelperClaimHandoff::new(claim.expose_secret()).map_err(|_| ClaimError::Handoff)
    }

    async fn mint_claim(&self, purpose: ClaimPurpose) -> Result<Claim, ClaimError> {
        if !self.shared.registration.active.load(Ordering::Acquire) {
            return Err(ClaimError::Unavailable);
        }
        let deadline = Instant::now() + CATALOG_UPDATE_TIMEOUT;
        loop {
            let reply = timeout_at(
                deadline,
                self.claim_call(&ClaimMintRequest::new(purpose.clone())),
            )
            .await
            .map_err(|_| ClaimError::Timeout)??;
            if reply.header.ty == FrameType::Error {
                let error: ErrorBody =
                    serde_json::from_slice(&reply.body).map_err(|_| ClaimError::Protocol)?;
                if error.code == error_codes::CLAIM_RATE_LIMITED {
                    let detail: ClaimRateLimitedDetail =
                        serde_json::from_value(error.detail.ok_or(ClaimError::Protocol)?)
                            .map_err(|_| ClaimError::Protocol)?;
                    // Use the daemon's delay, never its informational wall-clock
                    // claim expiry. A bounded call must not retry past its budget.
                    timeout_at(
                        deadline,
                        tokio::time::sleep(Duration::from_millis(detail.retry_after_ms)),
                    )
                    .await
                    .map_err(|_| ClaimError::Timeout)?;
                    continue;
                }
                return Err(ClaimError::Refused { code: error.code });
            }
            if reply.header.ty != FrameType::Response {
                return Err(ClaimError::Protocol);
            }
            let reply: ClaimMintReply =
                serde_json::from_slice(&reply.body).map_err(|_| ClaimError::Protocol)?;
            return Ok(reply.claim);
        }
    }

    async fn claim_call(&self, request: &ClaimMintRequest) -> Result<Frame, ClaimError> {
        let body = serde_json::to_vec(request).map_err(|_| ClaimError::Protocol)?;
        let (corr, writer, rx) = {
            let mut inner = self.shared.lock_inner();
            let writer = inner.writer.clone().ok_or(ClaimError::Unavailable)?;
            let corr = next_module_control_corr(&mut inner).ok_or(ClaimError::Unavailable)?;
            let (tx, rx) = oneshot::channel();
            inner.pending_claim_calls.insert(corr, tx);
            (corr, writer, rx)
        };
        let _guard = ClaimWaiter {
            handle: self.clone(),
            corr,
        };
        let frame = Frame::build_with_version(
            self.shared.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|_| ClaimError::Protocol)?;
        writer
            .send(frame)
            .await
            .map_err(|_| ClaimError::Unavailable)?;
        rx.await.map_err(|_| ClaimError::Unavailable)
    }
}

struct ClaimWaiter {
    handle: ModuleHandle,
    corr: u64,
}
impl Drop for ClaimWaiter {
    fn drop(&mut self) {
        self.handle
            .shared
            .lock_inner()
            .pending_claim_calls
            .remove(&self.corr);
    }
}

pub(crate) fn bound() -> bool {
    supervised() || HELPER_BOUND.load(Ordering::Acquire)
}

pub(crate) async fn bind_consumer<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    deadline: Duration,
) -> Result<(), ClaimError> {
    let claim = if supervised() {
        let shared = OWNER
            .get()
            .and_then(Weak::upgrade)
            .ok_or(ClaimError::Unavailable)?;
        Some(
            ModuleHandle { shared }
                .mint_claim(ClaimPurpose::SelfConnection)
                .await?,
        )
    } else {
        os::helper_claim()
            .map_err(|_| ClaimError::Handoff)?
            .map(|claim| {
                HELPER_BOUND.store(true, Ordering::Release);
                Claim::new(claim.value())
            })
    };
    let Some(claim) = claim else {
        return Ok(());
    };
    let frame = Frame::build(
        FrameType::Request,
        control_flags(),
        0,
        0,
        1,
        serde_json::to_vec(&ClaimPresentRequest::new(claim)).map_err(|_| ClaimError::Protocol)?,
    )
    .map_err(|_| ClaimError::Protocol)?;
    let reply = timeout(deadline, async {
        write_frame(stream, &frame)
            .await
            .map_err(|_| ClaimError::Unavailable)?;
        read_frame(stream)
            .await
            .map_err(|_| ClaimError::Unavailable)?
            .ok_or(ClaimError::Unavailable)
    })
    .await
    .map_err(|_| ClaimError::Timeout)??;
    if reply.header.channel != 0 || reply.header.epoch != 0 || reply.header.corr != 1 {
        return Err(ClaimError::Protocol);
    }
    if reply.header.ty == FrameType::Error {
        let error: ErrorBody =
            serde_json::from_slice(&reply.body).map_err(|_| ClaimError::Protocol)?;
        return Err(ClaimError::Refused { code: error.code });
    }
    if reply.header.ty != FrameType::Response {
        return Err(ClaimError::Protocol);
    }
    serde_json::from_slice::<ClaimPresentReply>(&reply.body).map_err(|_| ClaimError::Protocol)?;
    Ok(())
}

#[cfg(all(test, any(unix, windows)))]
#[path = "registration_tests.rs"]
mod registration_contract;
