#![forbid(unsafe_code)]

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    env,
    error::Error,
    ffi::{OsStr, OsString},
    fs, io as stdio,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc, RwLock, RwLockReadGuard, RwLockWriteGuard,
    },
    time::{Duration, SystemTime},
};

use rmcp::{
    model::{
        CallToolRequestParams, CallToolResult, CancelledNotificationParam, ClientCapabilities,
        ClientResult, CustomRequest, ErrorCode, ErrorData, Implementation, JsonObject,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProgressToken,
        RequestId, ServerCapabilities, ServerInfo, ServerRequest, Tool as McpTool, ToolAnnotations,
    },
    service::{NotificationContext, Peer, PeerRequestOptions, RequestContext, ServiceError},
    transport::async_rw::AsyncRwTransport,
    RoleServer, ServerHandler,
};
use serde::{
    de::{self, DeserializeOwned},
    Deserialize, Deserializer, Serialize,
};
use subc_control::{CatalogEntry, ClientControlRequest, ClientControlResponse, ConsumerIdentity};
use subc_jsonc::jsonc_to_json;
use subc_protocol::{
    manifest::{
        Bindings, ConsumerRole, ExecutionMode, IdentityBinding, ModuleManifest, ProviderRole,
        StorageBinding, StorageKind, StorageScope, Tool as ManifestTool, TrustTier,
    },
    session::{
        HealthReport, HealthStatus, ModuleControlRequest, ModuleControlResponse,
        MODULE_CONTROL_OP_HEALTH_CHECK,
    },
    BindIdentity, ErrorBody, Flags, Frame as SubcFrame, FrameType, ModuleHelloAckBody,
    ModuleHelloBody, Priority, RouteTarget, MAX_FRAME_BODY_LEN, PROTOCOL_VERSION,
    SUBC_LAUNCH_NONCE_ENV, SUBC_MODULE_ID_ENV,
};
use subc_transport::{
    authenticate_client, authenticate_server, connection_file, generate_daemon_id, generate_key,
    read_frame, write_frame, ConnectionInfo, Endpoint, SCHEMA_VERSION,
};
use tokio::{
    io::{self as tokio_io, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter},
    net::{tcp::OwnedWriteHalf, TcpListener, TcpStream},
    sync::{broadcast, mpsc, watch, Mutex},
    task::JoinHandle,
    time,
};

const AUTH_DEADLINE: Duration = Duration::from_secs(2);
const SUBC_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);
const SHIM_SCHEMA_VERSION: u32 = 1;
const MAX_SHIM_CONTROL_MESSAGE_LEN: u32 = 64 * 1024;
const MODULE_CONNECTION_FILE_NAME: &str = "subc-mcp-connection.json";
const DEFAULT_HARNESS: &str = "mcp:generic";
const PENDING_FRAME_BUFFER: usize = 8;
const SUBC_EVENT_BUFFER: usize = 64;
const CATALOG_POLL_INTERVAL: Duration = Duration::from_millis(500);
const SUPERVISION_HELLO_CORR: u64 = 1;
const MCP_CONFIG_RELATIVE_PATH: &str = "cortexkit/mcp.jsonc";
const PROJECT_MCP_CONFIG_RELATIVE_PATH: &str = ".cortexkit/mcp.jsonc";
const REVERSE_RELAY_PENDING_PER_SESSION: usize = 8;
const DEFAULT_REVERSE_RELAY_TTL: Duration = Duration::from_secs(10 * 60);
const REVERSE_RELAY_TTL_MS_ENV: &str = "SUBC_MCP_REVERSE_RELAY_TTL_MS";
const TOOLS_SEARCH_NAME: &str = "tools_search";
const TOOLS_INVOKE_NAME: &str = "tools_invoke";
const ACK_ONLY_TOOL_RESPONSE_TEXT: &str = "Queued for context compaction.";
const FACADE_DEFAULT_DISABLED: &[&str] = &["magic-context", "llm-runner"];

static NEXT_CONNECTION_TOKEN: AtomicU64 = AtomicU64::new(1);

const USAGE: &str = "usage:\n  subc-mcp shim [--module-connection-file <path>] [--harness <name>]\n  subc-mcp module --subc <subc-connection-file> [--connection-file <path>]";

type BoxError = Box<dyn Error + Send + Sync>;
type Result<T> = std::result::Result<T, BoxError>;
type PendingKey = (u16, u32, u64);
type PendingTx = mpsc::Sender<SubcFrame>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct RouteHandle {
    channel: u16,
    epoch: u32,
    connection_token: u64,
}

#[derive(Debug, Clone)]
enum SubcEvent {
    RouteGoodbye { handle: RouteHandle },
    CatalogChanged { generation: u64 },
}

#[derive(Debug, Clone, Copy)]
enum ReverseCapability {
    Elicitation,
    Sampling,
    Roots,
}

impl ReverseCapability {
    fn for_method(method: &str) -> Option<Self> {
        match method {
            "elicitation/create" => Some(Self::Elicitation),
            "sampling/createMessage" => Some(Self::Sampling),
            "roots/list" => Some(Self::Roots),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct ReverseCapabilities {
    elicitation: bool,
    sampling: bool,
    roots: bool,
}

impl ReverseCapabilities {
    fn from_client(capabilities: &ClientCapabilities) -> Self {
        Self {
            elicitation: capabilities.elicitation.is_some(),
            sampling: capabilities.sampling.is_some(),
            roots: capabilities.roots.is_some(),
        }
    }

    fn declared_consumer_capabilities(self) -> Option<Vec<String>> {
        let mut declared = Vec::new();
        if self.elicitation {
            declared.push("elicitation".to_string());
        }
        if self.sampling {
            declared.push("sampling".to_string());
        }
        if self.roots {
            declared.push("roots".to_string());
        }
        (!declared.is_empty()).then_some(declared)
    }

    fn supports(self, capability: ReverseCapability) -> bool {
        match capability {
            ReverseCapability::Elicitation => self.elicitation,
            ReverseCapability::Sampling => self.sampling,
            ReverseCapability::Roots => self.roots,
        }
    }
}

#[derive(Debug, Default)]
struct RelayPeerState {
    peer: Option<Peer<RoleServer>>,
    capabilities: Option<ReverseCapabilities>,
}

#[derive(Debug)]
struct RelaySession {
    id: String,
    peer_state: RwLock<RelayPeerState>,
}

impl RelaySession {
    fn new(id: String) -> Self {
        Self {
            id,
            peer_state: RwLock::new(RelayPeerState::default()),
        }
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn record_initialized_peer(&self, peer: Peer<RoleServer>) {
        let capabilities = peer
            .peer_info()
            .map(|info| ReverseCapabilities::from_client(&info.capabilities))
            .unwrap_or_default();
        let mut state = self.peer_state.write().unwrap_or_else(|poisoned| {
            eprintln!(
                "subc-mcp module: warning: recovering from poisoned reverse-relay peer-state write lock"
            );
            poisoned.into_inner()
        });
        state.peer = Some(peer);
        state.capabilities = Some(capabilities);
    }

    fn consumer_capabilities(&self) -> Option<Vec<String>> {
        let state = self.peer_state.read().unwrap_or_else(|poisoned| {
            eprintln!(
                "subc-mcp module: warning: recovering from poisoned reverse-relay peer-state read lock"
            );
            poisoned.into_inner()
        });
        state
            .capabilities
            .and_then(ReverseCapabilities::declared_consumer_capabilities)
    }

    fn peer_for_capability(
        &self,
        method: &str,
        capability: ReverseCapability,
    ) -> std::result::Result<Peer<RoleServer>, ErrorData> {
        let state = self.peer_state.read().unwrap_or_else(|poisoned| {
            eprintln!(
                "subc-mcp module: warning: recovering from poisoned reverse-relay peer-state read lock"
            );
            poisoned.into_inner()
        });
        let Some(peer) = state.peer.clone() else {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("MCP client has not declared support for {method}"),
                None,
            ));
        };
        let Some(capabilities) = state.capabilities else {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("MCP client has not declared support for {method}"),
                None,
            ));
        };
        if !capabilities.supports(capability) {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!("MCP client did not declare support for {method}"),
                None,
            ));
        }
        Ok(peer)
    }
}

#[derive(Debug, Deserialize)]
struct ReverseMcpRequest {
    method: String,
    #[serde(default)]
    params: Option<serde_json::Value>,
}

#[derive(Clone)]
struct RelayCancelHandle {
    peer: Peer<RoleServer>,
    request_id: RequestId,
}

impl RelayCancelHandle {
    async fn cancel(self, reason: &'static str) {
        if let Err(error) = self
            .peer
            .notify_cancelled(CancelledNotificationParam {
                request_id: self.request_id,
                reason: Some(reason.to_owned()),
            })
            .await
        {
            eprintln!("subc-mcp module: failed to cancel reverse MCP request: {error}");
        }
    }
}

#[derive(Clone)]
struct PendingRequest {
    reply: PendingTx,
    route_session: Option<Arc<RelaySession>>,
}

struct PendingRelayEntry {
    session_id: String,
    created_at: time::Instant,
    cancel: Option<RelayCancelHandle>,
    task: Option<JoinHandle<()>>,
}

impl PendingRelayEntry {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            created_at: time::Instant::now(),
            cancel: None,
            task: None,
        }
    }
}

/// Counters are allocated with the session policy so an acknowledgment takes
/// only a relaxed atomic increment.
#[derive(Debug, Default)]
struct AckOnlyAckMetrics {
    counters: RwLock<HashMap<String, Arc<AtomicU64>>>,
}

impl AckOnlyAckMetrics {
    fn counter_for(&self, tool_name: &str) -> Arc<AtomicU64> {
        let mut counters = self.counters.write().unwrap_or_else(|poisoned| {
            eprintln!(
                "subc-mcp module: warning: recovering from poisoned ack-only metrics write lock"
            );
            poisoned.into_inner()
        });
        Arc::clone(
            counters
                .entry(tool_name.to_owned())
                .or_insert_with(|| Arc::new(AtomicU64::new(0))),
        )
    }

    fn snapshot(&self) -> BTreeMap<String, u64> {
        let counters = self.counters.read().unwrap_or_else(|poisoned| {
            eprintln!(
                "subc-mcp module: warning: recovering from poisoned ack-only metrics read lock"
            );
            poisoned.into_inner()
        });
        counters
            .iter()
            .map(|(tool_name, count)| (tool_name.clone(), count.load(Ordering::Relaxed)))
            .collect()
    }
}

#[derive(Clone)]
struct ReverseRelay {
    tx: mpsc::Sender<SubcFrame>,
    connection_token: u64,
    routes: Arc<Mutex<HashMap<RouteHandle, Arc<RelaySession>>>>,
    live_epochs: Arc<Mutex<HashMap<u16, u32>>>,
    pending: Arc<Mutex<HashMap<PendingKey, PendingRelayEntry>>>,
    stale_epoch_drops: Arc<AtomicU64>,
    ack_only_acks: Arc<AckOnlyAckMetrics>,
    ttl: Duration,
    max_pending_per_session: usize,
}

impl ReverseRelay {
    fn new(tx: mpsc::Sender<SubcFrame>, connection_token: u64) -> Self {
        Self {
            tx,
            connection_token,
            routes: Arc::new(Mutex::new(HashMap::new())),
            live_epochs: Arc::new(Mutex::new(HashMap::new())),
            pending: Arc::new(Mutex::new(HashMap::new())),
            stale_epoch_drops: Arc::new(AtomicU64::new(0)),
            ack_only_acks: Arc::new(AckOnlyAckMetrics::default()),
            ttl: reverse_relay_ttl_from_env(),
            max_pending_per_session: REVERSE_RELAY_PENDING_PER_SESSION,
        }
    }

    fn route_handle(&self, channel: u16, epoch: u32) -> RouteHandle {
        RouteHandle {
            channel,
            epoch,
            connection_token: self.connection_token,
        }
    }

    fn ack_only_counter(&self, tool_name: &str) -> Arc<AtomicU64> {
        self.ack_only_acks.counter_for(tool_name)
    }

    async fn health_metrics(&self) -> serde_json::Value {
        let active_relay_routes = self.routes.lock().await.len();
        let pending_reverse_requests = self.pending.lock().await.len();
        serde_json::json!({
            "active_relay_routes": active_relay_routes,
            "pending_reverse_requests": pending_reverse_requests,
            "stale_epoch_drops": self.stale_epoch_drops.load(Ordering::Relaxed),
            "ack_only_acks": self.ack_only_acks.snapshot(),
        })
    }

    async fn install_route(&self, handle: RouteHandle, session: Arc<RelaySession>) -> Result<()> {
        if handle.connection_token != self.connection_token {
            return Err(other_error(
                "route handle belongs to a different subc connection",
            ));
        }
        if handle.channel == 0 || handle.epoch == 0 {
            return Err(other_error(format!(
                "route handle must have nonzero channel and epoch, got ({}, {})",
                handle.channel, handle.epoch
            )));
        }
        self.live_epochs
            .lock()
            .await
            .insert(handle.channel, handle.epoch);
        let mut routes = self.routes.lock().await;
        routes.retain(|existing, _| existing.channel != handle.channel);
        routes.insert(handle, session);
        Ok(())
    }

    async fn unregister_session_routes(&self, session: &RelaySession) {
        let removed = {
            let mut routes = self.routes.lock().await;
            let removed = routes
                .iter()
                .filter_map(|(handle, route_session)| {
                    (route_session.id() == session.id()).then_some(*handle)
                })
                .collect::<Vec<_>>();
            for handle in &removed {
                routes.remove(handle);
            }
            removed
        };
        let mut live_epochs = self.live_epochs.lock().await;
        for handle in removed {
            if live_epochs.get(&handle.channel) == Some(&handle.epoch) {
                live_epochs.remove(&handle.channel);
            }
        }
    }

    async fn route_session(&self, handle: RouteHandle) -> Option<Arc<RelaySession>> {
        self.routes.lock().await.get(&handle).cloned()
    }

    async fn validate_ingress(&self, channel: u16, epoch: u32) -> bool {
        let valid = self.live_epochs.lock().await.get(&channel) == Some(&epoch);
        if !valid {
            self.stale_epoch_drops.fetch_add(1, Ordering::Relaxed);
        }
        valid
    }

    async fn handle_reverse_request(&self, frame: SubcFrame) {
        let route_handle = self.route_handle(frame.header.channel, frame.header.epoch);
        let reverse_corr = frame.header.corr;
        let key = (route_handle.channel, route_handle.epoch, reverse_corr);

        if self.pending.lock().await.contains_key(&key) {
            return;
        }

        let request = match serde_json::from_slice::<ReverseMcpRequest>(&frame.body) {
            Ok(request) => request,
            Err(error) => {
                self.send_reverse_error(
                    route_handle,
                    reverse_corr,
                    ErrorData::invalid_params(
                        format!("malformed reverse MCP request body: {error}"),
                        None,
                    ),
                )
                .await;
                return;
            }
        };

        let Some(capability) = ReverseCapability::for_method(&request.method) else {
            self.send_reverse_error(
                route_handle,
                reverse_corr,
                ErrorData::new(
                    ErrorCode::METHOD_NOT_FOUND,
                    format!("unsupported reverse MCP method '{}'", request.method),
                    None,
                ),
            )
            .await;
            return;
        };

        let Some(session) = self.route_session(route_handle).await else {
            self.send_reverse_error(
                route_handle,
                reverse_corr,
                ErrorData::internal_error(
                    format!(
                        "no MCP session owns route handle ({}, {})",
                        route_handle.channel, route_handle.epoch
                    ),
                    None,
                ),
            )
            .await;
            return;
        };

        let peer = match session.peer_for_capability(&request.method, capability) {
            Ok(peer) => peer,
            Err(error) => {
                self.send_reverse_error(route_handle, reverse_corr, error)
                    .await;
                return;
            }
        };

        let created_at = {
            let mut pending = self.pending.lock().await;
            if pending.contains_key(&key) {
                return;
            }
            let session_pending = pending
                .values()
                .filter(|entry| entry.session_id == session.id())
                .count();
            if session_pending >= self.max_pending_per_session {
                drop(pending);
                self.send_reverse_error(
                    route_handle,
                    reverse_corr,
                    ErrorData::internal_error(
                        "too many pending reverse MCP requests for this MCP session",
                        None,
                    ),
                )
                .await;
                return;
            }
            let entry = PendingRelayEntry::new(session.id().to_owned());
            let created_at = entry.created_at;
            pending.insert(key, entry);
            created_at
        };

        let host_request =
            ServerRequest::CustomRequest(CustomRequest::new(request.method, request.params));
        let host_handle = match peer
            .send_cancellable_request(host_request, PeerRequestOptions::no_options())
            .await
        {
            Ok(handle) => handle,
            Err(error) => {
                if self.remove_pending_for_current_task(key).await.is_some() {
                    self.send_reverse_error(
                        route_handle,
                        reverse_corr,
                        service_error_to_reverse_error(error),
                    )
                    .await;
                }
                return;
            }
        };

        let cancel_handle = RelayCancelHandle {
            peer: host_handle.peer.clone(),
            request_id: host_handle.id.clone(),
        };
        let relay = self.clone();
        let ttl_deadline = created_at + self.ttl;
        let task = tokio::spawn(async move {
            tokio::select! {
                result = host_handle.await_response() => {
                    relay.settle_host_answer(key, result).await;
                }
                _ = time::sleep_until(ttl_deadline) => {
                    relay.expire_pending(key).await;
                }
            }
        });

        let mut task = Some(task);
        let mut cancel_if_gone = None;
        {
            let mut pending = self.pending.lock().await;
            if let Some(entry) = pending.get_mut(&key) {
                entry.cancel = Some(cancel_handle.clone());
                entry.task = task.take();
            } else {
                cancel_if_gone = Some(cancel_handle);
            }
        }
        if let Some(task) = task {
            task.abort();
        }
        if let Some(cancel) = cancel_if_gone {
            cancel
                .cancel("reverse MCP request was settled before the host request started")
                .await;
        }
    }

    async fn settle_host_answer(
        &self,
        key: PendingKey,
        result: std::result::Result<ClientResult, ServiceError>,
    ) {
        if self.remove_pending_for_current_task(key).await.is_none() {
            return;
        }
        let handle = self.route_handle(key.0, key.1);
        match result {
            Ok(result) => self.send_reverse_response(handle, key.2, result).await,
            Err(error) => {
                self.send_reverse_error(handle, key.2, service_error_to_reverse_error(error))
                    .await;
            }
        }
    }

    async fn expire_pending(&self, key: PendingKey) {
        let Some(entry) = self.remove_pending_for_current_task(key).await else {
            return;
        };
        if let Some(cancel) = entry.cancel {
            cancel.cancel("reverse MCP request relay expired").await;
        }
    }

    async fn fail_session(&self, session: &RelaySession, message: &'static str) {
        let entries = self
            .remove_pending_where(|_, entry| entry.session_id == session.id())
            .await;
        for (key, mut entry) in entries {
            if let Some(task) = entry.task.take() {
                task.abort();
            }
            if let Some(cancel) = entry.cancel.take() {
                cancel.cancel(message).await;
            }
            self.send_reverse_error(
                self.route_handle(key.0, key.1),
                key.2,
                ErrorData::internal_error(message.to_owned(), None),
            )
            .await;
        }
    }

    async fn drop_route(&self, handle: RouteHandle) {
        if handle.connection_token != self.connection_token {
            return;
        }
        self.routes.lock().await.remove(&handle);
        {
            let mut live_epochs = self.live_epochs.lock().await;
            if live_epochs.get(&handle.channel) == Some(&handle.epoch) {
                live_epochs.remove(&handle.channel);
            }
        }
        let entries = self
            .remove_pending_where(|(channel, epoch, _), _| {
                *channel == handle.channel && *epoch == handle.epoch
            })
            .await;
        for (_, mut entry) in entries {
            if let Some(task) = entry.task.take() {
                task.abort();
            }
        }
    }

    async fn cancel_route_prompts(&self, handle: RouteHandle, message: &'static str) {
        if handle.connection_token != self.connection_token {
            return;
        }
        let entries = self
            .remove_pending_where(|(channel, epoch, _), _| {
                *channel == handle.channel && *epoch == handle.epoch
            })
            .await;
        for (key, mut entry) in entries {
            if let Some(task) = entry.task.take() {
                task.abort();
            }
            if let Some(cancel) = entry.cancel.take() {
                cancel.cancel(message).await;
            }
            self.send_reverse_error(
                self.route_handle(key.0, key.1),
                key.2,
                ErrorData::internal_error(message.to_owned(), None),
            )
            .await;
        }
    }

    async fn clear_all(&self) {
        self.routes.lock().await.clear();
        self.live_epochs.lock().await.clear();
        let entries = self.remove_pending_where(|_, _| true).await;
        for (_, mut entry) in entries {
            if let Some(task) = entry.task.take() {
                task.abort();
            }
            if let Some(cancel) = entry.cancel.take() {
                cancel.cancel("subc connection closed").await;
            }
        }
    }

    async fn remove_pending_for_current_task(&self, key: PendingKey) -> Option<PendingRelayEntry> {
        self.pending.lock().await.remove(&key)
    }

    async fn remove_pending_where<F>(
        &self,
        mut predicate: F,
    ) -> Vec<(PendingKey, PendingRelayEntry)>
    where
        F: FnMut(&PendingKey, &PendingRelayEntry) -> bool,
    {
        let mut pending = self.pending.lock().await;
        let keys = pending
            .iter()
            .filter_map(|(key, entry)| predicate(key, entry).then_some(*key))
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| pending.remove(&key).map(|entry| (key, entry)))
            .collect()
    }

    async fn send_reverse_response(
        &self,
        handle: RouteHandle,
        reverse_corr: u64,
        result: ClientResult,
    ) {
        match serde_json::to_vec(&result) {
            Ok(body) => {
                self.send_reverse_frame(FrameType::Response, handle, reverse_corr, body)
                    .await
            }
            Err(error) => {
                self.send_reverse_error(
                    handle,
                    reverse_corr,
                    ErrorData::internal_error(
                        format!("failed to encode reverse MCP response: {error}"),
                        None,
                    ),
                )
                .await;
            }
        }
    }

    async fn send_reverse_error(&self, handle: RouteHandle, reverse_corr: u64, error: ErrorData) {
        let body = match serde_json::to_vec(&error) {
            Ok(body) => body,
            Err(error) => {
                eprintln!("subc-mcp module: failed to encode reverse MCP error: {error}");
                Vec::new()
            }
        };
        self.send_reverse_frame(FrameType::Error, handle, reverse_corr, body)
            .await;
    }

    async fn send_reverse_frame(
        &self,
        ty: FrameType,
        handle: RouteHandle,
        reverse_corr: u64,
        body: Vec<u8>,
    ) {
        if handle.connection_token != self.connection_token {
            eprintln!("subc-mcp module: refusing reverse reply for a stale subc connection");
            return;
        }
        let frame = match build_frame(
            ty,
            data_flags(),
            handle.channel,
            handle.epoch,
            reverse_corr,
            body,
        ) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("subc-mcp module: failed to build reverse relay frame: {error}");
                return;
            }
        };
        if let Err(error) = self.tx.send(frame).await {
            eprintln!("subc-mcp module: failed to send reverse relay frame: {error}");
        }
    }
}

fn reverse_relay_ttl_from_env() -> Duration {
    match env::var(REVERSE_RELAY_TTL_MS_ENV) {
        Ok(raw) => match raw.parse::<u64>() {
            Ok(0) => DEFAULT_REVERSE_RELAY_TTL,
            Ok(ms) => Duration::from_millis(ms),
            Err(error) => {
                eprintln!(
                    "subc-mcp module: ignoring invalid {REVERSE_RELAY_TTL_MS_ENV}={raw:?}: {error}"
                );
                DEFAULT_REVERSE_RELAY_TTL
            }
        },
        Err(_) => DEFAULT_REVERSE_RELAY_TTL,
    }
}

fn service_error_to_reverse_error(error: ServiceError) -> ErrorData {
    match error {
        ServiceError::McpError(error) => error,
        ServiceError::Cancelled { reason } => ErrorData::internal_error(
            reason.unwrap_or_else(|| "reverse MCP request was cancelled".to_owned()),
            None,
        ),
        ServiceError::Timeout { timeout } => ErrorData::internal_error(
            format!("reverse MCP request timed out after {timeout:?}"),
            None,
        ),
        ServiceError::TransportClosed => {
            ErrorData::internal_error("MCP client transport closed before response", None)
        }
        ServiceError::UnexpectedResponse => {
            ErrorData::internal_error("MCP client returned an unexpected response type", None)
        }
        ServiceError::TransportSend(error) => ErrorData::internal_error(
            format!("failed to send reverse MCP request to client: {error}"),
            None,
        ),
        other => ErrorData::internal_error(format!("reverse MCP request failed: {other}"), None),
    }
}

#[tokio::main]
async fn main() {
    // Side-effect-free provenance probe, evaluated before any runtime arg
    // parsing or I/O so `ck-subc-mcp --version` never reaches shim/module setup.
    if env::args_os().nth(1).is_some_and(|arg| arg == "--version") {
        println!("ck-subc-mcp {}", env!("CARGO_PKG_VERSION"));
        process::exit(0);
    }

    let code = match run_from_env().await {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("subc-mcp: {error}");
            let mut source = error.source();
            while let Some(err) = source {
                eprintln!("  caused by: {err}");
                source = err.source();
            }
            1
        }
    };
    // Terminate via an explicit exit instead of returning into the tokio runtime's
    // drop. The shim pipes the host's stdin through `tokio::io::stdin()`, which is
    // backed by an UNCANCELLABLE blocking-read thread parked on fd 0; when the
    // socket side closes first (e.g. the module rejects the attach), the async body
    // finishes but runtime shutdown would block forever waiting for that stranded
    // stdin thread, so the process would not exit until the host closed stdin. A
    // host waiting for a response never closes stdin, so fail-closed would hang the
    // host. stdout is already flushed in `pipe_stdio` before this point.
    process::exit(code);
}

async fn run_from_env() -> Result<()> {
    match parse_args(env::args_os())? {
        CommandMode::Shim(args) => run_shim(args).await,
        CommandMode::Module(args) => run_module(args).await,
    }
}

#[derive(Debug)]
enum CommandMode {
    Shim(ShimArgs),
    Module(ModuleArgs),
}

#[derive(Debug)]
struct ShimArgs {
    module_connection_file: Option<PathBuf>,
    harness: String,
}

#[derive(Debug)]
struct ModuleArgs {
    subc_connection_file: PathBuf,
    own_connection_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShimHello {
    schema: u32,
    project_root: PathBuf,
    harness: String,
    shim_session_id: String,
    /// Per-launch conversation token minted by a wrapper (e.g. the ck-claude
    /// launcher exporting CK_INSTANCE_TOKEN). When present it becomes the bind
    /// identity's `session` VERBATIM, so conversation-scoped modules can
    /// correlate this MCP session with the same launch's provider-wire traffic
    /// (ai-proxy resolves the token to its conversation key). Absent for
    /// unwrapped hosts; the module then falls back to a synthetic per-process
    /// session id. Optional and serde-defaulted: old shims and old modules
    /// interoperate without a schema bump.
    #[serde(default)]
    instance_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ShimHelloAck {
    schema: u32,
}

#[derive(Debug, Clone)]
struct AttachedSession {
    state: Arc<SessionState>,
    relay_session: Arc<RelaySession>,
}

#[derive(Debug, Clone)]
struct CatalogSnapshot {
    generation: u64,
    modules: Vec<CatalogEntry>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum SurfaceMode {
    #[default]
    Full,
    Search,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ToolMode {
    #[default]
    Forward,
    AckOnly,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum RefreshMode {
    #[default]
    OnAttach,
    Immediate,
}

impl<'de> Deserialize<'de> for RefreshMode {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        match String::deserialize(deserializer)?.as_str() {
            "on-attach" => Ok(Self::OnAttach),
            "immediate" => Ok(Self::Immediate),
            "on-hard" | "on-soft" => Err(de::Error::custom(
                "refresh value requires a bust-signal source; not available on the MCP path",
            )),
            other => Err(de::Error::unknown_variant(
                other,
                &["on-attach", "immediate"],
            )),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
struct ExposedTool {
    manifest: ManifestTool,
    description: String,
}

#[derive(Debug, Clone, Default)]
struct GatewayConfig {
    surface_mode: SurfaceMode,
    refresh: RefreshMode,
    providers: HashMap<String, ProviderConfig>,
}

#[derive(Debug, Clone, Default)]
struct ProviderConfig {
    enabled: Option<bool>,
    namespace: Option<String>,
    tools: ToolConfig,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ToolConfig {
    default_enabled: Option<bool>,
    overrides: HashMap<String, ToolOverride>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ToolOverride {
    enabled: Option<bool>,
    description: Option<String>,
    mode: Option<ToolMode>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigFileState {
    path: PathBuf,
    modified: Option<SystemTime>,
    len: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigFileSnapshot {
    user: ConfigFileState,
    project: ConfigFileState,
}

#[derive(Debug, Clone)]
struct ConfigSnapshot {
    effective: GatewayConfig,
    files: ConfigFileSnapshot,
}

#[derive(Debug)]
struct SessionState {
    config: RwLock<ConfigSnapshot>,
    identity: BindIdentity,
    inner: RwLock<SessionInner>,
}

#[derive(Debug, Clone)]
struct SessionInner {
    surface_mode: SurfaceMode,
    catalog_generation: u64,
    routes: HashMap<String, RouteHandle>,
    tools: Vec<ExposedTool>,
    bindings: HashMap<String, ToolBinding>,
}

/// Ack-only calls must not write to the provider route because their effect is
/// applied on a separate transport path. The binding is a sum type so an
/// ack-only tool carries no route at all: only the forward payload can
/// type-check into the route writer.
#[derive(Debug, Clone)]
enum ToolBinding {
    Forward(ForwardBinding),
    AckOnly { acks: Arc<AtomicU64> },
}

#[derive(Debug, Clone)]
struct ForwardBinding {
    route: RouteHandle,
    bare_tool_name: String,
}

#[derive(Debug, Clone)]
struct DesiredSession {
    providers: Vec<DesiredProvider>,
}

#[derive(Debug, Clone)]
struct DesiredProvider {
    module_id: String,
    tools: Vec<DesiredTool>,
}

#[derive(Debug, Clone)]
struct DesiredTool {
    bare_tool: ManifestTool,
    exposed_tool: ExposedTool,
    mode: ToolMode,
}

#[derive(Debug, Default)]
struct RawGatewayLayer {
    surface_mode: MaybeSet<SurfaceMode>,
    refresh: MaybeSet<RefreshMode>,
    providers: HashMap<String, RawProviderConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGatewayConfig {
    version: u8,
    #[serde(
        default,
        rename = "surfaceMode",
        deserialize_with = "deserialize_maybe_set"
    )]
    surface_mode: MaybeSet<SurfaceMode>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    refresh: MaybeSet<RefreshMode>,
    #[serde(default)]
    providers: HashMap<String, RawProviderConfig>,
    #[serde(default)]
    harness: HashMap<String, RawGatewayOverlayConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawGatewayOverlayConfig {
    #[serde(
        default,
        rename = "surfaceMode",
        deserialize_with = "deserialize_maybe_set"
    )]
    surface_mode: MaybeSet<SurfaceMode>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    refresh: MaybeSet<RefreshMode>,
    #[serde(default)]
    providers: HashMap<String, RawProviderConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawProviderConfig {
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    enabled: MaybeSet<bool>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    namespace: MaybeSet<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    tools: MaybeSet<RawToolConfig>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawToolConfig {
    #[serde(
        default,
        rename = "defaultEnabled",
        deserialize_with = "deserialize_maybe_set"
    )]
    default_enabled: MaybeSet<bool>,
    #[serde(default)]
    overrides: HashMap<String, RawToolOverrideValue>,
}

#[derive(Debug)]
enum RawToolOverrideValue {
    Object(RawToolOverrideObject),
    Bool(bool),
    Null(()),
}

impl<'de> Deserialize<'de> for RawToolOverrideValue {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        match value {
            serde_json::Value::Null => Ok(Self::Null(())),
            serde_json::Value::Bool(enabled) => Ok(Self::Bool(enabled)),
            serde_json::Value::Object(_) => serde_json::from_value(value)
                .map(Self::Object)
                .map_err(de::Error::custom),
            other => Err(de::Error::custom(format!(
                "tool override must be bool, null, or object, got {other}"
            ))),
        }
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawToolOverrideObject {
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    enabled: MaybeSet<bool>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    description: MaybeSet<String>,
    #[serde(default, deserialize_with = "deserialize_maybe_set")]
    mode: MaybeSet<ToolMode>,
}

#[derive(Debug, Clone, Default)]
enum MaybeSet<T> {
    #[default]
    Missing,
    Null,
    Value(T),
}

#[derive(Clone)]
struct SubcClient {
    tx: mpsc::Sender<SubcFrame>,
    pending: Arc<Mutex<HashMap<PendingKey, PendingRequest>>>,
    events: broadcast::Sender<SubcEvent>,
    relay: Arc<ReverseRelay>,
    connection_token: u64,
    last_corr: Arc<AtomicU64>,
    writer_shutdown: watch::Sender<bool>,
    catalog_poller_started: Arc<AtomicBool>,
}

impl SubcClient {
    fn start(stream: TcpStream) -> Self {
        let (read_half, write_half) = stream.into_split();
        let (tx, rx) = mpsc::channel(128);
        let pending = Arc::new(Mutex::new(HashMap::new()));
        let (events, _events_rx) = broadcast::channel(SUBC_EVENT_BUFFER);
        let connection_token = NEXT_CONNECTION_TOKEN
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |token| {
                token.checked_add(1)
            })
            .expect("subc connection token space exhausted");
        let relay = Arc::new(ReverseRelay::new(tx.clone(), connection_token));
        let (writer_shutdown, writer_shutdown_rx) = watch::channel(false);

        tokio::spawn(subc_reader_loop(
            read_half,
            Arc::clone(&pending),
            events.clone(),
            Arc::clone(&relay),
        ));
        tokio::spawn(subc_writer_loop(write_half, rx, writer_shutdown_rx));

        Self {
            tx,
            pending,
            events,
            relay,
            connection_token,
            last_corr: Arc::new(AtomicU64::new(0)),
            writer_shutdown,
            catalog_poller_started: Arc::new(AtomicBool::new(false)),
        }
    }

    fn next_corr(&self) -> Result<u64> {
        match allocate_corr(&self.last_corr) {
            Some(corr) => Ok(corr),
            None => {
                let _ = self.writer_shutdown.send(true);
                Err(other_error(
                    "subc correlation space exhausted; connection was closed before reuse",
                ))
            }
        }
    }

    fn route_handle(&self, channel: u16, epoch: u32) -> RouteHandle {
        RouteHandle {
            channel,
            epoch,
            connection_token: self.connection_token,
        }
    }

    fn validate_handle(&self, handle: RouteHandle) -> Result<()> {
        if handle.connection_token != self.connection_token {
            return Err(other_error(
                "route handle belongs to a stale subc connection",
            ));
        }
        Ok(())
    }

    fn build_route_frame(
        &self,
        ty: FrameType,
        flags: Flags,
        handle: RouteHandle,
        corr: u64,
        body: Vec<u8>,
    ) -> Result<SubcFrame> {
        self.validate_handle(handle)?;
        build_frame(ty, flags, handle.channel, handle.epoch, corr, body)
    }

    async fn send_route_frame(&self, handle: RouteHandle, frame: SubcFrame) -> Result<()> {
        self.validate_handle(handle)?;
        if frame.header.channel != handle.channel || frame.header.epoch != handle.epoch {
            return Err(other_error("route frame does not match its route handle"));
        }
        self.send(frame).await
    }

    fn subscribe_events(&self) -> broadcast::Receiver<SubcEvent> {
        self.events.subscribe()
    }

    fn relay(&self) -> Arc<ReverseRelay> {
        Arc::clone(&self.relay)
    }

    fn ensure_catalog_poller(&self) {
        if self
            .catalog_poller_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        let subc = self.clone();
        tokio::spawn(async move {
            let mut last_generation = None;
            let mut interval = time::interval(CATALOG_POLL_INTERVAL);
            loop {
                interval.tick().await;
                match catalog_list(&subc).await {
                    Ok(snapshot) => {
                        if last_generation != Some(snapshot.generation) {
                            last_generation = Some(snapshot.generation);
                            let _ = subc.events.send(SubcEvent::CatalogChanged {
                                generation: snapshot.generation,
                            });
                        }
                    }
                    Err(error) => {
                        eprintln!("subc-mcp module: catalog poll failed: {error}");
                    }
                }
            }
        });
    }

    async fn send(&self, frame: SubcFrame) -> Result<()> {
        self.tx
            .send(frame)
            .await
            .map_err(|err| other_error(format!("subc writer is closed: {err}")))
    }

    async fn request_frames(&self, frame: SubcFrame) -> Result<mpsc::Receiver<SubcFrame>> {
        self.request_frames_for_route_open(frame, None).await
    }

    async fn request_frames_for_route_open(
        &self,
        frame: SubcFrame,
        route_session: Option<Arc<RelaySession>>,
    ) -> Result<mpsc::Receiver<SubcFrame>> {
        let key = (frame.header.channel, frame.header.epoch, frame.header.corr);
        let (reply_tx, reply_rx) = mpsc::channel(PENDING_FRAME_BUFFER);
        let pending_request = PendingRequest {
            reply: reply_tx,
            route_session,
        };
        {
            let mut pending = self.pending.lock().await;
            if pending.insert(key, pending_request).is_some() {
                return Err(other_error(format!(
                    "duplicate pending subc request for handle ({}, {}) corr {}",
                    key.0, key.1, key.2
                )));
            }
        }

        if let Err(err) = self.tx.send(frame).await {
            self.pending.lock().await.remove(&key);
            return Err(other_error(format!("subc writer is closed: {err}")));
        }

        Ok(reply_rx)
    }

    async fn abandon_request(&self, handle: RouteHandle, corr: u64) {
        self.pending
            .lock()
            .await
            .remove(&(handle.channel, handle.epoch, corr));
    }

    async fn request(&self, frame: SubcFrame, wait: Duration) -> Result<SubcFrame> {
        self.request_with_route_session(frame, wait, None).await
    }

    async fn request_route_open(
        &self,
        frame: SubcFrame,
        wait: Duration,
        route_session: Arc<RelaySession>,
    ) -> Result<SubcFrame> {
        self.request_with_route_session(frame, wait, Some(route_session))
            .await
    }

    async fn request_with_route_session(
        &self,
        frame: SubcFrame,
        wait: Duration,
        route_session: Option<Arc<RelaySession>>,
    ) -> Result<SubcFrame> {
        let key = (frame.header.channel, frame.header.epoch, frame.header.corr);
        let retain_late_route_open = route_session.is_some();
        let mut reply_rx = self
            .request_frames_for_route_open(frame, route_session)
            .await?;

        match time::timeout(wait, async {
            loop {
                let Some(frame) = reply_rx.recv().await else {
                    return Err(other_error(format!(
                        "subc connection closed before response for handle ({}, {}) corr {}",
                        key.0, key.1, key.2
                    )));
                };
                if is_terminal_frame_type(frame.header.ty) {
                    return Ok(frame);
                }
            }
        })
        .await
        {
            Ok(result) => result,
            Err(_elapsed) => {
                if !retain_late_route_open {
                    self.pending.lock().await.remove(&key);
                }
                Err(other_error(format!(
                    "timed out waiting {wait:?} for subc response on handle ({}, {}) corr {}",
                    key.0, key.1, key.2
                )))
            }
        }
    }
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<CommandMode> {
    let mut args = args.into_iter();
    let _program = args.next();
    let Some(mode) = args.next() else {
        return Err(invalid_input(USAGE));
    };

    if mode == OsStr::new("shim") {
        parse_shim_args(args).map(CommandMode::Shim)
    } else if mode == OsStr::new("module") {
        parse_module_args(args).map(CommandMode::Module)
    } else {
        Err(invalid_input(format!(
            "unknown subcommand '{}'.\n{USAGE}",
            mode.to_string_lossy()
        )))
    }
}

fn parse_shim_args(args: impl IntoIterator<Item = OsString>) -> Result<ShimArgs> {
    let mut module_connection_file = None;
    let mut harness = DEFAULT_HARNESS.to_owned();
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--module-connection-file") {
            module_connection_file = Some(PathBuf::from(take_value(
                &mut args,
                "--module-connection-file",
            )?));
        } else if arg == OsStr::new("--harness") {
            let raw = take_value(&mut args, "--harness")?;
            harness = raw.into_string().map_err(|value| {
                invalid_input(format!(
                    "--harness must be valid UTF-8, got '{}'",
                    value.to_string_lossy()
                ))
            })?;
            if harness.trim().is_empty() {
                return Err(invalid_input("--harness must not be empty"));
            }
            // The shim IS the MCP facade, so every bind it produces is an
            // mcp-class identity. Providers validate harness against
            // opencode|pi|runner|mcp:<client> and reject bare tokens with an
            // opaque config_divergence — auto-prefix so `--harness claude-code`
            // means what the operator obviously intended. Explicit prefixed
            // values (and the reserved non-mcp identities) pass through.
            if !harness.contains(':') && !matches!(harness.as_str(), "opencode" | "pi" | "runner") {
                harness = format!("mcp:{harness}");
            }
        } else {
            return Err(invalid_input(format!(
                "unknown shim argument '{}'.\n{USAGE}",
                arg.to_string_lossy()
            )));
        }
    }

    Ok(ShimArgs {
        module_connection_file,
        harness,
    })
}

fn parse_module_args(args: impl IntoIterator<Item = OsString>) -> Result<ModuleArgs> {
    let mut subc_connection_file = None;
    let mut own_connection_file = None;
    let mut args = args.into_iter();

    while let Some(arg) = args.next() {
        if arg == OsStr::new("--subc") {
            subc_connection_file = Some(PathBuf::from(take_value(&mut args, "--subc")?));
        } else if arg == OsStr::new("--connection-file") {
            own_connection_file = Some(PathBuf::from(take_value(&mut args, "--connection-file")?));
        } else {
            return Err(invalid_input(format!(
                "unknown module argument '{}'.\n{USAGE}",
                arg.to_string_lossy()
            )));
        }
    }

    let Some(subc_connection_file) = subc_connection_file else {
        return Err(invalid_input(format!(
            "missing required module argument --subc <subc-connection-file>.\n{USAGE}"
        )));
    };

    Ok(ModuleArgs {
        subc_connection_file,
        own_connection_file,
    })
}

fn take_value(args: &mut impl Iterator<Item = OsString>, flag: &str) -> Result<OsString> {
    args.next()
        .ok_or_else(|| invalid_input(format!("missing value for {flag}.\n{USAGE}")))
}

async fn run_shim(args: ShimArgs) -> Result<()> {
    let project_root = resolve_project_root()?;
    let connection_file_path = args
        .module_connection_file
        .unwrap_or_else(default_module_connection_file_path);
    let mut stream = connect_authenticated(&connection_file_path).await?;

    let hello = ShimHello {
        schema: SHIM_SCHEMA_VERSION,
        project_root,
        harness: args.harness,
        shim_session_id: generated_id("shim")?,
        instance_token: instance_token_from_env(),
    };
    write_json_message(&mut stream, &hello, MAX_SHIM_CONTROL_MESSAGE_LEN).await?;

    let ack: ShimHelloAck = read_json_message(&mut stream, MAX_SHIM_CONTROL_MESSAGE_LEN).await?;
    if ack.schema != SHIM_SCHEMA_VERSION {
        return Err(other_error(format!(
            "module replied with unsupported ShimHelloAck schema {} (expected {SHIM_SCHEMA_VERSION})",
            ack.schema
        )));
    }

    pipe_stdio(stream).await
}

async fn run_module(args: ModuleArgs) -> Result<()> {
    require_spawn_attestation()?;
    let subc_stream = connect_authenticated(&args.subc_connection_file).await?;
    let subc = SubcClient::start(subc_stream);
    let _supervision_task =
        start_supervision_connection_if_configured(&args.subc_connection_file, subc.relay())
            .await?;

    let listener = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0))
        .await
        .map_err(|source| other_error(format!("failed to bind shim listener: {source}")))?;
    let port = listener
        .local_addr()
        .map_err(|source| other_error(format!("failed to read shim listener address: {source}")))?
        .port();

    let key = generate_key()?;
    let daemon_id = generate_daemon_id()?;
    let connection_file_path = args
        .own_connection_file
        .unwrap_or_else(default_module_connection_file_path);
    publish_module_connection_file(&connection_file_path, key.clone(), daemon_id, port)?;

    loop {
        let (stream, _peer) = listener
            .accept()
            .await
            .map_err(|source| other_error(format!("failed to accept shim connection: {source}")))?;
        let subc = subc.clone();
        let key = key.clone();
        tokio::spawn(async move {
            if let Err(error) = handle_shim_connection(stream, subc, key, daemon_id).await {
                eprintln!("subc-mcp module: shim connection failed: {error}");
            }
        });
    }
}

async fn start_supervision_connection_if_configured(
    connection_file_path: &Path,
    relay: Arc<ReverseRelay>,
) -> Result<Option<JoinHandle<()>>> {
    let module_id = match env::var(SUBC_MODULE_ID_ENV) {
        Ok(module_id) if !module_id.trim().is_empty() => module_id,
        Ok(_) => {
            return Err(other_error(format!(
                "{SUBC_MODULE_ID_ENV} must not be empty when set"
            )))
        }
        Err(env::VarError::NotPresent) => return Ok(None),
        Err(env::VarError::NotUnicode(value)) => {
            return Err(other_error(format!(
                "{SUBC_MODULE_ID_ENV} must be valid UTF-8, got '{}'",
                value.to_string_lossy()
            )))
        }
    };

    let mut stream = connect_authenticated(connection_file_path).await?;
    send_supervision_hello(&mut stream, &module_id).await?;
    let task_module_id = module_id.clone();
    let task = tokio::spawn(async move {
        if let Err(error) = supervision_control_loop(stream, relay, task_module_id).await {
            eprintln!("subc-mcp module: supervision control loop failed: {error}");
        }
    });
    Ok(Some(task))
}

async fn send_supervision_hello(stream: &mut TcpStream, module_id: &str) -> Result<()> {
    let body = serde_json::to_vec(&ModuleHelloBody {
        manifest: supervision_manifest(module_id.to_owned()),
        protocol_ver: PROTOCOL_VERSION,
        control_ops: Some(vec![MODULE_CONTROL_OP_HEALTH_CHECK.to_owned()]),
        // Echo the one-time launch nonce subc injects for a reserved module; absent
        // (None) when this module is not reserved.
        launch_nonce: env::var(subc_protocol::SUBC_LAUNCH_NONCE_ENV)
            .ok()
            .filter(|value| !value.is_empty()),
    })
    .map_err(|source| {
        other_error(format!(
            "failed to encode supervision HELLO for module_id={module_id}: {source}"
        ))
    })?;
    let frame = build_frame(
        FrameType::Hello,
        control_flags(),
        0,
        0,
        SUPERVISION_HELLO_CORR,
        body,
    )
    .map_err(|source| {
        other_error(format!(
            "failed to build supervision HELLO for module_id={module_id}: {source}"
        ))
    })?;

    write_frame(stream, &frame).await.map_err(|source| {
        other_error(format!(
            "failed to write supervision HELLO for module_id={module_id}: {source}"
        ))
    })?;
    stream.flush().await.map_err(|source| {
        other_error(format!(
            "failed to flush supervision HELLO for module_id={module_id}: {source}"
        ))
    })?;

    let Some(frame) = read_frame(stream).await.map_err(|source| {
        other_error(format!(
            "failed to read supervision HELLO_ACK for module_id={module_id}: {source}"
        ))
    })?
    else {
        return Err(other_error(format!(
            "subc closed before supervision HELLO_ACK for module_id={module_id}"
        )));
    };

    match frame.header.ty {
        FrameType::HelloAck => {
            let _ack: ModuleHelloAckBody =
                serde_json::from_slice(&frame.body).map_err(|source| {
                    other_error(format!(
                    "failed to decode supervision HELLO_ACK for module_id={module_id}: {source}"
                ))
                })?;
            eprintln!("subc-mcp module: registered for supervision module_id={module_id}");
            Ok(())
        }
        FrameType::Error => {
            let body: ErrorBody = serde_json::from_slice(&frame.body).map_err(|source| {
                other_error(format!(
                    "subc rejected supervision HELLO for module_id={module_id} with malformed ERROR body: {source}"
                ))
            })?;
            Err(other_error(format!(
                "subc rejected supervision HELLO for module_id={module_id}: {}: {}",
                body.code, body.message
            )))
        }
        ty => Err(other_error(format!(
            "unexpected supervision HELLO_ACK frame type for module_id={module_id}: {ty:?}"
        ))),
    }
}

/// Reads and answers daemon-to-module control RPCs on the dedicated supervision
/// socket. Sending the reply from this same task proves the supervision read
/// loop is alive instead of only proving that some unrelated writer task runs.
async fn supervision_control_loop(
    mut stream: TcpStream,
    relay: Arc<ReverseRelay>,
    module_id: String,
) -> Result<()> {
    loop {
        let Some(frame) = read_frame(&mut stream).await.map_err(|source| {
            other_error(format!(
                "failed to read supervision control frame for module_id={module_id}: {source}"
            ))
        })?
        else {
            eprintln!("subc-mcp module: supervision connection closed for module_id={module_id}");
            return Ok(());
        };

        let Some(reply) = handle_supervision_control_frame(&frame, &relay).await? else {
            return Ok(());
        };
        write_frame(&mut stream, &reply).await.map_err(|source| {
            other_error(format!(
                "failed to write supervision control reply for module_id={module_id}: {source}"
            ))
        })?;
        stream.flush().await.map_err(|source| {
            other_error(format!(
                "failed to flush supervision control reply for module_id={module_id}: {source}"
            ))
        })?;
    }
}

async fn handle_supervision_control_frame(
    frame: &SubcFrame,
    relay: &ReverseRelay,
) -> Result<Option<SubcFrame>> {
    if frame.header.ty == FrameType::Goodbye && frame.header.channel == 0 {
        return Ok(None);
    }

    if frame.header.ty != FrameType::Request || frame.header.channel != 0 {
        return supervision_error_frame(
            frame,
            "unsupported_control_frame",
            format!(
                "supervision connection only accepts channel-0 Request frames, got {:?} on channel {}",
                frame.header.ty, frame.header.channel
            ),
        )
        .map(Some);
    }

    let request = match serde_json::from_slice::<ModuleControlRequest>(&frame.body) {
        Ok(request) => request,
        Err(error) => {
            let (code, message) = supervision_decode_error(&frame.body, error);
            return supervision_error_frame(frame, code, message).map(Some);
        }
    };

    match request {
        ModuleControlRequest::HealthCheck {} => {
            let report = HealthReport {
                status: HealthStatus::Ok,
                detail: None,
                metrics: Some(relay.health_metrics().await),
            };
            let body =
                serde_json::to_vec(&ModuleControlResponse::from(report)).map_err(|source| {
                    other_error(format!("failed to encode health.check response: {source}"))
                })?;
            Ok(Some(supervision_response_frame(frame, body)?))
        }
        other => supervision_error_frame(
            frame,
            "unexpected_control_op",
            format!("supervision connection does not handle {other:?}"),
        )
        .map(Some),
    }
}

#[derive(Deserialize)]
struct ModuleControlOpProbe {
    op: Option<String>,
}

fn supervision_decode_error(body: &[u8], error: serde_json::Error) -> (&'static str, String) {
    match serde_json::from_slice::<ModuleControlOpProbe>(body) {
        Ok(ModuleControlOpProbe { op: Some(op) }) if op == MODULE_CONTROL_OP_HEALTH_CHECK => (
            "invalid_control_body",
            format!("malformed health.check request body: {error}"),
        ),
        Ok(ModuleControlOpProbe { op: Some(op) }) if op == "route.bind" => (
            "unexpected_control_op",
            format!("supervision connection does not handle route.bind: {error}"),
        ),
        Ok(ModuleControlOpProbe { op: Some(op) }) => (
            "unknown_control_op",
            format!("unsupported module-control op '{op}': {error}"),
        ),
        _ => (
            "invalid_control_body",
            format!("malformed module-control request body: {error}"),
        ),
    }
}

fn supervision_response_frame(request: &SubcFrame, body: Vec<u8>) -> Result<SubcFrame> {
    SubcFrame::build_with_version(
        request.header.ver,
        FrameType::Response,
        control_flags(),
        0,
        0,
        request.header.corr,
        body,
    )
    .map_err(|source| other_error(format!("failed to build health.check response: {source}")))
}

fn supervision_error_frame(
    request: &SubcFrame,
    code: &str,
    message: impl Into<String>,
) -> Result<SubcFrame> {
    let body = serde_json::to_vec(&ErrorBody {
        code: code.to_owned(),
        message: message.into(),
    })
    .map_err(|source| other_error(format!("failed to encode supervision ERROR: {source}")))?;
    SubcFrame::build_with_version(
        request.header.ver,
        FrameType::Error,
        control_flags(),
        0,
        0,
        request.header.corr,
        body,
    )
    .map_err(|source| other_error(format!("failed to build supervision ERROR: {source}")))
}

fn supervision_manifest(module_id: String) -> ModuleManifest {
    ModuleManifest {
        module_id,
        module_version: env!("CARGO_PKG_VERSION").to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: Vec::new(),
        consumes: vec![ConsumerRole::ToolClient { of: Vec::new() }],
        scheduled_tasks: Vec::new(),
        bindings: supervision_bindings(),
    }
}

fn supervision_bindings() -> Bindings {
    // Manifest v1 requires concrete binding records. subc-mcp is only a
    // gateway/consumer here: it owns no subc-managed storage schema, secrets,
    // or identity grant for the supervision registration itself. Per-call
    // identity is supplied later when the route is opened. Keep every grant
    // empty and explicitly decline storage schema ownership.
    Bindings {
        storage: StorageBinding {
            kind: StorageKind::Sqlite,
            scope: StorageScope::Project,
            owns_schema: false,
        },
        vault_grants: Vec::new(),
        identity: IdentityBinding {
            requires: Vec::new(),
            optional: Vec::new(),
        },
    }
}

fn publish_module_connection_file(
    path: &Path,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
    port: u16,
) -> Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|source| {
            other_error(format!(
                "failed to create module connection-file directory {}: {source}",
                parent.display()
            ))
        })?;
    }

    let info = ConnectionInfo {
        schema: SCHEMA_VERSION,
        wire_version: None,
        endpoints: vec![Endpoint {
            host: Ipv4Addr::LOCALHOST.to_string(),
            port,
        }],
        key,
        daemon_id,
        pid: process::id(),
        daemon_ver: env!("CARGO_PKG_VERSION").to_owned(),
    };
    connection_file::write_atomic(path, &info).map_err(|source| {
        other_error(format!(
            "failed to publish module connection file {}: {source}",
            path.display()
        ))
    })
}

async fn handle_shim_connection(
    mut stream: TcpStream,
    subc: SubcClient,
    key: Vec<u8>,
    daemon_id: [u8; subc_transport::DAEMON_ID_LEN],
) -> Result<()> {
    authenticate_server(
        &mut stream,
        &key,
        &daemon_id,
        env!("CARGO_PKG_VERSION"),
        AUTH_DEADLINE,
    )
    .await
    .map_err(|source| other_error(format!("shim authentication failed: {source}")))?;

    let hello: ShimHello = read_json_message(&mut stream, MAX_SHIM_CONTROL_MESSAGE_LEN).await?;
    if hello.schema != SHIM_SCHEMA_VERSION {
        return Err(other_error(format!(
            "unsupported ShimHello schema {} (expected {SHIM_SCHEMA_VERSION})",
            hello.schema
        )));
    }
    write_json_message(
        &mut stream,
        &ShimHelloAck {
            schema: SHIM_SCHEMA_VERSION,
        },
        MAX_SHIM_CONTROL_MESSAGE_LEN,
    )
    .await?;

    let attached = attach_session(&subc, &hello).await?;
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let reconcile_gate = Arc::new(Mutex::new(()));

    let handler = SubcMcpServer {
        subc: subc.clone(),
        state: Arc::clone(&attached.state),
        relay_session: Arc::clone(&attached.relay_session),
        lifecycle_started: Arc::new(AtomicBool::new(false)),
        shutdown: shutdown_rx,
        reconcile_gate: Arc::clone(&reconcile_gate),
    };
    let (read_half, write_half) = stream.into_split();
    let transport = AsyncRwTransport::<RoleServer, _, _>::new_server(read_half, write_half);
    let serve_result = serve_mcp_server(handler, transport).await;
    let _ = shutdown_tx.send(true);
    let _reconcile_guard = reconcile_gate.lock().await;
    subc.relay()
        .fail_session(&attached.relay_session, "MCP host session disconnected")
        .await;
    subc.relay()
        .unregister_session_routes(&attached.relay_session)
        .await;
    let goodbye_result = send_route_goodbyes(&subc, attached.state.route_handles()).await;

    match (serve_result, goodbye_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Err(serve_error), Err(goodbye_error)) => Err(other_error(format!(
            "rmcp shim server failed: {serve_error}; additionally failed to send route goodbyes: {goodbye_error}"
        ))),
    }
}

async fn attach_session(subc: &SubcClient, hello: &ShimHello) -> Result<AttachedSession> {
    let config = read_gateway_config(&hello.project_root, &hello.harness)?;
    let catalog = catalog_list(subc).await?;
    let desired = desired_session_from_catalog(&config.effective, &catalog.modules)?;
    let identity = BindIdentity {
        project_root: hello.project_root.clone(),
        harness: hello.harness.clone(),
        session: bind_session_from_hello(hello)?,
    };

    let relay_session = Arc::new(RelaySession::new(identity.session.clone()));
    let mut routes = HashMap::new();
    for provider in &desired.providers {
        match open_provider_route(
            subc,
            &provider.module_id,
            &identity,
            relay_session.consumer_capabilities(),
            Arc::clone(&relay_session),
        )
        .await
        {
            Ok(route) => {
                routes.insert(provider.module_id.clone(), route);
            }
            Err(error) => {
                subc.relay().unregister_session_routes(&relay_session).await;
                let opened_routes = routes.values().copied().collect::<Vec<_>>();
                let _ = send_route_goodbyes(subc, opened_routes).await;
                return Err(error);
            }
        }
    }

    let opened_routes = routes.values().copied().collect::<Vec<_>>();
    let inner = match session_inner_from_desired(
        subc,
        catalog.generation,
        desired,
        routes,
        config.effective.surface_mode,
    ) {
        Ok(inner) => inner,
        Err(error) => {
            subc.relay().unregister_session_routes(&relay_session).await;
            let _ = send_route_goodbyes(subc, opened_routes).await;
            return Err(error);
        }
    };
    Ok(AttachedSession {
        state: Arc::new(SessionState::new(config, identity, inner)),
        relay_session,
    })
}

async fn catalog_list(subc: &SubcClient) -> Result<CatalogSnapshot> {
    let request = ClientControlRequest::CatalogList { module_id: None };
    let body = serde_json::to_vec(&request)?;
    let corr = subc.next_corr()?;
    let frame = build_frame(FrameType::Request, control_flags(), 0, 0, corr, body)?;
    let response = subc.request(frame, SUBC_RESPONSE_TIMEOUT).await?;

    match response.header.ty {
        FrameType::Response if response.header.channel == 0 => {
            match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
                ClientControlResponse::CatalogList {
                    generation,
                    modules,
                    ..
                } => Ok(CatalogSnapshot {
                    generation,
                    modules,
                }),
                other => Err(other_error(format!(
                    "unexpected catalog.list response body: {other:?}"
                ))),
            }
        }
        FrameType::Error => Err(error_response("subc rejected catalog.list", &response.body)),
        ty => Err(other_error(format!(
            "unexpected catalog.list response frame {ty:?} on channel {} corr {}",
            response.header.channel, response.header.corr
        ))),
    }
}

async fn open_provider_route(
    subc: &SubcClient,
    module_id: &str,
    identity: &BindIdentity,
    consumer_capabilities: Option<Vec<String>>,
    route_session: Arc<RelaySession>,
) -> Result<RouteHandle> {
    let request = ClientControlRequest::RouteOpen {
        target: RouteTarget::ToolProvider {
            module_id: module_id.to_owned(),
        },
        identity: identity.clone(),
        consumer_identity: consumer_identity_from_env(),
        consumer_capabilities,
    };
    let body = serde_json::to_vec(&request)?;
    let corr = subc.next_corr()?;
    let frame = build_frame(FrameType::Request, control_flags(), 0, 0, corr, body)?;
    let response = subc
        .request_route_open(frame, SUBC_RESPONSE_TIMEOUT, route_session)
        .await?;

    match response.header.ty {
        FrameType::Response if response.header.channel == 0 => {
            match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
                ClientControlResponse::RouteOpen {
                    route_channel,
                    route_epoch,
                } => Ok(subc.route_handle(route_channel, route_epoch)),
                other => Err(other_error(format!(
                    "unexpected route.open response body: {other:?}"
                ))),
            }
        }
        FrameType::Error => Err(error_response(
            &format!("subc rejected route.open for provider '{module_id}'"),
            &response.body,
        )),
        ty => Err(other_error(format!(
            "unexpected route.open response frame {ty:?} on channel {} corr {}",
            response.header.channel, response.header.corr
        ))),
    }
}

/// Refuse to serve without daemon spawn attestation. The MCP facade fronts
/// remote-model callers, so its route binds must reach providers stamped as the
/// attested `reserved:<module_id>` principal (which provider policy distrusts),
/// never as `direct` (which it trusts). Both env vars are injected by the
/// daemon on spawn; a facade started any other way (manual launch, a supervisor
/// that stopped injecting the nonce, an SDK regression dropping the attach)
/// would silently bind as a trusted first-party — the exact downgrade this
/// guard turns into a loud startup failure.
fn require_spawn_attestation() -> Result<()> {
    let module_id_present = env::var(SUBC_MODULE_ID_ENV)
        .map(|value| !value.trim().is_empty())
        .unwrap_or(false);
    let nonce_present = env::var(SUBC_LAUNCH_NONCE_ENV)
        .map(|value| !value.is_empty())
        .unwrap_or(false);
    if module_id_present && nonce_present {
        return Ok(());
    }
    Err(other_error(format!(
        "subc-mcp module requires daemon spawn attestation: {SUBC_MODULE_ID_ENV} and \
         {SUBC_LAUNCH_NONCE_ENV} must both be set (they are injected when subc spawns \
         the module). Run it as a supervised module from subc.jsonc; an unattested \
         facade would bind with the trusted 'direct' principal instead of \
         'reserved:<module_id>'."
    )))
}

fn consumer_identity_from_env() -> Option<ConsumerIdentity> {
    let module_id = env::var(SUBC_MODULE_ID_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    let launch_nonce = env::var(SUBC_LAUNCH_NONCE_ENV)
        .ok()
        .filter(|value| !value.is_empty())?;
    Some(ConsumerIdentity {
        module_id,
        launch_nonce,
    })
}

fn desired_session_from_catalog(
    config: &GatewayConfig,
    modules: &[CatalogEntry],
) -> Result<DesiredSession> {
    let mut providers = Vec::new();
    let mut exposed_names = HashMap::<String, (String, String)>::new();

    for entry in modules {
        let mut manifest_tools = Vec::new();
        for role in &entry.roles {
            if let ProviderRole::ToolProvider { tools, .. } = role {
                manifest_tools.extend(tools.iter().cloned());
            }
        }
        if manifest_tools.is_empty() || !config.provider_enabled(&entry.module_id) {
            continue;
        }

        let namespace = config.provider_namespace(&entry.module_id);
        validate_mcp_name_component("provider namespace", &namespace).map_err(|message| {
            other_error(format!(
                "provider '{}' has invalid namespace '{namespace}': {message}; set providers.{}.namespace to an MCP-safe value",
                entry.module_id, entry.module_id
            ))
        })?;

        let mut tools = Vec::new();
        for tool in manifest_tools {
            validate_mcp_name_component("tool name", &tool.name).map_err(|message| {
                other_error(format!(
                    "provider '{}' manifest has invalid tool name '{}': {message}",
                    entry.module_id, tool.name
                ))
            })?;
            if !config.tool_enabled(&entry.module_id, &tool.name) {
                continue;
            }

            let exposed_name = format!("{namespace}_{}", tool.name);
            if is_reserved_meta_tool_name(&exposed_name) {
                return Err(other_error(format!(
                    "MCP tool name collision for reserved exposed name '{exposed_name}'"
                )));
            }
            if let Some((other_module, other_bare)) = exposed_names.insert(
                exposed_name.clone(),
                (entry.module_id.clone(), tool.name.clone()),
            ) {
                return Err(other_error(format!(
                    "MCP tool name collision for '{exposed_name}': {}.{} and {}.{}",
                    other_module, other_bare, entry.module_id, tool.name
                )));
            }

            let description = config
                .tool_description(&entry.module_id, &tool.name)
                .unwrap_or_else(|| default_tool_description(&exposed_name));
            let mut exposed_manifest = tool.clone();
            exposed_manifest.name = exposed_name;
            tools.push(DesiredTool {
                mode: config.tool_mode(&entry.module_id, &tool.name),
                bare_tool: tool,
                exposed_tool: ExposedTool {
                    manifest: exposed_manifest,
                    description,
                },
            });
        }

        if tools.is_empty() {
            continue;
        }

        providers.push(DesiredProvider {
            module_id: entry.module_id.clone(),
            tools,
        });
    }

    Ok(DesiredSession { providers })
}

fn session_inner_from_desired(
    subc: &SubcClient,
    catalog_generation: u64,
    desired: DesiredSession,
    routes: HashMap<String, RouteHandle>,
    surface_mode: SurfaceMode,
) -> Result<SessionInner> {
    let mut tools = Vec::new();
    let mut bindings = HashMap::new();

    for provider in desired.providers {
        let route = *routes.get(&provider.module_id).ok_or_else(|| {
            other_error(format!(
                "missing route channel for enabled provider '{}'",
                provider.module_id
            ))
        })?;
        for desired_tool in provider.tools {
            let exposed_name = desired_tool.exposed_tool.manifest.name.clone();
            let binding = match desired_tool.mode {
                ToolMode::Forward => ToolBinding::Forward(ForwardBinding {
                    route,
                    bare_tool_name: desired_tool.bare_tool.name,
                }),
                ToolMode::AckOnly => ToolBinding::AckOnly {
                    acks: subc.relay().ack_only_counter(&exposed_name),
                },
            };
            bindings.insert(exposed_name.clone(), binding);
            tools.push(desired_tool.exposed_tool);
        }
    }

    tools.sort_by(|left, right| left.manifest.name.cmp(&right.manifest.name));
    Ok(SessionInner {
        surface_mode,
        catalog_generation,
        routes,
        tools,
        bindings,
    })
}

impl SessionState {
    fn read_config(&self) -> RwLockReadGuard<'_, ConfigSnapshot> {
        self.config.read().unwrap_or_else(|poisoned| {
            eprintln!("subc-mcp module: warning: recovering from poisoned config read lock");
            poisoned.into_inner()
        })
    }

    fn write_config(&self) -> RwLockWriteGuard<'_, ConfigSnapshot> {
        self.config.write().unwrap_or_else(|poisoned| {
            eprintln!("subc-mcp module: warning: recovering from poisoned config write lock");
            poisoned.into_inner()
        })
    }

    fn read_inner(&self) -> RwLockReadGuard<'_, SessionInner> {
        self.inner.read().unwrap_or_else(|poisoned| {
            eprintln!("subc-mcp module: warning: recovering from poisoned session-state read lock");
            poisoned.into_inner()
        })
    }

    fn write_inner(&self) -> RwLockWriteGuard<'_, SessionInner> {
        self.inner.write().unwrap_or_else(|poisoned| {
            eprintln!(
                "subc-mcp module: warning: recovering from poisoned session-state write lock"
            );
            poisoned.into_inner()
        })
    }

    fn new(config: ConfigSnapshot, identity: BindIdentity, inner: SessionInner) -> Self {
        Self {
            config: RwLock::new(config),
            identity,
            inner: RwLock::new(inner),
        }
    }

    fn config_snapshot(&self) -> ConfigSnapshot {
        self.read_config().clone()
    }

    fn replace_config(&self, next: ConfigSnapshot) {
        *self.write_config() = next;
    }

    fn exposed_tools(&self) -> Vec<ExposedTool> {
        let inner = self.read_inner();
        match inner.surface_mode {
            SurfaceMode::Full => inner.tools.clone(),
            SurfaceMode::Search => search_meta_tools(),
        }
    }

    fn resolved_tools(&self) -> Vec<ExposedTool> {
        self.read_inner().tools.clone()
    }

    fn get_tool(&self, name: &str) -> Option<ExposedTool> {
        let inner = self.read_inner();
        match inner.surface_mode {
            SurfaceMode::Full => inner
                .tools
                .iter()
                .find(|tool| tool.manifest.name == name)
                .cloned(),
            SurfaceMode::Search => meta_tool(name),
        }
    }

    fn direct_binding(&self, name: &str) -> Option<ToolBinding> {
        let inner = self.read_inner();
        match inner.surface_mode {
            SurfaceMode::Full => inner.bindings.get(name).cloned(),
            SurfaceMode::Search => None,
        }
    }

    fn private_binding(&self, name: &str) -> Option<ToolBinding> {
        self.read_inner().bindings.get(name).cloned()
    }

    fn surface_mode(&self) -> SurfaceMode {
        self.read_inner().surface_mode
    }

    fn route_handles(&self) -> Vec<RouteHandle> {
        let mut handles = self
            .read_inner()
            .routes
            .values()
            .copied()
            .collect::<Vec<_>>();
        handles.sort_unstable_by_key(|handle| (handle.channel, handle.epoch));
        handles.dedup();
        handles
    }

    fn catalog_generation(&self) -> u64 {
        self.read_inner().catalog_generation
    }

    fn route_snapshot(&self) -> HashMap<String, RouteHandle> {
        self.read_inner().routes.clone()
    }

    fn remove_route(&self, handle: RouteHandle) -> bool {
        let mut inner = self.write_inner();
        let old_tools = inner.tools.clone();
        let removed_modules = inner
            .routes
            .iter()
            .filter(|(_, route)| **route == handle)
            .map(|(module_id, _)| module_id.clone())
            .collect::<HashSet<_>>();
        if removed_modules.is_empty() {
            return false;
        }

        inner
            .routes
            .retain(|module_id, _| !removed_modules.contains(module_id));
        // Ack-only bindings survive route death: they never touch the route, so
        // the tool stays serviceable while the provider reconnects.
        inner.bindings.retain(|_, binding| match binding {
            ToolBinding::Forward(forward) => forward.route != handle,
            ToolBinding::AckOnly { .. } => true,
        });
        let live_names = inner.bindings.keys().cloned().collect::<HashSet<_>>();
        inner
            .tools
            .retain(|tool| live_names.contains(&tool.manifest.name));
        old_tools != inner.tools
    }

    fn replace_inner(&self, next: SessionInner) -> bool {
        let mut inner = self.write_inner();
        let changed = inner.surface_mode != next.surface_mode || inner.tools != next.tools;
        *inner = next;
        changed
    }
}

impl GatewayConfig {
    fn facade_default() -> Self {
        let mut config = Self::default();
        for module_id in FACADE_DEFAULT_DISABLED {
            config
                .providers
                .entry((*module_id).to_owned())
                .or_default()
                .enabled = Some(false);
        }
        config
    }

    fn provider_enabled(&self, module_id: &str) -> bool {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.enabled)
            .unwrap_or(true)
    }

    fn provider_namespace(&self, module_id: &str) -> String {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.namespace.clone())
            .unwrap_or_else(|| module_id.to_owned())
    }

    fn tool_enabled(&self, module_id: &str, tool_name: &str) -> bool {
        let Some(provider) = self.providers.get(module_id) else {
            return true;
        };
        provider
            .tools
            .overrides
            .get(tool_name)
            .and_then(|override_config| override_config.enabled)
            .unwrap_or_else(|| provider.tools.default_enabled.unwrap_or(true))
    }

    fn tool_description(&self, module_id: &str, tool_name: &str) -> Option<String> {
        self.providers
            .get(module_id)?
            .tools
            .overrides
            .get(tool_name)?
            .description
            .clone()
    }

    fn tool_mode(&self, module_id: &str, tool_name: &str) -> ToolMode {
        self.providers
            .get(module_id)
            .and_then(|provider| provider.tools.overrides.get(tool_name))
            .and_then(|override_config| override_config.mode)
            .unwrap_or_default()
    }
}

fn default_tool_description(name: &str) -> String {
    format!("subc tool {name}")
}

fn is_reserved_meta_tool_name(name: &str) -> bool {
    matches!(name, TOOLS_SEARCH_NAME | TOOLS_INVOKE_NAME)
}

fn search_meta_tools() -> Vec<ExposedTool> {
    vec![
        ExposedTool {
            manifest: ManifestTool {
                name: TOOLS_SEARCH_NAME.to_owned(),
                description: None,
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": { "type": "string" },
                        "limit": { "type": "integer", "minimum": 0 }
                    },
                    "required": ["query"],
                    "additionalProperties": false
                }),
            },
            description: "Search the policy-enabled live subc tool catalog".to_owned(),
        },
        ExposedTool {
            manifest: ManifestTool {
                name: TOOLS_INVOKE_NAME.to_owned(),
                description: None,
                execution_mode: ExecutionMode::Unfenceable,
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": { "type": "string" },
                        "arguments": { "type": "object" }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
            },
            description: "Invoke a policy-enabled live subc tool by name".to_owned(),
        },
    ]
}

fn meta_tool(name: &str) -> Option<ExposedTool> {
    search_meta_tools()
        .into_iter()
        .find(|tool| tool.manifest.name == name)
}

fn validate_mcp_name_component(kind: &str, value: &str) -> std::result::Result<(), String> {
    if value.is_empty() {
        return Err(format!("{kind} must not be empty"));
    }
    if value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        Ok(())
    } else {
        Err(format!(
            "{kind} must contain only ASCII letters, digits, '_' or '-'"
        ))
    }
}

fn read_gateway_config(project_root: &Path, harness_name: &str) -> Result<ConfigSnapshot> {
    let mut effective = GatewayConfig::facade_default();
    let user_path = user_mcp_config_path();
    let project_path = project_root.join(PROJECT_MCP_CONFIG_RELATIVE_PATH);

    if let Some(raw) = read_raw_gateway_config("user", &user_path)? {
        let (top_level, harness_layers) = raw.into_parts();
        merge_gateway_config(&mut effective, top_level);
        for harness_layer in matching_harness_layers(harness_layers, harness_name) {
            merge_gateway_config(&mut effective, harness_layer);
        }
    }

    if let Some(raw) = read_raw_gateway_config("project", &project_path)? {
        let (top_level, harness_layers) = raw.into_parts();
        merge_project_gateway_config(&mut effective, top_level);
        for harness_layer in matching_harness_layers(harness_layers, harness_name) {
            merge_project_gateway_config(&mut effective, harness_layer);
        }
    }

    Ok(ConfigSnapshot {
        effective,
        files: gateway_config_file_snapshot(user_path, project_path)?,
    })
}

fn read_raw_gateway_config(tier: &str, path: &Path) -> Result<Option<RawGatewayConfig>> {
    let doc = match fs::read_to_string(path) {
        Ok(doc) => doc,
        Err(err) if err.kind() == stdio::ErrorKind::NotFound => return Ok(None),
        Err(err) => {
            return Err(other_error(format!(
                "failed to read {tier} MCP config {}: {err}",
                path.display()
            )))
        }
    };
    parse_gateway_config_doc(&doc, path).map(Some)
}

fn gateway_config_file_snapshot(
    user_path: PathBuf,
    project_path: PathBuf,
) -> Result<ConfigFileSnapshot> {
    Ok(ConfigFileSnapshot {
        user: config_file_state(user_path)?,
        project: config_file_state(project_path)?,
    })
}

fn config_file_state(path: PathBuf) -> Result<ConfigFileState> {
    match fs::metadata(&path) {
        Ok(metadata) => Ok(ConfigFileState {
            path,
            modified: metadata.modified().ok(),
            len: Some(metadata.len()),
        }),
        Err(err) if err.kind() == stdio::ErrorKind::NotFound => Ok(ConfigFileState {
            path,
            modified: None,
            len: None,
        }),
        Err(err) => Err(other_error(format!(
            "failed to stat MCP config {}: {err}",
            path.display()
        ))),
    }
}

fn config_files_changed(snapshot: &ConfigSnapshot) -> Result<bool> {
    Ok(gateway_config_file_snapshot(
        snapshot.files.user.path.clone(),
        snapshot.files.project.path.clone(),
    )? != snapshot.files)
}

fn user_mcp_config_path() -> PathBuf {
    if let Some(config_home) = non_empty_os_var("XDG_CONFIG_HOME") {
        return PathBuf::from(config_home).join(MCP_CONFIG_RELATIVE_PATH);
    }
    if let Some(home) = non_empty_os_var("HOME") {
        return PathBuf::from(home)
            .join(".config")
            .join(MCP_CONFIG_RELATIVE_PATH);
    }
    PathBuf::from(".config").join(MCP_CONFIG_RELATIVE_PATH)
}

fn parse_gateway_config_doc(doc: &str, path: &Path) -> Result<RawGatewayConfig> {
    let json = jsonc_to_json(doc).map_err(|message| {
        other_error(format!(
            "invalid JSONC in MCP config {}: {message}",
            path.display()
        ))
    })?;
    let raw: RawGatewayConfig = serde_json::from_str(&json).map_err(|source| {
        other_error(format!("invalid MCP config {}: {source}", path.display()))
    })?;
    if raw.version != 1 {
        return Err(other_error(format!(
            "invalid MCP config {}: version {} is unsupported (expected 1)",
            path.display(),
            raw.version
        )));
    }
    validate_raw_gateway_config(&raw, path)?;
    Ok(raw)
}

fn validate_raw_gateway_config(raw: &RawGatewayConfig, path: &Path) -> Result<()> {
    validate_raw_providers(&raw.providers, path, "providers")?;
    for (harness_name, harness) in &raw.harness {
        validate_raw_providers(
            &harness.providers,
            path,
            &format!("harness.{harness_name}.providers"),
        )?;
    }
    Ok(())
}

fn validate_raw_providers(
    providers: &HashMap<String, RawProviderConfig>,
    path: &Path,
    prefix: &str,
) -> Result<()> {
    for (module_id, provider) in providers {
        if let MaybeSet::Value(tools) = &provider.tools {
            validate_raw_tool_config(tools, path, &format!("{prefix}.{module_id}.tools"))?;
        }
    }
    Ok(())
}

fn validate_raw_tool_config(tools: &RawToolConfig, path: &Path, prefix: &str) -> Result<()> {
    for (tool_name, override_value) in &tools.overrides {
        if let RawToolOverrideValue::Object(object) = override_value {
            if matches!(object.enabled, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{tool_name}.enabled must be omitted instead of null; use null for the whole override entry to delete it",
                    path.display()
                )));
            }
            if matches!(object.description, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{tool_name}.description must be omitted instead of null",
                    path.display()
                )));
            }
            if matches!(object.mode, MaybeSet::Null) {
                return Err(other_error(format!(
                    "invalid MCP config {}: {prefix}.overrides.{tool_name}.mode must be omitted instead of null",
                    path.display()
                )));
            }
        }
    }
    Ok(())
}

impl RawGatewayConfig {
    fn into_parts(self) -> (RawGatewayLayer, HashMap<String, RawGatewayOverlayConfig>) {
        (
            RawGatewayLayer {
                surface_mode: self.surface_mode,
                refresh: self.refresh,
                providers: self.providers,
            },
            self.harness,
        )
    }
}

impl From<RawGatewayOverlayConfig> for RawGatewayLayer {
    fn from(raw: RawGatewayOverlayConfig) -> Self {
        Self {
            surface_mode: raw.surface_mode,
            refresh: raw.refresh,
            providers: raw.providers,
        }
    }
}

fn matching_harness_layers(
    harness_layers: HashMap<String, RawGatewayOverlayConfig>,
    harness_name: &str,
) -> Vec<RawGatewayLayer> {
    let wanted = harness_name.to_ascii_lowercase();
    let mut matches = harness_layers
        .into_iter()
        .filter(|(name, _)| name.to_ascii_lowercase() == wanted)
        .collect::<Vec<_>>();
    matches.sort_by(|(left, _), (right, _)| left.cmp(right));
    matches
        .into_iter()
        .map(|(_, layer)| RawGatewayLayer::from(layer))
        .collect()
}

fn merge_gateway_config(effective: &mut GatewayConfig, raw: RawGatewayLayer) {
    match raw.surface_mode {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.surface_mode = SurfaceMode::Full,
        MaybeSet::Value(surface_mode) => effective.surface_mode = surface_mode,
    }
    match raw.refresh {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.refresh = RefreshMode::OnAttach,
        MaybeSet::Value(refresh) => effective.refresh = refresh,
    }

    for (module_id, raw_provider) in raw.providers {
        let provider = effective.providers.entry(module_id).or_default();
        match raw_provider.enabled {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.enabled = None,
            MaybeSet::Value(enabled) => provider.enabled = Some(enabled),
        }
        match raw_provider.namespace {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.namespace = None,
            MaybeSet::Value(namespace) => provider.namespace = Some(namespace),
        }
        match raw_provider.tools {
            MaybeSet::Missing => {}
            MaybeSet::Null => provider.tools = ToolConfig::default(),
            MaybeSet::Value(tools) => merge_tool_config(&mut provider.tools, tools),
        }
    }
}

fn merge_project_gateway_config(effective: &mut GatewayConfig, raw: RawGatewayLayer) {
    match raw.surface_mode {
        MaybeSet::Missing => {}
        MaybeSet::Value(SurfaceMode::Search) => effective.surface_mode = SurfaceMode::Search,
        MaybeSet::Value(SurfaceMode::Full) | MaybeSet::Null => {
            if effective.surface_mode == SurfaceMode::Search {
                warn_project_drop(
                    "surfaceMode",
                    "project MCP config cannot widen a search surface back to full",
                );
            }
        }
    }
    if !matches!(raw.refresh, MaybeSet::Missing) {
        warn_project_drop(
            "refresh",
            "project MCP config cannot weaken user-chosen refresh latency",
        );
    }

    for (module_id, raw_provider) in raw.providers {
        let baseline = effective.clone();
        {
            let provider = effective.providers.entry(module_id.clone()).or_default();
            match raw_provider.enabled {
                MaybeSet::Missing => {}
                MaybeSet::Value(false) => provider.enabled = Some(false),
                MaybeSet::Value(true) => {
                    if baseline.provider_enabled(&module_id) {
                        provider.enabled = Some(true);
                    } else {
                        warn_project_drop(
                            &format!("providers.{module_id}.enabled"),
                            "project MCP config cannot enable a provider disabled by the user baseline",
                        );
                    }
                }
                MaybeSet::Null => {
                    if baseline.provider_enabled(&module_id) {
                        provider.enabled = None;
                    } else {
                        warn_project_drop(
                            &format!("providers.{module_id}.enabled"),
                            "project MCP config cannot delete a provider deny from the user baseline",
                        );
                    }
                }
            }
            if !matches!(raw_provider.namespace, MaybeSet::Missing) {
                warn_project_drop(
                    &format!("providers.{module_id}.namespace"),
                    "project MCP config cannot rename model-facing tool identities",
                );
            }
        }

        match raw_provider.tools {
            MaybeSet::Missing => {}
            MaybeSet::Null => warn_project_drop(
                &format!("providers.{module_id}.tools"),
                "project MCP config cannot reset inherited tool policy",
            ),
            MaybeSet::Value(tools) => {
                merge_project_tool_config(effective, &baseline, &module_id, tools);
            }
        }
    }
}

fn merge_tool_config(effective: &mut ToolConfig, raw: RawToolConfig) {
    match raw.default_enabled {
        MaybeSet::Missing => {}
        MaybeSet::Null => effective.default_enabled = None,
        MaybeSet::Value(default_enabled) => effective.default_enabled = Some(default_enabled),
    }
    for (tool_name, override_value) in raw.overrides {
        match override_value {
            RawToolOverrideValue::Null(()) => {
                effective.overrides.remove(&tool_name);
            }
            RawToolOverrideValue::Bool(enabled) => {
                effective.overrides.entry(tool_name).or_default().enabled = Some(enabled);
            }
            RawToolOverrideValue::Object(object) => {
                let override_config = effective.overrides.entry(tool_name).or_default();
                match object.enabled {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override enabled null"),
                    MaybeSet::Value(enabled) => override_config.enabled = Some(enabled),
                }
                match object.description {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override description null"),
                    MaybeSet::Value(description) => override_config.description = Some(description),
                }
                match object.mode {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override mode null"),
                    MaybeSet::Value(mode) => override_config.mode = Some(mode),
                }
            }
        }
    }
}

fn merge_project_tool_config(
    effective: &mut GatewayConfig,
    baseline: &GatewayConfig,
    module_id: &str,
    raw: RawToolConfig,
) {
    let baseline_default_enabled = baseline
        .providers
        .get(module_id)
        .map(|provider| provider.tools.default_enabled.unwrap_or(true))
        .unwrap_or(true);
    let provider = effective.providers.entry(module_id.to_owned()).or_default();
    match raw.default_enabled {
        MaybeSet::Missing => {}
        MaybeSet::Value(false) => provider.tools.default_enabled = Some(false),
        MaybeSet::Value(true) => {
            if baseline_default_enabled {
                provider.tools.default_enabled = Some(true);
            } else {
                warn_project_drop(
                    &format!("providers.{module_id}.tools.defaultEnabled"),
                    "project MCP config cannot enable tools disabled by the user baseline",
                );
            }
        }
        MaybeSet::Null => {
            if baseline_default_enabled {
                provider.tools.default_enabled = None;
            } else {
                warn_project_drop(
                    &format!("providers.{module_id}.tools.defaultEnabled"),
                    "project MCP config cannot delete a default tool deny from the user baseline",
                );
            }
        }
    }

    for (tool_name, override_value) in raw.overrides {
        let field = format!("providers.{module_id}.tools.overrides.{tool_name}");
        let baseline_callable =
            baseline.provider_enabled(module_id) && baseline.tool_enabled(module_id, &tool_name);
        let baseline_mode = baseline.tool_mode(module_id, &tool_name);
        match override_value {
            RawToolOverrideValue::Null(()) => {
                let has_description = provider
                    .tools
                    .overrides
                    .get(&tool_name)
                    .and_then(|override_config| override_config.description.as_ref())
                    .is_some();
                if baseline_callable && !has_description && baseline_mode == ToolMode::Forward {
                    provider.tools.overrides.remove(&tool_name);
                } else {
                    warn_project_drop(
                        &field,
                        "project MCP config cannot delete a deny, ack-only mode, or inherited tool description",
                    );
                }
            }
            RawToolOverrideValue::Bool(enabled) => merge_project_override_enabled(
                &mut provider.tools,
                &field,
                &tool_name,
                enabled,
                baseline_callable,
            ),
            RawToolOverrideValue::Object(object) => {
                if !matches!(object.description, MaybeSet::Missing) {
                    warn_project_drop(
                        &format!("{field}.description"),
                        "project MCP config cannot rewrite model-facing tool descriptions",
                    );
                }
                match object.enabled {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override enabled null"),
                    MaybeSet::Value(enabled) => merge_project_override_enabled(
                        &mut provider.tools,
                        &format!("{field}.enabled"),
                        &tool_name,
                        enabled,
                        baseline_callable,
                    ),
                }
                match object.mode {
                    MaybeSet::Missing => {}
                    MaybeSet::Null => unreachable!("validated object override mode null"),
                    MaybeSet::Value(mode) => merge_project_override_mode(
                        &mut provider.tools,
                        &format!("{field}.mode"),
                        &tool_name,
                        mode,
                        baseline_mode,
                    ),
                }
            }
        }
    }
}

fn merge_project_override_enabled(
    effective: &mut ToolConfig,
    field: &str,
    tool_name: &str,
    enabled: bool,
    baseline_callable: bool,
) {
    if !enabled || baseline_callable {
        effective
            .overrides
            .entry(tool_name.to_owned())
            .or_default()
            .enabled = Some(enabled);
    } else {
        warn_project_drop(
            field,
            "project MCP config cannot enable a tool disabled by the user baseline",
        );
    }
}

fn merge_project_override_mode(
    effective: &mut ToolConfig,
    field: &str,
    tool_name: &str,
    mode: ToolMode,
    baseline_mode: ToolMode,
) {
    if mode == ToolMode::AckOnly || baseline_mode == ToolMode::Forward {
        effective
            .overrides
            .entry(tool_name.to_owned())
            .or_default()
            .mode = Some(mode);
    } else {
        warn_project_drop(
            field,
            "project MCP config cannot forward a tool acknowledged by the user baseline",
        );
    }
}

fn warn_project_drop(field: &str, reason: &str) {
    eprintln!("subc-mcp module: warning: dropping project MCP config field {field}: {reason}");
}

fn deserialize_maybe_set<'de, D, T>(deserializer: D) -> std::result::Result<MaybeSet<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(|value| match value {
        Some(value) => MaybeSet::Value(value),
        None => MaybeSet::Null,
    })
}

async fn reconcile_session_from_catalog(
    subc: &SubcClient,
    state: &SessionState,
    relay_session: &Arc<RelaySession>,
    catalog: CatalogSnapshot,
    config: &GatewayConfig,
) -> Result<bool> {
    let desired = desired_session_from_catalog(config, &catalog.modules)?;
    let existing_routes = state.route_snapshot();
    let desired_modules = desired
        .providers
        .iter()
        .map(|provider| provider.module_id.clone())
        .collect::<HashSet<_>>();
    let removed_routes = existing_routes
        .iter()
        .filter_map(|(module_id, handle)| (!desired_modules.contains(module_id)).then_some(*handle))
        .collect::<Vec<_>>();

    let mut routes = HashMap::new();
    let mut opened_routes = Vec::new();
    for provider in &desired.providers {
        if let Some(route) = existing_routes.get(&provider.module_id) {
            routes.insert(provider.module_id.clone(), *route);
            continue;
        }
        let route = match open_provider_route(
            subc,
            &provider.module_id,
            &state.identity,
            relay_session.consumer_capabilities(),
            Arc::clone(relay_session),
        )
        .await
        {
            Ok(route) => route,
            Err(error) => {
                for route in &opened_routes {
                    subc.relay().drop_route(*route).await;
                }
                let _ = send_route_goodbyes(subc, opened_routes).await;
                return Err(error);
            }
        };
        opened_routes.push(route);
        routes.insert(provider.module_id.clone(), route);
    }

    let inner = match session_inner_from_desired(
        subc,
        catalog.generation,
        desired,
        routes,
        config.surface_mode,
    ) {
        Ok(inner) => inner,
        Err(error) => {
            for route_channel in &opened_routes {
                subc.relay().drop_route(*route_channel).await;
            }
            let _ = send_route_goodbyes(subc, opened_routes).await;
            return Err(error);
        }
    };
    let changed = state.replace_inner(inner);
    if !removed_routes.is_empty() {
        for route in &removed_routes {
            subc.relay().drop_route(*route).await;
        }
        let _ = send_route_goodbyes(subc, removed_routes).await;
    }
    Ok(changed)
}

async fn session_lifecycle(
    subc: SubcClient,
    state: Arc<SessionState>,
    relay_session: Arc<RelaySession>,
    mut events: broadcast::Receiver<SubcEvent>,
    peer: Peer<RoleServer>,
    mut shutdown: watch::Receiver<bool>,
    reconcile_gate: Arc<Mutex<()>>,
) {
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            event = events.recv() => {
                match event {
                    Ok(SubcEvent::RouteGoodbye { handle }) => {
                        if state.remove_route(handle) && !notify_tool_list_changed(&peer).await {
                            break;
                        }
                    }
                    Ok(SubcEvent::CatalogChanged { generation }) => {
                        if generation == state.catalog_generation() {
                            continue;
                        }
                        match catalog_list(&subc).await {
                            Ok(catalog) => {
                                if *shutdown.borrow() {
                                    break;
                                }
                                let reconciliation = {
                                    let _reconcile_guard = reconcile_gate.lock().await;
                                    if *shutdown.borrow() {
                                        break;
                                    }
                                    {
                                        let config = state.config_snapshot();
                                        reconcile_session_from_catalog(
                                            &subc,
                                            &state,
                                            &relay_session,
                                            catalog,
                                            &config.effective,
                                        )
                                        .await
                                    }
                                };
                                match reconciliation {
                                    Ok(true) => {
                                        if !notify_tool_list_changed(&peer).await {
                                            break;
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        eprintln!("subc-mcp module: keeping previous MCP tool snapshot after catalog reconciliation failed: {error}");
                                    }
                                }
                            }
                            Err(error) => {
                                eprintln!("subc-mcp module: failed to refresh catalog after generation {generation}: {error}");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        match catalog_list(&subc).await {
                            Ok(catalog) => {
                                if *shutdown.borrow() {
                                    break;
                                }
                                let reconciliation = {
                                    let _reconcile_guard = reconcile_gate.lock().await;
                                    if *shutdown.borrow() {
                                        break;
                                    }
                                    {
                                        let config = state.config_snapshot();
                                        reconcile_session_from_catalog(
                                            &subc,
                                            &state,
                                            &relay_session,
                                            catalog,
                                            &config.effective,
                                        )
                                        .await
                                    }
                                };
                                match reconciliation {
                                    Ok(true) => {
                                        if !notify_tool_list_changed(&peer).await {
                                            break;
                                        }
                                    }
                                    Ok(false) => {}
                                    Err(error) => {
                                        eprintln!("subc-mcp module: keeping previous MCP tool snapshot after lagged catalog reconciliation failed: {error}");
                                    }
                                }
                            }
                            Err(error) => {
                                eprintln!("subc-mcp module: failed to refresh catalog after lagged events: {error}");
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

async fn notify_tool_list_changed(peer: &Peer<RoleServer>) -> bool {
    match peer.notify_tool_list_changed().await {
        Ok(()) => true,
        Err(error) => {
            eprintln!("subc-mcp module: failed to notify MCP tools/list_changed: {error}");
            false
        }
    }
}

#[derive(Clone)]
struct SubcMcpServer {
    subc: SubcClient,
    state: Arc<SessionState>,
    relay_session: Arc<RelaySession>,
    lifecycle_started: Arc<AtomicBool>,
    shutdown: watch::Receiver<bool>,
    reconcile_gate: Arc<Mutex<()>>,
}

/// v1 subc-mcp ↔ provider tool-call request contract carried as an opaque
/// subc route-channel `REQUEST` body. `name` is the provider's bare manifest
/// tool name and `arguments` is the exact MCP request object. `Tool.schema` in
/// the manifest is the agent-facing schema the provider accepts; the gateway
/// never translates arguments.
#[derive(Debug, Serialize)]
struct RouteToolCallRequest {
    name: String,
    arguments: JsonObject,
    #[serde(skip_serializing_if = "Option::is_none")]
    progress_token: Option<ProgressToken>,
}

/// v1 subc-mcp ↔ provider progress contract carried as an opaque route-channel
/// `PUSH` body before the terminal response for the same correlation id.
#[derive(Debug, Deserialize)]
struct RouteToolProgress {
    progress: f64,
    #[serde(default)]
    total: Option<f64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsSearchArgs {
    query: String,
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolsInvokeArgs {
    name: String,
    #[serde(default)]
    arguments: JsonObject,
}

#[derive(Debug, Serialize)]
struct ToolsSearchMatch {
    name: String,
    description: String,
    input_schema: JsonObject,
    execution_mode: ExecutionMode,
}

impl ServerHandler for SubcMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_tool_list_changed()
                .build(),
        )
        .with_server_info(Implementation::new("subc-mcp", env!("CARGO_PKG_VERSION")))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<ListToolsResult, ErrorData> {
        self.refresh_policy_if_needed(&context.peer).await?;
        Ok(ListToolsResult {
            tools: self
                .state
                .exposed_tools()
                .iter()
                .map(mcp_tool_from_exposed)
                .collect(),
            ..Default::default()
        })
    }

    fn get_tool(&self, name: &str) -> Option<McpTool> {
        self.state
            .get_tool(name)
            .as_ref()
            .map(mcp_tool_from_exposed)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.refresh_policy_if_needed(&context.peer).await?;
        match self.state.surface_mode() {
            SurfaceMode::Full => self.call_tool_over_route(request, context).await,
            SurfaceMode::Search => self.call_search_mode_tool(request, context).await,
        }
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if self
            .lifecycle_started
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return;
        }

        self.subc.ensure_catalog_poller();
        let subc = self.subc.clone();
        let state = Arc::clone(&self.state);
        let relay_session = Arc::clone(&self.relay_session);
        let events = self.subc.subscribe_events();
        let peer = context.peer.clone();
        self.relay_session.record_initialized_peer(peer.clone());
        let shutdown = self.shutdown.clone();
        let reconcile_gate = Arc::clone(&self.reconcile_gate);
        tokio::spawn(session_lifecycle(
            subc,
            state,
            relay_session,
            events,
            peer,
            shutdown,
            reconcile_gate,
        ));
    }
}

impl SubcMcpServer {
    async fn refresh_policy_if_needed(
        &self,
        peer: &Peer<RoleServer>,
    ) -> std::result::Result<(), ErrorData> {
        let snapshot = self.state.config_snapshot();
        if snapshot.effective.refresh != RefreshMode::Immediate {
            return Ok(());
        }

        let _reconcile_guard = self.reconcile_gate.lock().await;
        let snapshot = self.state.config_snapshot();
        if snapshot.effective.refresh != RefreshMode::Immediate {
            return Ok(());
        }
        if !config_files_changed(&snapshot).map_err(mcp_internal_error)? {
            return Ok(());
        }

        let next_config = read_gateway_config(
            &self.state.identity.project_root,
            &self.state.identity.harness,
        )
        .map_err(mcp_internal_error)?;
        let catalog = catalog_list(&self.subc).await.map_err(mcp_internal_error)?;
        let changed = reconcile_session_from_catalog(
            &self.subc,
            &self.state,
            &self.relay_session,
            catalog,
            &next_config.effective,
        )
        .await
        .map_err(mcp_internal_error)?;
        self.state.replace_config(next_config);
        if changed && !notify_tool_list_changed(peer).await {
            return Err(ErrorData::internal_error(
                "failed to notify MCP tools/list_changed after immediate policy refresh",
                None,
            ));
        }
        Ok(())
    }

    async fn call_search_mode_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        match request.name.as_ref() {
            TOOLS_SEARCH_NAME => self.handle_tools_search(request.arguments).await,
            TOOLS_INVOKE_NAME => self.handle_tools_invoke(request.arguments, context).await,
            other => Err(unknown_tool_error(other)),
        }
    }

    async fn handle_tools_search(
        &self,
        arguments: Option<JsonObject>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let args: ToolsSearchArgs = parse_tool_arguments(TOOLS_SEARCH_NAME, arguments)?;
        let query = args.query.trim();
        let mut matches = self
            .state
            .resolved_tools()
            .into_iter()
            .filter_map(|tool| {
                lexical_search_rank(&tool, query).map(|rank| {
                    (
                        rank,
                        tool.manifest.name.clone(),
                        ToolsSearchMatch {
                            name: tool.manifest.name.clone(),
                            description: tool.description.clone(),
                            input_schema: schema_value_to_object(&tool.manifest.schema),
                            execution_mode: tool.manifest.execution_mode,
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        matches.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

        let limit = args.limit.unwrap_or(matches.len());
        let results = matches
            .into_iter()
            .take(limit)
            .map(|(_, _, result)| result)
            .collect::<Vec<_>>();
        json_tool_result(serde_json::json!(results))
    }

    async fn handle_tools_invoke(
        &self,
        arguments: Option<JsonObject>,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let args: ToolsInvokeArgs = parse_tool_arguments(TOOLS_INVOKE_NAME, arguments)?;
        let Some(binding) = self.state.private_binding(&args.name) else {
            return Err(unknown_tool_error(&args.name));
        };
        self.dispatch_bound_tool(binding, args.arguments, context)
            .await
    }

    async fn call_tool_over_route(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let Some(binding) = self.state.direct_binding(&request.name) else {
            return Err(unknown_tool_error(&request.name));
        };
        self.dispatch_bound_tool(binding, request.arguments.unwrap_or_default(), context)
            .await
    }

    async fn dispatch_bound_tool(
        &self,
        binding: ToolBinding,
        arguments: JsonObject,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        match binding {
            ToolBinding::AckOnly { acks } => {
                acks.fetch_add(1, Ordering::Relaxed);
                ack_only_tool_result()
            }
            ToolBinding::Forward(forward) => {
                self.call_bound_tool(forward, arguments, context).await
            }
        }
    }

    async fn call_bound_tool(
        &self,
        binding: ForwardBinding,
        arguments: JsonObject,
        context: RequestContext<RoleServer>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let route = binding.route;
        let progress_token = context.meta.get_progress_token();
        let body = RouteToolCallRequest {
            name: binding.bare_tool_name,
            arguments,
            progress_token: progress_token.clone(),
        };
        let body = serde_json::to_vec(&body).map_err(mcp_internal_error)?;
        let corr = self.subc.next_corr().map_err(mcp_internal_error)?;
        let frame = self
            .subc
            .build_route_frame(FrameType::Request, data_flags(), route, corr, body)
            .map_err(mcp_internal_error)?;
        let mut frames = self
            .subc
            .request_frames(frame)
            .await
            .map_err(mcp_internal_error)?;

        loop {
            tokio::select! {
                _ = context.ct.cancelled() => {
                    let cancel_result = self.send_route_cancel(route, corr).await;
                    self.subc.abandon_request(route, corr).await;
                    cancel_result.map_err(mcp_internal_error)?;
                    return Err(ErrorData::new(
                        ErrorCode::INTERNAL_ERROR,
                        "tool call cancelled by MCP client",
                        None,
                    ));
                }
                frame = frames.recv() => {
                    let Some(frame) = frame else {
                        return Err(ErrorData::internal_error(
                            format!(
                                "subc route ({}, {}) closed before terminal tool response for corr {corr}",
                                route.channel, route.epoch
                            ),
                            None,
                        ));
                    };

                    match frame.header.ty {
                        FrameType::Push => {
                            if let Err(err) =
                                forward_progress(&context, progress_token.clone(), &frame.body).await
                            {
                                let cancel_result = self.send_route_cancel(route, corr).await;
                                self.subc.abandon_request(route, corr).await;
                                cancel_result.map_err(mcp_internal_error)?;
                                return Err(err);
                            }
                        }
                        FrameType::Response => {
                            return serde_json::from_slice::<CallToolResult>(&frame.body).map_err(|source| {
                                ErrorData::internal_error(
                                    format!("provider returned malformed tool result: {source}"),
                                    None,
                                )
                            });
                        }
                        FrameType::Error => {
                            return Err(subc_error_to_mcp("subc route tool call failed", &frame.body));
                        }
                        ty => {
                            return Err(ErrorData::internal_error(
                                format!(
                                    "unexpected route frame {ty:?} on handle ({}, {}) corr {}",
                                    frame.header.channel, frame.header.epoch, frame.header.corr
                                ),
                                None,
                            ));
                        }
                    }
                }
            }
        }
    }

    async fn send_route_cancel(&self, route: RouteHandle, corr: u64) -> Result<()> {
        self.subc.validate_handle(route)?;
        self.subc
            .relay()
            .cancel_route_prompts(route, "enclosing route request was cancelled")
            .await;
        let frame = self.subc.build_route_frame(
            FrameType::Cancel,
            data_flags(),
            route,
            corr,
            Vec::new(),
        )?;
        self.subc.send_route_frame(route, frame).await
    }
}

fn parse_tool_arguments<T: DeserializeOwned>(
    tool_name: &str,
    arguments: Option<JsonObject>,
) -> std::result::Result<T, ErrorData> {
    serde_json::from_value(serde_json::Value::Object(arguments.unwrap_or_default())).map_err(
        |source| {
            ErrorData::invalid_params(
                format!("invalid arguments for '{tool_name}': {source}"),
                None,
            )
        },
    )
}

fn unknown_tool_error(name: &str) -> ErrorData {
    ErrorData::invalid_params(format!("unknown tool '{name}'"), None)
}

fn lexical_search_rank(tool: &ExposedTool, query: &str) -> Option<usize> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() {
        return Some(0);
    }

    let name = tool.manifest.name.to_ascii_lowercase();
    let description = tool.description.to_ascii_lowercase();
    if name == query {
        Some(0)
    } else if name.contains(&query) {
        Some(1)
    } else if description.contains(&query) {
        Some(2)
    } else {
        let tokens = query
            .split_whitespace()
            .filter(|token| !token.is_empty())
            .collect::<Vec<_>>();
        (!tokens.is_empty()
            && tokens
                .iter()
                .all(|token| name.contains(token) || description.contains(token)))
        .then_some(3)
    }
}

fn ack_only_tool_result() -> std::result::Result<CallToolResult, ErrorData> {
    serde_json::from_value(serde_json::json!({
        "content": [{ "type": "text", "text": ACK_ONLY_TOOL_RESPONSE_TEXT }],
        "isError": false,
    }))
    .map_err(mcp_internal_error)
}

fn json_tool_result(value: serde_json::Value) -> std::result::Result<CallToolResult, ErrorData> {
    serde_json::from_value(serde_json::json!({
        "content": [{ "type": "text", "text": value.to_string() }],
        "isError": false,
    }))
    .map_err(mcp_internal_error)
}

async fn serve_mcp_server<R, W>(
    handler: SubcMcpServer,
    transport: AsyncRwTransport<RoleServer, R, W>,
) -> Result<()>
where
    R: AsyncRead + Send + Unpin + 'static,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let running = rmcp::serve_server(handler, transport)
        .await
        .map_err(|source| other_error(format!("failed to initialize rmcp server: {source}")))?;
    running
        .waiting()
        .await
        .map(|_reason| ())
        .map_err(|source| other_error(format!("rmcp server task failed: {source}")))
}

fn mcp_tool_from_exposed(tool: &ExposedTool) -> McpTool {
    McpTool::new(
        tool.manifest.name.clone(),
        tool.description.clone(),
        Arc::new(schema_value_to_object(&tool.manifest.schema)),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(tool.manifest.execution_mode == ExecutionMode::Pure)
            .destructive(tool.manifest.execution_mode != ExecutionMode::Pure),
    )
}

fn schema_value_to_object(value: &serde_json::Value) -> JsonObject {
    match value {
        serde_json::Value::Object(object) => object.clone(),
        other => {
            let mut object = JsonObject::new();
            object.insert(
                "type".to_owned(),
                serde_json::Value::String("object".to_owned()),
            );
            object.insert("x-subc-schema".to_owned(), other.clone());
            object
        }
    }
}

async fn forward_progress(
    context: &RequestContext<RoleServer>,
    progress_token: Option<ProgressToken>,
    body: &[u8],
) -> std::result::Result<(), ErrorData> {
    let Some(progress_token) = progress_token else {
        return Ok(());
    };
    let progress = serde_json::from_slice::<RouteToolProgress>(body).map_err(|source| {
        ErrorData::internal_error(
            format!("provider returned malformed progress: {source}"),
            None,
        )
    })?;
    let mut notification = ProgressNotificationParam::new(progress_token, progress.progress);
    if let Some(total) = progress.total {
        notification = notification.with_total(total);
    }
    if let Some(message) = progress.message {
        notification = notification.with_message(message);
    }
    context
        .peer
        .notify_progress(notification)
        .await
        .map_err(mcp_internal_error)
}

fn subc_error_to_mcp(prefix: &str, body: &[u8]) -> ErrorData {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(error) => ErrorData::internal_error(
            format!("{prefix}: {}: {}", error.code, error.message),
            Some(serde_json::json!({ "subc_code": error.code })),
        ),
        Err(source) => ErrorData::internal_error(
            format!(
                "{prefix}: invalid error body ({} bytes): {source}",
                body.len()
            ),
            None,
        ),
    }
}

fn mcp_internal_error(error: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error(error.to_string(), None)
}

async fn send_route_goodbye(subc: &SubcClient, route: RouteHandle) -> Result<()> {
    let frame = subc.build_route_frame(FrameType::Goodbye, data_flags(), route, 0, Vec::new())?;
    subc.send_route_frame(route, frame).await
}

async fn send_route_goodbyes(subc: &SubcClient, routes: Vec<RouteHandle>) -> Result<()> {
    let mut errors = Vec::new();
    for route in routes {
        if let Err(error) = send_route_goodbye(subc, route).await {
            errors.push(format!(
                "handle ({}, {}): {error}",
                route.channel, route.epoch
            ));
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(other_error(format!(
            "failed to send route GOODBYE for {} route(s): {}",
            errors.len(),
            errors.join("; ")
        )))
    }
}

async fn subc_reader_loop<R>(
    mut read_half: R,
    pending: Arc<Mutex<HashMap<PendingKey, PendingRequest>>>,
    events: broadcast::Sender<SubcEvent>,
    relay: Arc<ReverseRelay>,
) where
    R: AsyncRead + Unpin,
{
    loop {
        match read_frame(&mut read_half).await {
            Ok(Some(frame)) => {
                if frame.header.channel != 0
                    && !relay
                        .validate_ingress(frame.header.channel, frame.header.epoch)
                        .await
                {
                    continue;
                }

                if frame.header.ty == FrameType::Push && frame.header.channel == 0 {
                    eprintln!(
                        "subc-mcp module: ignoring unrecognized channel-0 Push corr={}",
                        frame.header.corr
                    );
                    continue;
                }

                if frame.header.ty == FrameType::Request && frame.header.channel != 0 {
                    relay.handle_reverse_request(frame).await;
                    continue;
                }

                let route = relay.route_handle(frame.header.channel, frame.header.epoch);
                if frame.header.ty == FrameType::Goodbye && frame.header.channel != 0 {
                    relay.drop_route(route).await;
                    fail_pending_on_route(&pending, route, "subc route closed by provider GOODBYE")
                        .await;
                    let _ = events.send(SubcEvent::RouteGoodbye { handle: route });
                    continue;
                }

                if frame.header.ty == FrameType::Cancel && frame.header.channel != 0 {
                    relay
                        .cancel_route_prompts(
                            route,
                            "provider cancelled the enclosing route request",
                        )
                        .await;
                }

                let key = (frame.header.channel, frame.header.epoch, frame.header.corr);
                let terminal = is_terminal_frame_type(frame.header.ty);
                let reply = if terminal {
                    pending.lock().await.remove(&key)
                } else {
                    pending.lock().await.get(&key).cloned()
                };
                if let Some(reply) = reply {
                    let mut opened = None;
                    if terminal {
                        if let Some(route_session) = reply.route_session.as_ref() {
                            opened = match serde_json::from_slice::<ClientControlResponse>(
                                &frame.body,
                            ) {
                                Ok(ClientControlResponse::RouteOpen {
                                    route_channel,
                                    route_epoch,
                                }) if frame.header.ty == FrameType::Response => {
                                    Some(relay.route_handle(route_channel, route_epoch))
                                }
                                _ => None,
                            };
                            if let Some(handle) = opened {
                                if let Err(error) =
                                    relay.install_route(handle, Arc::clone(route_session)).await
                                {
                                    eprintln!(
                                        "subc-mcp module: rejected invalid route.open handle: {error}"
                                    );
                                    continue;
                                }
                            }
                        }
                    }
                    if reply.reply.send(frame).await.is_err() {
                        if let Some(handle) = opened {
                            relay.drop_route(handle).await;
                            relay
                                .send_reverse_frame(FrameType::Goodbye, handle, 0, Vec::new())
                                .await;
                        } else if !terminal {
                            pending.lock().await.remove(&key);
                        }
                    }
                } else {
                    eprintln!(
                        "subc-mcp module: dropping unsolicited subc frame type={:?} handle=({}, {}) corr={}",
                        frame.header.ty,
                        frame.header.channel,
                        frame.header.epoch,
                        frame.header.corr
                    );
                }
            }
            Ok(None) => {
                eprintln!("subc-mcp module: subc connection closed");
                break;
            }
            Err(error) => {
                eprintln!("subc-mcp module: subc read failed: {error}");
                break;
            }
        }
    }

    pending.lock().await.clear();
    relay.clear_all().await;
}

async fn fail_pending_on_route(
    pending: &Arc<Mutex<HashMap<PendingKey, PendingRequest>>>,
    route: RouteHandle,
    message: &str,
) {
    let replies = {
        let mut pending = pending.lock().await;
        let keys = pending
            .keys()
            .filter_map(|(channel, epoch, corr)| {
                (*channel == route.channel && *epoch == route.epoch)
                    .then_some((*channel, *epoch, *corr))
            })
            .collect::<Vec<_>>();
        keys.into_iter()
            .filter_map(|key| pending.remove(&key).map(|reply| (key, reply)))
            .collect::<Vec<_>>()
    };

    for ((channel, epoch, corr), reply) in replies {
        let body = match serde_json::to_vec(&ErrorBody {
            code: "target_unavailable".to_owned(),
            message: message.to_owned(),
        }) {
            Ok(body) => body,
            Err(error) => {
                eprintln!("subc-mcp module: failed to encode route GOODBYE error: {error}");
                Vec::new()
            }
        };
        let frame = match build_frame(FrameType::Error, data_flags(), channel, epoch, corr, body) {
            Ok(frame) => frame,
            Err(error) => {
                eprintln!("subc-mcp module: failed to build route GOODBYE error: {error}");
                continue;
            }
        };
        let _ = reply.reply.send(frame).await;
    }
}

fn is_terminal_frame_type(frame_type: FrameType) -> bool {
    matches!(
        frame_type,
        FrameType::Response | FrameType::Error | FrameType::StreamEnd | FrameType::Cancel
    )
}

async fn subc_writer_loop(
    mut write_half: OwnedWriteHalf,
    mut rx: mpsc::Receiver<SubcFrame>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut writer = BufWriter::new(&mut write_half);
    loop {
        let frame = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return;
                }
                continue;
            }
            frame = rx.recv() => {
                let Some(frame) = frame else {
                    return;
                };
                frame
            }
        };
        if let Err(error) = write_frame(&mut writer, &frame).await {
            eprintln!("subc-mcp module: subc write failed: {error}");
            return;
        }
        while let Ok(frame) = rx.try_recv() {
            if *shutdown.borrow() {
                return;
            }
            if let Err(error) = write_frame(&mut writer, &frame).await {
                eprintln!("subc-mcp module: subc write failed: {error}");
                return;
            }
        }
        if let Err(error) = writer.flush().await {
            eprintln!("subc-mcp module: subc flush failed: {error}");
            return;
        }
    }
}

async fn pipe_stdio(stream: TcpStream) -> Result<()> {
    let (mut socket_read, mut socket_write) = stream.into_split();
    let mut stdin = tokio_io::stdin();
    let mut stdout = tokio_io::stdout();

    let stdin_to_socket = async {
        let copied = tokio_io::copy(&mut stdin, &mut socket_write).await?;
        socket_write.shutdown().await?;
        stdio::Result::Ok(copied)
    };
    let socket_to_stdout = async {
        let copied = tokio_io::copy(&mut socket_read, &mut stdout).await?;
        stdout.flush().await?;
        stdio::Result::Ok(copied)
    };
    tokio::pin!(stdin_to_socket);
    tokio::pin!(socket_to_stdout);

    tokio::select! {
        result = &mut socket_to_stdout => {
            result?;
        }
        result = &mut stdin_to_socket => {
            result?;
            socket_to_stdout.await?;
        }
    }

    Ok(())
}

async fn connect_authenticated(connection_file_path: &Path) -> Result<TcpStream> {
    let conn = connection_file::read_for_client(connection_file_path).map_err(|source| {
        other_error(format!(
            "failed to read connection file {}: {source}",
            connection_file_path.display()
        ))
    })?;
    let endpoint = conn.endpoints.first().ok_or_else(|| {
        other_error(format!(
            "connection file {} has no endpoints",
            connection_file_path.display()
        ))
    })?;
    let endpoint_label = format!("{}:{}", endpoint.host, endpoint.port);
    let ip: IpAddr = endpoint.host.parse().map_err(|source| {
        other_error(format!(
            "connection file {} endpoint {endpoint_label} is not an IP address: {source}",
            connection_file_path.display()
        ))
    })?;
    if !ip.is_loopback() {
        return Err(other_error(format!(
            "connection file {} endpoint {endpoint_label} is not a loopback address",
            connection_file_path.display()
        )));
    }
    let mut stream = TcpStream::connect(SocketAddr::new(ip, endpoint.port))
        .await
        .map_err(|source| {
            other_error(format!(
                "failed to connect to {} from {}: {source}",
                endpoint_label,
                connection_file_path.display()
            ))
        })?;
    authenticate_client(&mut stream, &conn, AUTH_DEADLINE)
        .await
        .map_err(|source| {
            other_error(format!(
                "failed to authenticate to {} from {}: {source}",
                endpoint_label,
                connection_file_path.display()
            ))
        })?;
    Ok(stream)
}

fn resolve_project_root() -> Result<PathBuf> {
    let mut attempted = Vec::new();
    for candidate in project_root_candidates() {
        attempted.push(candidate.display().to_string());
        if candidate.is_dir() {
            return fs::canonicalize(&candidate).map_err(|source| {
                other_error(format!(
                    "failed to canonicalize project root {}: {source}",
                    candidate.display()
                ))
            });
        }
    }

    Err(other_error(format!(
        "failed to resolve project root from CLAUDE_PROJECT_DIR, WORKSPACE_FOLDER_PATHS, or current directory; no candidate is an existing directory (attempted: {})",
        if attempted.is_empty() {
            "<none>".to_owned()
        } else {
            attempted.join(", ")
        }
    )))
}

fn project_root_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(path) = non_empty_os_var("CLAUDE_PROJECT_DIR") {
        candidates.push(PathBuf::from(path));
    }
    if let Some(paths) = non_empty_os_var("WORKSPACE_FOLDER_PATHS") {
        if let Some(path) = env::split_paths(&paths).next() {
            candidates.push(path);
        }
    }
    if let Ok(path) = env::current_dir() {
        candidates.push(path);
    }
    candidates
}

fn default_module_connection_file_path() -> PathBuf {
    if let Some(runtime_dir) = non_empty_os_var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir).join(MODULE_CONNECTION_FILE_NAME);
    }

    env::temp_dir().join(format!(
        "subc-mcp-{}.connection.json",
        user_connection_token()
    ))
}

fn user_connection_token() -> String {
    for key in ["USER", "USERNAME", "HOME", "USERPROFILE"] {
        if let Some(value) = non_empty_os_var(key) {
            return sanitize_token(&value.to_string_lossy());
        }
    }
    "unknown".to_owned()
}

fn non_empty_os_var(key: &str) -> Option<OsString> {
    let value = env::var_os(key)?;
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn sanitize_token(raw: &str) -> String {
    let mut token = String::new();
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_') {
            token.push(ch);
        } else {
            token.push('_');
        }
    }
    if token.is_empty() {
        "unknown".to_owned()
    } else {
        token
    }
}

fn generated_id(prefix: &str) -> Result<String> {
    Ok(format!("{prefix}-{}", hex(&generate_daemon_id()?)))
}

fn generated_session_id(shim_session_id: &str) -> Result<String> {
    Ok(format!(
        "session-{}-{}",
        sanitize_token(shim_session_id),
        hex(&generate_daemon_id()?)
    ))
}

/// Environment variable a launch wrapper sets to name the conversation this
/// process belongs to (mirrored on the provider wire as an x-ck-instance
/// header by the same wrapper).
const INSTANCE_TOKEN_ENV: &str = "CK_INSTANCE_TOKEN";

/// The bind session for a shim attach. A wrapper-minted instance token becomes
/// the session VERBATIM so conversation-scoped modules (magic-context via
/// ai-proxy's session.resolve) can correlate this MCP session with the same
/// launch's provider-wire traffic. The module re-validates rather than
/// trusting the shim: the shim is an unauthenticated-content byte pipe from
/// the module's perspective, and an oversized or charset-hostile value must
/// not become a bind identity. Invalid or absent → the synthetic per-process
/// session id (today's behavior).
fn bind_session_from_hello(hello: &ShimHello) -> Result<String> {
    match hello.instance_token.as_deref() {
        Some(token) if valid_instance_token(token) => Ok(token.to_string()),
        Some(_) => {
            eprintln!(
                "subc-mcp module: ignoring invalid instance token from shim hello; using synthetic session id"
            );
            generated_session_id(&hello.shim_session_id)
        }
        None => generated_session_id(&hello.shim_session_id),
    }
}

/// Shared validity rule for instance tokens, matching ai-proxy's validation of
/// the x-ck-instance header form so both consumers accept the same tokens:
/// non-empty, at most [`MAX_INSTANCE_TOKEN_LEN`] bytes, charset [A-Za-z0-9._-].
/// The token is otherwise OPAQUE — never parsed for shape.
fn valid_instance_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_INSTANCE_TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// Maximum accepted instance-token length in bytes, matching ai-proxy's
/// validation of the header form so both consumers accept the same tokens.
const MAX_INSTANCE_TOKEN_LEN: usize = 128;

/// Read the wrapper-minted conversation token from the environment. Invalid
/// values are dropped with a warning rather than failing the shim: a bad token
/// only costs conversation correlation, not tool access.
fn instance_token_from_env() -> Option<String> {
    let raw = env::var(INSTANCE_TOKEN_ENV).ok()?;
    if valid_instance_token(&raw) {
        Some(raw)
    } else {
        eprintln!(
            "subc-mcp shim: ignoring {INSTANCE_TOKEN_ENV}: must be non-empty, at most {MAX_INSTANCE_TOKEN_LEN} bytes, charset [A-Za-z0-9._-]"
        );
        None
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

fn allocate_corr(last_corr: &AtomicU64) -> Option<u64> {
    last_corr
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            last.checked_add(1)
        })
        .ok()
        .map(|last| last + 1)
}

fn build_frame(
    ty: FrameType,
    flags: Flags,
    channel: u16,
    epoch: u32,
    corr: u64,
    body: Vec<u8>,
) -> Result<SubcFrame> {
    if ty.is_pure_header() && !body.is_empty() {
        return Err(other_error(format!(
            "pure-header frame {ty:?} cannot carry {} body bytes",
            body.len()
        )));
    }
    if body.len() > MAX_FRAME_BODY_LEN as usize {
        return Err(other_error(format!(
            "frame body too large: {} bytes (max {MAX_FRAME_BODY_LEN})",
            body.len()
        )));
    }
    SubcFrame::build(ty, flags, channel, epoch, corr, body)
        .map_err(|source| other_error(format!("failed to build frame: {source}")))
}

fn control_flags() -> Flags {
    Flags::new(false, Priority::Passive, false)
}

fn data_flags() -> Flags {
    Flags::new(false, Priority::Interactive, false)
}

async fn read_json_message<R, T>(reader: &mut R, max_len: u32) -> Result<T>
where
    R: AsyncRead + Unpin,
    T: DeserializeOwned,
{
    let Some(bytes) = read_len_prefixed_bytes(reader, max_len).await? else {
        return Err(other_error("connection closed before JSON message"));
    };
    serde_json::from_slice(&bytes)
        .map_err(|source| other_error(format!("invalid JSON message: {source}")))
}

async fn write_json_message<W, T>(writer: &mut W, value: &T, max_len: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value)?;
    write_len_prefixed_bytes(writer, &bytes, max_len).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_len_prefixed_bytes<R>(reader: &mut R, max_len: u32) -> Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let mut len_bytes = [0u8; 4];
    if !read_exact_or_clean_eof(reader, &mut len_bytes).await? {
        return Ok(None);
    }
    let len = u32::from_le_bytes(len_bytes);
    if len > max_len {
        return Err(other_error(format!(
            "length-prefixed message too large: {len} bytes (max {max_len})"
        )));
    }

    let mut bytes = vec![0u8; len as usize];
    if !bytes.is_empty() {
        read_exact_or_unexpected_eof(reader, &mut bytes).await?;
    }
    Ok(Some(bytes))
}

async fn write_len_prefixed_bytes<W>(writer: &mut W, bytes: &[u8], max_len: u32) -> Result<()>
where
    W: AsyncWrite + Unpin,
{
    let len = u32::try_from(bytes.len())
        .map_err(|_| other_error(format!("message too large: {} bytes", bytes.len())))?;
    if len > max_len {
        return Err(other_error(format!(
            "length-prefixed message too large: {len} bytes (max {max_len})"
        )));
    }

    writer.write_all(&len.to_le_bytes()).await?;
    if !bytes.is_empty() {
        writer.write_all(bytes).await?;
    }
    Ok(())
}

async fn read_exact_or_clean_eof<R>(reader: &mut R, buf: &mut [u8]) -> stdio::Result<bool>
where
    R: AsyncRead + Unpin,
{
    let mut actual = 0;
    while actual < buf.len() {
        let read = reader.read(&mut buf[actual..]).await?;
        if read == 0 {
            if actual == 0 {
                return Ok(false);
            }
            return Err(unexpected_eof(buf.len(), actual));
        }
        actual += read;
    }
    Ok(true)
}

async fn read_exact_or_unexpected_eof<R>(reader: &mut R, buf: &mut [u8]) -> stdio::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut actual = 0;
    while actual < buf.len() {
        let read = reader.read(&mut buf[actual..]).await?;
        if read == 0 {
            return Err(unexpected_eof(buf.len(), actual));
        }
        actual += read;
    }
    Ok(())
}

fn unexpected_eof(expected: usize, actual: usize) -> stdio::Error {
    stdio::Error::new(
        stdio::ErrorKind::UnexpectedEof,
        format!("expected {expected} bytes, read {actual} before EOF"),
    )
}

fn error_response(prefix: &str, body: &[u8]) -> BoxError {
    match serde_json::from_slice::<ErrorBody>(body) {
        Ok(error) => other_error(format!("{prefix}: {}: {}", error.code, error.message)),
        Err(source) => other_error(format!(
            "{prefix}: invalid error body ({} bytes): {source}",
            body.len()
        )),
    }
}

fn invalid_input(message: impl Into<String>) -> BoxError {
    stdio::Error::new(stdio::ErrorKind::InvalidInput, message.into()).into()
}

fn other_error(message: impl Into<String>) -> BoxError {
    stdio::Error::other(message.into()).into()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_test_config(doc: &str) -> RawGatewayConfig {
        parse_gateway_config_doc(doc, Path::new("test-mcp.jsonc")).unwrap()
    }

    fn compose_test_config(
        user_doc: Option<&str>,
        project_doc: Option<&str>,
        harness_name: &str,
    ) -> GatewayConfig {
        let mut effective = GatewayConfig::facade_default();
        if let Some(user_doc) = user_doc {
            let (top, harness) = parse_test_config(user_doc).into_parts();
            merge_gateway_config(&mut effective, top);
            for layer in matching_harness_layers(harness, harness_name) {
                merge_gateway_config(&mut effective, layer);
            }
        }
        if let Some(project_doc) = project_doc {
            let (top, harness) = parse_test_config(project_doc).into_parts();
            merge_project_gateway_config(&mut effective, top);
            for layer in matching_harness_layers(harness, harness_name) {
                merge_project_gateway_config(&mut effective, layer);
            }
        }
        effective
    }

    fn shim_args(harness: &str) -> ShimArgs {
        parse_shim_args(vec![OsString::from("--harness"), OsString::from(harness)]).unwrap()
    }

    async fn connected_tcp_stream_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr);
        let server = listener.accept();
        let (client, server) = tokio::join!(client, server);
        let (server, _) = server.unwrap();
        (client.unwrap(), server)
    }

    async fn assert_open_provider_route_consumer_capabilities(
        declared: Option<Vec<String>>,
        expected: Option<Vec<String>>,
    ) {
        let (client_stream, mut server_stream) = connected_tcp_stream_pair().await;
        let subc = SubcClient::start(client_stream);
        let identity = BindIdentity {
            project_root: PathBuf::from("/tmp/subc-mcp-route-open"),
            harness: DEFAULT_HARNESS.to_string(),
            session: "shim-session".to_string(),
        };
        let expected_for_server = expected.clone();
        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let frame = read_frame(&mut server_stream).await.unwrap().unwrap();
            let request: ClientControlRequest = serde_json::from_slice(&frame.body).unwrap();
            let ClientControlRequest::RouteOpen {
                target,
                consumer_capabilities,
                ..
            } = request
            else {
                panic!("unexpected control request: {request:?}");
            };
            assert_eq!(
                target,
                RouteTarget::ToolProvider {
                    module_id: "aft".to_string(),
                }
            );
            assert_eq!(consumer_capabilities, expected_for_server);

            let body = serde_json::to_vec(&ClientControlResponse::RouteOpen {
                route_channel: 7,
                route_epoch: 11,
            })
            .unwrap();
            let response = build_frame(
                FrameType::Response,
                control_flags(),
                0,
                0,
                frame.header.corr,
                body,
            )
            .unwrap();
            write_frame(&mut server_stream, &response).await.unwrap();
            server_stream.flush().await.unwrap();
            let _ = close_rx.await;
        });

        let route_session = Arc::new(RelaySession::new("shim-session".to_owned()));
        let route = open_provider_route(&subc, "aft", &identity, declared, route_session)
            .await
            .unwrap();
        assert_eq!((route.channel, route.epoch), (7, 11));
        assert!(
            subc.relay().route_session(route).await.is_some(),
            "route.open must install the handle before resolving the caller"
        );
        let _ = close_tx.send(());
        server.await.unwrap();
    }

    #[test]
    fn shim_harness_bare_token_gets_mcp_prefix() {
        assert_eq!(shim_args("claude-code").harness, "mcp:claude-code");
    }

    #[test]
    fn shim_harness_prefixed_and_reserved_pass_through() {
        assert_eq!(shim_args("mcp:codex").harness, "mcp:codex");
        assert_eq!(shim_args("custom:thing").harness, "custom:thing");
        assert_eq!(shim_args("opencode").harness, "opencode");
        assert_eq!(shim_args("pi").harness, "pi");
        assert_eq!(shim_args("runner").harness, "runner");
    }

    #[test]
    fn shim_harness_default_unchanged_when_flag_absent() {
        let args = parse_shim_args(Vec::<OsString>::new()).unwrap();
        assert_eq!(args.harness, DEFAULT_HARNESS);
    }
    #[test]
    fn schema_accepts_legacy_bool_and_object_overrides() {
        let config = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": {
                        "defaultEnabled": false,
                        "overrides": {
                          "read": true,
                          "write": { "enabled": false, "description": "curated write", "mode": "ack_only" },
                          "drop_me": null
                        }
                      }
                    }
                  }
                }
                "#,
            ),
            None,
            DEFAULT_HARNESS,
        );

        let tools = &config.providers["aft"].tools;
        assert_eq!(tools.default_enabled, Some(false));
        assert_eq!(tools.overrides["read"].enabled, Some(true));
        assert_eq!(tools.overrides["write"].enabled, Some(false));
        assert_eq!(tools.overrides["write"].mode, Some(ToolMode::AckOnly));
        assert_eq!(
            tools.overrides["write"].description.as_deref(),
            Some("curated write")
        );
        assert!(!tools.overrides.contains_key("drop_me"));
    }

    #[test]
    fn schema_rejects_reserved_refresh_values_unknown_fields_and_object_nulls() {
        let reserved = parse_gateway_config_doc(
            r#"{ "version": 1, "refresh": "on-hard" }"#,
            Path::new("reserved.jsonc"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            reserved.contains("requires a bust-signal source; not available on the MCP path"),
            "unexpected reserved-refresh error: {reserved}"
        );

        let unknown_field_docs = [
            r#"{ "version": 1, "unknown": true }"#,
            r#"{ "version": 1, "harness": { "mcp:generic": { "unknown": true } } }"#,
            r#"{ "version": 1, "providers": { "aft": { "unknown": true } } }"#,
            r#"{ "version": 1, "providers": { "aft": { "tools": { "unknown": true } } } }"#,
            r#"{ "version": 1, "providers": { "aft": { "tools": { "overrides": { "read": { "unknown": true } } } } } }"#,
        ];
        for doc in unknown_field_docs {
            let err = parse_gateway_config_doc(doc, Path::new("unknown.jsonc"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("unknown field"), "unexpected error: {err}");
        }

        let null_inside_object = parse_gateway_config_doc(
            r#"
            {
              "version": 1,
              "providers": {
                "aft": {
                  "tools": {
                    "overrides": { "read": { "enabled": null } }
                  }
                }
              }
            }
            "#,
            Path::new("null-object.jsonc"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            null_inside_object.contains("enabled must be omitted instead of null"),
            "unexpected object-null error: {null_inside_object}"
        );

        let mode_null = parse_gateway_config_doc(
            r#"
            {
              "version": 1,
              "providers": {
                "aft": {
                  "tools": {
                    "overrides": { "read": { "mode": null } }
                  }
                }
              }
            }
            "#,
            Path::new("mode-null-object.jsonc"),
        )
        .unwrap_err()
        .to_string();
        assert!(
            mode_null.contains("mode must be omitted instead of null"),
            "unexpected mode-null error: {mode_null}"
        );
    }

    #[test]
    fn global_harness_sections_compose_and_unknown_harness_is_ignored() {
        let config = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "surfaceMode": "search",
                  "refresh": "immediate",
                  "providers": {
                    "aft": {
                      "namespace": "global",
                      "tools": {
                        "defaultEnabled": false,
                        "overrides": {
                          "read": false,
                          "write": { "enabled": true, "description": "global write" }
                        }
                      }
                    }
                  },
                  "harness": {
                    "MCP:GENERIC": {
                      "surfaceMode": "full",
                      "refresh": null,
                      "providers": {
                        "aft": {
                          "namespace": "harnessed",
                          "tools": {
                            "defaultEnabled": null,
                            "overrides": {
                              "read": null,
                              "write": { "description": "harness write" }
                            }
                          }
                        }
                      }
                    },
                    "other": {
                      "providers": { "aft": { "enabled": false } }
                    }
                  }
                }
                "#,
            ),
            None,
            "mcp:generic",
        );

        assert_eq!(config.surface_mode, SurfaceMode::Full);
        assert_eq!(config.refresh, RefreshMode::OnAttach);
        assert_eq!(config.provider_namespace("aft"), "harnessed");
        let tools = &config.providers["aft"].tools;
        assert_eq!(tools.default_enabled, None);
        assert!(!tools.overrides.contains_key("read"));
        assert_eq!(tools.overrides["write"].enabled, Some(true));
        assert_eq!(
            tools.overrides["write"].description.as_deref(),
            Some("harness write")
        );
        assert!(config.provider_enabled("aft"));
    }

    #[test]
    fn project_tier_only_narrows_and_preserves_global_model_facing_strings() {
        let config = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "refresh": "immediate",
                  "providers": {
                    "aft": {
                      "enabled": false,
                      "namespace": "global",
                      "tools": {
                        "defaultEnabled": false,
                        "overrides": {
                          "read": false,
                          "write": { "enabled": true, "description": "global write" }
                        }
                      }
                    }
                  }
                }
                "#,
            ),
            Some(
                r#"
                {
                  "version": 1,
                  "surfaceMode": "search",
                  "refresh": "on-attach",
                  "providers": {
                    "aft": {
                      "enabled": true,
                      "namespace": "project",
                      "tools": {
                        "defaultEnabled": true,
                        "overrides": {
                          "read": null,
                          "grant": true,
                          "write": { "enabled": false, "description": "project write" }
                        }
                      }
                    }
                  }
                }
                "#,
            ),
            DEFAULT_HARNESS,
        );

        assert_eq!(config.surface_mode, SurfaceMode::Search);
        assert_eq!(config.refresh, RefreshMode::Immediate);
        assert!(!config.provider_enabled("aft"));
        assert_eq!(config.provider_namespace("aft"), "global");
        let tools = &config.providers["aft"].tools;
        assert_eq!(tools.default_enabled, Some(false));
        assert_eq!(tools.overrides["read"].enabled, Some(false));
        assert!(!tools.overrides.contains_key("grant"));
        assert_eq!(tools.overrides["write"].enabled, Some(false));
        assert_eq!(
            tools.overrides["write"].description.as_deref(),
            Some("global write")
        );
    }

    #[test]
    fn project_tier_can_narrow_tool_mode_but_cannot_widen_ack_only() {
        let project_narrowed = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "forward" } } }
                    }
                  }
                }
                "#,
            ),
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "ack_only" } } }
                    }
                  }
                }
                "#,
            ),
            DEFAULT_HARNESS,
        );
        assert_eq!(project_narrowed.tool_mode("aft", "read"), ToolMode::AckOnly);

        let attempted_widen = compose_test_config(
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "ack_only" } } }
                    }
                  }
                }
                "#,
            ),
            Some(
                r#"
                {
                  "version": 1,
                  "providers": {
                    "aft": {
                      "tools": { "overrides": { "read": { "mode": "forward" } } }
                    }
                  }
                }
                "#,
            ),
            DEFAULT_HARNESS,
        );
        assert_eq!(attempted_widen.tool_mode("aft", "read"), ToolMode::AckOnly);
    }

    #[test]
    fn facade_default_disabled_modules_require_global_enable() {
        let baseline = GatewayConfig::facade_default();
        assert!(!baseline.provider_enabled("magic-context"));
        assert!(!baseline.provider_enabled("llm-runner"));

        let project_only = compose_test_config(
            None,
            Some(r#"{ "version": 1, "providers": { "magic-context": { "enabled": true } } }"#),
            DEFAULT_HARNESS,
        );
        assert!(!project_only.provider_enabled("magic-context"));

        let global_enabled = compose_test_config(
            Some(r#"{ "version": 1, "providers": { "magic-context": { "enabled": true } } }"#),
            None,
            DEFAULT_HARNESS,
        );
        assert!(global_enabled.provider_enabled("magic-context"));
    }

    #[tokio::test]
    async fn open_provider_route_stamps_declared_elicitation_capability() {
        assert_open_provider_route_consumer_capabilities(
            ReverseCapabilities {
                elicitation: true,
                ..ReverseCapabilities::default()
            }
            .declared_consumer_capabilities(),
            Some(vec!["elicitation".to_string()]),
        )
        .await;
    }

    #[tokio::test]
    async fn open_provider_route_omits_consumer_capabilities_when_none_are_declared() {
        assert_open_provider_route_consumer_capabilities(
            ReverseCapabilities::default().declared_consumer_capabilities(),
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn health_metrics_report_ack_only_acks_by_tool() {
        let (tx, _rx) = mpsc::channel(1);
        let relay = ReverseRelay::new(tx, 91);
        relay
            .ack_only_counter("fake-aft_fake_read")
            .fetch_add(2, Ordering::Relaxed);

        let metrics = relay.health_metrics().await;
        assert_eq!(
            metrics["ack_only_acks"],
            serde_json::json!({ "fake-aft_fake_read": 2 })
        );
    }

    #[test]
    fn correlation_allocator_is_monotonic_and_never_reuses_after_exhaustion() {
        let corr = AtomicU64::new(0);
        assert_eq!(allocate_corr(&corr), Some(1));
        assert_eq!(allocate_corr(&corr), Some(2));

        corr.store(u64::MAX - 1, Ordering::Relaxed);
        assert_eq!(allocate_corr(&corr), Some(u64::MAX));
        assert_eq!(allocate_corr(&corr), None);
        assert_eq!(allocate_corr(&corr), None);
        assert_eq!(corr.load(Ordering::Relaxed), u64::MAX);
    }

    #[tokio::test]
    async fn reader_drops_stale_epoch_before_dispatch_or_lifecycle_callbacks() {
        let (mut server, client) = tokio::io::duplex(4096);
        let pending: Arc<Mutex<HashMap<PendingKey, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (reply_tx, mut reply_rx) = mpsc::channel(PENDING_FRAME_BUFFER);
        pending.lock().await.insert(
            (7, 2, 44),
            PendingRequest {
                reply: reply_tx,
                route_session: None,
            },
        );
        let (outbound_tx, _outbound_rx) = mpsc::channel(4);
        let relay = Arc::new(ReverseRelay::new(outbound_tx, 91));
        let current = relay.route_handle(7, 2);
        relay
            .install_route(current, Arc::new(RelaySession::new("e2".to_owned())))
            .await
            .unwrap();
        let (events, mut events_rx) = broadcast::channel(SUBC_EVENT_BUFFER);
        let reader = tokio::spawn(subc_reader_loop(
            client,
            Arc::clone(&pending),
            events,
            Arc::clone(&relay),
        ));

        let stale_response = build_frame(
            FrameType::Response,
            data_flags(),
            7,
            1,
            44,
            br#"{"stale":true}"#.to_vec(),
        )
        .unwrap();
        write_frame(&mut server, &stale_response).await.unwrap();
        let stale_goodbye =
            build_frame(FrameType::Goodbye, data_flags(), 7, 1, 0, Vec::new()).unwrap();
        write_frame(&mut server, &stale_goodbye).await.unwrap();
        server.flush().await.unwrap();
        time::sleep(Duration::from_millis(25)).await;

        assert!(matches!(
            reply_rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        assert!(pending.lock().await.contains_key(&(7, 2, 44)));
        assert!(matches!(
            events_rx.try_recv(),
            Err(broadcast::error::TryRecvError::Empty)
        ));
        assert!(relay.route_session(current).await.is_some());
        assert_eq!(relay.stale_epoch_drops.load(Ordering::Relaxed), 2);

        reader.abort();
        let _ = reader.await;
    }

    #[tokio::test]
    async fn delayed_reverse_reply_retains_its_ingress_epoch_after_slot_reuse() {
        let (tx, mut rx) = mpsc::channel(4);
        let relay = ReverseRelay::new(tx, 27);
        let e1 = relay.route_handle(9, 1);
        let e2 = relay.route_handle(9, 2);
        relay
            .install_route(e1, Arc::new(RelaySession::new("e1-session".to_owned())))
            .await
            .unwrap();
        relay.pending.lock().await.insert(
            (e1.channel, e1.epoch, 77),
            PendingRelayEntry::new("e1-session".to_owned()),
        );
        relay
            .install_route(e2, Arc::new(RelaySession::new("e2-session".to_owned())))
            .await
            .unwrap();

        relay
            .settle_host_answer(
                (e1.channel, e1.epoch, 77),
                Err(ServiceError::TransportClosed),
            )
            .await;

        let reply = rx.recv().await.unwrap();
        assert_eq!(
            (reply.header.channel, reply.header.epoch, reply.header.corr),
            (e1.channel, e1.epoch, 77),
            "a delayed E1 host answer must never be stamped as the live E2 route"
        );
        assert_eq!(relay.route_session(e2).await.unwrap().id(), "e2-session");
    }

    #[tokio::test]
    async fn reverse_reply_from_another_connection_token_emits_no_frame() {
        let (tx, mut rx) = mpsc::channel(1);
        let relay = ReverseRelay::new(tx, 2);
        relay
            .send_reverse_error(
                RouteHandle {
                    channel: 4,
                    epoch: 1,
                    connection_token: 1,
                },
                8,
                ErrorData::internal_error("stale", None),
            )
            .await;
        assert!(matches!(
            rx.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
    }

    #[tokio::test]
    async fn client_ignores_unknown_channel_zero_push() {
        let (mut server, client) = tokio::io::duplex(4096);
        let pending: Arc<Mutex<HashMap<PendingKey, PendingRequest>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (reply_tx, mut reply_rx) = mpsc::channel(PENDING_FRAME_BUFFER);
        pending.lock().await.insert(
            (0, 0, 42),
            PendingRequest {
                reply: reply_tx,
                route_session: None,
            },
        );

        let reader_pending = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            let (events, _events_rx) = broadcast::channel(SUBC_EVENT_BUFFER);
            let (tx, _rx) = mpsc::channel(1);
            let relay = Arc::new(ReverseRelay::new(tx, 1));
            subc_reader_loop(client, reader_pending, events, relay).await;
        });

        let push = build_frame(
            FrameType::Push,
            control_flags(),
            0,
            0,
            42,
            br#"{"op":"catalog.changed","generation":2}"#.to_vec(),
        )
        .unwrap();
        write_frame(&mut server, &push).await.unwrap();
        server.flush().await.unwrap();

        time::sleep(Duration::from_millis(25)).await;
        assert!(
            pending.lock().await.contains_key(&(0, 0, 42)),
            "unknown channel-0 Push must not satisfy a pending request"
        );

        let response = build_frame(
            FrameType::Response,
            control_flags(),
            0,
            0,
            42,
            br#"{"op":"route.open","route_channel":7,"route_epoch":1}"#.to_vec(),
        )
        .unwrap();
        write_frame(&mut server, &response).await.unwrap();
        server.flush().await.unwrap();

        let delivered = time::timeout(Duration::from_secs(1), reply_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivered.header.ty, FrameType::Response);
        assert_eq!(delivered.header.channel, 0);
        assert_eq!(delivered.header.corr, 42);

        drop(server);
        reader.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_non_loopback_connection_file_endpoint() {
        let daemon_id = generate_daemon_id().unwrap();
        let path = env::temp_dir().join(format!("subc-mcp-non-loopback-{}.json", hex(&daemon_id)));
        let info = ConnectionInfo {
            schema: SCHEMA_VERSION,
            wire_version: None,
            endpoints: vec![Endpoint {
                host: "0.0.0.0".to_owned(),
                port: 0,
            }],
            key: vec![0x42; subc_transport::KEY_LEN],
            daemon_id,
            pid: process::id(),
            daemon_ver: "test".to_owned(),
        };

        connection_file::write_atomic(&path, &info).unwrap();
        let err = connect_authenticated(&path).await.unwrap_err();
        assert!(
            err.to_string().contains("not a loopback address"),
            "unexpected error: {err}"
        );
        let _ = fs::remove_file(&path);
    }

    fn shim_hello_with_token(instance_token: Option<&str>) -> ShimHello {
        ShimHello {
            schema: SHIM_SCHEMA_VERSION,
            project_root: PathBuf::from("/tmp/instance-token-test"),
            harness: "mcp:claude-code".to_owned(),
            shim_session_id: "shim-abc123".to_owned(),
            instance_token: instance_token.map(str::to_owned),
        }
    }

    #[test]
    fn valid_instance_token_becomes_bind_session_verbatim() {
        let token = "53fa73e8-1c2d-4f00-9a51-instance.token_1";
        let session = bind_session_from_hello(&shim_hello_with_token(Some(token))).unwrap();
        assert_eq!(session, token);
    }

    #[test]
    fn absent_instance_token_falls_back_to_synthetic_session() {
        let session = bind_session_from_hello(&shim_hello_with_token(None)).unwrap();
        assert!(
            session.starts_with("session-shim-abc123-"),
            "expected synthetic id, got {session}"
        );
    }

    #[test]
    fn invalid_instance_tokens_fall_back_to_synthetic_session() {
        let oversized = "a".repeat(MAX_INSTANCE_TOKEN_LEN + 1);
        for bad in ["", "has space", "semi;colon", "uni\u{241f}code", &oversized] {
            let session = bind_session_from_hello(&shim_hello_with_token(Some(bad))).unwrap();
            assert!(
                session.starts_with("session-shim-abc123-"),
                "token {bad:?} must not become a bind session; got {session}"
            );
        }
    }

    #[test]
    fn shim_hello_without_instance_token_field_still_decodes() {
        // Old-shim compatibility: the field is serde-defaulted, so a hello
        // serialized before the field existed decodes with None.
        let old_wire = serde_json::json!({
            "schema": SHIM_SCHEMA_VERSION,
            "project_root": "/tmp/x",
            "harness": "mcp:claude-code",
            "shim_session_id": "shim-old"
        });
        let hello: ShimHello = serde_json::from_value(old_wire).unwrap();
        assert_eq!(hello.instance_token, None);
    }
}
