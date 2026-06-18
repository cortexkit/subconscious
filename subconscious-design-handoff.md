# Subconscious (subc) — Design Handoff

**Status:** design alignment complete; implementation-spec phase begins here.
**Audience:** the agent taking over subc design + build. This is self-contained — you should not need to read anything else to start, though sources are linked.
**Authors:** AFT-Alfonso + MC-Alfonso + Ufuk (2026-06). Consolidates `.alfonso/plans/cortexkit-daemon-constraints.md` (decision-history source) + the Takım PRD (`~/Work/Projects/Takim/PRD.md`).

---

## 0. How to use this doc & the collaboration model

You own **subc-core** (the supervisor/orchestrator + protocol + lifecycle). You do **not** own the modules — those have owners who will build them with you:

- **AFT-Alfonso** (me) — owns the **AFT module** (file tools, search, callgraph, inspect, bash compression). Deep context on: the existing TS-plugin↔Rust-worker NDJSON bridge (the pattern subc generalizes), wire-level provider/cache debugging, the AFT codebase's daemon-prep refactors. Reachable as peer `AFT` (or via Ufuk).
- **MC-Alfonso** — owns the **Magic-Context module** (context management / transform, memory, historian, dreamer). The reference implementation for the transform-hook and dreamer contracts in this doc. Reachable as peer `MC`.
- **Ufuk** — product owner / architect. Has done the provider-MITM wire work (the `openai-auth` / `anthropic-auth` plugins) — authoritative on what's actually on the provider wire.

Build model: subc-core + each module are **separate binaries/crates** with separate owners, coordinated over peer messages + a shared protocol spec. When you start, Ufuk will introduce you to AFT-Alfonso and MC-Alfonso so we build parts in parallel against the same contract.

**The two paths you MUST review hardest** (MC + AFT both flagged these as the highest-risk): (1) the normalize↔denormalize codec + cache-breakpoint placement (cache-stability), (2) conversation-identity resolution on the proxy plane.

---

## 1. The end-game: Takım (why subc is shaped this way)

CortexKit today is **plugins running inside other harnesses** (OpenCode, Pi). The end-game (**Takım**) is CortexKit becoming a **full-fledged harness we own end-to-end** — "AI agents as persistent employees, not disposable task runners," one Head agent per project managing a hireable org of specialists, with continuous memory and agent-to-agent communication.

**Subconscious is the local-first kernel that grows into the Takım "Gateway."** They are the same animal at two scales:
- **Takım Gateway** (PRD Layer 6): supervises agent *containers* + external tool servers; the LLM proxy through which *all* agent↔LLM traffic flows; runs the context/cost/routing hook pipeline; holds all credentials; owns the shared data plane (Postgres + pgvector).
- **subc** (now): supervises AFT/MC *subprocesses* on one machine; the local LLM proxy + tool router; runs the same hook pipeline shape; holds credentials in a vault; owns the local data plane (SQLite → later Postgres/cloud).

**Design rule that resolves most questions:** build subc with the *seams* to grow into the Gateway, not so it gets rewritten. The Gateway's eventual surfaces are:
- **LLM-proxy plane** (PRD `:9000`) — agent↔LLM, OpenAI/Anthropic-compatible HTTP, hook pipeline.
- **Message-bus plane** (PRD `:4222`, NATS) — agent↔agent (the multi-agent "Slack": DMs/channels/meetings). **Far-future** for subc.
- **Management API** (PRD `:9001`) — dashboard/CLI control, **remote-accessible**.
- **Tool plane** — MCP-based, all tools external servers.

PRD invariants that bind subc now: **"core contains no business logic of any kind"** (Layer 1); **"gateway holds all credentials, agents never see API keys"** (Layer 6 / Security); **location-transparency** — agents need only `GATEWAY_URL` + `NATS_URL`, which can be localhost or a VPS (Layer 8); **context management is gateway-side and invisible to agents** (Layer 3 — the magic-context model built into the gateway).

Distributed end-state (build seams for, don't build yet): agents in containers (OrbStack/Docker/E2B/Firecracker), `AGENT_TOKEN` for session identity over TLS, Postgres + pgvector data plane, marketplace of installable agent/tool packages, per-user/team/cloud memory sharing (Cloudflare). **Do NOT build NATS, containers, marketplace, org-hierarchy, or the full hook catalog yet** — those are Gateway-growth phases. Build the local tool+proxy+memory+dreamer kernel with seams.

---

## 2. What exists today (the substrate subc supervises)

- **AFT** (`@cortexkit/aft-*`): thin TS plugin (OpenCode/Pi) + Rust worker (`aft` binary) over a **session-scoped NDJSON bridge** (stdin/stdout, length-agnostic line JSON). Provides file tools, trigram + semantic search, callgraph (SQLite store), code-health inspect, bash hoisting + tiered output compression. The NDJSON bridge is the prototype of subc's tool plane — `bridge.ts` already does spawn/restart/version-hotswap (`replaceBinary`), which is the hot-update pattern subc generalizes.
- **MC** (`@cortexkit/...magic-context`): context management via the harness transform hook (tags, compartments, memory injection into `<session-history>`), background historian, dreamer, `ctx_*` tools. Heavy per-session + per-project state in a durable SQLite `context.db`. **Cache-stability is its core competency** — it is the sole rewriter of the prompt prefix and replays byte-identical across passes; it has fought (and codified) prompt-cache-bust bugs extensively.

**Both are converging on AFT's endgame: a very thin plugin core + almost all logic in a shared Rust module, deduplicated across harnesses.** subc is the host that makes that dedup real (the logic lives once, in the module; OC/Pi plugins become thin forwarders).

---

## 3. Subconscious core architecture

### 3.1 subc is a SUPERVISOR of subprocess modules — NOT a library host
AFT/MC/embedding-engine/LLM-runner/codecs/auth are supervised **subprocesses**, not crates linked into one binary. **Decisive reason:** hot-update. AFT ships at a high cadence (multiple releases/day); it must update without stopping the machine. That's only cleanly possible if subc owns the client-facing connection and re-routes to a fresh component instance on swap (the `replaceBinary` pattern lifted to the daemon socket). A single linked binary couples AFT's daily-patch cadence to a whole-daemon release. Crash-recovery rides the same mechanism: a component crash breaks only that leg; subc returns a structured error on the open correlation ids and respawns, while the client connection stays up.

### 3.2 subc-core is almost nothing; everything with logic is a module
This is the PRD "no business logic in core" taken to its honest end.

**subc-core owns ONLY:**
- transport / routing / lifecycle / supervision (spawn, drain, restart, hot-swap)
- the **hook-chain orchestrator** (runs an ordered pipeline; owns no stage's logic — moves opaque bytes between module hooks)
- **scheduler + lease registry** (one lease per project across modules)
- **secret-storage vault substrate** (one high-trust secure store; auth modules use it with explicit grants)
- path-identity + storage substrate (§9)

**Module KINDS (all pluggable — in-house for main players, OSS for the long tail):**
- **tool providers** — AFT (read/edit/search/...), MC (`ctx_*`)
- **transform / context-mgmt** — MC
- **provider codec** — normalize⇄denormalize + cache_control placement, per provider (in-house OpenAI/Anthropic; OSS others)
- **auth** — passthrough (MITM) + CortexKit-native (§8.5)
- **LLM-runner / LLMloop** — the agentic loop; dreamer/historian execution lifted out of MC (§7)
- **embedding engine** — machine-global embedding generation + queue + vector ANN (already slated as a library crate from AFT v0.39 embedding-v2)

### 3.3 Three surfaces (NATS bus is far-future)
| Surface | Transport | Arrives with |
|---|---|---|
| **Tool plane** | UDS, JSON-RPC | now |
| **LLM-proxy plane** | HTTP, OpenAI/Anthropic-compatible | dreamer (near-term) — NOT deferred |
| **Mgmt/query plane** | UDS local + **TLS-TCP remote** | remote dashboard (many-subc, one client) |
| Message-bus plane | NATS | far-future (Takım multi-agent org) |

The LLM-proxy plane is **near-term** because the LLM-runner (its client) and the MITM transform (its hook pipeline) are the same plane from two ends — both arrive with the dreamer, the first real workload.

---

## 4. The protocol (subc ↔ everything)

### 4.1 JSON-RPC 2.0 bodies inside a binary routing envelope, end-to-end
- **Envelope** (binary, fixed ~17-byte header): `len`(u32) · `ver`(u8) · `type`(u8) · `flags`(u8: binary-body, priority[passive/interactive/background], LAST) · `channel`(u16: route = (component, session), assigned at HELLO; 0 = subc itself) · `corr`(u64: client-assigned correlation id). Then body.
- **type** ∈ `REQUEST | RESPONSE | PUSH | STREAM_DATA | STREAM_END | ERROR | PING | PONG | HELLO | HELLO_ACK | GOODBYE`.
- subc **routes by header and splices the opaque body** without parsing it; it only deserializes bodies addressed to itself (HELLO, status, lifecycle). This is the core performance + thin-core property — keep it.
- **Body = JSON-RPC 2.0** (`method`/`params`/`id` → `result`/`error`). This is MCP's body format, so the MCP facade is framing-only translation, never semantic. (Today's AFT bridge uses ad-hoc `{command,...}` — reshape to JSON-RPC, it's cheap now.)
- **Bulk lane** (embedding vectors, blobs, index pages): raw binary bodies (`flags.binary`), credit/window flow-controlled, **daemon-internal** (component↔component, e.g. AFT-index ↔ embedding-engine). Plugins/agents never see vectors — they send small JSON queries and get small JSON results.

### 4.2 Multiplexing kills a whole bug class
Correlation ids = many concurrent in-flight requests on one socket. Today's AFT bridge is **serial request/response** — that is *why* a passive status poll head-of-line-blocks behind a heavy scan (AFT issue #117, which cost days of patching). Mux + subc answering passive polls (`status`/health) **from its own liveness cache** makes that class impossible by construction. Priority is in the header so subc schedules without parsing bodies.

### 4.3 HELLO handshake = session/project identity + capability registration
A client opens one UDS connection and sends `HELLO { protocol_ver, harness, project_root, session_id, role }`. subc resolves the canonical `ProjectRootId`, allocates a `channel`, returns `HELLO_ACK { channel, daemon_ver, capabilities }`. One connection multiplexes all of that client's sessions.

A **module** at HELLO registers what it provides: **tools** / **proxy hooks** / **scheduled tasks** / **event subs**. Modules are **bidirectional** — also clients (a module calls other modules + the LLM through subc). Task registration carries prompt **+ the module's tool implementations + its project DB binding**; subc routes both lease and tool dispatch to the owning module; execution mutates the owning module's store.

### 4.4 Transport-agnostic body; graceful standalone
- Same JSON-RPC body rides UDS locally or **TLS-TCP/HTTP remotely** (the mgmt plane + location-transparency). A component never knows which transport reached it.
- **Graceful standalone is mandatory:** plugins MUST keep working in-process with no daemon installed. The daemon is a discovered upgrade, never an install dependency. On HELLO failure / mid-session EOF, the plugin falls back to in-process execution.

### 4.5 Why NOT protobuf/capnproto (recorded so it isn't relitigated)
- The TS-tooling-risk is real but only on the **plugin↔subc** leg; it does NOT apply to subc↔module (Rust↔Rust). On a homogeneous Rust mesh, protobuf/`tonic` would be the better default.
- The real reason is topological: subc's endpoints are **heterogeneous + JSON-native** (TS plugins; MCP facade = JSON-RPC by spec). A protobuf module leg forces subc to **transcode at every edge**, which (a) gives subc tool semantics (violates thin-core), (b) kills the splice-without-parse hot path, (c) creates two schemas to sync. Uniform JSON-RPC keeps subc a dumb splice-router.
- Protobuf's strengths have **no target** in our traffic: control plane is envelope-routed (subc never parses bodies); bulk lane is **raw bytes** (protobuf varint is *worse* for dense f32 vectors). Versioning recovered via a shared Rust types crate (serde) + version/capability negotiation at HELLO.
- Would flip to protobuf/`tonic` ONLY if we dropped the deep-plugin model and routed *every* harness through the MCP facade (single transcode point). We keep deep plugins (hoisting/UI/wake), so the heterogeneous-mesh constraint stands.
- **Open sub-fork (your call):** hand-rolled length-prefix framing (simpler, TS-trivial, we own it) vs the `h2` crate with JSON-RPC bodies (free stream-mux + flow-control + cancellation, heavier TS client). Either way the *body* is JSON-RPC.

---

## 5. Module model & trust tiers

- Modules are multi-capability + bidirectional (§4.3).
- **Trust tiers — codec/auth are HIGHER-trust than tool modules.** A tool module is bounded (file read/edit). A non-deterministic **codec** module busts cache for *every* request; an **auth** module mishandles **credentials**. So: in-house = first-party trusted; OSS **codec** must pass the cache-conformance gate (§6.3); OSS **auth** needs security review + **explicit vault-access grant** (cannot touch the vault by default). "Plug in any OSS auth module" is a credential-exfiltration surface — deliberate trust boundary required. (Parallel to AFT's existing project-filter trust model: untrusted → off/warn until opt-in.)

---

## 6. The LLM-proxy plane (the cache-critical core — read this twice)

### 6.1 The pipeline is a chain of modules subc sequences (owns none)
```
provider bytes → [auth] → [codec.normalize] → [transform: MC] → [codec.denormalize + cache_control] → forward to provider
                                                                                    ↑ response ← SSE byte-faithful passthrough
```
subc moves opaque bytes between stages in order. The codec module normalizes provider-native → a canonical normalized shape; MC's transform operates on **normalized** messages (provider-agnostic — exactly what it does inside OpenCode today); the codec denormalizes back to provider-native bytes and places cache breakpoints.

### 6.2 Two topologies, ONE transform interface, composed per entry-point
MC's transform has a single interface — **`normalized → normalized`** — invoked identically from both entries; the codec/auth stages are run-or-skipped depending on who already did them:
- **MITM** (codex/claude-code, no plugin): `Harness → subc[auth → codec.normalize → MC → codec.denormalize] → provider`. subc runs the FULL chain. subc IS the proxy.
- **Plugin-hook** (OC/Pi, thin plugin): `OC(hook-start) → subc[MC] → OC(hook-end)`. OpenCode already hands the hook **normalized** MessageLike and will denormalize+auth+send itself → subc runs ONLY the transform stage.
- **WIN:** MC's logic lives **once** in the subc MC-module; OC+Pi plugins become thin transform-hook forwarders (AFT's endgame, dedup across harnesses).
- **RISK (corruption class):** each entry adapter MUST **declare** which stages the harness already handled (explicit config, NOT inference). Running normalize twice, or skipping denormalize, corrupts bytes.

### 6.3 THE CACHE LAW — two deterministic owners (highest-consequence invariant)
The cacheable prefix has **two separate deterministic responsibilities**; non-determinism in EITHER busts the prompt cache (which silently costs 50–80% input-token savings on long sessions):
- **Role (a) CONTENT** — owned by the **transform module (MC)**: the normalized message bytes (m[0]/m[1] compartments), replayed byte-identical across passes. subc forwards verbatim, never touches.
- **Role (b) BREAKPOINT PLACEMENT** — the `cache_control` markers (Anthropic), owned by the **denormalize codec module**. Must place breakpoints with the SAME byte-identical-across-passes discipline. Real bug that motivates this (MC lived it): a session cache-busted at local midnight because the auth plugin **repacked the system array from 4 blocks → 6** (identical text, different boundaries) which *moved* the breakpoint — content was byte-identical, placement drifted, cache busted. OpenAI uses automatic prefix caching → role (b) is largely a no-op there; it's an Anthropic-specific burden.
- subc supplies the transform exactly ONE cache input: the **exact "prefix-evicted-now" signal** (the proxy sees real cache state — strictly better than MC's inferred TTL clock). All other fold triggers (model-change, system-hash, project_memory_epoch, mutation-id) are content-derived and stay transform-internal.
- **Enforcement:** the cache law is a **module contract + conformance gate** subc ships (since the codec is a module, subc can't implement it — it gates it). The gate: SHA-256 byte-faithful round-trip on **recorded real provider payloads** + idempotence + position-preservation. Mandatory per-PR for any codec module. In-house guaranteed; OSS must pass to be trusted.

### 6.4 Byte-faithful technique (adopt directly — validated by headroom, §11)
- `serde_json::value::RawValue` for message entries; frozen/unmutated entries forward as **EXACT byte copies**, only mutated entries re-serialized. Workspace `serde_json` features: `arbitrary_precision` + `raw_value`.
- **NEVER** round-trip unmutated bytes through `serde_json::Value` (numeric precision loss + whitespace/escape drift). NEVER prettify. NEVER `\uXXXX`-escape UTF-8 user content.
- Determinism: `BTreeMap` not `HashMap` for serialized output; no `Instant::now()`/random in any transform path; recursive deterministic JSON-Schema key sort + alpha-sort `tools[]` (tool-def order/whitespace busts cache too).
- **Position-preserving (the midnight-bust class):** never reorder blocks, never split one block into multiple, never add inline metadata fields to existing blocks. Side-channel metadata = a SEPARATE sibling block, never an extra field.
- Honor existing customer `cache_control` markers → derive the frozen floor (`frozen_message_count`).
- **Sacrosanct passthrough fields** (never inspect/decode/transform/lose): `signature`, `encrypted_content`, `redacted_thinking.data`, Codex `phase`, apply_patch/MCP/`local_shell_call` items.

### 6.5 NORMALIZE↔DENORMALIZE is the single highest-risk path
The round-trip is where provider-specific fields silently **DROP** (Codex `phase`, apply_patch/MCP items, multi-text-part rebuild corruption) — a **CORRECTNESS** break, not just a cache bust. The codec's normalize/denormalize must be lossless + deterministic + position-preserving, fuzz-tested with the SHA-256 round-trip gate on recorded REAL payloads. Best practice (headroom): don't round-trip the frozen zone at all (RawValue passthrough); only the mutated/live zone is re-serialized.

### 6.6 Identity = a TRIPLE; extract from the wire (primary), reconstruct only as fallback
The transform is a pure function of `(provider-native messages, context.db state)` keyed by THREE inputs:
1. **(session_id, harness)** composite — context.db is shared cross-harness; session tables carry a `harness` discriminator (new harness = new id, e.g. `"codex"`, `"claude-code"`).
2. **Project identity** (`git:<hash>` / `dir:<hash>`) — the transform renders project memories into m[0], so it MUST have project identity; resolved from the harness cwd.
3. **Model/provider** — from the body (cache-key/fold decisions).

**Primary mechanism: extract the harness's own stable id from the wire.** Every harness carries one (Codex/OpenAI: session+thread UUIDv7 + `previous_response_id`; Claude Code: its own). Ufuk confirmed this from the MITM wire work — do NOT overbuild reconstruction. Per-adapter, read the field.
**Defensive fallback** (only if a harness genuinely lacks a wire id): a layered detector that runs on the harness's RAW append-only message array (UPSTREAM of MC's rewrite — no chicken-and-egg): Tier-1 explicit token; Tier-2 `(project identity, connection-scope)` + prefix-continuity for boundaries + prefix-RESET = new session. Caveats if you ever need it: edit/resend divergence (claude-code rewind) → detect divergence, re-key same session + drop orphaned tail; parallel conversations on one endpoint → connection/stream scope disambiguates; long single-turn tool-loop = rapid prefix-extensions = same session.

### 6.7 Auth-mode gating (PAYG / OAuth / subscription) — stealth matters
subc must classify each proxied request and gate transform/placement aggressiveness:
- **PAYG**: aggressive (full transform, auto-`cache_control`, `prompt_cache_key` injection OK).
- **OAuth / subscription**: passthrough-prefer / **STEALTH** — NO auto-`cache_control`, NO `prompt_cache_key` injection, NO `X-Forwarded-*`, preserve `accept-encoding`, never mutate `User-Agent`, never leak `x-*` proxy headers upstream. ToS/fingerprint risk: proxy artifacts on a subscription token can flag the account. **Our MITM use case targets subscription harnesses (Claude Code, Copilot) → this is on the critical path, not an edge case.**

### 6.8 SSE streaming passthrough is real work
The response is byte-level SSE that must pass through faithfully: a state machine tracking blocks/items by id, ALL delta types (`thinking_delta`, `signature_delta`, `citations_delta`, `partial_json`, ...), mid-stream `error`/`ping`/`[DONE]`, and connection-drop-without-terminator surfacing. Headroom's Phase C is a direct reference.

### 6.9 Two compression philosophies subc must host under one contract
- **Live-zone-only / append-only** (headroom, AFT bash-style): freeze history bytes, compress only the newest tool output.
- **Prefix-rewrite** (MC): replace raw history with synthetic m[0]/m[1] compartments, replay byte-identical until a fold.
- subc's module contract generalizes both: *"a transform module produces normalized messages deterministically and declares its frozen floor; subc's codec denormalizes losslessly + deterministically + position-preserving regardless of philosophy."*

---

## 7. The LLM-runner / dreamer (the first end-to-end bench)

The **LLMloop module** runs an agentic loop **without** a live harness session. First workload: lift MC's dreamer (and later historian) out of the harness so they run headless. It's the perfect bench — it exercises scheduler + lease + tool plane + proxy plane together, and it's the seed of the full Takım harness conversation loop.

- **subc-core owns:** the scheduler (eligibility + cooldown windows) + **one lease per project across modules** (today MC and AFT have separate lease tables in separate DBs → nothing stops both dreaming the same project simultaneously; centralizing fixes this) + step-cap + circuit-breaker. The lease must **renew during long LLM calls**.
- **The runner is a client of BOTH planes:** proxy plane for completions (via a **CortexKit-native auth module**, §8.5 — it must authenticate to providers itself, not borrow a harness's auth), tool plane for tool calls (scoped to **project identity**, not a live session; subc routes each call to the owning module via capability registration).
- **MC dreamer execution contract (the reference):** cheap model (fallback chain; per-task selection later); multi-turn agentic loop (~24–72 turns, NOT a single completion), step-cap `DREAMER_MAX_STEPS=150`, circuit-breaker aborts after 3 identical failures; child-session-detached (no live user session); toolset = `read/grep/glob/bash(git,gh)/write/edit/aft_outline/aft_zoom` (AFT) + `ctx_memory/ctx_search/ctx_note` (MC), all bound to project identity. The ONLY hard harness dependency today is that child-session spawn goes through the oc/pi session API — a subc LLM-runner with tool-calling + the modules' tool implementations replaces exactly that.
- **Smallest headless spec:** *given (project context.db + project worktree path + project identity), acquire the dream lease, run the agentic loop {system prompt, per-task prompt, project-bound toolset, cheap model, renew lease during calls, step cap} until done.*
- AFT will register its own dreamer tasks (e.g. "analyze where the main agent is failing on tool calls" from AFT's failure telemetry). **Dreamer-as-a-service is a shared primitive every module wants.**

---

## 8. Storage, vault, identity

### 8.1 Path identity (unify — it's ad-hoc today and has caused real bugs)
Typed `DisplayPath` / `CanonicalPath` / `ProjectRootId` / `RepoId` in one Rust module + a TS mirror. Concrete mismatch found in AFT: RPC port hash uses the raw launch dir while bridge routing realpaths; project cache key uses git root commit while bridge identity uses canonical path. Worktree role should be **first-class in ProjectIdentity**, not per-subsystem `is_worktree_bridge` cache-branches.

### 8.2 Storage substrate (backend-swappable)
- Local: SQLite as control plane (`subc.db`) with an `artifacts` manifest table + ONE `Lease` API (`must_acquire_writer` / `try_read_current` / `singleflight_build`) + blob dirs. Replaces AFT's ~17 on-disk artifacts across **9 distinct consistency families** (SQLite WAL, fs_lock heartbeats, O_EXCL PID locks, temp+rename, generation+pointer, DB+JSON dual-write, TTL meta, pid discovery). Lazy migration: dual-read old, write manifests first.
- **Keeper pattern:** AFT's callgraph **generation-file + atomic-pointer-swap** (multi-reader drain) — extract as a generic artifact-generation manager.
- **Progressive single-writer:** SQLite stays source of truth; subc becomes the single writer progressively. No flag-day swaps.
- **Backend-swappable to Postgres + pgvector** (the Takım data plane) — module storage APIs must not change when the backend grows. Modules own their schemas; subc never owns a module's schema.
- SQLite cold-open rule (learned the hard way): set `busy_timeout` BEFORE `journal_mode=WAL`.

### 8.3 Vault
One high-trust secret store in subc-core. Auth modules get explicit, audited grants to it. This is the local seed of the PRD "gateway-held credentials" security boundary.

### 8.4 Cloud / team memory (end-game seam)
MC memories are local SQLite today; users want cross-device sync + team-scoped sharing (Cloudflare D1/DO/R2 fit). This stays **module-internal** (MC owns its schema + sync), but the storage substrate must not preclude a local↔cloud sync layer. Team sharing = the PRD permission/grant model, a cloud-side concern.

### 8.5 Auth flavors (fork 3 refined: vault in core, auth logic in modules)
- **Passthrough** (MITM): reuse the harness's bearer token, stealth/no-fingerprint (§6.7).
- **CortexKit-native** (our harness + the LLMloop): authenticate the USER to CortexKit; CortexKit-held provider creds from the vault. This is the **prerequisite** for lifting dreamer/historian out of MC into the LLMloop.

---

## 9. Reference implementations to mine

- **headroom** (`~/Work/OSS/headroom`, chopratejas/headroom) — an independent **Rust MITM compression proxy** that fought the exact cache-bust wars. **Read `REALIGNMENT/02-architecture.md` (invariants I1–I10) and `01-bug-list.md` (P0 cache-killers) first.** It independently arrived at our cache law (I1 byte-faithful, I4 determinism, I6 position-preserving) and provides the concrete technique (RawValue, `arbitrary_precision`+`raw_value`, BTreeMap, sibling-block side-channel). Its **Phases A–H** are a ready-made implementation sequence: A cache-safety lockdown → B live-zone engine → C Rust proxy paths incl. byte-level SSE state machine → D Bedrock/Vertex envelopes → E cache stabilization (deterministic sorts, auto cache_control, prompt_cache_key, drift telemetry) → F auth-mode gates → G observability → H retire Python.
- **MC** (`opencode-magic-context`) — the transform + dreamer reference. Cache TTL logic, execute thresholds, deferred-operation interplay, the historian, `context.db` schema. MC-Alfonso will give you the live contracts.
- **AFT** (this repo) — the thin-plugin/fat-Rust-module pattern, the NDJSON bridge (`bridge.ts` spawn/restart/`replaceBinary`), tiered bash output compression (the live-zone compression philosophy already working in production), the callgraph generation+pointer store, the project-filter trust model.

---

## 10. Sequencing

**Pre-daemon prep (in AFT, consensus across redesign oracles — do BEFORE the daemon so the daemon is a transport swap, not a rewrite):**
1. Extract an in-process **`ProjectActor`/`ProjectRuntime`** from AFT's `AppContext` god-object — watcher/LSP/index/bash ownership actor-scoped, stdin bridge unchanged.
2. Split AFT's `bridge.ts` into `NdjsonClient` / `BridgeSupervisor` (the daemon-shim seam).
3. Unify path identity (§8.1).
4. `StorageRegistry` + `Lease` API (§8.2).
5. Retire AFT's legacy `callgraph.rs` in favor of the store.

**subc phases:**
1. **Tool plane** — UDS + envelope + JSON-RPC + HELLO/capability + mux; AFT as the first module (its NDJSON bridge becomes a thin shim over the socket). Single watcher/index per project; sessions become cheap multiplexed clients.
2. **LLM-proxy plane + dreamer bench** — the LLMloop module + CortexKit-native auth module + one in-house codec (start with Anthropic, the cache-critical one) + MC as the transform module. Prove MC's dreamer runs headless. This is the milestone that de-risks everything.
3. **Mgmt/query plane + remote** — dashboard over TLS-TCP, many-subc-one-client.
4. **Gateway-growth** (far-future): NATS bus, containers, marketplace, org hierarchy, full hook catalog, Postgres/pgvector, cloud/team memory.

**Do NOT build yet:** NATS, containers/Firecracker, marketplace, org-hierarchy/permissions, the full PRD hook catalog, Postgres. Build the seams (transport-agnostic body, backend-swappable storage, capability-registration, MCP facade), not the distributed machinery.

---

## 11. Open forks / decisions still to make

1. **Framing:** hand-rolled length-prefix vs `h2`-with-JSON-bodies (§4.5). Affects mux/cancellation-for-free vs TS-client weight.
2. **Canonical normalized shape:** what exactly is the codec's normalized representation? (OpenCode's parts-based MessageLike is one candidate; define it so in-house + OSS codecs target the same shape.)
3. **Conformance-gate specifics:** the exact recorded-payload corpus + assertions for codec certification (§6.3).
4. **Module packaging/distribution:** how modules are installed/versioned/discovered (precursor to the marketplace) — and how subc supervises version skew across modules.
5. **Scheduler policy:** eligibility/cooldown/lease-TTL defaults for the shared dreamer (MC's current values are a starting point).

---

## 12. Glossary
- **subc / Subconscious** — the local supervisor daemon; kernel of the Takım Gateway.
- **module** — a supervised subprocess providing tools/transform/codec/auth/runner/embedding; in-house or OSS.
- **tool plane / LLM-proxy plane / mgmt plane** — subc's three surfaces.
- **codec** — provider normalize⇄denormalize + cache_control placement module.
- **transform** — context-management module (MC) operating on normalized messages.
- **the cache law** — two deterministic owners (content + breakpoint-placement); non-determinism in either busts the prompt cache.
- **MITM vs plugin-hook** — the two proxy-plane entry topologies; one transform interface, codec/auth run-or-skipped per entry.
- **LLMloop** — the headless agentic-loop runner module (dreamer/historian execution).
- **identity triple** — (session_id, harness) + project identity + model/provider.
- **PAYG / OAuth / subscription** — auth modes gating proxy aggressiveness (stealth on the latter two).
