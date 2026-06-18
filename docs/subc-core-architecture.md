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

- **AFT is a singleton too** (decided 2026-06, authoritative). One `aft` process total; AFT self-demuxes by `project_root` internally via an in-process **ProjectActor map** keyed by `ProjectRootId`. Per-root idle-evict is AFT-internal (today's 30-min idle-bridge policy lifted inside AFT). subc still resolves the canonical `ProjectRootId` at HELLO — needed for the per-project **lease**, the **scheduler**, and other modules (MC renders project memories, etc.) — but does **not** use it to select an AFT instance.
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

One connection **multiplexes** many logical channels (route = `(component, session)`, assigned at HELLO). Correlation ids carry many in-flight requests on one socket — this is what makes head-of-line blocking impossible at the transport layer, and lets subc answer liveness/status from its own cache without forwarding.

### 4.2 HELLO handshake

A client:
```jsonc
HELLO {
  "protocol_ver": 1,
  "actor": "client",
  "harness": "opencode" | "pi" | "codex" | "ck-app" | "cli" | ...,
  "project_root": "/abs/path",      // resolved to canonical ProjectRootId by subc
  "session_id": "ses_...",          // (session_id, harness) composite
  "role": "agent" | "dashboard" | "cli"
}
HELLO_ACK { "channel": u16, "daemon_ver": "...", "capabilities": {...}, "project_id": "git:<hash>|dir:<hash>" }
```

A module registers a **manifest** (below) instead of a client identity, and receives a control channel plus any consumer channels it asks for.

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
- subc-core owns the scheduler + the single project lease; the LLM-runner module *executes* the loop. Today MC and AFT have separate lease tables in separate DBs — centralizing the lease here is what stops two modules dreaming the same project at once.

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

---

### 4.8 The wire envelope & versioning

The frame that carries every message in §4.1–4.7 on the local subc↔module leg: a **fixed 17-byte little-endian header**, then the body. subc makes every routing and scheduling decision from the header alone and **splices the body without parsing it**.

```
 offset  size  field     type    purpose
   0      4    len       u32     # of BODY bytes after this header (4 GiB frame cap; large data streams via the bulk lane)
   4      1    ver       u8      envelope version
   5      1    type      u8      REQUEST/RESPONSE/PUSH/STREAM_DATA/STREAM_END/ERROR/CANCEL/PING/PONG/HELLO/HELLO_ACK/GOODBYE
   6      1    flags     u8      bit0 BINARY (bulk-lane raw body) · bits1-2 PRIORITY (passive/interactive/background) · bit3 LAST (stream-final) · 4-7 reserved
   7      2    channel   u16     route = (component, session); 0 = subc itself
   9      8    corr      u64     correlation id; CANCEL carries the target call's corr
  17 → body
```

`CANCEL`/`PING`/`PONG`/`GOODBYE` are pure-header frames (`len = 0`); only `HELLO`/`HELLO_ACK` and RPC payloads carry bodies. Endianness little-endian (same-machine, native, no byte-swap on the hot path); `len` counts body bytes after the header.

**Versioning — two mechanisms.**
1. **HELLO negotiation (primary):** both ends agree on one envelope version per connection — no mixed-version frames. subc, being the central hot-swappable component, speaks the **superset** and negotiates **down** to each module; module version skew is the normal, handled case and subc never receives an un-negotiated version.
2. **Per-frame `ver` (self-describing):** lets a reader dispatch each frame to the right per-version parser.

**The locked invariant that makes versioning work — the frozen prefix:**

> `len` (u32 @ 0) and `ver` (u8 @ 4) keep fixed meaning and position in **every** future version.

So any reader of any version can always: read 5 bytes → learn `ver` → look up that version's header length → read the rest → splice `len` body bytes. Corollary: `len` stays u32 forever (large payloads stream via the bulk lane, not as one giant frame).

**Two tiers of extension:**
- **Small additions — no bump:** new `type` values (~12 of 256 used) and new `flags` bits (4 reserved) are already accommodated.
- **Structural changes — bump `ver`:** new or resized fields (e.g. `channel` u16 → u32 = a 19-byte v2 header). Old peers negotiate down; v2-capable peers use v2.

**Transport is a separate, independently swappable layer.** Moving the local leg to HTTP/2 later (or the remote leg to HTTP/TLS) is negotiated at connection setup and does not touch envelope versioning — the body is transport-agnostic. Two independent evolution axes: the envelope via `ver` + frozen-prefix, the transport via connection negotiation.

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

### 6.1 subc's delivery contract (locked; built day one)

subc models concurrency from the first release, regardless of how far any module's executor has matured:
- **Concurrent in-flight** — many requests outstanding per channel, identified by correlation id.
- **Out-of-order responses** — a response carries its corr id; subc never assumes response order matches request order.
- **Per-channel FIFO _delivery_** — subc hands a channel's requests to the module in submission order, but does not wait for a response before delivering the next. Delivery order only; *execution* order is the module's.
- **Cancel** — subc forwards a CANCEL for a corr id (session interrupted, harness aborts, client gone). First-class from day one — long callgraph cold-builds and embedding runs make it matter before Leg 2 even exists.
- **Per-channel flow-control window** — a bounded number of un-acked in-flight requests per channel, so a slow module cannot make subc buffer unboundedly.

This contract is **delivery semantics only.** subc never learns which calls mutate, never orders execution, never reasons about resources. All execution consistency — including two calls touching the same resource in one parallel batch — is the module's.

### 6.2 The module side (declared; grows into the contract)

A module honors the contract per its declared `concurrency` (§4.4). AFT declares `module_managed` and matures in two legs **without the wire contract changing** (source-verified by AFT):
- **Leg 1 — transport + per-root router.** NDJSON-on-stdin → subc socket; the main loop becomes a router; one `AppContext` instance per root. Delivers **cross-root** parallelism; each per-root actor is still single-threaded and processes concurrent-in-flight calls through an internal FIFO — serial per root, for now.
- **Leg 2 — within-root reader-writer executor.** Non-mutating handlers run on a thread pool against shareable index state; mutating handlers take a write barrier. This is what makes non-mutating tools actually run in parallel within a root.

Because the contract is "concurrent-in-flight + cancel + windows," **subc does not gate on AFT's Leg 2 timing** — AFT accepts concurrency from Leg 1 (serial internally) and runs it truly concurrently after Leg 2. Both legs are in v1 (the headline is Leg 2), built Leg-1-then-Leg-2, not big-banged.

### 6.3 What each leg delivers (so the metric is honest)

- **subc mux, immediately:** the bridge-kill half of AFT #117 (a sidebar status poll timing out behind a heavy scan and killing the bridge) is gone the moment subc owns the connection, independent of either leg — subc supervises the process, so **liveness is always answerable by subc directly** and never queues behind the busy actor. One Leg-1 caveat: module-originated **status** (index state, code-health) is refreshed by the module's push-frames, and a single-threaded Leg-1 actor cannot emit a refresh mid-scan until it yields — so subc's cached *status* may be briefly stale during a long scan (liveness never is). Stale status is a far lesser issue than the bridge-kill, and Leg 2 removes the staleness window entirely.
- **Leg 2:** the "heavy scan blocks the quick reads behind it, _within one root_" half is gone only when the reader-writer executor lands. The v1 concurrency metric (a slow search + N quick reads that don't queue) therefore depends on Leg 2 — which is in v1.

### 6.4 The Leg 2 substrate cost (source-verified by AFT)

Leg 2 is interior-mutability surgery, not a thin wrapper, because AFT's non-mutating handlers are not clean against shared state today:
- The query substrate is `RefCell<Option<T>>` (`search_index`, `semantic_index`, `callgraph`, `callgraph_store`) — `!Sync`, so `AppContext` cannot be shared across threads even for pure reads; accessors hand out borrows, not clones.
- Several "reads" mutate internally: the callgraph navigation ops (`callers`/`call_tree`/`impact`/`trace_*`) lazily open + populate + generation-revalidate the store via `borrow_mut()`; `semantic_search` lazily loads the embedding model via `borrow_mut()`; `outline` fills the symbol cache on miss; `config()`/`lsp()` hand out `Ref`/`RefMut`.

So Leg 2 requires: **(a)** convert the index structures from `RefCell`-owned to genuinely shareable (`Arc` + internal `RwLock`, or `Arc<immutable snapshot>`; `CallGraphStore` is SQLite → a read-only connection pool or snapshot handle); **(b)** move all lazy-build / lazy-open / model-load / generation-revalidation **out of the read path** (eager, or behind a one-time write barrier) so reads are genuinely `&`-only. Already thread-safe and needing no work: `symbol_cache`, `inspect_manager`, `bash_background`, `filter_registry`, and the channel-backed persistent stores. _(Full per-field conversion inventory: forthcoming from AFT.)_

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

---

## 8. Open forks (carried)

1. **Framing** — hand-rolled length-prefix vs `h2`-with-JSON-bodies (mux/cancellation for free vs heavier TS client).
2. **Canonical normalized shape** — the exact representation codecs and the transform target (OpenCode parts-based MessageLike is a candidate).
3. **Conformance-gate corpus** — recorded real provider payloads + assertions for codec certification.
4. **AFT concurrency-refactor shape** — reader-writer over per-root state; size pending AFT-Alfonso (do non-mutating handlers touch RefCell state, or are they clean against the Arc indexes?).
5. **Session/message-data owner** — what serves the dashboard's session/message view in a harness-agnostic, remote world (pending MC-Alfonso).
6. **Single-process vs per-root** — measurement-gated (RSS decomposition); secondary to concurrency.
