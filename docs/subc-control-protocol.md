# subc channel-0 control protocol (v0.4 — AFT co-signed)

Status: AFT co-signed the module-facing surface (pm_251e0356) — module-facing wire LOCKED. subc-control (client side) free to evolve pre-release.
Supersedes: the ad-hoc channel-0 shapes in `subc-protocol::session` + `subc-core::status`.
Origin: councils `bg_3c0936ae` (shape) + `bg_7049add2` (adversarial gap audit) + AFT co-sign review + the pre-release "cleanest-from-scratch" principle.
v0.2→v0.3: folded all 9 wire-shape must-fixes from the gap audit.
v0.3→v0.4 (AFT co-sign pins, all behavior/text — NO wire change): (1) module route.bind rejection on the Error lane is relayed VERBATIM to the client's route.open corr (config_divergence diff must reach the user intact); (2) route.poll is answered ONLY from subc's cache/supervision — NEVER forwarded to the module (zero frames to module — AFT issue #117 hang-restart guard); (3) subc OWNS project_root canonicalization at the route.open→route.bind boundary; the module consumes it as-is and never re-canonicalizes; (4) the baseline `control_ops` set (when None) is enumerated; (5) subc does no RouteStatus fan-out (module emits per route_channel) and a baseline-only v1 module may ignore subc_ops.

This is the COMPLETE 4-phase wire contract (client↔subc + module↔subc). It is designed so every foreseeable module (MC, embedding pipeline, LLM-runner/dreamer, Alfonso bus, InterSUBC federation, generic router) integrates with NO new `FrameType` and NO new subc-understood control op.

---

## 0. Principles (binding)
- **Thin core.** subc understands ONLY: routing, route open/close, catalog, poll(status/liveness), lifecycle, supervision, lease, and RAW config-tier transport. Everything that interprets module/business semantics is OPAQUE and rides a route channel. Test: *if the op changes subc's routing/lifecycle/resource state, subc understands it; if it changes module-owned domain state, it's opaque route-channel RPC.*
- **Envelope is LOCKED** (17 bytes: len/ver/type/flags/channel/corr). No new `FrameType`. New control = new tagged JSON body on channel 0.
- **Direction-split.** Tagged enums, one per direction. Requests, responses, AND pushes are all internally-tagged (`#[serde(tag="op")]`, fields inline) — no peer ever runs a try-deserialize cascade.
- **Additive-only forward-compat.** Response/push structs MUST NOT use `#[serde(deny_unknown_fields)]` (so adding a field never breaks an old peer). Unknown `op` → typed error; unknown channel-0 **Push** op → IGNORE (never error). Breaking change = a NEW op name (`route.open.v2`), never an in-place reshape.
- **Weak-agent parseability** is a hard product constraint: flat familiar JSON over dense abstractions.

## 1. Crate layout (the boundary cut)
- **`subc-protocol`** (published, serde-only) = the subc↔MODULE wire + SHARED PRIMITIVES. What AFT and every provider module depends on.
  - Envelope + `FrameType` (locked).
  - Shared primitives: `ConfigTier`, `ErrorBody`, `BindIdentity`, `RouteTarget`, `ModuleManifest` (+ `ProviderRole`, `ConsumerRole`).
  - Module-facing control: `ModuleHelloBody`/`ModuleHelloAckBody`, `ModuleControlRequest`, `ModuleControlResponse`, `ModuleControlPush`.
- **`subc-control`** (NEW, published, serde-only) = the client↔SUBC wire. What subc-mcp / TS client / CK-app / CLI depend on WITHOUT pulling the daemon. Depends on `subc-protocol` for shared primitives.
  - `ClientControlRequest` / `ClientControlResponse` / `ClientControlPush` (daemon-originated only — see §6).
- **`subc-core`** = implementation only. Holds NO source-of-truth wire structs.

Layering: `subc-protocol` (base) ← `subc-control` ← clients (subc-mcp, TS, CK-app); `subc-protocol` ← AFT/provider modules; both ← `subc-core`.

## 2. Header-only signals (FrameTypes — unchanged, NOT JSON ops)
Pure-header frames (len==0, enforced by `decode_header`); the high-frequency/transport-level signals, deliberately NOT JSON ops:
- `PING` / `PONG` — singleton liveness probe at connect.
- `GOODBYE` — **the single route/connection teardown primitive, in BOTH directions.** channel 0 = whole connection; non-zero channel = close THAT route. Used client→subc AND subc→client (route death) AND module-side. There is NO JSON `route.close`/`route.release` op (one mechanism, no redundancy).
  - **Permanent tradeoff (deliberate):** GOODBYE is pure-header — it carries NO close reason and NO graceful/abrupt flag, ever. If a close-reason need ever arises it is a NEW coexisting op, never a GOODBYE body.
- `CANCEL` — cancel an in-flight request on a route channel (dumb-forwarded).
- `StreamData` / `StreamEnd` — RESERVED for opaque streaming on ROUTE channels (data plane). Never used on channel 0.

### 2.1 GOODBYE-on-route semantics (both directions — normative)
A non-zero-channel GOODBYE is **terminal + idempotent** for that route, from either side:
- Release the route binding (run the same release path — `handle_route_goodbye` — regardless of which side initiated; a MODULE-originated route GOODBYE must ALSO run subc's release path, not just be forwarded, so `client_to_module`/`module_to_client` never retain stale bindings).
- Close/release the per-channel flow-control window (credit) for that route.
- Drop any late frames arriving on the released channel.
- Fail any pending `corr` on that route with a route-closed error.
- A second GOODBYE / GOODBYE on an unknown route = typed no-op (connection survives).

## 3. Shared primitives (`subc-protocol`)
```rust
pub struct ConfigTier { pub tier: String, pub source: String, pub doc: String } // unchanged

pub struct BindIdentity { pub project_root: PathBuf, pub harness: String, pub session: String }

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RouteTarget {                          // explicit provider/surface selection
    ToolProvider      { module_id: String },
    ManagementSurface { module_id: String },
    InternalService   { module_id: String, service_id: String },
    // NOTE: NO BusSurface in v1 — added later together with ProviderRole::BusSurface (both additive).
}

pub struct ErrorBody { pub code: String, pub message: String } // code = open string; v1 set in §8

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Principal {
    Reserved { module_id: String },
    Direct,
    Unverified,
}
```
**RouteTarget.kind ↔ ProviderRole mapping (normative):**
| RouteTarget.kind | required ProviderRole | disambiguator |
|---|---|---|
| `tool_provider` | `ToolProvider` | v1: ≤1 per module |
| `management_surface` | `ManagementSurface` | v1: ≤1 per module |
| `internal_service` | `InternalService` | `service_id` (multiple allowed) |
- `ProviderRole::PipelineStage` is intentionally **unroutable** (pipeline modules are wired by an orchestrator, not opened by clients) — documented, not a target.
- v1 manifest constraint: at most one `ToolProvider` and one `ManagementSurface` role per module (so `RouteTarget` needs no surface-id for those). `InternalService` is disambiguated by `service_id`. (Relaxing this later = additive surface-id, no reshape.)

## 4. Client ↔ subc wire (`subc-control`)
```rust
#[derive(Serialize, Deserialize)]
#[serde(tag = "op")]                            // NOT deny_unknown_fields
pub enum ClientControlRequest {
    #[serde(rename = "server.describe")]        ServerDescribe {},
    #[serde(rename = "catalog.list")]           CatalogList { module_id: Option<String> }, // NO plane filter (thin-core)
    #[serde(rename = "route.open")]             RouteOpen { target: RouteTarget, identity: BindIdentity,
                                                            #[serde(default, skip_serializing_if = "Option::is_none")]
                                                            consumer_identity: Option<ConsumerIdentity>,
                                                            #[serde(default)] config: Vec<ConfigTier> },
    #[serde(rename = "route.poll")]             RoutePoll { route_channel: u16, kind: PollKind }, // status|liveness
    #[serde(rename = "supervisor.list")]        SupervisorList {},
    #[serde(rename = "supervisor.restart")]     SupervisorRestart { module_id: String },
    #[serde(rename = "supervisor.reload")]      SupervisorReload { module_id: String }, // drain-to-quiescence hot-swap
    #[serde(rename = "supervisor.rescan")]      SupervisorRescan {}, // reconcile module keys/specs from disk
    #[serde(rename = "supervisor.release_reserved")] SupervisorReleaseReserved { module_id: String },
    #[serde(rename = "supervisor.set_enabled")] SupervisorSetEnabled { module_id: String, enabled: bool },
    #[serde(rename = "supervisor.health_probe")]SupervisorHealthProbe { module_id: String },
    #[serde(rename = "supervisor.health")]      SupervisorHealth {},
    #[serde(rename = "supervisor.routes")]      SupervisorRoutes { module_id: Option<String> },
    #[serde(rename = "supervisor.provenance")]  SupervisorProvenance { module_id: Option<String> },
    #[serde(rename = "supervisor.stderr_tail")] SupervisorStderrTail { module_id: String, max_lines: Option<u32>, max_bytes: Option<u32> },
    #[serde(rename = "supervisor.terminals")]   SupervisorTerminals { module_id: String },
    // FUTURE (additive): config.* (raw-tier get/put/changed)
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op")]                            // mirror-tagged; NOT deny_unknown_fields
pub enum ClientControlResponse {
    #[serde(rename = "server.describe")] ServerDescribe { protocol_ver: u8, subc_ops: Vec<String>, capabilities: Vec<String> },
    #[serde(rename = "catalog.list")]    CatalogList { generation: u64, modules: Vec<CatalogEntry>, subc_ops: Vec<String> },
    #[serde(rename = "route.open")]      RouteOpen { route_channel: u16 },
    #[serde(rename = "route.poll")]      RoutePoll { status: Option<String>, live: Option<bool> },
    #[serde(rename = "supervisor.list")] SupervisorList { generation: u64, modules: Vec<SupervisorEntry> },
    #[serde(rename = "supervisor.ack")]  SupervisorAck { module_id: String, applied: bool },
    #[serde(rename = "supervisor.rescan")] SupervisorRescan { #[serde(flatten)] result: SupervisorRescanResult },
    #[serde(rename = "supervisor.health_probe")] SupervisorHealthProbe { module_id: String, status: HealthStatus, detail: Option<String>, metrics: Option<Value> },
    #[serde(rename = "supervisor.health")] SupervisorHealth { generation: u64, modules: Vec<SupervisorHealthEntry> },
    #[serde(rename = "supervisor.routes")] SupervisorRoutes { modules: Vec<SupervisorRouteModule> },
    #[serde(rename = "supervisor.provenance")] SupervisorProvenance { daemon: SupervisorDaemonProvenance, modules: Vec<SupervisorModuleProvenance> },
    #[serde(rename = "supervisor.stderr_tail")] SupervisorStderrTail { module_id: String, #[serde(flatten)] tail: StderrTail },
    #[serde(rename = "supervisor.terminals")] SupervisorTerminals { module_id: String, #[serde(flatten)] terminals: TerminalHistory },
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum ClientControlPush {
    #[serde(rename = "route.closing")] RouteClosing { module_id: String, reason: RouteCloseReason },
    #[serde(rename = "route.closed")]  RouteClosed { module_id: String, reason: RouteCloseReason,
                                                        drained: bool, abandoned: u32, terminal: bool },
}

#[serde(rename_all="snake_case")] pub enum RouteCloseReason { Reload, Restart, Disable, Crash }

#[serde(rename_all="snake_case")] pub enum PollKind { Status, Liveness }
pub struct ConsumerIdentity { pub module_id: String, pub launch_nonce: String }
pub struct CatalogEntry { pub module_id: String, pub roles: Vec<ProviderRole>, pub control_ops: Vec<String> }
pub struct SupervisorEntry { pub module_id: String, pub state: String, pub enabled: bool, pub live: bool }
pub struct SupervisorRescanResult { pub added: Vec<String>, pub removed: Vec<String>, pub changed_pending_reload: Vec<String>, pub unchanged: u32 }
// Route teardown = GOODBYE frame (§2). Errors = ErrorBody on FrameType::Error (§8), correlated by channel+corr.
```
Notes:
- `catalog.list` SUBSUMES `manifest_list`; returns modules + roles + per-module control ops; client filters by plane/role itself (subc owns no role→plane mapping). The `generation` counter lets a future `catalog.changed` push reconcile missed events with no reshape.
- No client HELLO in v1 — `server.describe` is the client's discovery handshake. Client capability negotiation is discovery-only; `op_not_allowed` enforcement applies to MODULE ops in v1.
- `route.closing { module_id, reason }` is enqueued before a planned drain starts. `reason` is `reload`, `restart`, or `disable`; it makes no completion claim.
- `route.closed { module_id, reason, drained, abandoned, terminal }` is enqueued after the forwarding-quiescence wait and before the released-route GOODBYEs. `drained` is the exact boolean returned by that wait, not a later route-state inference. `abandoned` counts pending `route.bind` relays forced down before the wait; they are never covered by `drained`.
- `terminal` answers one question on every `route.closed`: will subc bring this module back without operator action? It is a claim about the module's future, not the connection that died. `disable` is `true`; `reload` and `restart` are `false`; `crash` is `!(enabled && restart_count < max_restarts)` for the crash being reported before that crash consumes a restart. At `max_restarts - 1`, crash is `terminal:false` and subc respawns; at `max_restarts`, crash is the first `terminal:true` close. Clients key on `terminal` alone: they must not recreate policy from reason strings or correlate a close with `supervisor.list` counters/config presence.

## 5. Module ↔ subc wire (`subc-protocol`) — the AFT co-sign surface
```rust
// module → subc
pub struct ModuleHelloBody {
    pub manifest: ModuleManifest,
    pub protocol_ver: u8,
    pub control_ops: Option<Vec<String>>,  // None = legacy baseline ONLY (never "all"); Some([])=no optional; Some([..])=exactly those
}

`ModuleManifest.provenance` is optional. It is a module declaration, not a daemon
observation:

```rust
pub struct ModuleManifest {
    // ...required manifest fields...
    pub capabilities: Option<CapabilityDeclarations>,
    pub provenance: Option<ManifestProvenance>,
}

pub struct ManifestProvenance {
    pub build_git_sha: Option<String>,
    pub build_lock_digest: Option<String>,
    pub wire_crate_version: Option<String>,
    pub store_schema_version: Option<String>,
}
```

Each field is independently optional. When present, each value must be non-empty,
at most 128 bytes, and printable ASCII (`0x20`–`0x7e`). A manifest that violates any
of these rules is refused at HELLO with `invalid_manifest`; the module does not
register. An absent `provenance` block is different: it is valid, registration
proceeds normally, and the response reports `module_declared.status = unverifiable`.
The bound exists because declared values are module-controlled and reach operator
terminals, so the daemon limits their size and character set at its boundary.

The current `subc-core` build script emits
`SUBC_BUILD_GIT_SHA` and `SUBC_BUILD_LOCK_DIGEST`; its exact grammar is:

```text
cargo:rustc-env=SUBC_BUILD_GIT_SHA=<40-hex SHA, or unavailable; -dirty when the tree is dirty>
cargo:rustc-env=SUBC_BUILD_LOCK_DIGEST=<SHA-256 Cargo.lock digest, or unavailable>
```

`wire_crate_version` and `store_schema_version` are supplied by the SDK/build
integration, not by `subc-core/build.rs`.

Old daemons ignore this additive optional block while decoding HELLO, so an absent or
present block remains HELLO-compatible. Rust consumers that construct `ModuleManifest`
with a struct literal are different: they must add `provenance: None` (or a value), or
the path-dependency consumer can fail with a missing-field compile error after the
protocol bump.

The client request is `supervisor.provenance { module_id: Option<String> }`. The
response keeps sources separate:

```rust
pub struct SupervisorModuleProvenance {
    pub module_id: String,
    pub module_declared: ModuleDeclaredProvenance,
    pub daemon_observed: SupervisorObservedProcess,
}
```

`module_declared` is `reported { build: ManifestProvenance }` or `unverifiable` when
HELLO omitted the block. `daemon_observed` contains pid, spawn time, exact
spawned-from path, and the running-image agreement. The response's `daemon` member
contains the daemon's own build identity and observed process facts. A module claim
never becomes a daemon-attested fact.

Running-image evidence has three platform paths:

* Linux opens `/proc/<pid>/exe` and the captured spawn path, then compares SHA-256
  digests. Open handles make path replacement visible as a mismatch.
* macOS compares the spawn-time device/inode with the current path's device/inode.
  This is comparison-only and weaker than a hash. That is deliberate.
* Other platforms return typed `unavailable { reason: "unsupported_platform" }`, never
  a placeholder digest.

The first Linux observation is a cold read of both executable files. A process-local
cache retains up to 64 file identities (device, inode, size, and modification time);
when it reaches 64 entries it is cleared.

`ck provenance <module>` prints source labels in human output. `ck --json provenance
<module>` prints the typed response without merging or relabeling fields. Non-printable
bytes in a malformed declared value are escaped in diagnostics rather than emitted
raw. This surface deliberately excludes `origin_delta`, `buildable_at_head`, deploy,
git, and network logic.

#[derive(Serialize, Deserialize)] #[serde(tag = "op")]   // NOT deny_unknown_fields
pub enum ModuleControlPush {
    #[serde(rename = "route.status")] RouteStatus { route_channel: u16, status: String }, // opaque, cached verbatim
    // route teardown is the GOODBYE FRAME (§2), NOT a JSON op — in BOTH directions.
}
#[derive(Serialize, Deserialize)] #[serde(tag = "op")]
pub enum ModuleControlResponse {
    #[serde(rename = "route.bind")] RouteBindAck {},  // ACK-ONLY success; ALL rejections go on the FrameType::Error lane
}

// subc → module
pub struct ModuleHelloAckBody {
    pub negotiated_ver: u8,
    pub subc_ops: Vec<String>,          // precise op allowlist subc offers this module
    pub subc_capabilities: Vec<String>, // coarse capability flags
    // NO `channels` — route channels are allocated at route.bind, never at HELLO.
}
#[derive(Serialize, Deserialize)] #[serde(tag = "op")]
pub enum ModuleControlRequest {
    #[serde(rename = "route.bind")] RouteBind { route_channel: u16, target: RouteTarget,
                                                identity: BindIdentity, #[serde(default)] config: Vec<ConfigTier>,
                                                #[serde(default, skip_serializing_if = "Option::is_none")]
                                                principal: Option<Principal> },
}
```

## 6. Multi-provider routing model + reserved subc→client direction
- subc replaces the single `active_module: Option<...>` slot with a REGISTRY keyed by `module_id` (+ role/surface). `route.open { target }` resolves `target.module_id` to that module's connection, allocates a route channel, relays `route.bind`, commits on `RouteBindAck` (rejection → Error lane).
- **route_channel uniqueness is per CLIENT CONNECTION across ALL providers** (the union of all open routes on that connection), NOT per-endpoint. The allocator scans the client's `(ConnectionId, *)` space + reservations. `route_limit` exhaustion is per-client. No channel reuse on an endpoint until reconnect/generation reset. (Conformance test: one client opens routes to two providers → assert distinct channels.)
- A client learns valid targets from `catalog.list`, then opens routes explicitly. subc validates `target.module_id` exists + has the required role (per the §3 table); else `unknown_module` / `target_unavailable`.
- Role-aware registration (merged) generalizes: only modules whose manifest declares a routable role enter the routable set; consumer-only modules (the MCP gateway) register for supervision/liveness only.
- **`BindIdentity` is per-route context, NOT a routing key** (the key is `target.module_id`). subc does NOT dedup identity; the same triple may open N routes (e.g. one project → AFT + MC). Any per-identity uniqueness is module-enforced.
- **subc OWNS `project_root` canonicalization at the `route.open`→`route.bind` boundary** (AFT pin #2). subc canonicalizes `BindIdentity.project_root` via `cortexkit-paths` BEFORE emitting `route.bind`; the module consumes it as-is and MUST NOT re-canonicalize (the shared crate makes both sides' `ProjectRootId` byte-identical, but single-ownership prevents a double-canonicalize divergence on symlinked/case-folded paths). `harness`/`session` are passed through verbatim.
- **Principal extension:** `route.open.consumer_identity` is optional. When absent, subc stamps `RouteBind.principal = {"kind":"direct"}`. When present, subc verifies `module_id` + `launch_nonce` against the supervisor's current spawn-nonce state and stamps `{"kind":"reserved","module_id":...}` only on a match. Unknown module ids, empty nonces, and mismatches reject `route.open` with `bad_consumer_identity`; subc never downgrades a failed reserved claim to direct. `{"kind":"unverified"}` is reserved vocabulary and is not emitted today.
- **subc does NO `RouteStatus` fan-out** (AFT pin #5). When a module has N routes open for one project, the MODULE emits `route.status` per `route_channel` itself; subc only caches per route_channel (it never replicates one status across routes).
- v1 ships AFT as the single registered tool-provider; the registry is exercised in tests against a SECOND stub provider so multi-provider is proven, not hypothetical.
- **Daemon-originated-only channel-0 pushes.** `ClientControlPush` is emitted only by subc directly to client connection sinks. Modules have no module→client push relay and cannot express this enum on their control channel, so this direction cannot be forged through a module. **Clients MUST ignore unrecognized channel-0 Push ops (never error)**; this keeps future pushes additive.
  - **How to obey that without swallowing corruption.** Op enums stay closed — no catch-all variant — because an unknown op and a MALFORMED body for a KNOWN op decode alike under a fallback, and those two warrant opposite responses. Decode in two steps, mirroring what subc already does for `ModuleControlPush` (`is_known_module_push_op`): on a decode failure, read just the `op` string; if it is one this peer knows, the body is malformed and that is a real error worth surfacing; if it is not, ignore the frame. Unknown-op and malformed-known-op stay distinguishable.
  - **The line is different for string-valued diagnostic enums nested in list responses.** A receiver does not dispatch on a diagnostic reason, disposition, or health value; it stores or renders it. There is no second failure mode to conflate, and rejecting one new value discards the entire response. These enums therefore use a retained-string fallback (`Unknown(String)`), so the value remains distinguishable and legible rather than being discarded. This is measured, not hypothetical: an older client failed to decode all three modules' `supervisor.provenance` when one module carried the new `process_identity_unconfirmed` reason, while the control response with a known reason decoded cleanly.
  - **Tagged diagnostic enums use the same tolerance with recursive object retention.** The six nested diagnostic enums (`ModuleDeclaredProvenance`, `RunningImageAgreement`, `RunningImageEvidence`, `SupervisorRouteConsumer`, `StderrCaptureState`, and `StderrTailEntry`) decode an unknown tag as `Unknown { tag: String, body: OrderedJsonObject }`. `body` retains the complete object, including the discriminator, as recursively ordered pairs captured directly from the deserializer stream; serialization is byte-faithful at every nesting depth and for any member order without enabling a serde_json map-order feature. A malformed body (non-object or missing a string discriminator) still fails; this fallback is for unknown tags, not damaged data. Op enums remain closed and are not reopened by this rule.
  - **A semantic `unknown`, `other`, or `unspecified` member makes a poor fallback candidate.** `Unknown(String)` then collides with an existing state that means something materially different: an asserted value is not determined versus a decoder does not recognise the value. `SupervisorHealthStatus` is the live example; it keeps its semantic `unknown` member closed rather than introducing that ambiguity. Such an enum needs a separately named fallback or no fallback until its contract is ruled.
- **Route lifecycle pushes are ephemeral.** `route.closing` and `route.closed` are emitted from teardown state and retained nowhere; a client that misses one is in the same position as before this family existed. A crash has no drain: subc emits `route.closed { reason: crash, drained: false, abandoned: 0, terminal }` with no preceding `route.closing`. That asymmetry is normative: a `route.closed` without `route.closing` means nobody planned the closure. The claim covers daemon-owned recovery only. For an unsupervised/self-connecting module subc reports `terminal:false`: it owns no respawn policy and cannot claim that an external process will not reconnect. Router cleanup and supervisor exit handling are independent tasks, so the daemon uses the shared recovery policy for an undecided snapshot and quotes an already-recorded `Restarting` decision as non-terminal or `Failed`/`Disabled` decision as terminal; it does not promise that the restart decision always happens later.
- **Enqueue-order is the ONLY claim.** `FrameSink::send` enqueues; it does not complete the write. So “pushed before the GOODBYE” can only ever mean “enqueued before”.

## 7. Thin-core ops table
**subc UNDERSTANDS (closed set):** `hello`/`hello_ack`; `PING`/`PONG`/`GOODBYE`/`CANCEL` (frames); `server.describe`; `catalog.list`; `route.open`→`route.bind`; `route.poll`(status|liveness); daemon `route.closing`/`route.closed` pushes; `supervisor.*`; (future) `config.*` as RAW tier get/put/changed transport ONLY; module `route.status` push (opaque string, cached verbatim). (`lease.*` retired 2026-08-14 with `scheduler.*`: cross-module single-writer arbitration ships at the store layer via `cortexkit-lease` — advisory lock + persisted epoch CAS per store — so a daemon lease op family would duplicate an invariant the storage substrate already enforces.)

**`route.poll` is answered LOCALLY ONLY — NEVER forwarded to the module (AFT pin #1, load-bearing):** subc caches the latest `route.status` push per `route_channel` verbatim and answers `route.poll{kind:status}` from that cache; `route.poll{kind:liveness}` is answered from subc's own supervision state. **A `route.poll` MUST produce ZERO frames to the module** — synchronous forwarding of a poll to the module is forbidden. (This is why the module PUSHes `route.status`: to stay off the synchronous poll path. AFT issue #117 was a passive status poll that hit the bridge mid-scan and tripped a hang-restart; this rule prevents that class by construction.)
**OPAQUE-FORWARDED on route channels (open set, subc never parses):** all MCP/tool-call semantics; `llm.complete` + selection objectives; management operation BODIES (`memory.*`, dashboard reads — subc resolves the provider, body opaque); `bus.subscribe`/`publish`/`invite`/DM/fan-out; embedding ops + vector streams; federation/WAN payloads; generic-router policy; pipeline transforms.
**Guardrail (P4):** a conformance test stubs MC/embedding/LLM-runner/bus/federation and asserts each integrates with ZERO new `FrameType` and ZERO new subc-understood op. Unknown control op → `unknown_control_op` (never silent). subc owns the config STORE substrate + raw-tier transport, NEVER the config CRUD OPERATION (`memory.list`/`config.upsert` bodies are module RPC).

## 8. Errors, capability negotiation, versioning
- **Errors:** `ErrorBody { code, message }` on `FrameType::Error`; channel+corr identify the failing request (corr is unique per route channel). v1 code set: `unknown_control_op`, `invalid_control_body`, `op_not_allowed`, `unknown_channel`, `stale_route_epoch`, `unknown_module`, `target_unavailable`, `bad_consumer_identity`, `module_reloading`, `reload_failed`, `route_limit`, `config_divergence`, `version_unsupported`, `duplicate_module_id`, `invalid_hello`, `invalid_manifest`. State machine: `unknown_module` = no registration/supervised module with that id; `target_unavailable` = registered but down/restarting/disabled/wrong-role; `bad_consumer_identity` = route.open presented a consumer_identity whose module_id is unknown to the supervisor's spawn-nonce table, whose nonce is empty, stale, or otherwise does not match; `module_reloading` = reload drain gate is rejecting new route.open / route REQUEST admission; `reload_failed` = supervised reload drained the old generation but the replacement could not register. `stale_route_epoch` = a client or module request was dropped before forwarding because its route handle was released or replaced; the requester must discard that handle before re-establishing the route. `unknown_channel` = a request reached an absent or reserved route handle; the requester must establish a route from scratch before retrying. `invalid_daemon_config` = rescan could not parse or validate the complete config and applied no module mutations; `rescan_unavailable` = the daemon was not booted with a reloadable config path; `rescan_failed` = reconciliation failed after validation.
- **MODULE rejection is relayed VERBATIM to the client (AFT pin #3, normative):** when a module rejects `route.bind` on the Error lane, subc relays that `ErrorBody { code, message }` **verbatim** to the originating client's `route.open` corr — subc MUST NOT synthesize a generic "bind failed" or truncate `message`. Rationale: `config_divergence`'s diff (which RootConfig keys conflict, active-vs-incoming) rides in `ErrorBody.message`; the user needs it intact to fix their `aft.jsonc`. This applies to ANY module Error-lane rejection of a relayed request, not just attach.
- **Capability negotiation:** module direction is BIDIRECTIONAL — module HELLO carries `control_ops: Option<...>` (None = legacy BASELINE only), subc HELLO_ACK carries `subc_ops` (precise allowlist) + `subc_capabilities` (coarse). Client direction is DISCOVERY-ONLY via `server.describe` (no client HELLO). Known-but-ungranted module op → `op_not_allowed`.
- **The BASELINE control op set (what `control_ops: None` grants a module — AFT pin #4, enumerated):** RECEIVE `route.bind` + GOODBYE (route teardown); EMIT `RouteBindAck` / Error-lane rejection (responses), `route.status` push, and GOODBYE. I.e. a module that declares `None` can fully participate in routing + status + teardown without enumerating anything. `control_ops: Some([...])` is only needed to opt INTO ops ADDED later (e.g. a future `config.*` op that a module wants to receive). **Pushes are always accepted (ignored-if-unknown), so `route.status` emission is NOT gated by `subc_ops`** — a baseline-only v1 module may ignore `subc_ops` entirely (AFT pin #5b).
- **Versioning:** NO per-body version field. Envelope `ver` (negotiated at HELLO) for binary/structural; additive `op` values (new variant + new dotted string; old peers reject unknown with a typed error, EXCEPT pushes which are ignored); reserved op PREFIXES (`server.* catalog.* route.* supervisor.* config.*`). Breaking body change = new op name, capability-gated.

## 9. Behavior the contract pins (no wire break — implement per this text)
- **Module death/disable → client notification + subc cleanup:** subc translates module loss/disable into `route.closed` as specified in §6, then a route `GOODBYE` to each affected client, and runs the route-release path on its own forwarding state.
- **GOODBYE-on-route drain:** per §2.1 (terminal, idempotent, credit release, late-frame drop, pending-corr failure).
- **supervisor.restart/set_enabled side-effects:** restart bumps generation → already-open routes to that module go stale → liveness reports false → subc GOODBYEs the affected client routes. `SupervisorAck.applied` = true (action taken) / false (no-op / already in state); not-found → `unknown_module`.
- **supervisor.reload drain-to-quiescence hot-swap:** `supervisor.reload { module_id }` is distinct from restart. subc first marks the current module endpoint reloading in the forwarding table (before checking quiescence), rejects new `route.open` and new route `REQUEST`s with retryable `module_reloading`, lets already-admitted requests drain by header-level `ChannelFlow` credit accounting, then route-GOODBYEs clients, sends channel-0 GOODBYE to the old module, respawns `spec.program`, and returns `supervisor.ack` only after the replacement registers. If the replacement cannot spawn/register, the request returns `reload_failed`; the old generation is not overlapped or rolled back, and only replacement failures consume restart-policy crash budget.
- **supervisor.rescan module-set reconcile:** `supervisor.rescan {}` re-reads the config path selected at daemon boot. Parse/schema/reserved-prefix validation completes before any mutation. Added modules use the boot supervision path (including launch nonce, storage descriptor, and health settings); removed modules drain with route/module GOODBYEs and are retired from supervisor state; changed launch specs are stored for the next reload while enabled flips apply immediately. Existing unchanged modules are not restarted or re-registered. Non-module config changes require a daemon restart. Rescan queues behind restart/reload/set_enabled and other rescans under one daemon-wide operation lock.
- **Ordering/readiness:** `route.open` for a module that hasn't finished HELLO → `target_unavailable`; `catalog.list` before any module registers → empty list (+ generation). Two clients racing `route.open` to one provider → independent routes (no identity dedup, §6).

## 10. Migration from current (renames) + co-sign surface
Current → new:
- `AttachRequest`(client) → `ClientControlRequest::RouteOpen` (+ explicit `RouteTarget`); `AttachAck` → `ClientControlResponse::RouteOpen`. MOVE to `subc-control`.
- `PassivePoll` → `route.poll` (`PollOp.op` field renamed → `kind`); `StatusReply`/`LivenessReply` → `ClientControlResponse::RoutePoll`. MOVE to `subc-control`. Drop `deny_unknown_fields` from these.
- `manifest_list`(planned) → `catalog.list` (+ `generation`).
- `AttachRelay` → `ModuleControlRequest::RouteBind` (+ `RouteTarget`); `AttachRelayResponse` → `ModuleControlResponse::RouteBindAck` (ack-only); `DetachRelay` → DELETED (route teardown = GOODBYE frame both ways); `StatusUpdate` → `ModuleControlPush::RouteStatus`. STAY in `subc-protocol`.
- `Hello` gains `control_ops: Option<...>`; `HelloAck` drops `channels`, gains `subc_ops`. STAY in `subc-protocol`.
- `subc-protocol` 0.1.0 → **0.2.0** (republish; AFT re-pins).
- **AFT CO-SIGN SURFACE (all AFT consumes):** `ModuleHelloBody{+control_ops}`/`ModuleHelloAckBody{subc_ops, no channels}`, `ModuleControlRequest::RouteBind{route_channel,target,identity,config}`, `ModuleControlResponse::RouteBindAck{}` (rejects on Error lane), `ModuleControlPush::RouteStatus`, route teardown via GOODBYE frame, and shared primitives (`RouteTarget` [no BusSurface], `BindIdentity`, `ConfigTier`, `ErrorBody`, `ModuleManifest`). AFT is unaffected by everything in `subc-control` and is pre-consumption (hasn't wired attach) — clean co-sign, not a break.

## 11. Phases (all four, one coordinated change)
- **P1 — crate ownership + vocab.** Create `subc-control`; move `Hello*`/`StatusUpdate` into `subc-protocol`; define shared primitives (`BindIdentity`, `RouteTarget` [no BusSurface], mapping table); reserve dotted-op prefixes; rename `PollOp.op`→`kind`; drop `deny_unknown_fields` from response/push structs. No behavior change.
- **P2 — kill the cascade.** Implement the direction-split tagged enums; replace the `PassivePoll`-then-`AttachRequest` cascade with one match per direction; tag responses+pushes; `route.open{RouteTarget}`; `catalog.list{generation}`; ack-only `route.bind`; standardize the §8 error codes; reserve the subc→client Push direction (clients ignore unknown push ops); update subc-mcp + fake-aft-stub + tests; delete the obsolete unambiguity test.
- **P3 — multi-provider registry.** Replace `active_module` with the keyed registry; per-client-connection channel uniqueness; `route.open{target}` resolves explicitly; module-death → client GOODBYE + cleanup; prove against a 2nd stub provider.
- **P4 — capability gating + closure.** `control_ops`/`subc_ops` negotiation + simple allowlist enforcement (`op_not_allowed`); `supervisor.*` + side-effects (§9); conformance test stubbing MC/embedding/LLM-runner/bus/federation (zero new FrameType / zero new subc op); TS/Rust golden-JSON drift vectors; fold in the #287 test-setup de-dup helper.
