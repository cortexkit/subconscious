# subc channel-0 control protocol (v0.4 — AFT co-signed)

Status: AFT co-signed the module-facing surface (pm_251e0356) — module-facing wire LOCKED. subc-control (client side) free to evolve pre-release.
Supersedes: the ad-hoc channel-0 shapes in `subc-protocol::session` + `subc-core::status`.
Origin: councils `bg_3c0936ae` (shape) + `bg_7049add2` (adversarial gap audit) + AFT co-sign review + the pre-release "cleanest-from-scratch" principle.
v0.2→v0.3: folded all 9 wire-shape must-fixes from the gap audit.
v0.3→v0.4 (AFT co-sign pins, all behavior/text — NO wire change): (1) module route.bind rejection on the Error lane is relayed VERBATIM to the client's route.open corr (config_divergence diff must reach the user intact); (2) route.poll is answered ONLY from subc's cache/supervision — NEVER forwarded to the module (zero frames to module — AFT issue #117 hang-restart guard); (3) subc OWNS project_root canonicalization at the route.open→route.bind boundary; the module consumes it as-is and never re-canonicalizes; (4) the baseline `control_ops` set (when None) is enumerated; (5) subc does no RouteStatus fan-out (module emits per route_channel) and a baseline-only v1 module may ignore subc_ops.

This is the COMPLETE 4-phase wire contract (client↔subc + module↔subc). It is designed so every foreseeable module (MC, embedding pipeline, LLM-runner/dreamer, Alfonso bus, InterSUBC federation, generic router) integrates with NO new `FrameType` and NO new subc-understood control op.

---

## 0. Principles (binding)
- **Thin core.** subc understands ONLY: routing, route open/close, catalog, poll(status/liveness), lifecycle, supervision, lease/scheduler, and RAW config-tier transport. Everything that interprets module/business semantics is OPAQUE and rides a route channel. Test: *if the op changes subc's routing/lifecycle/resource state, subc understands it; if it changes module-owned domain state, it's opaque route-channel RPC.*
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
  - `ClientControlRequest` / `ClientControlResponse`. (`ClientControlPush` direction RESERVED — see §6, not defined in v1.)
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
    #[serde(rename = "supervisor.set_enabled")] SupervisorSetEnabled { module_id: String, enabled: bool },
    // FUTURE (additive): scheduler.*, config.* (raw-tier get/put/changed), watch.* (subc-owned events)
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
}

#[serde(rename_all="snake_case")] pub enum PollKind { Status, Liveness }
pub struct ConsumerIdentity { pub module_id: String, pub launch_nonce: String }
pub struct CatalogEntry { pub module_id: String, pub roles: Vec<ProviderRole>, pub control_ops: Vec<String> }
pub struct SupervisorEntry { pub module_id: String, pub state: String, pub enabled: bool, pub live: bool }
// Route teardown = GOODBYE frame (§2). Errors = ErrorBody on FrameType::Error (§8), correlated by channel+corr.
```
Notes:
- `catalog.list` SUBSUMES `manifest_list`; returns modules + roles + per-module control ops; client filters by plane/role itself (subc owns no role→plane mapping). The `generation` counter lets a future `catalog.changed` push reconcile missed events with no reshape.
- No client HELLO in v1 — `server.describe` is the client's discovery handshake. Client capability negotiation is discovery-only; `op_not_allowed` enforcement applies to MODULE ops in v1.

## 5. Module ↔ subc wire (`subc-protocol`) — the AFT co-sign surface
```rust
// module → subc
pub struct ModuleHelloBody {
    pub manifest: ModuleManifest,
    pub protocol_ver: u8,
    pub control_ops: Option<Vec<String>>,  // None = legacy baseline ONLY (never "all"); Some([])=no optional; Some([..])=exactly those
}
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
- **RESERVED: the subc→client Push direction.** v1 defines NO `ClientControlPush` enum, but the direction is reserved: **clients MUST ignore unrecognized channel-0 Push ops (never error)**. This makes `route.*`/`catalog.*`/`watch.*` client pushes (e.g. `catalog.changed{generation}`) additive-later with no re-bump. **Route death is already expressed by GOODBYE:** on module crash/disable, subc emits a route `GOODBYE` to each affected client AND tears down its own forwarding state (this BEHAVIOR is required in v1; the new push family is not).

## 7. Thin-core ops table
**subc UNDERSTANDS (closed set):** `hello`/`hello_ack`; `PING`/`PONG`/`GOODBYE`/`CANCEL` (frames); `server.describe`; `catalog.list`; `route.open`→`route.bind`; `route.poll`(status|liveness); `supervisor.*`; (future) `scheduler.*`/`lease.*`, `config.*` as RAW tier get/put/changed transport ONLY, `watch.*` for subc-owned events; module `route.status` push (opaque string, cached verbatim).

**`route.poll` is answered LOCALLY ONLY — NEVER forwarded to the module (AFT pin #1, load-bearing):** subc caches the latest `route.status` push per `route_channel` verbatim and answers `route.poll{kind:status}` from that cache; `route.poll{kind:liveness}` is answered from subc's own supervision state. **A `route.poll` MUST produce ZERO frames to the module** — synchronous forwarding of a poll to the module is forbidden. (This is why the module PUSHes `route.status`: to stay off the synchronous poll path. AFT issue #117 was a passive status poll that hit the bridge mid-scan and tripped a hang-restart; this rule prevents that class by construction.)
**OPAQUE-FORWARDED on route channels (open set, subc never parses):** all MCP/tool-call semantics; `llm.complete` + selection objectives; management operation BODIES (`memory.*`, dashboard reads — subc resolves the provider, body opaque); `bus.subscribe`/`publish`/`invite`/DM/fan-out; embedding ops + vector streams; federation/WAN payloads; generic-router policy; pipeline transforms.
**Guardrail (P4):** a conformance test stubs MC/embedding/LLM-runner/bus/federation and asserts each integrates with ZERO new `FrameType` and ZERO new subc-understood op. Unknown control op → `unknown_control_op` (never silent). subc owns the config STORE substrate + raw-tier transport, NEVER the config CRUD OPERATION (`memory.list`/`config.upsert` bodies are module RPC).

## 8. Errors, capability negotiation, versioning
- **Errors:** `ErrorBody { code, message }` on `FrameType::Error`; channel+corr identify the failing request (corr is unique per route channel). v1 code set: `unknown_control_op`, `invalid_control_body`, `op_not_allowed`, `unknown_channel`, `unknown_module`, `target_unavailable`, `bad_consumer_identity`, `module_reloading`, `reload_failed`, `route_limit`, `config_divergence`, `version_unsupported`, `duplicate_module_id`, `invalid_hello`, `invalid_manifest`. State machine: `unknown_module` = no registration/supervised module with that id; `target_unavailable` = registered but down/restarting/disabled/wrong-role; `bad_consumer_identity` = route.open presented a consumer_identity whose module_id is unknown to the supervisor's spawn-nonce table, whose nonce is empty, stale, or otherwise does not match; `module_reloading` = reload drain gate is rejecting new route.open / route REQUEST admission; `reload_failed` = supervised reload drained the old generation but the replacement could not register.
- **MODULE rejection is relayed VERBATIM to the client (AFT pin #3, normative):** when a module rejects `route.bind` on the Error lane, subc relays that `ErrorBody { code, message }` **verbatim** to the originating client's `route.open` corr — subc MUST NOT synthesize a generic "bind failed" or truncate `message`. Rationale: `config_divergence`'s diff (which RootConfig keys conflict, active-vs-incoming) rides in `ErrorBody.message`; the user needs it intact to fix their `aft.jsonc`. This applies to ANY module Error-lane rejection of a relayed request, not just attach.
- **Capability negotiation:** module direction is BIDIRECTIONAL — module HELLO carries `control_ops: Option<...>` (None = legacy BASELINE only), subc HELLO_ACK carries `subc_ops` (precise allowlist) + `subc_capabilities` (coarse). Client direction is DISCOVERY-ONLY via `server.describe` (no client HELLO). Known-but-ungranted module op → `op_not_allowed`.
- **The BASELINE control op set (what `control_ops: None` grants a module — AFT pin #4, enumerated):** RECEIVE `route.bind` + GOODBYE (route teardown); EMIT `RouteBindAck` / Error-lane rejection (responses), `route.status` push, and GOODBYE. I.e. a module that declares `None` can fully participate in routing + status + teardown without enumerating anything. `control_ops: Some([...])` is only needed to opt INTO ops ADDED later (e.g. a future `scheduler.*`/`lease.*` a module wants to receive). **Pushes are always accepted (ignored-if-unknown), so `route.status` emission is NOT gated by `subc_ops`** — a baseline-only v1 module may ignore `subc_ops` entirely (AFT pin #5b).
- **Versioning:** NO per-body version field. Envelope `ver` (negotiated at HELLO) for binary/structural; additive `op` values (new variant + new dotted string; old peers reject unknown with a typed error, EXCEPT pushes which are ignored); reserved op PREFIXES (`server.* catalog.* route.* supervisor.* scheduler.* config.* watch.*`). Breaking body change = new op name, capability-gated.

## 9. Behavior the contract pins (no wire break — implement per this text)
- **Module death/disable → client notification + subc cleanup:** subc translates module loss/disable into a route `GOODBYE` to each affected client and runs the route-release path on its own forwarding state (fixes the current bug where `cleanup_connection` returns empty on module loss and bindings go stale).
- **GOODBYE-on-route drain:** per §2.1 (terminal, idempotent, credit release, late-frame drop, pending-corr failure).
- **supervisor.restart/set_enabled side-effects:** restart bumps generation → already-open routes to that module go stale → liveness reports false → subc GOODBYEs the affected client routes. `SupervisorAck.applied` = true (action taken) / false (no-op / already in state); not-found → `unknown_module`.
- **supervisor.reload drain-to-quiescence hot-swap:** `supervisor.reload { module_id }` is distinct from restart. subc first marks the current module endpoint reloading in the forwarding table (before checking quiescence), rejects new `route.open` and new route `REQUEST`s with retryable `module_reloading`, lets already-admitted requests drain by header-level `ChannelFlow` credit accounting, then route-GOODBYEs clients, sends channel-0 GOODBYE to the old module, respawns `spec.program`, and returns `supervisor.ack` only after the replacement registers. If the replacement cannot spawn/register, the request returns `reload_failed`; the old generation is not overlapped or rolled back, and only replacement failures consume restart-policy crash budget.
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
