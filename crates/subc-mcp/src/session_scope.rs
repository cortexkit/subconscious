use super::*;
use subc_client_rs::{ModuleHandle, ScopeCallError};
use subc_protocol::{
    scope::{ScopeEnd, ScopeKind, ScopeRecord, ScopeRecordOutcome},
    Principal,
};
use tokio::sync::OnceCell;

#[derive(Debug)]
struct ScopeRefusal(String);

impl std::fmt::Display for ScopeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ScopeRefusal {}

fn scope_refusal(message: String) -> BoxError {
    Box::new(ScopeRefusal(message))
}

fn scope_call_error(error: ScopeCallError) -> BoxError {
    if matches!(error, ScopeCallError::Refused { .. }) {
        scope_refusal(error.to_string())
    } else {
        Box::new(error)
    }
}

pub(super) struct ScopeOwner {
    module_id: String,
    handle: ModuleHandle,
    state: Mutex<ScopeState>,
}

#[derive(Default)]
struct ScopeState {
    generation: u64,
    synced: bool,
}

impl ScopeState {
    fn advance(&mut self) -> Result<u64> {
        self.generation = self
            .generation
            .checked_add(1)
            .ok_or_else(|| other_error("scope generation exhausted"))?;
        Ok(self.generation)
    }
}

impl ScopeOwner {
    pub(super) fn new(module_id: String, handle: ModuleHandle) -> Self {
        Self {
            module_id,
            handle,
            state: Mutex::default(),
        }
    }

    async fn apply(&self, upsert: Vec<ScopeRecord>, end: Vec<ScopeEnd>) -> Result<()> {
        // Keep scope changes ordered; the SDK reads replies and daemon stop
        // independently, so an in-flight scope change cannot hold up shutdown.
        let mut state = self.state.lock().await;
        if !state.synced {
            let generation = state.advance()?;
            let reply = self
                .handle
                .scope_sync(generation, vec![])
                .await
                .map_err(scope_call_error)?;
            if reply.generation != generation {
                return Err(other_error("unexpected initial scope.sync generation"));
            }
            state.synced = true;
        }
        let generation = state.advance()?;
        let reply = self
            .handle
            .scope_apply(generation, upsert, end)
            .await
            .map_err(scope_call_error)?;
        if reply.generation != generation {
            return Err(other_error("unexpected scope.apply generation"));
        }
        if let Some(refusal) = reply
            .results
            .iter()
            .find(|r| r.outcome == ScopeRecordOutcome::Refused)
        {
            return Err(scope_refusal(format!(
                "scope record refused: {:?}: {:?}",
                refusal.code, refusal.message
            )));
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(super) struct LocalSessionScope {
    fallback: PathBuf,
    scope_ref: String,
    ready: OnceCell<()>,
    submitted: AtomicBool,
}

impl LocalSessionScope {
    pub(super) fn new(fallback: PathBuf) -> Result<Self> {
        Ok(Self {
            fallback,
            scope_ref: generated_id("mcp")?,
            ready: OnceCell::new(),
            submitted: AtomicBool::new(false),
        })
    }

    pub(super) async fn ensure(
        &self,
        subc: &SubcClient,
        state: &SessionState,
        session: &Arc<RelaySession>,
        peer: &Peer<RoleServer>,
        gate: &Mutex<()>,
    ) {
        // Remember the attribution decision so tool calls neither retry a failed
        // registration nor choose new metadata for this session.
        self.ready
            .get_or_init(|| async {
                let _guard = gate.lock().await;
                if let Err(error) = self.initialize(subc, state, session, peer).await {
                    let _ = session.unscoped_reason.set(error.to_string());
                }
            })
            .await;
    }

    async fn initialize(
        &self,
        subc: &SubcClient,
        state: &SessionState,
        session: &Arc<RelaySession>,
        peer: &Peer<RoleServer>,
    ) -> Result<()> {
        let owner = subc
            .scope_owner
            .get()
            .ok_or_else(|| other_error("no registered session scope owner"))?;
        if !daemon_supports_workdir(subc).await?
            || !owner.handle.daemon_supports("scopes/v1")
            || !owner.handle.daemon_supports("scope-workdir/v1")
        {
            let _ = session
                .unscoped_reason
                .set("daemon lacks scope-workdir/v1 or scope.apply support".into());
            return Ok(());
        }
        let workdir = session_workdir(peer, &self.fallback).await?;
        let project_id = resolve_project(subc, &state.identity, session, &workdir)
            .await
            .unwrap_or_else(|error| {
                tracing::info!(
                    "project registry unavailable; session project_id left unset: {error}"
                );
                None
            });
        let mut record = ScopeRecord::new(&self.scope_ref, 1, ScopeKind::Head);
        record.workdir = Some(workdir);
        record.project_id = project_id;
        self.submitted.store(true, Ordering::Relaxed);
        if let Err(error) = owner.apply(vec![record], vec![]).await {
            if error.downcast_ref::<ScopeRefusal>().is_some() {
                self.submitted.store(false, Ordering::Relaxed);
            }
            return Err(error);
        }
        let selector = ScopeSelector::new(
            Principal::Reserved {
                module_id: owner.module_id.clone(),
            },
            &self.scope_ref,
            Some(1),
        );
        session
            .scope
            .set(selector)
            .map_err(|_| other_error("MCP session scope already chosen"))?;
        Ok(())
    }

    pub(super) async fn end(&self, subc: &SubcClient) {
        // End by the allocated ref even if initialization was cancelled after
        // submitting its upsert but before storing the selector on the session.
        if !self.submitted.load(Ordering::Relaxed) {
            return;
        }
        let Some(owner) = subc.scope_owner.get() else {
            return;
        };
        if let Err(error) = owner
            .apply(vec![], vec![ScopeEnd::new(&self.scope_ref, 1)])
            .await
        {
            tracing::warn!("failed to end session attribution record: {error}");
        }
    }
}

async fn daemon_supports_workdir(subc: &SubcClient) -> Result<bool> {
    let body = serde_json::to_vec(&ClientControlRequest::ServerDescribe {})?;
    let frame = build_frame(
        FrameType::Request,
        control_flags(),
        0,
        0,
        subc.next_corr()?,
        body,
    )?;
    let response = subc.request(frame, SUBC_RESPONSE_TIMEOUT).await?;
    if response.header.ty == FrameType::Error {
        return Err(error_response("server.describe failed", &response.body));
    }
    match serde_json::from_slice::<ClientControlResponse>(&response.body)? {
        ClientControlResponse::ServerDescribe { capabilities, .. } => Ok(capabilities
            .iter()
            .any(|capability| capability == "scope-workdir/v1")),
        _ => Err(other_error(
            "unexpected server.describe response for session attribution",
        )),
    }
}

/// The folder in the spelling the daemon stores and Entorhinal keys on:
/// canonical, and on Windows without the verbatim `\\?\` prefix that
/// `std::fs::canonicalize` returns there. Sending the verbatim form would make
/// Entorhinal's `resolve` look up a root string no project was registered under.
pub(super) fn canonical_dir(path: &Path) -> Result<PathBuf> {
    cortexkit_paths::ProjectRootId::from_path(path)
        .map(cortexkit_paths::ProjectRootId::into_path_buf)
        .map_err(|error| other_error(format!("session workdir {}: {error}", path.display())))
}

// rmcp 3 marks the roots API deprecated because the 2026-07-28 protocol drops
// it as a server-to-client request. Hosts that sent `initialize` (2025-11-25
// and older) declared roots there, and `roots/list` is how the gateway learns
// their project folder. A host that started without `initialize` has no peer
// info, so it is never asked and the shim's project root is used.
#[allow(deprecated)]
async fn session_workdir(peer: &Peer<RoleServer>, fallback: &Path) -> Result<String> {
    let supports_roots = peer
        .peer_info()
        .is_some_and(|info| info.capabilities.roots.is_some());
    let root: Result<Option<PathBuf>> = async {
        if !supports_roots {
            return Ok(None);
        }
        let roots = time::timeout(SUBC_RESPONSE_TIMEOUT, peer.list_roots())
            .await
            .map_err(|_| other_error("MCP roots/list timed out"))??;
        let root = roots
            .roots
            .first()
            .map(|root| file_uri_path(&root.uri))
            .transpose()?;
        root.map(|root| canonical_dir(&root)).transpose()
    }
    .await;
    let (path, normalize) = match root {
        Ok(Some(path)) => (path, true),
        Ok(None) => {
            tracing::info!(fallback = %fallback.display(), "MCP client has no roots; using shim project root for session workdir");
            (canonical_dir(fallback)?, false)
        }
        Err(error) => {
            tracing::warn!(fallback = %fallback.display(), "MCP roots unavailable; using shim project root for session workdir: {error}");
            (canonical_dir(fallback)?, false)
        }
    };
    let canonical = path;
    let root = if normalize {
        canonical
            .ancestors()
            .find(|ancestor| ancestor.join(".git").exists())
            .unwrap_or(&canonical)
    } else {
        &canonical
    };
    root.to_str()
        .map(str::to_owned)
        .ok_or_else(|| other_error("session workdir is not UTF-8"))
}

// Pass the available local session selector to every desired tool provider
// opened during reconciliation, regardless of scope-workdir/v1. The daemon uses
// the admitted stamp to fill compatibility identity fields for targets without
// bind-no-project/v1, before stripping stamp workdir/project_id for targets
// without scope-workdir/v1. A rootless, unscoped opener gets `/` only in that
// compatibility encoding.
pub(super) fn provider_scope(selector: Option<&ScopeSelector>) -> Option<ScopeSelector> {
    selector.cloned()
}

fn file_uri_path(uri: &str) -> Result<PathBuf> {
    let path = uri
        .strip_prefix("file://")
        .ok_or_else(|| other_error("MCP root must be a file URI"))?;
    let path = if let Some(path) = path.strip_prefix("localhost/") {
        format!("/{path}")
    } else {
        path.to_owned()
    };
    if !path.starts_with('/') || path.contains(['?', '#']) {
        return Err(other_error("MCP root must be a local absolute file URI"));
    }
    let mut decoded = Vec::new();
    let mut bytes = path.bytes();
    while let Some(byte) = bytes.next() {
        decoded.push(if byte == b'%' {
            let hex = [
                bytes
                    .next()
                    .ok_or_else(|| other_error("invalid root URI escape"))?,
                bytes
                    .next()
                    .ok_or_else(|| other_error("invalid root URI escape"))?,
            ];
            u8::from_str_radix(std::str::from_utf8(&hex)?, 16)?
        } else {
            byte
        });
    }
    let path = String::from_utf8(decoded)?;
    if path.contains('\0') {
        return Err(other_error("MCP root contains NUL"));
    }
    #[cfg(windows)]
    let path = path
        .strip_prefix('/')
        .filter(|p| p.as_bytes().get(1) == Some(&b':'))
        .unwrap_or(&path)
        .to_owned();
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        return Err(other_error("MCP root is not absolute"));
    }
    Ok(path)
}

pub(super) async fn resolve_project(
    subc: &SubcClient,
    identity: &BindIdentity,
    session: &Arc<RelaySession>,
    workdir: &str,
) -> Result<Option<String>> {
    let route = open_route(
        subc,
        RouteTarget::ManagementSurface {
            module_id: "entorhinal".into(),
        },
        "entorhinal",
        identity,
        None,
        session.clone(),
    )
    .await?;
    let result: Result<Option<String>> = async {
        let body = serde_json::to_vec(
            &serde_json::json!({"method":"resolve", "params":{"canonicalRoot":workdir}}),
        )?;
        let frame = subc.build_route_frame(
            FrameType::Request,
            data_flags(),
            route,
            subc.next_corr()?,
            body,
        )?;
        let response = subc
            .request_on_route(route, frame, SUBC_RESPONSE_TIMEOUT)
            .await?;
        if response.header.ty != FrameType::Response {
            return Err(error_response(
                "project registry lookup failed",
                &response.body,
            ));
        }
        let reply: serde_json::Value = serde_json::from_slice(&response.body)?;
        let result = reply
            .get("result")
            .ok_or_else(|| other_error("invalid project registry reply"))?;
        match result.get("via").and_then(|v| v.as_str()) {
            Some("implicit") => Ok(None),
            Some("root" | "walk" | "worktree") => {
                let id = result
                    .get("projectId")
                    .and_then(|value| value.as_str())
                    .filter(|id| {
                        !id.is_empty()
                            && id.len() <= 128
                            && id.bytes().all(|b| (0x20..=0x7e).contains(&b))
                    })
                    .ok_or_else(|| other_error("invalid project id in registry reply"))?;
                Ok(Some(id.to_owned()))
            }
            _ => Err(other_error("invalid project registry resolution source")),
        }
    }
    .await;
    subc.relay().drop_route(route).await;
    let _ = send_route_goodbye(subc, route).await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_selector_is_used_for_all_providers() {
        let selector = ScopeSelector::new(
            Principal::Reserved {
                module_id: "subc-mcp".into(),
            },
            "mcp-session",
            Some(1),
        );
        assert_eq!(provider_scope(Some(&selector)), Some(selector));
        assert_eq!(provider_scope(None), None);
    }

    #[test]
    fn root_uri_decodes_local_absolute_paths_only() {
        #[cfg(not(windows))]
        assert_eq!(
            file_uri_path("file://localhost/a%20b").unwrap(),
            PathBuf::from("/a b")
        );
        #[cfg(windows)]
        assert_eq!(
            file_uri_path("file:///C:/a%20b").unwrap(),
            PathBuf::from("C:/a b")
        );
        for uri in [
            "https://host/a",
            "file://remote/a",
            "file:///a%",
            "file:///a#b",
            "file:///a%00",
        ] {
            assert!(file_uri_path(uri).is_err(), "{uri}");
        }
    }
}
