# Subconscious Core — Architecture & Module-Integration Contract

**Status:** drafting (living document)
**Date:** 2026-06-18
**Relationship to the handoff:** `subconscious-design-handoff.md` is the decision history and rationale. This document operationalizes it into a buildable contract — specifically the part that makes the project's north star true.

---

## 1. North star and the design test

**One daemon that supervises and coordinates separate-process modules.** The design goal is not a feature list; it is a property:

> **Any foreseeable module integrates without a big core change.**

That property is only achievable if the contract between subc-core and a module is **role-based** and the set of roles is **closed**. We make closure *structural* rather than enumerated: a module's roles are **per-plane provider/consumer participations**, and the set of planes is what is closed. A new module is then a new bundle of already-known participations — registration, not core surgery. The only thing that can force a new core surface is a genuinely new **plane** (the far-future message bus), and we keep the protocol body transport-agnostic so even that is additive.

---

## 2. Thin core — what subc-core owns

subc-core is a **splice-router with services around it**. It moves opaque bytes between endpoints by header, and deserializes only bodies addressed to itself.

**Owns:**
- transport · routing · **multiplexing** · lifecycle · supervision · **hot-swap**
- the **hook-chain orchestrator** (runs an ordered pipeline; owns no stage's logic)
- **scheduler + one lease per project** across all modules
- **vault** + audited secret grants
- **storage substrate** (path-identity + backend-swappable store; modules own their schemas)
- **identity** resolution (session + project)
- **PUSH / event** forwarding
- **bulk-lane** flow control (intra-mesh binary, e.g. embedding vectors)
- **trust tiers + conformance-gate** enforcement

**Owns no module business logic.** It never parses a tool's semantics, never classifies a request as mutating, never knows what a codec does. Those live in the modules. This is the property that lets AFT patch daily without a daemon release, and lets an OSS module plug in without core changes.

---

### 2.1 Module cardinality (supervision)

Every v1 module — AFT, MC transform, embedding engine, codec, auth, LLM-runner — is a **singleton**: one instance per (per-user) machine, supervised as one long-lived process. subc routes traffic to a module by module-kind + channel, never to a per-root instance.

- **AFT is a singleton too** (decided 2026-06, authoritative). One `aft` process total; AFT self-demuxes by `project_root` internally via an in-process **ProjectActor map** keyed by `ProjectRootId`. Per-root idle-evict is AFT-internal (today's 30-min idle-bridge policy lifted inside AFT). subc resolves the canonical `ProjectRootId` at **session attach** (the `route.open` boundary, §4.2) — needed for the per-project **lease**, the **scheduler**, and other modules (MC renders project memories, etc.) — but does **not** use it to select an AFT instance.
- **Why one process, not a per-root pool:** operability (one PID, one log stream — users reason about a single process) + **upgrade atomicity** (the process is the upgrade unit; all roots must run the same AFT version — a per-root pool would force an N-process version barrier to get the consistency you want anyway; "independent per-root hot-update" is a mis-feature, not a benefit). RAM is not a factor — the embedding pipeline moves to the singleton embedding module regardless.
- **Accepted cost:** crash blast-radius — a panic serving one root takes the process down for all roots; mitigated by AFT's panic-hardening + subc's auto-restart. Acceptable for a local tool.
- **`per-project-pool` cardinality is NOT built** — no current or foreseeable module needs it. It is an additive supervision capability if a future module ever requires it, not a v1 surface.

---

## 3. The planes

| Plane | Providers | Consumers | v1? |
|---|---|---|---|
| **Tool** | tool-providers (AFT; MC `ctx_*`) | harnesses/agents; module-clients | **yes** |
| **Proxy / LLM** | pipeline-stages (codec, auth, transform/MC) | harnesses via MITM; the LLM-runner | later |
| **Mgmt** | management-surfaces — MC (`context.db` state), **per-harness session-store** (raw transcript / session-meta / token-usage), AFT config/status | CortexKit app; CLI; human | near-term |
| _Message bus_ | _(agents)_ | _(agents)_ | _far-future (Takım)_ |

Cross-cutting, plane-independent core services (scheduler+lease, vault, storage, identity, supervision, bulk lane, PUSH) serve every plane.

---

## 4. The module-integration contract

This is the spine. Everything else hangs off it.

### 4.1 Actors and connections

Two kinds of actor open a connection to subc:

- **Clients** consume: harnesses/agents (tool plane), the CK app/CLI/human (mgmt plane), the LLM-runner (proxy + tool planes).
- **Modules** provide one or more roles, and may *also* be clients (bidirectional).

One connection **multiplexes** many logical channels. A channel is the per-frame route handle (u16); it is **bound once, at `route.open`** (§4.2), to a `(target provider, BindIdentity{project_root, harness, session})` pair, and never re-sent per frame. One client connection can hold **many** route channels — one per `(project × provider)` — so the MCP gateway fans a single connection out to AFT + MC + future providers; channel numbers are unique **per client connection** across all its providers (§4.9). Correlation ids carry many in-flight requests on one socket — this is what makes head-of-line blocking impossible at the transport layer, and lets subc answer liveness/status from its own cache without forwarding.

> **The channel-0 control protocol is specified in full in [`docs/subc-control-protocol.md`](./subc-control-protocol.md) (v0.4, AFT co-signed).** This section gives the model; that doc is the authoritative wire contract (direction-split tagged enums, dotted ops, the `subc-control` client crate vs `subc-protocol` module crate split).

### 4.2 HELLO handshake

There are **two distinct channel-0 handshakes** (do not conflate them):

**(1) Module registration** — a *module* (e.g. AFT) connects and registers its **manifest** (§4.3). Implemented; body shapes are the `subc-protocol` module-facing types ([control-protocol §5](./subc-control-protocol.md)):
```jsonc
HELLO     { "manifest": { /* ModuleManifest, §4.3 */ }, "protocol_ver": 1,
            "control_ops": null }  // Option<[String]>: null = full baseline (recv route.bind+GOODBYE, emit RouteBindAck/Error + route.status + GOODBYE); Some([...]) opts into ops added later
HELLO_ACK { "negotiated_ver": u8, "subc_ops": ["..."], "subc_capabilities": ["..."] }
```
No channel is allocated at HELLO — route channels exist only after `route.bind` (§4.9). Version below `MIN_SUPPORTED_VERSION` → a channel-0 `ERROR { code, message }` (the unified error body, §4.8), not registered. Duplicate active `module_id` is rejected (route-hijack guard); stale registrations are released on GOODBYE or connection-drop. A HELLO whose manifest declares **no routable provider role** registers for supervision/liveness only and never becomes a forwarding target (a subc-supervised *consumer* — e.g. the MCP gateway — cannot hijack a provider's slot).

**Reserved-id ceremony (two-sided, and the module half is invisible in its own repo).** A security-boundary module — the credential vault (`claustrum`), the federation courier (`callosum`) — must not be impersonable by any other local process that completes the loopback HMAC handshake. Protection requires **both halves**, and getting either alone is silently wrong:
- **Module half:** the module echoes the supervisor's injected `SUBC_LAUNCH_NONCE` in its HELLO (`launch_nonce`). This compiles, runs, and registers correctly *whether or not the daemon marks the id reserved* — nothing observable in the module distinguishes the protected case from the unprotected one, so **a new module gets this wrong by default and cannot detect it**.
- **Daemon half:** the module's `subc.jsonc` block carries `"reserved": true`. Only then does the daemon record an expected nonce and refuse any HELLO claiming that id without it (`reserved_module` error), upstream of the registry's duplicate-id path — so an impostor can never even contend for the live slot. Prefix ownership (`reserved_prefixes`, e.g. `fed:`) authorizes claims under a namespace by the owner's spawn nonce.

The daemon half is the load-bearing one: the nonce echo without the config flag is a key with no lock installed. When adding a supervised module that owns a security boundary, set `reserved: true` at onboarding and verify with a live impostor HELLO (claim the id with no/forged nonce; expect `reserved_module`) — re-registering cleanly proves only that you did not break yourself, never that the gate refuses anyone.

**Why a green e2e suite is not evidence the flag is deployed.** Claustrum ran in production for fourteen months WITHOUT `reserved: true` while its real-daemon e2e harness wrote `reserved: true` into its OWN generated `subc.jsonc` from day one — seven tests exercising the ceremony correctly on every CI run, the whole time production was unprotected. The suite proves the two halves work together WHEN BOTH ARE PRESENT, which is exactly what a passing run means; it reads no deployed config, and should not. The general trap (CKCRED's framing, worth its own line): **a harness that constructs its own correct environment proves the MECHANISM and says nothing about the INSTALLED one — the more faithfully it reproduces production, the more convincingly it stands in for a check nobody ran.** So the deployment check is the live impostor HELLO against the running daemon, never the suite.

**(2) Session attach = `route.open`** — a *client* (harness session, or the MCP gateway) opens a route to a specific provider. It carries an explicit `RouteTarget` + `BindIdentity` + config; subc canonicalizes `project_root` (via `cortexkit-paths`, at this boundary — single-owner, §4.9), validates the target against the registry, then **relays `route.bind` to the module — a vetoed request/response, not a local ack** (the module owns config reconciliation, §4.7):
```jsonc
// client → subc  (subc-control::ClientControlRequest)
{ "op": "route.open",
  "target":   { "kind": "tool_provider"|"management_surface"|"internal_service", "module_id": "aft" /*, "service_id" for internal_service */ },
  "identity": { "project_root": "/abs/path", "harness": "opencode"|"pi"|"mcp:claude-code"|..., "session": "ses_..." },
  "config":   [ /* opaque, ordered provenance-tagged tiers, §4.7 */ ] }
// subc → module  (subc-protocol::ModuleControlRequest::RouteBind{ route_channel, target, identity, config })
// module → subc  RouteBindAck{}  (ack-only; rejection rides the FrameType::Error lane)
```
- **Accept** (`RouteBindAck`) → subc binds a **route channel** and replies `route.open → { route_channel }`. The channel is now the per-frame handle (§4.1).
- **Reject** → the module sends an `ErrorBody{code,message}` on the `FrameType::Error` lane; subc relays it **verbatim** to the client's `route.open` corr (e.g. `config_divergence` carries its key-diff intact so the user can fix `aft.jsonc`). subc binds nothing.
- **Resolution errors** (subc-side): `unknown_module` (no such id) / `target_unavailable` (registered but down/disabled/wrong-role) / `route_limit` (per-client channel exhaustion).

`BindIdentity` is per-route context, **not** a routing key — the same triple may open N routes across providers; subc does not dedup it. Data frames thereafter are `channel + corr + opaque body`. See §4.9 for the multi-provider routing model this feeds.

### 4.3 Capability registration — the module manifest

A module declares everything it participates in. subc validates it, gates it by trust tier, and routes accordingly. The manifest is the whole contract surface:

```jsonc
{
  "module_id": "aft",
  "module_version": "0.39.2",
  "protocol_ver": 1,
  "trust_tier": "first_party",          // first_party | reviewed | untrusted

  "provides":  [ /* provider role objects, §4.4 */ ],
  "consumes":  [ /* client needs, §4.5 */ ],
  "scheduled_tasks": [ /* §4.6 */ ],
  "bindings":  { /* §4.7 */ }
}
```

### 4.4 Provider roles (`provides[]`)

Tagged union on `role`. A module lists as many as it plays.

**tool_provider** (tool plane)
```jsonc
{
  "role": "tool_provider",
  "tools": [ { "name": "read", "mutates": false, "schema": {…} }, … ],
  "identity_scope": ["session", "project"],   // which identity keys route a call
  "concurrency": "serial" | "module_managed" | "stateless_parallel",
  "emits_push": true,                          // subc forwards async PUSH frames
  "sub_supervises": true                       // module spawns child procs (lifecycle nesting)
}
```
- **`concurrency` is the load-bearing field for v1.** It is the module's *declared* contract for how subc may deliver concurrent in-flight calls:
  - `serial` — subc delivers one call at a time (safe default for simple/OSS modules).
  - `module_managed` — subc fans in concurrent calls; the module schedules them safely (AFT's reader-writer executor: non-mutating in parallel, mutating serialized). **This is how the v1 concurrency goal is delivered** — subc mux + the module's executor, together.
  - `stateless_parallel` — subc may parallelize freely (pure-function modules).
- **Correctness stays in the module.** `mutates` is optional metadata for observability/clients; subc never acts on it. subc's only promise is honest delivery per the declared `concurrency`; the module owns the parallel-safety.

**pipeline_stage** (proxy plane)
```jsonc
{
  "role": "pipeline_stage",
  "stage": "transform" | "codec" | "auth",
  "applies_to": { "provider": "anthropic" | "*", "model": "*" },
  "interface": "normalized->normalized",       // transform
  "declares_frozen_floor": true,               // transform declares its frozen message count
  "needs_signals": ["prefix_evicted"],         // the one cache input subc supplies
  "conformance_class": "cache_law_v1"          // codec must pass this gate to be trusted
}
```

**management_surface** (mgmt plane)
```jsonc
{
  "role": "management_surface",
  "operations": [ { "name": "memory.list", "kind": "query" },
                  { "name": "memory.upsert", "kind": "mutate" }, … ],
  "config_schema": { /* JSON-schema of the module's editable config */ },
  "observability": [ { "name": "compartments", "kind": "snapshot" | "stream" }, … ],
  "identity_scope": ["project", "session?"]
}
```
- This is the CK-app contract. The dashboard renders forms from `config_schema`, lists/edits via `operations`, and views state via `observability` — **all served by subc from the module over the mgmt plane, never read from a local db or file.**
- **Dashboard data is _composed_ from multiple mgmt-plane providers, never proxied through one** (established by MC's audit of the current dashboard). The compartment/memory view is ~99% MC's `context.db` (compartments with p1–p4 paraphrase tiers, importance, sequence, ordinals + message_ids; memories; `transform_decisions` cache attribution; key_files). The rest genuinely lives in the harness — raw message transcript (the Messages tab; `context.db` holds only a stripped, text-only FTS index, not the verbatim transcript), session/directory/worktree metadata, and per-step token usage — served by a **per-harness session-store provider** (also a `management_surface`). The CK app composes the two; **no module proxies another's data.** The seam already exists in MC today (`get_session_messages` switches OpenCode-SQL vs Pi-JSONL), which is proof it generalizes. For MITM harnesses later, subc-the-proxy can itself be that session-store, since it sees the transcript on the wire. A harness is therefore a tool-plane _consumer_ and, via this adapter, a mgmt-plane _provider_.

**internal_service** (intra-mesh, never agent-facing)
```jsonc
{
  "role": "internal_service",
  "service_id": "embedding.v2",
  "transport": "bulk",            // raw binary, credit/window flow-controlled
  "agent_facing": false,
  "operations": ["embed", "ann_query", "upsert_vectors"]
}
```

### 4.5 Consumer roles (`consumes[]`)

```jsonc
[
  { "role": "tool_client",    "of": ["aft", "mc"] },                       // LLM-runner calls tools
  { "role": "llm_client",     "via": "proxy", "auth": "cortexkit_native" },// needs completions
  { "role": "service_client", "of": ["embedding.v2"] }                     // AFT consumes embeddings
]
```

### 4.6 Scheduled tasks (`scheduled_tasks[]`)

```jsonc
{
  "task_id": "mc.dreamer",
  "eligibility": { "cooldown": "…", "window": "…" },
  "lease_scope": "project",            // subc enforces ONE lease per project across modules
  "renews_during_calls": true,         // lease renews during long LLM calls
  "toolset": ["read","grep","glob","bash","write","edit","aft_outline","aft_zoom",
              "ctx_memory","ctx_search","ctx_note"],
  "model_policy": { "tier": "cheap", "fallback_chain": ["…"] },
  "step_cap": 150,
  "circuit_breaker": { "identical_failures": 3 }
}
```
- subc-core owns the scheduler + the single project lease; the LLM-runner module *executes* the loop. Today MC and AFT have separate lease tables in separate DBs — centralizing the lease here is what stops two modules dreaming the same project at once. The lease is keyed **per canonical `ProjectRootId` (per-path), not per-repo** — a git worktree is its own root (distinct working state, often a distinct branch) and gets its own lease; per-repo keying would wrongly serialize genuinely-different contexts (and worktrees are a first-class workflow here). RepoId stays a module concern (§4.9).

### 4.7 Bindings (`bindings{}`)

```jsonc
{
  "storage": { "kind": "sqlite", "scope": "project", "owns_schema": true },  // subc supplies path/backend; module owns schema
  "config":  { "source": "subc_mediated",
               "tiers": ["user", "project"],                 // layered: project overrides user
               "expansion": { "user": ["env","file"], "project": [] } },  // per-tier trust boundary
  "vault_grants": [ { "secret": "provider_api_key", "reason": "cortexkit_native auth" } ],
  "identity": { "requires": ["project"], "optional": ["session"] }
}
```
- **Config is a binding, not a file.** `source: subc_mediated` means the module's config lives behind subc and is editable remotely through the mgmt plane. The module owns the *schema* (declared in `management_surface.config_schema`); subc owns the *store and the transport*. This is what makes "control my server's config from my laptop" work.
- **Config is layered, and the layering is part of the contract** (MC's constraint). subc stores the declared `tiers` (e.g. `user` overridden by `project`) as **data, never flattened**, and never mixes tiers across the edit transport. Token expansion (`{env:}`/`{file:}`) is gated **per tier** — allowed at `user`, denied at `project` (secret-exfiltration safety). subc does not expand tokens: the module merges and expands at **use-time on the machine where it runs**, so `{file:}`/`{env:}` resolve on the server and secrets never travel to the CK app. subc owns the layered store + per-tier transport; the module owns merge + expansion + the trust rule.
- **subc's config store is provenance-preserving dumb storage.** Each tier is kept as a distinct, origin-tagged, raw document; subc does **no semantic preprocessing** — no merge, no expansion, no validation, no field-stripping. The module's per-tier trust policy is broader than token-gating and is applied at load *before* merge (e.g. MC strips privilege-escalation and process-level fields from the untrusted project tier, and drops an inherited embedding key on endpoint redirect), so any flattening, pre-expansion, or field alteration by subc would erase what the module must police. The stored and transported form is **always the raw token** — the CK app reads/edits the literal `{env:…}`/`{file:…}`, never a resolved value; secrets resolve once at module use-time on the module's machine and never re-enter the store or the wire. This is the config instance of subc's general rule: **store and route without interpreting** (the same splice-without-parse property the tool and proxy planes have).
- **Config cardinality is the module's schema, computed module-side (AFT, source-verified).** Because one module instance serves many roots and harnesses (§4.9), config splits three ways — but subc does **not** know this partition (knowing which keys are which = interpreting config = breaking thin-core). subc forwards the raw provenance-tagged tiers (§4.2 ATTACH); the module computes: **RootConfig** = artifact-shaping/executable-choosing keys (search max-file-size; semantic backend/model/base_url; LSP server set + binary paths; diagnostic-cache cap) — **canonical per root, reconciled across harnesses**; **demand overlay** = feature booleans (search/semantic/callgraph/inspect) — **ref-counted union** (build the root artifact if *any* attached session wants it, expose the tool surface per session); **SessionConfig** = per-`(harness,session)` (restrict-to-root, url-fetch, formatter/checker, bash, backups, filters). This is why the index artifacts can be safely root-shared: search/semantic indexes are **not** pure functions of files (semantic fingerprint = backend/model/base_url/dim), so sharing is sound **only** with a canonical-per-root config — and a later harness whose RootConfig **diverges** is rejected at attach (`config_divergence`, §4.2) with an actionable diff. That structural rule is what kills the embedding flip/flop (NFR10): one canonical embedding backend per root, never a silent rebuild war.

---

### 4.8 The wire envelope & versioning

The frame that carries every message in §4.1–4.7 on the local subc↔module leg: a **fixed 21-byte little-endian header**, then the body. subc makes every routing and scheduling decision from the header alone and **splices the body without parsing it**.

```
 offset  size  field     type    purpose
   0      4    len       u32     # of BODY bytes after this header (4 GiB frame cap; large data streams via the bulk lane)
   4      1    ver       u8      envelope version
   5      1    type      u8      REQUEST/RESPONSE/PUSH/STREAM_DATA/STREAM_END/ERROR/CANCEL/PING/PONG/HELLO/HELLO_ACK/GOODBYE
  6      1    flags     u8      bit0 BINARY (bulk-lane raw body) · bits1-2 PRIORITY (passive/interactive/background) · bit3 LAST (stream-final) · bits4-5 ADMISSION · bit6 DAEMON_ORIGIN · bit7 reserved
    7      2    channel   u16     route handle; bound at route.open, rewritten per-hop (client-local ↔ module-local, §4.9); 0 = subc control plane
    9      4    epoch     u32     per-slot binding epoch; 0 on channel 0
   13      8    corr      u64     correlation id; CANCEL carries the target call's corr
   21 → body
```

`CANCEL`/`PING`/`PONG`/`GOODBYE` are pure-header frames (`len = 0`); only `HELLO`/`HELLO_ACK` and RPC payloads carry bodies. Endianness little-endian (same-machine, native, no byte-swap on the hot path); `len` counts body bytes after the header.

**Versioning — two mechanisms.**
1. **HELLO negotiation (primary):** both ends agree on one envelope version per connection — no mixed-version frames. subc, being the central hot-swappable component, speaks the **superset** and negotiates **down** to each module; module version skew is the normal, handled case and subc never receives an un-negotiated version.
2. **Per-frame `ver` (self-describing):** lets a reader dispatch each frame to the right per-version parser.

**The locked invariant that makes versioning work — the frozen prefix:**

> `len` (u32 @ 0) and `ver` (u8 @ 4) keep fixed meaning and position in **every** future version.

So any reader of any version can always: read 5 bytes → learn `ver` → look up that version's header length → read the rest → splice `len` body bytes. Corollary: `len` stays u32 forever (large payloads stream via the bulk lane, not as one giant frame).

**Two tiers of extension:**
- **Small additions — no bump:** new `type` values (~12 of 256 used) and the allocated `DAEMON_ORIGIN` flag bit are already accommodated; bit 7 remains reserved for the next allocation.
- **Structural changes — bump `ver`:** new or resized fields (e.g. `channel` u16 → u32 = a 19-byte v2 header). Old peers negotiate down; v2-capable peers use v2.

**Transport is a separate, independently swappable layer.** Moving the local leg to HTTP/2 later (or the remote leg to HTTP/TLS) is negotiated at connection setup and does not touch envelope versioning — the body is transport-agnostic. Two independent evolution axes: the envelope via `ver` + frozen-prefix, the transport via connection negotiation.

---

### 4.9 Multi-provider routing & the session-attach data plane

The routing model (§2.1), source-verified with AFT, generalized to many clients × many providers:

- **Route by module-kind to one pid; the module self-demuxes.** subc does not run a per-root process pool. All AFT traffic goes to the one AFT process; AFT picks the right internal `ProjectActor` by `project_root`. subc resolves the canonical `ProjectRootId` at `route.open` — for its own lease/scheduler (§4.6) and PUSH routing — **not** to select an instance.
- **Independent channel spaces + rewrite (the multi-provider model).** A route has **two** channel numbers: a client-local one (unique per client connection) and a module-local one (unique per module endpoint). subc rewrites the envelope `channel` on every forwarded data frame (client→module → module-local; module→client → client-local), preserving `corr` and splicing the body untouched. This is what lets one client (the MCP gateway) hold routes to AFT + MC + future providers without channel collision, and many clients share one provider — neither side's channel namespace constrains the other. (subc replaced the single-`active_module` slot with a registry keyed by `module_id`.) Per-provider **generation** invalidates only that module's routes on its restart.
- **Channel-0 relay + teardown.** `route.bind` (subc→module: binds the module-local channel to the target + `BindIdentity` — also the module's *ensure-or-create-`ProjectActor`* signal; first bind warms a cold root, later binds bind to the warm actor). Route teardown is the **`GOODBYE` frame** (pure-header) in **both** directions — there is no JSON detach op. A non-zero-channel `GOODBYE` is terminal + idempotent: release the binding, free the flow-control credit, drop late frames, fail pending corrs. On **module death/disable**, subc emits a `GOODBYE` to each affected client on its client-local channel and tears down its own forwarding state (so a client learns its routes died without polling).
- **PUSH fan-out is the module's, not subc's.** subc stays a pure rewriting router. Unsolicited PUSH (bash completion, watcher `status_changed`, bash_watch) is targeted by the module: request-triggered PUSH echoes the originating channel; a root-wide `status_changed` fans out to **all** channels bound to that root (the module's `root → [channels]` index *is* its `ProjectActor` session set — zero new state). subc rewrites each PUSH's channel to the client-local value on the way out.
- **Liveness/status without forwarding.** subc caches the module's `route.status` push per route (client-keyed) and answers `route.poll` (status|liveness) **locally** — a poll produces **zero** frames to the module (keeps passive polls off the module's synchronous path). Liveness derives from subc's own supervision state.
- **Channel-gone = two converging signals → one module policy.** The proactive `GOODBYE` relay (authoritative) and a reactive per-send channel-gone outcome (subc best-effort drops a PUSH handed to a dead connection during the emit-before-detach race). The module applies one per-frame-type policy: **queue-for-replay** (completion/watch — disk/DB-durable, keyed by `(harness,session)`, survives detach **and** actor-evict **and** process-restart) vs **drop/coalesce** (status). subc just delivers a replayed PUSH on the new channel after re-attach.
- **RepoId is the module's, not shared.** subc keys everything per-path (`ProjectRootId`); a per-repo notion (git common-dir) lives module-side and only matters if a real per-repo coordinated resource ever emerges (not v1).
- **Path-identity is shared by construction.** The `ProjectRootId` canonicalization is a small published, dependency-light, cortexkit-neutral crate (`cortexkit-paths`) that subc *and* AFT call — byte-identical canonicalization, not two algorithms reconciled by test vectors. **subc owns canonicalization at the `route.open`→`route.bind` boundary** (it canonicalizes before relaying; the module consumes the result as-is, never re-canonicalizing — single-owner prevents a double-canonicalize divergence on symlinked/case-folded paths). Workspace-root walk-up is harness-owned (shared in the plugin bridge so OC+Pi can't diverge).

### 4.10 Spawn-mode & connect-failure contract (subc → spawned child)

How subc tells a process it spawned "you are running under me." Co-signed with AFT; applies to **any** subc-spawned child (AFT, the MCP gateway `subc-mcp`, future modules), not AFT-specifically. (Discovery + auth mechanics are §4.11; this section is *mode* + *failure policy*.)

- **Mode is the explicit invocation, not an env var.** subc launches a child in **module-mode** (it connects to subc per §4.11 and runs its under-subc paths — e.g. AFT picks shared-storage + multi-root `ProjectActor` demux); the harness-client launches a standalone child via the **standalone invocation** (today's in-process/NDJSON path). The spawner always knows which it wants and picks the invocation — explicit, **never probe-detected**. There is no ambient signal to inherit (the old `SUBC_SOCKET` env var is gone), so the entire stale-env footgun is eliminated by construction.
- **Concrete invocation token: `--subc <connection-file-path>`.** subc spawns a module-mode child with this CLI flag — **presence ⇒ subc-mode** (the child reads the §4.11 connection file at that path for endpoint + key + `daemon_id`, runs the auth handshake, then HELLO/manifest; fail-loud if unreachable); **absence ⇒ standalone** (today's invocation). It is the non-inheritable successor to `SUBC_SOCKET`: a **CLI flag cannot be ambiently inherited** (a standalone child simply lacks it — nothing to pick up by accident), and it points at the auth'd *connection file*, never a raw endpoint. This flag is a module's **dormancy gate** — all subc-mode codepaths branch on it and stay dark in standalone. The child owns the exact spelling within its own arg parser; subc passes `--subc <path>`.
- **Bind-publish-before-spawn (no race).** subc binds, starts accepting, and **atomically publishes the connection file** (§4.11) *before* the Supervisor spawns any module-mode child. So in correct operation a module-mode child always finds a live, listening daemon.
- **Present-but-unreachable ⟹ FAIL LOUD, never a silent downgrade.** A module-mode child that finds the connection file but cannot connect/authenticate (after a short bounded retry, ~100–500 ms) **errors and exits** — it does *not* silently run standalone. Silent fallback there is precisely the split-brain the daemon exists to kill (subc believes it owns the root while the child quietly ran standalone with its own watcher/index/storage → invisible duplication + divergent state, the §6/4.3 two-writer hazard).
- **The child never decides topology — the harness-client does.** A module-mode child obeys its invocation and fails loud if subc is unreachable; subc's Supervisor handles the exit. **All** topology/fallback decisions belong to the harness-side subc-client: *daemon-absent* (no live subc) → it falls back to in-process standalone execution (Story 1.2), launching the **standalone invocation**; *daemon-died mid-session* → it reconciles and falls back (Story 4.3, no two-writer). "Silent fallback to standalone" exists, but it is the harness-client reacting to subc-**absence**, never a child reacting to a dead endpoint.

### 4.11 Transport & connection auth (loopback TCP + key)

The local IPC transport. **Loopback TCP + a shared key** — chosen over Unix-domain sockets for uniform cross-platform behavior (Windows tokio has no `UnixListener`; Node uses named pipes, not AF_UNIX; Windows AF_UNIX has no `0600`). Threat model: **other local non-root processes/users** (a non-root peer cannot sniff another process's loopback traffic, so the channel needs authentication, not encryption — no TLS in v1). The **17-byte envelope (§4.8) is unchanged**; auth is a *pre-envelope* prelude.

- **Endpoint.** subc binds **loopback only** (`127.0.0.1`, and `::1` if present) — **NEVER `0.0.0.0`** (normative: `0.0.0.0` exposes the daemon to the LAN and demotes the key to the sole defense). On a **fixed, configurable port** (default + daemon-config override). On a bind conflict subc **fails loud** ("port in use → set the port in config") — never silently reselects (a known port is the point; silent reselect destroys diagnosability + reopens ambiguity).
- **Singleton = an atomic per-user start-lock**, not the port (`O_CREAT|O_EXCL` lockfile / lock dir), held during startup + stale-file replacement. Keeps the connect-first → lock → re-probe → reclaim-stale shape; the port is no longer the contended resource.
- **Connection file = discovery + key.** subc atomically publishes (temp + rename, owner-only perms pre-applied) a per-user connection file at a well-known path: `{schema, wire_version (current envelope version; omitted by legacy daemons), endpoints:[{host,port}], key (≥256-bit random), daemon_id (random per start), pid (advisory only), daemon_ver}`. Clients reject a declared mismatched `wire_version` before attempting TCP, while omitted legacy fields remain accepted. Clients read the file to discover the endpoint **and** the key; readers never observe a partial file (normative: atomic temp+rename + owner-only `0600`/user-ACL applied *before* publish — a truncated/partial-key read is impossible by construction).
- **Liveness = an authenticated daemon proof, never "the port accepts."** A stale connection file's port may have been reused by an unrelated process — so a daemon is "live" only if its endpoint returns a valid subc server-proof for *that file's* `key` + `daemon_id`. Connect-refused, timeout, invalid proof, wrong `daemon_id`, or non-subc garbage all mean stale/foreign.
- **Auth handshake — server proves itself first, the raw key never goes on the wire.** Order: (1) client → server: a non-secret `client_nonce` (+ role); (2) server → client: `daemon_id`, `server_nonce`, `daemon_ver`, and `server_proof = HMAC(key, "subc-server-v1" ‖ client_nonce ‖ server_nonce ‖ daemon_id)`; (3) client **verifies** `server_proof` (constant-time) **and** `daemon_id` against the connection file — only then sends `client_auth = HMAC(key, "subc-client-v1" ‖ client_nonce ‖ server_nonce ‖ daemon_id)`; (4) server verifies `client_auth` (constant-time) → the connection is authenticated and proceeds to the normal §4.2 HELLO / session-attach. A reused-port stranger cannot forge `server_proof`, so the client aborts before disclosing anything — the key-leak hazard is sealed. Domain-separated prefixes prevent reflection; nonces prevent replay. **Crypto pins (normative):** HMAC = **HMAC-SHA256**; `client_nonce`/`server_nonce` from a **CSPRNG, ≥128-bit each** (impl: 256-bit); constant-time verify on **both** proofs; the raw key is never transmitted or logged.
- **Auth gates before routing.** The handshake is enforced in the connection accept path *before* any frame reaches the router (session-attach clients use channel-0 too, so the gate cannot live in HELLO alone). DoS posture: a small pre-auth parser (not the 64 MiB envelope-body path), a 1–2 s auth deadline, a global unauthenticated-connection cap **plus a cap on concurrent in-progress pre-auth handshakes with idle-timeout** (bounds stranger-driven HMAC compute — cheap-DoS guard), constant-time verification, fast silent reject. subc **fails startup** if it cannot create owner-only files.
- **Key rotates per daemon restart.** A new subc generates a new `key` + `daemon_id` and republishes the file. On connection loss a client discards its cached `{endpoint, key, daemon_id}`, re-reads the file, re-validates the server-proof, and re-authenticates/re-attaches; old keys are never accepted (folds into the §Story-4.3 daemon-death reconnect).
- **The auth prelude is the subc-client core's job.** Every client (AFT's module shim, `subc-mcp`, the TS subc-client) speaks it; it lives in the **generic core** so adapters inherit it. (The MCP *harness→sMCP* boundary uses a **separate** `subc-mcp`-issued bearer token, never the subc connection-file key — so an MCP-token leak can't become full subc impersonation.)

---

## 5. Core services a module can rely on

| Service | Contract |
|---|---|
| **Identity** | canonical `ProjectRootId` + `(session_id, harness)`; worktree role first-class |
| **Mux + routing** | many in-flight requests; out-of-order responses; liveness answered by subc |
| **PUSH forwarding** | async frames (completions, status changes) delivered on the owning channel |
| **Scheduler + lease** | eligibility/cooldown; one lease per project; renew-during-call |
| **Vault** | audited, explicit secret grants; default-deny |
| **Storage substrate** | path-identity + backend-swappable store; module owns schema |
| **Bulk lane** | flow-controlled binary, component↔component only |
| **Supervision** | spawn/drain/restart/**hot-swap**; nested child-process cleanup |
| **Trust + conformance** | tier gating; codec cache-law gate; auth vault-grant review |

---

## 6. Concurrency & the delivery contract

The headline v1 goal — non-mutating tools run in parallel, mutating serialized — is delivered by **two parts that interlock but version independently**: subc's delivery contract, and the module's executor.

### 6.1 subc's delivery contract (locked) — and what is built

The contract is **locked**. Epic 2 implemented the subc side of it; everything below is **proven end-to-end against the fake-AFT stub** (real-AFT e2e is gated on AFT Leg 1, and within-root parallelism on Leg 2 — see §6.2/§6.3). Honest status:

| Contract element | Status |
|---|---|
| Sink-based async backend + per-connection writer task | **built** (streaming/PUSH-capable) |
| Per-connection backpressure (bounded outbound queue) | **built** |
| Forwarding data plane (`route.open` → `ForwardBackend` → multi-provider channel rewrite; PUSH; GOODBYE teardown both ways; module-death→client GOODBYE) | **built** (Epic 1 + control-protocol P1–P4) |
| Concurrent in-flight dispatch + out-of-order responses (corr carried, never assumed ordered) | **built + proven** (2.1/2.2 — `ForwardBackend` returns without awaiting; per-connection tasks; module-reader demux. Cross-session: fast call unblocked while a 500ms call runs) |
| CANCEL (pure-header, dumb-forward — module owns abort/terminal) | **built + proven** (2.3) |
| Per-channel flow-control window (un-acked cap), sized by declared `concurrency` | **built + proven** (2.4 — `ChannelFlow` credit counter, type-byte only; `serial→1` enforced) |
| Liveness/status answered from subc cache (passive poll never forwarded) | **built + proven** (2.5 + v0.4 — `route.poll` answered locally, zero frames to the module; kills the #117 passive-poll-behind-scan; `route.status` cache client-keyed; AFT co-signed) |
| Within-root parallelism (slow read doesn't block quick reads on one root) | **Leg-2-gated** (2.6 — AFT's reader-writer executor; §6.4) |

**Per-mode FIFO (FR16):** ordering is per-module, declared via manifest `concurrency`: `serial` = one in-flight, strict order (now **enforced** by the flow-control window 2.4 — subc never sends a 2nd request before the 1st's terminal); `module_managed` = concurrent in-flight across channels, per-channel FIFO submission preserved within a channel (module schedules internally) — AFT's mode; `stateless_parallel` = fully parallel, no ordering.

**Honest limitation (2.4):** the per-channel window bounds each channel's outstanding work, but does NOT give full same-socket cross-channel fairness — one ordered client stream means a window-blocked channel can still head-of-line later frames on its own socket. Full per-channel fairness (client-side credits / per-channel queues) is a later scheduler story, deliberately out of v1.

This contract is **delivery semantics only.** subc never learns which calls mutate, never orders execution, never reasons about resources. All execution consistency — including two calls touching the same resource in one parallel batch — is the module's.

### 6.2 The module side (declared; grows into the contract)

A module honors the contract per its declared `concurrency` (§4.4). AFT declares `module_managed` and matures in two legs **without the wire contract changing** (source-verified by AFT):
- **Leg 1 — in-process `ProjectActor` extraction.** AFT stays **one process** (it is a singleton module, §2.1); subc routes all AFT traffic to that one pid and AFT **self-demuxes by `project_root`** internally. Leg 1 lifts the single `AppContext` into a `ProjectActor` map keyed by `ProjectRootId`, sharing the few genuinely process-global services (bash registry, filter registry, stdout), plus the transport shim (stdin → subc socket). Ships incrementally, single-root working throughout. It does **not** itself deliver parallelism — each actor is still single-threaded.
- **Leg 2 — within-root reader-writer executor (`RefCell` → `Arc`).** Non-mutating handlers run on a thread pool against shareable index state; mutating handlers take a write barrier. This is what makes non-mutating tools actually run in parallel within a root — and once the `ProjectActor` map is concurrent, cross-root parallelism falls out for free.

Because the contract is "concurrent-in-flight + cancel + windows," **subc does not gate on AFT's Leg 2 timing** — AFT accepts the concurrency contract from Leg 1 (serial internally) and runs it truly concurrently after Leg 2. The felt within-root headline arrives **with** Leg 2 (so it is the critical path for the headline; cross-session concurrency is subc-owned and lands independently).

### 6.3 What each leg delivers (so the metric is honest)

- **subc mux, immediately (implemented — Story 2.5):** the bridge-kill half of AFT #117 (a sidebar status poll timing out behind a heavy scan and killing the bridge) is gone the moment subc owns the connection, independent of either leg — subc supervises the process, so **liveness is always answerable by subc directly** and never queues behind the busy actor. subc serves a passive liveness/status poll **locally from its own state + an opaque module-pushed status cache, never forwarding to the busy module** (proven: polls answered in ~100µs while a serial module is occupied by a 2s request). One Leg-1 caveat: module-originated **status** (index state, code-health) is refreshed by the module's push-frames, and a single-threaded Leg-1 actor cannot emit a refresh mid-scan until it yields — so subc's cached *status* may be briefly stale during a long scan (liveness never is). Stale status is a far lesser issue than the bridge-kill, and Leg 2 removes the staleness window entirely.
- **Leg 2:** the "heavy scan blocks the quick reads behind it, _within one root_" half is gone only when the reader-writer executor lands. The v1 concurrency metric (a slow search + N quick reads that don't queue) therefore depends on Leg 2 — which is in v1.

### 6.4 The Leg 2 substrate cost (source-verified by AFT)

Leg 2 is interior-mutability surgery, not a thin wrapper, because AFT's non-mutating handlers are not clean against shared state today:
- The query substrate is `RefCell<Option<T>>` (`search_index`, `semantic_index`, `callgraph`, `callgraph_store`) — `!Sync`, so `AppContext` cannot be shared across threads even for pure reads; accessors hand out borrows, not clones.
- Several "reads" mutate internally: the callgraph navigation ops (`callers`/`call_tree`/`impact`/`trace_*`) lazily open + populate + generation-revalidate the store via `borrow_mut()`; `semantic_search` lazily loads the embedding model via `borrow_mut()`; `outline` fills the symbol cache on miss; `config()`/`lsp()` hand out `Ref`/`RefMut`.

So Leg 2 requires: **(a)** convert the index structures from `RefCell`-owned to genuinely shareable (`Arc` + internal `RwLock`, or `Arc<immutable snapshot>`; `CallGraphStore` is SQLite → a read-only connection pool or snapshot handle); **(b)** move all lazy-build / lazy-open / model-load / generation-revalidation **out of the read path** (eager, or behind a one-time write barrier) so reads are genuinely `&`-only. Already thread-safe and needing no work: `symbol_cache`, `inspect_manager`, `bash_background`, `filter_registry`, and the channel-backed persistent stores. _(Full per-field inventory delivered by AFT — the AppContext substrate inventory (v3): a combined `ProjectActor`-extraction sizer + Leg-2 `Arc`-readiness map, Oracle-validated. Two corrections it surfaced: the provider/parser is a read-path mutator too (via `outline`/`zoom`) and `inspect` is a serial **writer** not a read; and `callgraph_store` downgrades barrier→wrap — it already holds `Mutex<Connection>`, so `Arc`-shared parallel readers are safe with no pool.)_

---

## 7. The closure argument

Every foreseeable module expressed purely as `{provides, consumes, scheduled_tasks, bindings}` — nothing left over:

- **AFT** — `provides:[tool_provider(concurrency: module_managed, emits_push, sub_supervises)]`, `consumes:[service_client(embedding)]`, `bindings:{sqlite/project}`.
- **MC** — `provides:[tool_provider(ctx_*), pipeline_stage(transform, frozen_floor, needs prefix_evicted), management_surface(context.db: compartments + memories + cache-decisions + key-files + config)]`, `scheduled_tasks:[dreamer, historian]`, `consumes:[llm_client(native), tool_client(aft)]`, `bindings:{sqlite context.db, config subc_mediated(user/project tiers)}`.
- **Harness session-store (per harness)** — `provides:[management_surface(raw transcript + session/worktree metadata + per-step token usage)]`. A harness plays two roles: tool-plane _consumer_ + mgmt-plane _provider_ via this adapter. (OC = reads `opencode.db`; Pi = reads JSONL; MITM harnesses later = subc-the-proxy is the store.)
- **Embedding engine** — `provides:[internal_service(embedding.v2, bulk)]`, `scheduled_tasks:[index-build queue]`, `bindings:{vector store}`.
- **LLM-runner / LLMloop** — `consumes:[llm_client(native), tool_client(*)]`, executes `scheduled_tasks` registered by other modules, holds the lease.
- **Provider codec** — `provides:[pipeline_stage(codec, conformance_class: cache_law_v1)]`, stateless.
- **Auth** — `provides:[pipeline_stage(auth)]`, `bindings:{vault_grants}`.
- **Alfonso (later)** — `consumes:[tool_client, llm_client]`, optionally `provides:[tool_provider]`.

Nothing requires a core change. The claim holds **until a module needs a plane that does not exist** — the only event that touches core, and the one we keep additive via the transport-agnostic body.

This is no longer just an argument — it is an **executable test**. `crates/subc-core/tests/closure.rs` stands up five foreseeable archetypes (MC management-surface, embedding, LLM-runner with a `selection` objective, a bus pub/sub provider, federation), routes each via `route.open` + opaque route-channel payloads, and asserts they integrate with **zero new `FrameType` and zero new subc-understood control op** — every domain body opaque-forwarded byte-identically, and the negative guardrail (an unknown channel-0 op → `unknown_control_op`) holding. The golden-JSON drift vectors (`tests/golden/`) pin every wire shape so the TS mirror can't silently diverge.

---

## 8. Open forks (carried)

Genuinely open (all later-plane / cross-team, none blocking v1 tool-plane):
1. **Canonical normalized shape** (proxy plane) — the exact representation codecs and the transform target (OpenCode parts-based MessageLike is a candidate).
2. **Conformance-gate corpus** (proxy plane) — recorded real provider payloads + assertions for codec certification.
3. **Session/message-data owner** (mgmt plane) — what serves the dashboard's session/message view in a harness-agnostic, remote world (pending MC-Alfonso).
4. **Route-channel tool-call contract + structured-result→MCP `content[]` mapping** (subc-mcp↔provider) — the opaque body shape the MCP gateway sends providers, and how AFT's structured/text results map onto MCP `content[]`. v1 = `{name, arguments, progress_token?}` → `{content:[{type:"text",text}], isError}` (single text block + isError); co-signed with AFT at their attach wiring (some AFT surfaces — search/inspect/callgraph — already emit agent-facing text; edit/read are JSON → decide single-block vs summary+data split per-surface against real shapes).

**Resolved since (were open forks):** framing → locked 21-byte length-prefix envelope (§4.8); single-process-vs-per-root → **single AFT process**, singleton module (§2.1, §4.9); AFT concurrency-refactor shape → in-process `ProjectActor` extraction (Leg 1) + `RefCell`→`Arc` (Leg 2), sized by AFT's substrate inventory v3 (§6.2/§6.4); **channel-0 control protocol** → v0.4 direction-split tagged enums, `subc-control` (client wire) vs `subc-protocol` (module wire) crate split, multi-provider `route.open{RouteTarget}` routing, capability negotiation, `supervisor.*` ([`docs/subc-control-protocol.md`](./subc-control-protocol.md), AFT co-signed); **`cortexkit-paths` repo home** → published from the `cortexkit/commons` monorepo, consumed by subc + AFT as a pinned crates.io dep; **cross-machine topology** → subc↔subc **federation** (every machine runs its own subc + local modules loopback-only; WAN quarantined to a future subc-supervised InterSUBC module), not scattered remote modules.
