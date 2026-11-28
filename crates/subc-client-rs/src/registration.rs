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

static OWNER: OnceLock<RegistrationOwner> = OnceLock::new();
static OWNER_READY: tokio::sync::Notify = tokio::sync::Notify::const_new();
static HELPER_BOUND: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SUPERVISED: OnceLock<bool> = OnceLock::new();
static CHANNEL_TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static CHANNEL_ADOPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A custom module loop's channel-0 `claim.mint` RPC implementation.
///
/// Startup order is: call [`adopt_channel`], authenticate the returned stream,
/// send the launched module's HELLO without `launch_nonce`, validate HELLO_ACK,
/// call [`install_claim_minter`], then open consumers. If adoption returns `None`,
/// use the legacy connection and launch-nonce path instead; do not install a minter.
///
/// Implement with [`crate::async_trait`]. Send each request on the adopted module
/// connection and correlate its reply while continuing to read that connection.
/// Validate the reply's type and operation, map daemon refusals to
/// [`ClaimError::Refused`], and honor `claim_rate_limited` retry delays within a
/// bounded budget. Return [`ClaimError::Unavailable`] when the connection closes;
/// never reconnect the registration channel or fall back to nonce authentication.
/// The SDK bounds each mint and ignores the reply's informational wall-clock expiry.
///
/// A module using this path must not pass
/// [`CallOptions::consumer_identity`](crate::consumer::CallOptions::consumer_identity):
/// a claim-bound connection refuses that override before sending it.
#[async_trait]
pub trait ClaimMinter: Send + Sync {
    async fn mint(&self, request: ClaimMintRequest) -> Result<ClaimMintReply, ClaimError>;
}

/// Failure to adopt or install the process's registration owner.
#[derive(Debug)]
#[non_exhaustive]
pub enum RegistrationError {
    Channel(os::RegistrationChannelError),
    Transport(io::Error),
    NotAdopted,
    AlreadyInstalled,
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Channel(error) => error.fmt(f),
            Self::Transport(_) => f.write_str("could not open registration transport"),
            Self::NotAdopted => f.write_str("registration channel has not been adopted"),
            Self::AlreadyInstalled => f.write_str("registration owner already installed"),
        }
    }
}

impl Error for RegistrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Channel(error) => Some(error),
            Self::Transport(error) => Some(error),
            _ => None,
        }
    }
}

/// Take the daemon's private registration stream for a custom module loop.
///
/// Call once at startup, before spawning children. In a Tokio runtime:
/// adopt, authenticate, send HELLO without `launch_nonce`, validate HELLO_ACK,
/// install the owner with [`install_claim_minter`], then open consumers.
/// See [`ClaimMinter`] for the loop's RPC and connection-lifetime obligations.
/// A second call, even after `None`, returns
/// [`RegistrationError::Channel`] containing
/// [`os::RegistrationChannelError::AlreadyTaken`].
///
/// `None` means no validated channel was handed to this process (for example,
/// an older daemon or a non-daemon launch); keep the legacy nonce path then.
pub fn adopt_channel() -> Result<Option<Box<dyn ModuleStream>>, RegistrationError> {
    init_supervised();
    take_channel_stream()
}

/// Install a custom loop's claim minter after HELLO_ACK and before opening consumers.
///
/// Refuses installation before channel adoption or after any owner was installed,
/// including an owner installed by [`crate::serve`]. The process retains the minter;
/// it must report a closed registration connection as unavailable. Installation
/// cannot be undone and must never authorize reconnecting the registration channel.
pub fn install_claim_minter(minter: Arc<dyn ClaimMinter>) -> Result<(), RegistrationError> {
    install_adopted_owner(RegistrationOwner::Custom(minter))
}

fn install_adopted_owner(owner: RegistrationOwner) -> Result<(), RegistrationError> {
    if !CHANNEL_ADOPTED.load(Ordering::Acquire) {
        return Err(RegistrationError::NotAdopted);
    }
    publish_owner(owner)
}

fn publish_owner(owner: RegistrationOwner) -> Result<(), RegistrationError> {
    OWNER
        .set(owner)
        .map_err(|_| RegistrationError::AlreadyInstalled)?;
    OWNER_READY.notify_waiters();
    Ok(())
}

enum RegistrationOwner {
    Custom(Arc<dyn ClaimMinter>),
    Sdk(SdkClaimMinter),
}

impl RegistrationOwner {
    fn sdk(handle: &ModuleHandle) -> Self {
        Self::Sdk(SdkClaimMinter {
            generations: Arc::downgrade(&handle.generations),
            consumer_dial_stop: CancellationToken::new(),
        })
    }

    fn minter(&self) -> &dyn ClaimMinter {
        match self {
            Self::Custom(minter) => minter.as_ref(),
            Self::Sdk(minter) => minter,
        }
    }
}

struct SdkClaimMinter {
    generations: Weak<live_upgrade::Generations>,
    consumer_dial_stop: CancellationToken,
}

#[async_trait]
impl ClaimMinter for SdkClaimMinter {
    async fn mint(&self, request: ClaimMintRequest) -> Result<ClaimMintReply, ClaimError> {
        ModuleHandle {
            generations: self.generations.upgrade().ok_or(ClaimError::Unavailable)?,
        }
        .mint_claim_reply(request)
        .await
    }
}

fn sdk_owner() -> Option<&'static SdkClaimMinter> {
    match OWNER.get()? {
        RegistrationOwner::Sdk(owner) => Some(owner),
        RegistrationOwner::Custom(_) => None,
    }
}

pub(crate) fn check_consumer_dial() -> Result<(), ConsumerError> {
    if sdk_owner().is_some_and(|owner| owner.consumer_dial_stop.is_cancelled()) {
        return Err(ConsumerError::Closed);
    }
    Ok(())
}

pub(crate) async fn consumer_dial_stopped() {
    if let Some(owner) = sdk_owner() {
        owner.consumer_dial_stop.cancelled().await;
    } else {
        std::future::pending().await
    }
}

#[derive(Default)]
pub(crate) struct Session {
    active: std::sync::atomic::AtomicBool,
    pub(super) prefix_child: std::sync::atomic::AtomicBool,
    pool: tokio::sync::Mutex<Vec<PoolEntry>>,
}

impl Session {
    pub(super) fn is_active(&self) -> bool {
        self.active.load(Ordering::Acquire)
    }
}

struct PoolEntry {
    path: PathBuf,
    options: ConsumerOptions,
    local: bool,
    consumer: Weak<SubcConsumer>,
}

/// Claim failures never permit continuing with an unbound connection. Debug and
/// Display omit daemon-authored messages and bodies, which can contain secrets.
/// During an announced daemon upgrade, a new mint is refused locally with
/// `module_reloading` before sending to the daemon.
#[derive(Debug)]
#[non_exhaustive]
pub enum ClaimError {
    Unavailable,
    Timeout,
    Protocol,
    Refused {
        code: String,
    },
    Handoff,
    /// Prefix children cannot open consumer connections in this SDK yet.
    PrefixChildConsumer,
}

impl fmt::Display for ClaimError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if matches!(self, Self::PrefixChildConsumer) {
            f.write_str("prefix children have no consumer connections in this SDK yet")
        } else {
            f.write_str("claim authentication failed")
        }
    }
}
impl Error for ClaimError {}

pub(crate) fn supervised() -> bool {
    SUPERVISED.get().copied().unwrap_or(false)
}

pub(crate) fn adopt(module_id: &str) -> Result<bool, SubcModuleError> {
    let supervised = init_supervised();
    // The inherited channel belongs to the launched module, not every module
    // exported by its process. Sharing a process does not grant other module ids
    // the launched module's authority; they need separate registration authority.
    Ok(supervised && is_launched_module(module_id)?)
}

fn init_supervised() -> bool {
    *SUPERVISED.get_or_init(|| {
        let supervised = os::supervised();
        if let Some(reason) = os::registration_refusal() {
            tracing::warn!(%reason, "registration channel not adopted");
        }
        supervised
    })
}

pub(crate) fn is_launched_module(module_id: &str) -> Result<bool, SubcModuleError> {
    Ok(module_id_from_env()?.is_none_or(|launched_id| launched_id == module_id))
}

pub(crate) fn take_stream() -> Result<Option<Box<dyn ModuleStream>>, SubcModuleError> {
    if !supervised() {
        return Ok(None);
    }
    match take_channel_stream() {
        Err(RegistrationError::Channel(os::RegistrationChannelError::AlreadyTaken)) => {
            let error = match module_id_from_env()? {
                Some(module_id) => io::Error::other(format!(
                    "launched module {module_id:?} is already serving on the registration channel"
                )),
                None => io::Error::other(os::RegistrationChannelError::AlreadyTaken),
            };
            Err(SubcModuleError::RegistrationChannel(error))
        }
        Err(RegistrationError::Transport(error)) => {
            Err(SubcModuleError::RegistrationChannel(error))
        }
        result => {
            result.map_err(|error| SubcModuleError::RegistrationChannel(io::Error::other(error)))
        }
    }
}

fn take_channel_stream() -> Result<Option<Box<dyn ModuleStream>>, RegistrationError> {
    if CHANNEL_TAKEN.swap(true, Ordering::AcqRel) {
        return Err(RegistrationError::Channel(
            os::RegistrationChannelError::AlreadyTaken,
        ));
    }
    let channel = os::take_registration_channel().map_err(RegistrationError::Channel)?;
    let Some(channel) = channel else {
        return Ok(None);
    };
    #[cfg(unix)]
    let stream = {
        let stream = channel.into_unix_stream();
        stream
            .set_nonblocking(true)
            .map_err(RegistrationError::Transport)?;
        tokio::net::UnixStream::from_std(stream).map_err(RegistrationError::Transport)?
    };
    #[cfg(windows)]
    let stream = channel
        .into_tokio_pipe_client()
        .map_err(RegistrationError::Transport)?;
    #[cfg(any(unix, windows))]
    {
        CHANNEL_ADOPTED.store(true, Ordering::Release);
        Ok(Some(Box::new(stream)))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = channel;
        Err(RegistrationError::Transport(io::Error::new(
            io::ErrorKind::Unsupported,
            "registration transport unsupported",
        )))
    }
}

pub(crate) fn install_owner(handle: &ModuleHandle) -> Result<(), SubcModuleError> {
    handle
        .shared()
        .registration
        .active
        .store(true, Ordering::Release);
    install_adopted_owner(RegistrationOwner::sdk(handle))
        .map_err(|error| SubcModuleError::RegistrationChannel(io::Error::other(error)))
}

pub(super) fn rebind_owner(shared: &ModuleHandleShared) {
    shared.registration.active.store(true, Ordering::Release);
}

pub(super) fn install_resumed_nonce_owner(handle: &ModuleHandle) -> Result<(), SubcModuleError> {
    // A nonce launch does not have a claim minter until the daemon accepts its
    // first resume. From then on the minter follows the stable handle just as
    // an inherited channel's minter does.
    publish_owner(RegistrationOwner::sdk(handle))
        .map_err(|error| SubcModuleError::RegistrationChannel(io::Error::other(error)))
}

pub(crate) async fn prefix_claim(module_id: &str) -> Result<Option<Claim>, ClaimError> {
    let deadline = Instant::now() + CATALOG_UPDATE_TIMEOUT;
    timeout_at(deadline, async {
        let owner = loop {
            // Register the waiter before checking OWNER so installation cannot
            // race the transition to sleep.
            let ready = OWNER_READY.notified();
            tokio::pin!(ready);
            ready.as_mut().enable();
            if let Some(owner) = OWNER.get() {
                break owner;
            }
            ready.await;
        };
        match mint_claim(
            owner.minter(),
            ClaimPurpose::Prefix {
                module_id: module_id.to_owned(),
            },
        )
        .await
        {
            Err(ClaimError::Refused { code }) if code == error_codes::CLAIM_PREFIX_REFUSED => {
                Ok(None)
            }
            result => result.map(Some),
        }
    })
    .await
    .map_err(|_| ClaimError::Timeout)?
}

pub(crate) fn serve_claim_error(module_id: &str, error: ClaimError) -> SubcModuleError {
    let code = match error {
        ClaimError::Refused { code } => code,
        ClaimError::Unavailable => "claim_unavailable".into(),
        ClaimError::Timeout => "claim_timeout".into(),
        _ => "claim_protocol".into(),
    };
    SubcModuleError::RegistrationChannel(io::Error::other(format!(
        "prefix registration for module {module_id:?} failed: {code}"
    )))
}

impl ModuleHandle {
    pub(super) fn fence_consumer_dials(&self) {
        if let Some(owner) = sdk_owner() {
            // A secondary module can drain without stopping the process owner.
            if owner.generations.ptr_eq(&Arc::downgrade(&self.generations)) {
                owner.consumer_dial_stop.cancel();
            }
        }
    }

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
        let shared = self.shared();
        Self::check_claim_fence(&shared).map_err(ConsumerError::claim)?;
        if shared.registration.prefix_child.load(Ordering::Acquire) {
            return Err(ConsumerError::claim(ClaimError::PrefixChildConsumer));
        }
        if !shared.registration.active.load(Ordering::Acquire) || shared.close_token.is_cancelled()
        {
            return Err(ConsumerError::claim(ClaimError::Unavailable));
        }
        let mut pool = shared.registration.pool.lock().await;
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
        self.mint_claim_reply(ClaimMintRequest::new(purpose))
            .await
            .map(|reply| reply.claim)
    }

    async fn mint_claim_reply(
        &self,
        request: ClaimMintRequest,
    ) -> Result<ClaimMintReply, ClaimError> {
        let shared = self.shared();
        Self::check_claim_fence(&shared)?;
        if !shared.registration.active.load(Ordering::Acquire) {
            return Err(ClaimError::Unavailable);
        }
        let deadline = Instant::now() + CATALOG_UPDATE_TIMEOUT;
        loop {
            let reply = timeout_at(deadline, Self::claim_call(&shared, &request))
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
            return Ok(reply);
        }
    }

    fn check_claim_fence(shared: &ModuleHandleShared) -> Result<(), ClaimError> {
        shared.check_upgrade_fence().map_err(claim_send_error)
    }

    async fn claim_call(
        shared: &Arc<ModuleHandleShared>,
        request: &ClaimMintRequest,
    ) -> Result<Frame, ClaimError> {
        let body = serde_json::to_vec(request).map_err(|_| ClaimError::Protocol)?;
        let (corr, writer, rx) = {
            let mut inner = shared.lock_inner();
            if inner.upgrade.fenced() {
                return Err(claim_send_error(CatalogUpdateError::Rejected(
                    live_upgrade::upgrade_refusal(),
                )));
            }
            let writer = inner.writer.clone().ok_or(ClaimError::Unavailable)?;
            let corr = next_module_control_corr(&mut inner).ok_or(ClaimError::Unavailable)?;
            let (tx, rx) = oneshot::channel();
            inner.pending_claim_calls.insert(corr, tx);
            (corr, writer, rx)
        };
        let _guard = ClaimWaiter {
            shared: shared.clone(),
            corr,
        };
        let frame = Frame::build_with_version(
            shared.negotiated_ver,
            FrameType::Request,
            control_flags(),
            0,
            0,
            corr,
            body,
        )
        .map_err(|_| ClaimError::Protocol)?;
        shared
            .queue_request(&writer, frame)
            .await
            .map_err(claim_send_error)?;
        rx.await.map_err(|_| ClaimError::Unavailable)
    }
}

struct ClaimWaiter {
    shared: Arc<ModuleHandleShared>,
    corr: u64,
}
impl Drop for ClaimWaiter {
    fn drop(&mut self) {
        self.shared
            .lock_inner()
            .pending_claim_calls
            .remove(&self.corr);
    }
}

fn claim_send_error(error: CatalogUpdateError) -> ClaimError {
    match error {
        CatalogUpdateError::Rejected(body) => ClaimError::Refused { code: body.code },
        _ => ClaimError::Unavailable,
    }
}

pub(crate) fn bound() -> bool {
    supervised() || OWNER.get().is_some() || HELPER_BOUND.load(Ordering::Acquire)
}

async fn mint_claim(minter: &dyn ClaimMinter, purpose: ClaimPurpose) -> Result<Claim, ClaimError> {
    timeout(
        CATALOG_UPDATE_TIMEOUT,
        minter.mint(ClaimMintRequest::new(purpose)),
    )
    .await
    .map_err(|_| ClaimError::Timeout)?
    .map(|reply| reply.claim)
}

pub(crate) async fn bind_consumer<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    deadline: Duration,
) -> Result<(), ClaimError> {
    let claim = if let Some(owner) = OWNER.get() {
        Some(mint_claim(owner.minter(), ClaimPurpose::SelfConnection).await?)
    } else if supervised() {
        return Err(ClaimError::Unavailable);
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
    present_claim(stream, claim, deadline).await
}

pub(crate) async fn present_claim<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    claim: Claim,
    deadline: Duration,
) -> Result<(), ClaimError> {
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
