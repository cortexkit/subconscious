use super::*;
use subc_protocol::{
    scope::{ScopeEnd, ScopeKind, ScopeRecord, ScopeRecordOutcome},
    session::{ModuleControlRequestFromModule, ModuleControlResponseToModule},
    Principal,
};
use tokio::sync::{oneshot, OnceCell};

#[derive(Debug)]
struct ScopeRefusal(String);

impl std::fmt::Display for ScopeRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for ScopeRefusal {}

// A daemon stop ends supervision even when it interrupts an optional scope change.
#[derive(Debug)]
struct ScopeStopped;

impl std::fmt::Display for ScopeStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("scope owner stopped by daemon")
    }
}

impl Error for ScopeStopped {}

fn scope_refusal(message: String) -> BoxError {
    Box::new(ScopeRefusal(message))
}

pub(super) struct ScopeChange {
    upsert: Vec<ScopeRecord>,
    end: Vec<ScopeEnd>,
    reply: oneshot::Sender<Result<()>>,
}

#[derive(Clone, Debug)]
pub(super) struct ScopeOwner {
    module_id: String,
    changes: mpsc::Sender<ScopeChange>,
    supported: bool,
}

impl ScopeOwner {
    pub(super) fn new(module_id: String, supported: bool) -> (Self, mpsc::Receiver<ScopeChange>) {
        let (changes, rx) = mpsc::channel(64);
        (
            Self {
                module_id,
                changes,
                supported,
            },
            rx,
        )
    }

    async fn apply(&self, upsert: Vec<ScopeRecord>, end: Vec<ScopeEnd>) -> Result<()> {
        if !self.supported {
            return Err(other_error(
                "daemon lacks standalone MCP scope capabilities",
            ));
        }
        time::timeout(SUBC_RESPONSE_TIMEOUT, async {
            let (reply, rx) = oneshot::channel();
            self.changes
                .send(ScopeChange { upsert, end, reply })
                .await
                .map_err(|_| other_error("scope owner connection closed"))?;
            rx.await
                .map_err(|_| other_error("scope owner reply lost"))?
        })
        .await
        .map_err(|_| other_error("scope registration timed out"))?
    }
}

// A dedicated reader never cancels a partial frame when a scope change arrives.
// The command task serializes writes and generations, and answers daemon health
// probes even while a scope reply is outstanding.
pub(super) async fn supervision_loop(
    stream: TcpStream,
    subc: SubcClient,
    changes: mpsc::Receiver<ScopeChange>,
) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let (tx, mut frames) = mpsc::channel(64);
    let reader = tokio::spawn(async move {
        loop {
            let frame = match read_frame(&mut reader).await {
                Ok(Some(frame)) => Ok(frame),
                Ok(None) => break,
                Err(error) => Err(other_error(error.to_string())),
            };
            let failed = frame.is_err();
            if tx.send(frame).await.is_err() || failed {
                break;
            }
        }
    });
    let result = supervision_commands(
        &mut writer,
        &mut frames,
        subc,
        changes,
        SUBC_RESPONSE_TIMEOUT,
    )
    .await;
    reader.abort();
    result
}

async fn supervision_commands(
    stream: &mut OwnedWriteHalf,
    frames: &mut mpsc::Receiver<Result<SubcFrame>>,
    subc: SubcClient,
    mut changes: mpsc::Receiver<ScopeChange>,
    apply_timeout: Duration,
) -> Result<()> {
    let mut generation = 0u64;
    loop {
        tokio::select! {
            frame = frames.recv() => {
                let Some(frame) = frame else { return Ok(()); };
                let frame = frame?;
                // An optional scope RPC may have timed out before its reply arrived.
                if matches!(frame.header.ty, FrameType::Response | FrameType::Error) {
                    continue;
                }
                let Some(reply) = handle_supervision_control_frame(&frame, &subc).await? else { return Ok(()); };
                write_frame(stream, &reply).await?;
                stream.flush().await?;
            }
            change = changes.recv() => {
                let Some(change) = change else { return Ok(()); };
                let result = async {
                    if generation == 0 {
                        generation = 1;
                        let reply = control_rpc(stream, frames, &subc, ModuleControlRequestFromModule::ScopeSync {
                            generation, scopes: vec![],
                        }, SUBC_RESPONSE_TIMEOUT).await?;
                        if !matches!(reply, ModuleControlResponseToModule::ScopeSync { generation: 1, .. }) {
                            return Err(other_error("unexpected initial scope.sync reply"));
                        }
                    }
                    generation = generation.checked_add(1).ok_or_else(|| other_error("scope generation exhausted"))?;
                    let reply = control_rpc(stream, frames, &subc, ModuleControlRequestFromModule::ScopeApply {
                        generation, upsert: change.upsert, end: change.end,
                    }, apply_timeout).await?;
                    match reply {
                        ModuleControlResponseToModule::ScopeApply { generation: accepted, results, .. } if accepted == generation => {
                            if let Some(refusal) = results.iter().find(|r| r.outcome == ScopeRecordOutcome::Refused) {
                                return Ok(Some(scope_refusal(format!("scope record refused: {:?}: {:?}", refusal.code, refusal.message))));
                            }
                            Ok(None)
                        }
                        _ => Err(other_error("unexpected scope.apply reply")),
                    }
                }.await;
                let result = result.and_then(|refusal| refusal.map_or(Ok(()), Err));
                let stopped = result.as_ref().err().is_some_and(|error| error.downcast_ref::<ScopeStopped>().is_some());
                let _ = change.reply.send(result);
                if stopped {
                    return Ok(());
                }
            }
        }
    }
}

async fn control_rpc(
    stream: &mut OwnedWriteHalf,
    frames: &mut mpsc::Receiver<Result<SubcFrame>>,
    subc: &SubcClient,
    request: ModuleControlRequestFromModule,
    wait: Duration,
) -> Result<ModuleControlResponseToModule> {
    let corr = subc.next_corr()?;
    let frame = build_frame(
        FrameType::Request,
        control_flags(),
        0,
        0,
        corr,
        serde_json::to_vec(&request)?,
    )?;
    write_frame(stream, &frame).await?;
    stream.flush().await?;
    time::timeout(wait, async {
        loop {
            let frame = frames
                .recv()
                .await
                .ok_or_else(|| other_error("scope owner connection ended"))??;
            if frame.header.channel == 0 && frame.header.corr == corr {
                match frame.header.ty {
                    FrameType::Response => return Ok(serde_json::from_slice(&frame.body)?),
                    FrameType::Error => {
                        let refusal: ErrorBody = serde_json::from_slice(&frame.body)?;
                        return Err(scope_refusal(format!(
                            "scope control refused: {}: {}",
                            refusal.code, refusal.message
                        )));
                    }
                    _ => {}
                }
            }
            if matches!(frame.header.ty, FrameType::Response | FrameType::Error) {
                // Do not answer a late reply as though it were a daemon request.
                continue;
            }
            let Some(reply) = handle_supervision_control_frame(&frame, subc).await? else {
                return Err(Box::new(ScopeStopped) as BoxError);
            };
            write_frame(stream, &reply).await?;
            stream.flush().await?;
        }
    })
    .await
    .map_err(|_| other_error("scope control timed out"))?
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
                    tracing::warn!(
                        "session attribution failed; using unscoped provider routes: {error}"
                    );
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
        if !daemon_supports_workdir(subc).await? || !owner.supported {
            tracing::warn!("session attribution skipped; using unscoped provider routes: daemon lacks scope-workdir/v1 or scope.apply support");
            return Ok(());
        }
        let workdir = session_workdir(peer, &self.fallback).await?;
        let project_id = resolve_project(subc, &state.identity, session, &workdir).await;
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
fn canonical_dir(path: &Path) -> Result<PathBuf> {
    cortexkit_paths::ProjectRootId::from_path(path)
        .map(cortexkit_paths::ProjectRootId::into_path_buf)
        .map_err(|error| other_error(format!("session workdir {}: {error}", path.display())))
}

// rmcp 3 marks the roots API deprecated because the 2026-07-28 protocol drops
// it. The gateway only serves 2025-11-25 and older sessions, where `roots/list`
// is how it learns the host's project folder.
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

// Legacy providers must receive the same bind body as before scope attribution
// existed. Only a provider that opted into folder attribution gets our selector.
pub(super) fn provider_scope(
    selector: Option<&ScopeSelector>,
    entry: Option<&CatalogEntry>,
) -> Option<ScopeSelector> {
    entry
        .filter(|entry| {
            entry.capabilities.as_ref().is_some_and(|capabilities| {
                capabilities
                    .provides
                    .iter()
                    .any(|capability| capability == "scope-workdir/v1")
            })
        })
        .and(selector)
        .cloned()
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

async fn resolve_project(
    subc: &SubcClient,
    identity: &BindIdentity,
    session: &Arc<RelaySession>,
    workdir: &str,
) -> Option<String> {
    let result = async {
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
                return Ok(None);
            }
            let reply: serde_json::Value = serde_json::from_slice(&response.body)?;
            let Some(result) = reply.get("result") else {
                return Ok(None);
            };
            if !matches!(
                result.get("via").and_then(|v| v.as_str()),
                Some("root" | "walk" | "worktree")
            ) {
                return Ok(None);
            }
            Ok(result
                .get("projectId")
                .and_then(|v| v.as_str())
                .filter(|id| {
                    !id.is_empty()
                        && id.len() <= 128
                        && id.bytes().all(|b| (0x20..=0x7e).contains(&b))
                })
                .map(str::to_owned))
        }
        .await;
        subc.relay().drop_route(route).await;
        let _ = send_route_goodbye(subc, route).await;
        result
    }
    .await;
    match result {
        Ok(id) => id,
        Err(error) => {
            tracing::info!(
                "Entorhinal resolve unavailable; session project_id left unset: {error}"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROMPT_STOP: Duration = Duration::from_secs(2);

    async fn stream_pair() -> (TcpStream, TcpStream) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (client, server) = tokio::join!(TcpStream::connect(addr), listener.accept());
        (client.unwrap(), server.unwrap().0)
    }

    async fn queue_scope_change(
        changes: &mpsc::Sender<ScopeChange>,
    ) -> oneshot::Receiver<Result<()>> {
        let (reply, pending) = oneshot::channel();
        changes
            .send(ScopeChange {
                upsert: vec![ScopeRecord::new("pending-session", 1, ScopeKind::Head)],
                end: vec![],
                reply,
            })
            .await
            .unwrap();
        pending
    }

    async fn read_scope_request(daemon: &mut TcpStream, op: &str) -> SubcFrame {
        let frame = time::timeout(PROMPT_STOP, read_frame(daemon))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame.header.ty, FrameType::Request);
        assert_eq!(frame.header.channel, 0);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&frame.body).unwrap()["op"],
            op
        );
        frame
    }

    fn scope_response(corr: u64, body: serde_json::Value) -> SubcFrame {
        build_frame(
            FrameType::Response,
            control_flags(),
            0,
            0,
            corr,
            serde_json::to_vec(&body).unwrap(),
        )
        .unwrap()
    }

    fn daemon_goodbye() -> SubcFrame {
        build_frame(FrameType::Goodbye, control_flags(), 0, 0, 0, vec![]).unwrap()
    }

    #[tokio::test]
    async fn goodbye_during_scope_apply_fails_pending_change_and_stops_supervision() {
        let (module, mut daemon) = stream_pair().await;
        let (relay, _relay_daemon) = stream_pair().await;
        let subc = SubcClient::start(relay);
        let (changes, receiver) = mpsc::channel(2);
        let supervision = tokio::spawn(supervision_loop(module, subc, receiver));
        let mut pending = queue_scope_change(&changes).await;

        let sync = read_scope_request(&mut daemon, "scope.sync").await;
        let response = scope_response(
            sync.header.corr,
            serde_json::json!({"op":"scope.sync", "generation":1, "results":[]}),
        );
        write_frame(&mut daemon, &response).await.unwrap();
        let _apply = read_scope_request(&mut daemon, "scope.apply").await;
        assert!(matches!(
            pending.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));

        // Keep both the daemon socket and change sender open: only GOODBYE may stop the loop.
        write_frame(&mut daemon, &daemon_goodbye()).await.unwrap();
        let failure = time::timeout(PROMPT_STOP, pending)
            .await
            .expect("GOODBYE must settle the pending scope change")
            .expect("pending scope change must receive an explicit failure")
            .expect_err("GOODBYE cannot acknowledge an unfinished scope change");
        assert!(failure.to_string().contains("stopped by daemon"));
        time::timeout(PROMPT_STOP, supervision)
            .await
            .expect("GOODBYE during scope.apply must stop supervision without waiting for EOF")
            .expect("supervision task must not panic")
            .expect("daemon stop is a clean supervision exit");
        assert!(changes.is_closed());
    }

    #[tokio::test]
    async fn goodbye_after_scope_rpc_timeout_still_stops_supervision() {
        let (module, mut daemon) = stream_pair().await;
        let (_reader, mut writer) = module.into_split();
        let (relay, _relay_daemon) = stream_pair().await;
        let subc = SubcClient::start(relay);
        let (changes, receiver) = mpsc::channel(2);
        let (frames, mut incoming) = mpsc::channel(4);

        let supervision = tokio::spawn(async move {
            supervision_commands(&mut writer, &mut incoming, subc, receiver, Duration::ZERO).await
        });
        let pending = queue_scope_change(&changes).await;
        let sync = read_scope_request(&mut daemon, "scope.sync").await;
        frames
            .send(Ok(scope_response(
                sync.header.corr,
                serde_json::json!({"op":"scope.sync", "generation":1, "results":[]}),
            )))
            .await
            .unwrap();
        let apply = read_scope_request(&mut daemon, "scope.apply").await;
        let failure = time::timeout(PROMPT_STOP, pending)
            .await
            .expect("scope RPC timeout must settle the pending change")
            .expect("pending change must receive an explicit timeout failure")
            .expect_err("unanswered scope.apply cannot succeed");
        assert!(failure.to_string().contains("scope control timed out"));
        assert!(
            !supervision.is_finished(),
            "an attribution timeout alone must not stop supervision"
        );

        frames.send(Ok(scope_response(apply.header.corr, serde_json::json!({"op":"scope.apply", "generation":2, "results":[], "end_results":[]})))).await.unwrap();
        frames.send(Ok(daemon_goodbye())).await.unwrap();
        time::timeout(PROMPT_STOP, supervision)
            .await
            .expect("GOODBYE after a scope timeout must stop supervision without EOF")
            .expect("supervision task must not panic")
            .expect("daemon stop after a timeout is a clean exit");
        assert!(changes.is_closed());
    }

    #[test]
    fn local_selector_requires_provider_workdir_opt_in() {
        let selector = ScopeSelector::new(
            Principal::Reserved {
                module_id: "subc-mcp".into(),
            },
            "mcp-session",
            Some(1),
        );
        let mut entry = CatalogEntry::new("provider", vec![], vec![]);
        assert_eq!(provider_scope(Some(&selector), Some(&entry)), None);
        entry.capabilities = Some(subc_protocol::manifest::CapabilityDeclarations {
            provides: vec!["scopes/v1".into()],
            requires: vec![],
            must_never_reach: vec![],
        });
        assert_eq!(provider_scope(Some(&selector), Some(&entry)), None);
        entry
            .capabilities
            .as_mut()
            .unwrap()
            .provides
            .push("scope-workdir/v1".into());
        assert_eq!(
            provider_scope(Some(&selector), Some(&entry)),
            Some(selector)
        );
        assert_eq!(provider_scope(None, Some(&entry)), None);
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
