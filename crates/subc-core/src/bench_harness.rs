//! In-process forwarding table setup for contention benchmarks (feature `bench-harness` only).

use std::{collections::BTreeMap, sync::Arc};

use subc_protocol::{
    manifest::{
        Bindings, Concurrency, ConfigBinding, ConfigSource, ExecutionMode, IdentityBinding,
        IdentityScope, ModuleManifest, ProviderRole, StorageBinding, StorageKind, StorageScope,
        Tool, TrustTier,
    },
    Flags, FrameType, Priority, PROTOCOL_VERSION,
};
use tokio::sync::mpsc;

use crate::{
    forwarding::{DataRoute, RouteBindRelayOutcome},
    registry::ConnectionId,
    router::{ForwardBackend, FrameSink, RouteCtx, RouterError},
    ForwardingTable, Frame, Registry,
};

/// One committed client route used by concurrent bench workers.
#[derive(Clone)]
pub struct BenchClientRoute {
    pub connection_id: ConnectionId,
    pub client_channel: u16,
    pub module_channel: u16,
    pub ctx: RouteCtx,
}

pub struct BenchForwardingSetup {
    pub registry: Arc<Registry>,
    pub forwarding: Arc<ForwardingTable>,
    pub forward_backend: ForwardBackend,
    pub module_id: String,
    pub client_routes: Vec<BenchClientRoute>,
    _module_drain: tokio::task::JoinHandle<()>,
}

pub fn bench_tool_provider_manifest(module_id: &str) -> ModuleManifest {
    ModuleManifest {
        module_id: module_id.to_string(),
        module_version: "0.0.0-bench".to_string(),
        protocol_ver: PROTOCOL_VERSION,
        trust_tier: TrustTier::FirstParty,
        provides: vec![ProviderRole::ToolProvider {
            tools: vec![Tool {
                name: "read".to_string(),
                execution_mode: ExecutionMode::Pure,
                schema: serde_json::json!({"type": "object"}),
            }],
            identity_scope: vec![IdentityScope::Project, IdentityScope::Session],
            concurrency: Concurrency::StatelessParallel,
            emits_push: false,
            sub_supervises: false,
        }],
        consumes: Vec::new(),
        scheduled_tasks: Vec::new(),
        bindings: Bindings {
            storage: StorageBinding {
                kind: StorageKind::Sqlite,
                scope: StorageScope::Project,
                owns_schema: false,
            },
            config: ConfigBinding {
                source: ConfigSource::SubcMediated,
                tiers: vec!["user".to_string(), "project".to_string()],
                expansion: BTreeMap::new(),
            },
            vault_grants: Vec::new(),
            identity: IdentityBinding {
                requires: vec![IdentityScope::Project],
                optional: vec![IdentityScope::Session],
            },
        },
    }
}

fn manifest_concurrency(manifest: &ModuleManifest) -> Concurrency {
    manifest
        .provides
        .iter()
        .find_map(|provider| match provider {
            ProviderRole::ToolProvider { concurrency, .. } => Some(concurrency.clone()),
            ProviderRole::PipelineStage { .. }
            | ProviderRole::ManagementSurface { .. }
            | ProviderRole::InternalService { .. } => None,
        })
        .unwrap_or(Concurrency::ModuleManaged)
}

/// Fixed small request body (bench measures forwarding locks, not parsing).
pub fn bench_data_request_frame(client_channel: u16, corr: u64) -> Frame {
    const PAYLOAD: &[u8] = br#"{"jsonrpc":"2.0","id":1,"method":"read","params":{}}"#;
    Frame::build(
        FrameType::Request,
        Flags::new(false, Priority::Interactive, false),
        client_channel,
        corr,
        PAYLOAD.to_vec(),
    )
    .expect("bench frame")
}

/// Client→module forward path: fused route lookup + bound backend (production hot path).
pub async fn bench_client_forward_op(
    forwarding: &ForwardingTable,
    forward_backend: &ForwardBackend,
    route: &BenchClientRoute,
    corr: u64,
) -> Result<(), RouterError> {
    let frame = bench_data_request_frame(route.client_channel, corr);
    let channel = frame.header.channel;
    let binding = match forwarding
        .lookup_data_route(route.connection_id, channel)
        .map_err(RouterError::Forwarding)?
    {
        DataRoute::Client(Some(binding)) => binding,
        DataRoute::Client(None) | DataRoute::Module(_) => {
            return Err(RouterError::UnknownChannel { channel, corr });
        }
    };
    forward_backend.handle_bound(frame, binding).await
}

pub async fn build_bench_forwarding_setup(
    num_clients: usize,
    routes_per_client: usize,
) -> BenchForwardingSetup {
    assert!(num_clients >= 1);
    assert!(routes_per_client >= 1);

    let registry = Arc::new(Registry::default());
    let forwarding = Arc::new(ForwardingTable::default());
    let forward_backend = ForwardBackend::new(Arc::clone(&forwarding));
    let module_id = "bench-fake-aft".to_string();
    let module_connection = ConnectionId::new(1);
    let manifest = bench_tool_provider_manifest(&module_id);

    registry
        .register_with_control_ops(
            manifest.clone(),
            PROTOCOL_VERSION,
            module_connection,
            Vec::new(),
        )
        .expect("register module");

    let (module_tx, module_rx) = mpsc::channel(65_536);
    let forwarding_for_echo = Arc::clone(&forwarding);
    let module_drain = tokio::spawn(bench_module_echo_drain(
        forwarding_for_echo,
        module_connection,
        module_rx,
    ));

    let endpoint = forwarding
        .register_module_connection(
            module_connection,
            module_id.clone(),
            PROTOCOL_VERSION,
            manifest_concurrency(&manifest),
            FrameSink::new(module_tx),
        )
        .expect("register module connection");

    let mut client_routes = Vec::with_capacity(num_clients * routes_per_client);
    for client_index in 0..num_clients {
        let client_connection = ConnectionId::new(100 + client_index as u64);
        for _route in 0..routes_per_client {
            let pending = forwarding
                .begin_route_bind_relay_for(client_connection, &module_id)
                .expect("begin route bind");
            assert_eq!(pending.endpoint, endpoint);
            forwarding
                .complete_pending_relay(
                    module_connection,
                    pending.corr,
                    RouteBindRelayOutcome::Accepted,
                )
                .expect("complete relay");
            let (ctx, mut client_rx) = bench_route_ctx(client_connection);
            tokio::spawn(async move { while client_rx.recv().await.is_some() {} });
            forwarding
                .commit_route(
                    client_connection,
                    ctx.egress.clone(),
                    PROTOCOL_VERSION,
                    endpoint,
                    pending.client_channel,
                    pending.module_channel,
                )
                .expect("commit route");
            client_routes.push(BenchClientRoute {
                connection_id: client_connection,
                client_channel: pending.client_channel,
                module_channel: pending.module_channel,
                ctx,
            });
        }
    }

    BenchForwardingSetup {
        registry,
        forwarding,
        forward_backend,
        module_id,
        client_routes,
        _module_drain: module_drain,
    }
}

/// Mimics fake-aft-stub echo: module receives REQUEST, router returns RESPONSE and releases credit.
async fn bench_module_echo_drain(
    forwarding: Arc<ForwardingTable>,
    module_connection: ConnectionId,
    mut module_rx: mpsc::Receiver<Frame>,
) {
    while let Some(frame) = module_rx.recv().await {
        if frame.header.ty != FrameType::Request {
            continue;
        }
        let module_channel = frame.header.channel;
        let corr = frame.header.corr;
        let body = frame.body.clone();
        let route = match forwarding
            .lookup_data_route(module_connection, module_channel)
            .expect("lookup_data_route")
        {
            DataRoute::Module(Some(route)) => route,
            DataRoute::Module(None) | DataRoute::Client(_) => continue,
        };
        if let Ok(response) = Frame::build(
            FrameType::Response,
            frame.header.flags,
            route.client_channel,
            corr,
            body,
        ) {
            let _ = route.client_sink.try_send(response);
            route.flow.release();
        }
    }
}

fn bench_route_ctx(connection_id: ConnectionId) -> (RouteCtx, mpsc::Receiver<Frame>) {
    let (tx, rx) = mpsc::channel(65_536);
    (
        RouteCtx {
            connection_id,
            egress: FrameSink::new(tx),
        },
        rx,
    )
}
