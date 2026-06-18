---
stepsCompleted: ["step-01-validate-prerequisites", "step-02-design-epics", "step-03-create-stories", "step-04-final-validation"]
inputDocuments:
  - "_bmad-output/planning-artifacts/prds/prd-subconscious-2026-06-17/prd.md"
  - "docs/subc-core-architecture.md"
  - "_bmad-output/planning-artifacts/prds/prd-subconscious-2026-06-17/.decision-log.md"
scope: "v1 — tool plane (modes 1 & 2), concurrency-first"
---

# subconscious (subc) — Epic Breakdown

## Overview

Epic and story breakdown for **subc v1 — the tool plane**: a machine-wide daemon that supervises AFT as a managed module, reached by harnesses through `harness ⟷ subc ⟷ aft` in classic-MCP (mode 1) and thin-plugin (mode 2) modes. The headline is **concurrent non-mutating tool execution**; cross-instance dedup is secondary. The proxy/LLM plane, the mgmt plane / CK app, MC's headless dreamer, and the MITM mode are **out of v1** and tracked separately.

## Requirements Inventory

### Functional Requirements

FR1: subc runs as a persistent machine-wide daemon listening on a local Unix domain socket.
FR2: subc supervises AFT as a managed subprocess — spawn, health-monitor, restart on crash, drain, and hot-swap the binary.
FR3: subc spawns and shares exactly one AFT instance per canonical project root, reused across all harness instances and connections (cross-instance dedup).
FR4: A client performs a HELLO handshake carrying `{protocol_ver, harness, project_root, session_id, role}`; subc resolves the canonical `ProjectRootId` and returns `HELLO_ACK {channel, daemon_ver, capabilities, project_id}`.
FR5: One connection multiplexes many logical channels (route = component+session), assigned by subc at HELLO.
FR6: subc frames messages with the fixed 17-byte little-endian envelope and routes by header alone, splicing bodies without parsing them.
FR7: subc carries concurrent in-flight requests per channel (by correlation id) and returns responses out of order.
FR8: subc forwards a CANCEL frame for an in-flight correlation id to the owning module.
FR9: subc enforces a per-channel flow-control window (bounded un-acked in-flight requests).
FR10: subc answers liveness and cached status for passive polls from its own state, without forwarding to a busy module.
FR11: subc forwards asynchronous PUSH frames from a module (bash completions, watch/pattern matches, status_changed) to the owning channel.
FR12: A module registers a manifest at HELLO declaring its roles (v1: `tool_provider` with tools, `identity_scope`, `concurrency`, `emits_push`, `sub_supervises`) and bindings; subc validates and routes accordingly.
FR13: Mode 1 (classic MCP) — subc exposes AFT's tools to any MCP-capable harness via JSON-RPC 2.0, translating MCP framing only (no semantic transform).
FR14: Mode 2 (thin deep-plugin) — OC/Pi plugins forward their hoisted built-in tool slots (read/write/edit/bash/grep) to subc over the envelope protocol, preserving hoisting.
FR15: subc maintains dual-scope identity per `(session_id, project_root)` — session-scoped (undo/backup/bash-tasks/checkpoints) vs project-scoped (watcher/index/LSP/callgraph).
FR16: subc honors a module's declared `concurrency` (`serial` / `module_managed` / `stateless_parallel`) when delivering concurrent calls.
FR17: subc supervises nested child processes — module termination/hot-swap cleans up the module's child bash tasks.
FR18: Envelope version negotiation at HELLO — subc speaks the superset and negotiates down per connection.
FR19: Graceful standalone fallback — on HELLO failure, daemon-absent, or mid-session EOF, the plugin falls back to in-process AFT execution with no user-visible error.

### NonFunctional Requirements

NFR1 (Concurrency — headline): with a heavy non-mutating call in flight, concurrent quick reads/status return without queueing behind it. Delivered by subc mux + AFT's Leg 2 executor.
NFR2 (Mutation correctness): concurrent non-mutating reads never observe a partial mutation; mutating commands stay serialized/ordered. Execution consistency (incl. intra-batch) is module-owned.
NFR3 (Zero regression): AFT's tool behavior is identical from the agent's view before/after subc; AFT's test suite passes through the subc transport; no capability lost.
NFR4 (Thin-core): subc never parses message-body semantics; all tool semantics + execution consistency live in the module.
NFR5 (Latency): tool round-trip through subc stays within a small bound of the direct NDJSON bridge; concurrency must not raise single-call latency.
NFR6 (Resource — secondary): one AFT per project root machine-global → measurable RAM / process count / LSP-child reduction vs the per-instance model.
NFR7 (Hot-update): a binary swap under active load completes with 0 dropped sessions and 0 failed in-flight requests.
NFR8 (Standalone): the daemon is a discovered upgrade, never an install dependency.
NFR9 (Coverage — v1-done bar): AFT verified working through the MCP facade in ≥1 harness beyond OpenCode and Pi.
NFR10 (Correctness — flip/flop): the cross-harness embedding flip/flop is eliminated (one AFT per root owns the embedding work).

### Additional Requirements

- **Shared Rust types crate** (serde) for the envelope + manifest + JSON-RPC contract; capability/version negotiation at HELLO.
- **17-byte little-endian envelope** with frozen-prefix versioning (`len` u32@0, `ver` u8@4 fixed forever); small extensions via reserved `flags` bits / spare `type` values, structural changes via `ver` bump.
- **Module-integration manifest** vocabulary (roles = per-plane provider/consumer); v1 exercises `tool_provider` only.
- **AFT two-leg concurrency migration** (owned by AFT-Alfonso, referenced by subc stories): Leg 1 = transport→subc socket + per-root router/ProjectActor; Leg 2 = within-root reader-writer executor. subc's delivery contract is decoupled from Leg 2 timing (subc models concurrency day one; AFT grows into it).
- **Path-identity unification**: canonical `ProjectRootId`; worktree role first-class.
- **Store-and-route-without-interpreting** principle (config provenance-preservation) — applies when the mgmt plane lands (near-term, not v1 tool plane).

### UX Design Requirements

None. subc v1 is agent/harness-facing infrastructure with no UI. The CortexKit app / MC dashboard is a **mgmt-plane** concern (near-term, separate from the v1 tool plane).

### FR Coverage Map

FR1 (daemon + UDS): Epic 1
FR2 (supervise AFT): Epic 1 (spawn/health/restart/drain) + Epic 4 (hot-swap)
FR3 (one AFT/root shared across instances): Epic 1 (dedup close)
FR4 (HELLO handshake): Epic 1
FR5 (channel multiplexing/routing): Epic 1
FR6 (17-byte envelope + splice routing): Epic 1
FR7 (concurrent in-flight + out-of-order): Epic 2
FR8 (CANCEL): Epic 2
FR9 (per-channel flow-control window): Epic 2
FR10 (liveness/status from cache): Epic 2
FR11 (PUSH forwarding): Epic 1
FR12 (manifest registration / tool_provider): Epic 1
FR13 (mode 1 — MCP facade): Epic 3
FR14 (mode 2 — thin plugin, hoisting): Epic 1
FR15 (dual-scope identity): Epic 1
FR16 (honor declared concurrency): Epic 2
FR17 (nested child-process supervision): Epic 1 (basic) + Epic 4 (hot-swap cleanup)
FR18 (envelope version negotiation): Epic 1
FR19 (graceful standalone fallback): Epic 4

NFR1 (concurrency headline): Epic 2 · NFR2 (mutation correctness): Epic 2 · NFR3 (zero regression): Epic 1 · NFR4 (thin-core): Epic 1 · NFR5 (latency): Epic 1/2 · NFR6 (resource/dedup): Epic 1 (mode-2) + Epic 3 (mode-1) · NFR7 (hot-update): Epic 4 · NFR8 (standalone): Epic 4 · NFR9 (≥1 new harness): Epic 3 · NFR10 (flip/flop): Epic 1

## Epic List

### Epic 1: AFT through subc + cross-instance dedup (the spine)
A harness reaches AFT through the daemon — `harness ⟷ subc ⟷ aft` — with behavior identical to today on the OpenCode dogfood path (mode 2, hoisting preserved); and because subc owns one AFT per project root, a second harness instance on the same repo (a 2nd OpenCode window, or Pi) shares it instead of spawning a duplicate. The minimal end-to-end vertical slice that de-risks the daemon model **and closes on a felt win**.
**Demo (epic close):** "two harnesses stop duplicating / rebuilding the same repo's index."
**FRs covered:** FR1, FR2 (spawn/health/restart/drain), FR3, FR4, FR5, FR6, FR11, FR12, FR14, FR15, FR17 (basic), FR18 · **NFRs:** NFR3, NFR4, NFR6 (mode-2 dedup), NFR10 (flip/flop)
**Cross-track dependency:** AFT Leg 1 (transport NDJSON→subc socket + per-root router/ProjectActor) — owned by AFT-Alfonso. Only the final e2e story depends on it; all prior stories test against a **fake-AFT stub**.
**Story Zero (precondition):** freeze + co-sign the subc⟷AFT wire+manifest+command contract before Epic 1 (envelope locked §4.8; manifest + command set to confirm with AFT).

### Epic 2: Concurrent tool execution (the headline)
Non-mutating tools run in parallel while mutating tools stay ordered — a slow call never blocks the quick calls behind it. subc's full delivery contract.
**FRs covered:** FR7, FR8, FR9, FR10, FR16 · **NFRs:** NFR1, NFR2, NFR5
**Notes:** subc ships the contract day one (concurrent in-flight, out-of-order, cancel, per-channel windows, liveness-from-cache) and is decoupled from AFT Leg 2 timing. The headline metric (slow search + N quick reads don't queue, within a root) is realized when AFT Leg 2 (within-root reader-writer executor) lands — AFT-Alfonso's track.

### Epic 3: Any MCP harness (coverage)
Any MCP-capable harness reaches AFT through subc via the classic-MCP facade (framing-only translation, no hoisting) — proving the coverage thesis. New mode-1 harnesses automatically share the per-root AFT delivered in Epic 1, extending dedup to them.
**Demo (epic close):** "a harness we never wrote a plugin for now has full AFT."
**FRs covered:** FR13 · **NFRs:** NFR9 (≥1 harness beyond OC/Pi), NFR6 (extended to mode-1)
**Notes:** the dedup *capability* (FR3) ships in Epic 1; Epic 3 adds mode-1 consumers of it. No hoisting in mode 1 (adds aft_ tools only).

### Epic 4: Live updates & standalone resilience (robustness)
subc swaps the AFT binary under active load with zero dropped sessions, and the plugin keeps working with no daemon installed or on daemon failure.
**FRs covered:** FR2 (hot-swap), FR17 (hot-swap cleanup), FR19 · **NFRs:** NFR7, NFR8
**Notes:** hot-update (drain + re-route to a fresh AFT instance, in-flight requests preserved) + graceful standalone fallback (daemon-absent / mid-session EOF → in-process execution, no user-visible error).

---

## Epic 1: AFT through subc + cross-instance dedup (the spine)

Deliver the end-to-end `harness ⟷ subc ⟷ aft` path on the OpenCode dogfood route (mode 2, hoisting preserved), behavior identical to today, closing on the dedup felt-win. Built against a **fake-AFT stub** so every story except the final e2e one is independent of AFT Leg 1.

### Story 1.1: Freeze the subc⟷AFT contract (story zero)

As the subc and AFT teams,
I want the wire envelope + capability manifest + command set frozen and co-signed in a shared Rust types crate,
So that every later story (and the fake-AFT stub) builds against a contract that won't be re-cut underneath it.

**Acceptance Criteria:**

**Given** the locked 17-byte envelope (§4.8) and the AFT command contract,
**When** the contract is published as a shared `subc-protocol` types crate (envelope + JSON-RPC method/param shapes + manifest schema),
**Then** subc and AFT both compile against it, and the fake-AFT stub conforms to the same crate.
**And** AFT-Alfonso co-signs the manifest + command set; the envelope's frozen-prefix rule (`len`@0, `ver`@4) is documented as invariant.

### Story 1.2: Daemon-absent fallback (connect-time safety net)

As the OpenCode/Pi plugin,
I want to fall back to in-process AFT when no daemon socket is present,
So that the harness keeps working with zero subc dependency while the daemon is being built and dogfooded.

**Acceptance Criteria:**

**Given** no subc socket at the expected path (or connect refused),
**When** the plugin starts a session,
**Then** it runs AFT in-process exactly as today, with no user-visible error.
**And** the choice is logged at debug level; the daemon-present path is exercised in later stories. (Daemon-death-mid-session recovery is out of scope here — Epic 4.)

### Story 1.3: Wire envelope codec

As subc-core,
I want a pure encode/decode for the 17-byte envelope,
So that framing is correct and mux-ready before anything depends on it.

**Acceptance Criteria:**

**Given** a frame with `len/ver/type/flags/channel/corr` fields,
**When** it is encoded then decoded,
**Then** the round-trip is byte-identical (property-tested over field ranges, little-endian).
**And** malformed frames (truncated header, impossible `len`) are rejected with a typed error, never a panic.
**And** the codec reads `len`+`ver` from the frozen prefix and dispatches header length by version.

### Story 1.4: Socket transport + splice router (mux-ready)

As subc-core,
I want to accept on the Unix socket, frame in/out, and route by `channel` splicing the body unparsed,
So that messages reach the right component without subc reading their semantics.

**Acceptance Criteria:**

**Given** an in-memory echo backend on two channels,
**When** frames for channel X and channel Y arrive interleaved,
**Then** each is spliced to its backend byte-identically and demuxed correctly (**mux-ready: two channels interleave frames on the wire even though execution is serial**).
**And** the router never deserializes the body; only channel-0 (subc-addressed) frames are parsed.

### Story 1.5: Process supervision + fake-AFT stub

As subc-core,
I want to spawn, health-monitor, and restart the AFT subprocess (tested against a fake-AFT stub that speaks the wire),
So that the module lifecycle is owned by subc and every downstream story can test without the real binary.

**Acceptance Criteria:**

**Given** a fake-AFT stub conforming to the story-1.1 contract,
**When** subc spawns it, it crashes, and subc detects the exit,
**Then** subc restarts it and surfaces a structured error on any open correlation ids.
**And** the stub exposes a programmable hook (later extended in Epic 2 with per-channel delay) — it is the standard test seam for Epic 1 and Epic 2.

### Story 1.6: HELLO + capability manifest registration

As a module,
I want to register a manifest at HELLO declaring my `tool_provider` role, tools, identity scope, concurrency, and PUSH/sub-supervision flags,
So that subc exposes my capabilities and routes calls to me.

**Acceptance Criteria:**

**Given** a module connecting over the transport,
**When** it sends HELLO with a manifest,
**Then** subc validates it, allocates a channel, returns HELLO_ACK with `{channel, daemon_ver, capabilities, project_id}`, and exposes the declared tools.
**And** a version mismatch negotiates down to a common envelope version; an invalid manifest is rejected with a typed error.

### Story 1.7: Dual-scope identity (session + project)

As subc-core,
I want to key state per `(session_id, project_root)` — session-scoped vs project-scoped — resolving the canonical `ProjectRootId`,
So that undo/backup/bash/checkpoints isolate per session while watcher/index/LSP/callgraph share per project.

**Acceptance Criteria:**

**Given** two sessions on the same project root and one session on a different root,
**When** calls are routed,
**Then** project-scoped state is shared within a root and isolated across roots; session-scoped state is isolated per session.
**And** the canonical `ProjectRootId` resolution treats worktree role as first-class.

### Story 1.8: Async PUSH forwarding

As a module,
I want subc to forward my server-initiated PUSH frames (bash completions, watch/pattern matches, status_changed) on the owning channel,
So that streaming progress reaches the harness without a pending request.

**Acceptance Criteria:**

**Given** an established channel,
**When** the module emits a PUSH frame with no outstanding request,
**Then** subc delivers it to the owning channel preserving order, distinct from request/response correlation.

### Story 1.9: End-to-end — real AFT on OpenCode (single-session-serial)

As an OpenCode user,
I want my session to reach the real AFT through subc with behavior identical to today,
So that the spine is proven on the dogfood path.

**Acceptance Criteria:**

**Given** real AFT (via AFT Leg 1 transport) registered with subc,
**When** OpenCode runs a normal session through the daemon,
**Then** AFT's existing test suite passes through the subc transport with no capability lost (NFR3).
**And** this is the only Epic 1 story that depends on AFT Leg 1; all prior stories passed against the stub.

### Story 1.10: Cross-instance dedup close (the felt win)

As a developer running the same repo in two harnesses,
I want them to share one AFT per project root instead of each spawning its own,
So that I stop paying duplicate RAM/indexing and the cross-harness embedding flip/flop disappears.

**Acceptance Criteria:**

**Given** the same repository open in a 2nd OpenCode window (or Pi alongside OpenCode),
**When** both sessions run through subc,
**Then** exactly one AFT process serves the root, shared across both (FR3); process/LSP/index count does not grow with instances (NFR6).
**And** the two harnesses no longer rebuild each other's `semantic.bin` (flip/flop eliminated, NFR10).
**And** the epic closes on the demoable sentence: "two harnesses stop duplicating the same repo's index."

## Epic 2: Concurrent tool execution (the headline)

subc's full delivery contract, shipped day one and decoupled from AFT Leg 2. Tested against the fake-AFT stub from Story 1.5 (extended with per-channel delay).

### Story 2.1: Concurrent in-flight + out-of-order responses

As an agent firing parallel tool calls,
I want subc to carry many in-flight requests per channel and return responses out of order by correlation id,
So that calls are not forced into lockstep on the wire.

**Acceptance Criteria:**

**Given** N requests dispatched without awaiting,
**When** the module responds in arbitrary order,
**Then** subc correlates each response to its request by `corr`, delivering all correctly.
**And** subc honors the module's declared `concurrency` (serial / module_managed / stateless_parallel).

### Story 2.2: Cross-session concurrency (the headline demo)

As an agent with a slow call running,
I want a quick call in another session to return without waiting for the slow one,
So that a heavy scan never head-of-line-blocks unrelated work.

**Acceptance Criteria:**

**Given** the fake-AFT stub with `delay(channel A)=500ms` and `delay(channel B)=0`,
**When** a request is sent on A at t0 and on B at t0+ε,
**Then** B's response arrives before A's, and B's latency is bounded by B's own service time (<50ms), not A's 500ms.
**And** out-of-order correlation holds; both responses are correct and well-formed.
**And** this proves subc's scheduler does not block across sessions — **without** AFT Leg 2.

### Story 2.3: Request cancellation

As a harness aborting a turn,
I want to cancel an in-flight call,
So that abandoned work stops and resources free.

**Acceptance Criteria:**

**Given** an in-flight correlation id,
**When** subc forwards a CANCEL for it,
**Then** the module decides abort-safety (a non-mutating read is droppable; a mutation mid-commit completes), and subc resolves the corr id as cancelled.
**And** CANCEL is a pure-header frame (`len=0`).

### Story 2.4: Per-channel flow-control window

As subc-core,
I want a bounded number of un-acked in-flight requests per channel,
So that a slow module cannot make subc buffer unboundedly.

**Acceptance Criteria:**

**Given** a channel at its in-flight window limit,
**When** more requests arrive,
**Then** subc applies backpressure (does not over-buffer) until the module drains, without dropping or reordering.

### Story 2.5: Liveness/status answered from subc cache

As a harness polling status,
I want subc to answer liveness and cached status without forwarding to a busy module,
So that a passive poll never queues behind a heavy scan (#117 passive-poll kill).

**Acceptance Criteria:**

**Given** a module mid heavy scan,
**When** a passive liveness/status poll arrives,
**Then** subc answers liveness directly (it supervises the process) and serves cached status without forwarding.
**And** status may be briefly stale during a long serial (Leg-1-era) scan; liveness never is.

### Story 2.6: Within-root parallelism (Leg-2-gated)

As an agent firing parallel reads on one repo,
I want non-mutating tools to run concurrently within a single project root,
So that a slow search does not block quick reads in the same session.

**Acceptance Criteria:**

**Given** AFT Leg 2 (within-root reader-writer executor) landed,
**When** a slow non-mutating call and N quick non-mutating calls are issued on the same root,
**Then** the quick calls return without waiting for the slow one; mutating calls remain serialized.
**And** this AC is **explicitly Leg-2-gated** and kept lexically separate from Story 2.2 — the cross-session metric must not be claimed as the within-root one.

## Epic 3: Any MCP harness (coverage)

### Story 3.1: Classic-MCP facade

As any MCP-capable harness,
I want to reach AFT's tools through subc over MCP,
So that I get AFT without a bespoke deep plugin.

**Acceptance Criteria:**

**Given** an MCP harness connected to subc,
**When** it lists and calls tools,
**Then** subc translates MCP framing ↔ the envelope/JSON-RPC with no semantic transform, exposing AFT's tools as MCP tools (no hoisting; adds `aft_` tools only).
**And** the harness shares the same per-root AFT delivered in Epic 1 (dedup extends to mode 1).

### Story 3.2: Verify a third harness end-to-end (v1-done bar)

As the subc team,
I want AFT proven working through the MCP facade in ≥1 harness beyond OpenCode and Pi,
So that the coverage thesis is demonstrated, not asserted.

**Acceptance Criteria:**

**Given** a chosen third harness (e.g. Claude Code or Cursor),
**When** it runs AFT tools through subc end-to-end,
**Then** core AFT tools work (read/search/outline/zoom/callgraph) and the epic closes on: "a harness we never wrote a plugin for now has full AFT."

## Epic 4: Live updates & standalone resilience (robustness)

### Story 4.1: Hot-swap the AFT binary under load

As an operator updating AFT,
I want subc to swap the AFT binary with no dropped session and no lost in-flight request,
So that AFT's high release cadence doesn't force harness restarts.

**Acceptance Criteria:**

**Given** active sessions with in-flight requests,
**When** subc drains the old AFT and routes to a fresh instance,
**Then** 0 sessions drop and 0 in-flight requests fail (NFR7); the client connection stays up throughout.

### Story 4.2: Nested child-process cleanup on swap/termination

As subc-core,
I want module termination/hot-swap to clean up the module's child bash tasks,
So that no orphaned processes leak across a swap.

**Acceptance Criteria:**

**Given** AFT with running child bash tasks,
**When** AFT is terminated or hot-swapped,
**Then** its child processes are cleaned up (subc → AFT → bash nesting), verified by process count returning to baseline.

### Story 4.3: Daemon-death-mid-session recovery

As a harness whose daemon dies mid-session,
I want graceful degradation back to in-process execution,
So that a daemon crash never breaks an active session.

**Acceptance Criteria:**

**Given** an active session using the daemon,
**When** the daemon dies mid-session (EOF),
**Then** the plugin falls back to in-process execution with no user-visible error, reconciling any in-flight requests.
**And** this is distinct from Story 1.2 (connect-time absence): this is runtime recovery under live load.

---

## v1 Hardening (inversion + assumption-audit pass)

Added ACs and spikes from the pre-implementation elicitation pass. Fold into the parent stories at the story-file stage; kept here as a traceable layer. Gaps flagged by BOTH methods marked ⚑ (highest priority — all are silent-failure classes that pass the current demos while violating a core contract).

### Story Zero (1.1) — strengthened
- ⚑ **Command→frame-shape mapping gate:** the contract freeze is gated on a reviewed mapping of all ~67 AFT commands + every PUSH type to a concrete frame sequence (single req/resp vs STREAM_DATA/STREAM_END vs bulk-lane) — proves the envelope carries real payloads before freeze (avoids a post-freeze `ver` bump).
- ⚑ **Dual conformance suite:** one shared conformance suite runs against BOTH the fake-AFT stub AND a minimal real-AFT build; both pass identically — so stub-green is evidence, not tautology.
- ⚑ **Per-command scope table:** all ~67 commands tagged session / project / both, co-signed with AFT — the validated input to dual-scope identity (1.7).
- ⚑ **v1 Definition-of-Done split:** spine-done = Leg 1 + dedup (1.9/1.10); headline-done = Leg 2 (2.6). AFT's AppContext substrate-conversion inventory is a HARD input (it sizes Leg 2), not a cosmetic follow-on. Leg 2 is treated as critical path.

### Epic 1 — added ACs
- **1.5 (supervision/stub):** (a) ⚑ stub conformance-tested against a **recorded real-AFT trace** (a read, a callgraph cold-build, a bash-with-PUSH, a cancel-mid-scan) — frame sequence + timing-class must match; (b) **module-death mid-mutation:** subc fails the owning corr-id with an error distinguishable from a normal failure; no stale corr-id from the dead instance resolves on the new instance; session-state loss is reported, not silently presented as empty. (Distinct from Epic 4 daemon-death.)
- **1.6 (HELLO):** **no-common-version floor** — when HELLO finds no common envelope version, subc rejects with a typed `version_unsupported` and admits no channel; no frames spliced on an un-negotiated version.
- **1.7 (identity):** ⚑ **ProjectRootId canonicalization** property-tested over symlinked paths, relative-vs-absolute entry, a git worktree of the same repo, and the `git:`→`dir:` fallback boundary — distinct roots never collide to one id; the same root via different path spellings always resolves identically. + the per-command scope table is exercised here.
- **1.8 (PUSH):** under two sessions on one shared AFT, session A's bash-completion PUSH is delivered only to A; a PUSH whose owning channel has closed is dropped cleanly (logged, no cross-delivery, no panic).
- **1.9 (e2e):** ⚑ **hoisting fidelity** — a hoisted built-in (`read`/`write`/`edit`/`bash`/`grep`) resolves to AFT's implementation through subc (verified by an AFT-only behavior, e.g. line-numbered read / backup-on-write), not the harness's native tool. (Mode-2's defining property — currently unverified.)
- **1.10 (dedup):** ⚑ **session-isolation under sharing** — with one AFT serving two sessions on a root, a session-scoped op in A (undo, checkpoint restore, bash-task list) never observes or mutates B's state; undo/backup stacks partitioned by `(session_id, project_root)`.
- **1.11 (NEW — latency gate):** ⚑ single-call round-trip through subc (mode-2, real AFT) measured against the direct NDJSON bridge baseline for representative quick tools (read/outline/status); overhead within the agreed bound (define it); recorded as a **regression gate**, not a one-off. Concurrency must not raise single-call latency (tie to 2.2).

### Epic 2 — added ACs
- **2.1 (in-flight):** ⚑ **per-channel FIFO _delivery_ order** — subc delivers a single channel's requests to the module in submission order even while carrying them concurrently in-flight; verified by a stub that records delivery order under interleaved multi-channel load. (This is the precondition that makes "intra-batch ordering is the module's problem" actually true.)
- **2.3 (cancel):** a CANCEL racing a RESPONSE on the same corr-id resolves the corr-id **exactly once** (cancel-then-late-response dropped, not mis-delivered); a duplicate CANCEL is idempotent; a CANCEL for an unknown/already-resolved corr-id is a typed no-op, never a panic or cross-talk.
- **2.4 (flow-control):** two channels routed to one serial (Leg-1) module, channel A saturating its window → channel B still makes forward progress (**no starvation**); backpressure on A never blocks subc's read loop or B's delivery.

### Epic 3 — added
- **Spike B (before Story 3.1):** half-day MCP-facade transform trace — 3 representative tools (a plain read, a PUSH-emitting bash, a cancellable callgraph build) hand-traced MCP-client → facade → envelope → AFT → back. If any transform beyond byte-reframing is required (tool namespacing, JSON-Schema reshape, ERROR→MCP error, PUSH→MCP progress/notification, cancel mapping), Story 3.1 gains an explicit **"MCP facade transform layer"** AC and is reclassified (not NFR4-pure). Validates assumption A2 ("facade is framing-only") before building on it.

### Epic 4 — added ACs
- **4.1 (hot-swap):** across a swap, session-scoped state (undo/backup stacks, running bash tasks, checkpoints) **survives OR the swap is deferred until safe** — a live session never silently loses undo history or orphans a bash task. + **manifest re-read on swap:** the new instance's tool set is reflected to connected harnesses; a call to a tool the new instance no longer provides returns a typed `unknown_tool`, never a hang.
- **4.3 (daemon-death):** fallback to in-process is **mutually exclusive** with daemon-routed execution — an in-flight mutating request at daemon death is completed-via-daemon OR retried-in-process, never both; the in-process AFT and any surviving daemon-owned AFT never write the same project index concurrently (no two-writer flip/flop).

### Spike A (before Epic 2 — highest priority)
⚑ **Stub-fidelity spike (~1–2 days):** capture a real-AFT NDJSON trace for a representative command mix; build the Story-1.5 stub to match the **recorded** frame sequence + timing class, not an idealized contract. Converts stub-green from tautology to evidence, and wire-tests the contract freeze (de-risks the "envelope carries everything" assumption A5). Both elicitation methods independently nominated this as THE spike to run first — the single highest silent-risk in the plan (9/10 Epic-1 stories + all of Epic 2 validate against the stub).

---

## Council review — applied changes (disposition of audit bg_7b31d58e)

### New stories (A — approved)

**Story 1.0: Daemon bootstrap, discovery & singleton (per-user)**
As a harness starting up, I want to find-or-start exactly one subc daemon for my user, so that FR3's one-AFT-per-root guarantee has a daemon to enforce.
- **Per-user socket:** socket at a per-user path (`$XDG_RUNTIME_DIR/subc.sock`, fallback `/tmp/subc-$UID.sock`); two users on one machine never collide; perms 0600 (owner-only).
- **Singleton (race-free):** concurrent harness launches converge on ONE daemon via atomic socket bind / lockfile (loser connects to the winner); a stale socket from a dead daemon is detected and reclaimed, not fatal.
- **Discovery:** no daemon present → auto-start one OR fall to in-process (Story 1.2); the choice is explicit.
- **Conflict guard (Ufuk):** the per-user socket path + bind-race resolution IS the conflict guard — no fixed TCP port in v1 (the UDS path is the identity). If a TCP/mgmt port is ever added (remote plane), it must be per-user-allocated, never fixed.

**Story 1.8b: TypeScript mode-2 plugin client**
As an OpenCode/Pi plugin, I want a thin TS client that forwards hoisted built-in slots to subc over the envelope, so that the harness-facing half of mode 2 exists (gates 1.9/1.10).
- TS client speaks the frozen envelope (length-prefix framing, HELLO, request/response, CANCEL, receives PUSH).
- Forwards the hoisted slots (`read`/`write`/`edit`/`bash`/`grep`); falls back in-process on connect failure (ties to 1.2).
- **Thin:** no tool logic locally beyond forwarding + fallback.
- **Sequence:** lands before Story 1.9 (the e2e needs it); tests against the fake-AFT stub, so independent of AFT Leg 1.

### Scope cut (B — approved): Epic 4 relaxed to drain-to-quiescence
- **4.1 is now drain-to-quiescence:** on swap, finish in-flight requests, briefly pause new ones, swap, resume — NOT zero-pause-under-load. Satisfies the release-cadence motivation. **Zero-pause hot-swap deferred to v1.1.**
- State survival simplifies: drain means nothing is mid-action at swap (running bash tasks drain or hand off; undo/checkpoints are AFT-persisted and reattach after swap).

### C — tenancy decided: per-user daemon
subc is **per-user** (one daemon per OS user, per-user socket). No machine-wide multi-user daemon in v1. Conflict guard = per-user socket path + race-free bind (Story 1.0), perms 0600.

### D — headline framing (recorded honestly; do NOT gate Epic 2 on Leg 2)
v1 delivers concurrency in three honest tiers:
1. **Concurrency CONTRACT** (mux / cancel / windows / out-of-order) — subc-owned, ships in v1.
2. **CROSS-SESSION concurrency** (slow call in session A doesn't block session B) — subc-owned, demoable in Epic 2 **without** Leg 2.
3. **WITHIN-ROOT parallelism** (the PRD's actual stated pain: a slow call not blocking quick reads in the *same* session/repo) — **Leg-2-gated**, arrives when AFT Leg 2 ships.

subc's Epic 2 ships tiers 1+2 independently; tier 3 is stated as "arrives with AFT Leg 2," never claimed before. **→ AFT Leg 2 is the true critical path for the felt headline** (size it with AFT; the substrate inventory is the sizing input).

### Mechanical fixes (folded; merge into stories at story-file stage)
- **Spike A → Story 1.1 exit criteria** (it PRODUCES the freeze, not a post-hoc check). The recorded real-AFT trace is captured from **today's NDJSON AFT** (no Leg 1 needed) — this resolves the dual-conformance contradiction: Story Zero = freeze + stub-matches-recorded-trace; real-AFT-through-subc conformance stays at Story 1.9 (already Leg-1-gated).
- **Freeze gate trimmed:** the command→frame-shape mapping covers a **~8–10 command e2e subset** exercising each frame-shape CLASS (single req/resp, STREAM_DATA/END, bulk-lane, PUSH, cancel) — NOT all ~67 (which contradicts the versioned-envelope extensibility).
- **Channel lifecycle (Story 1.6):** define open (HELLO_ACK) → active → GOODBYE/teardown; channels close on session end / disconnect; resources released; orphaned-channel cleanup.
- **subc-protocol crate home:** the shared types crate gets a defined repo + cross-team versioning (semver + HELLO capability negotiation); decide repo topology before Story Zero closes.
- **Spike B earlier (before Epic 2):** run the MCP-facade transform trace early (cheap) so any MCP cancel/PUSH-expressibility gap feeds back into Story 2.3 (cancel) and 1.8 (PUSH), not surfacing in Epic 3.
- **2.5 status-cache protocol:** define how subc's status cache is populated (module PUSH on status_changed) + its staleness bound (the Leg-1 mid-scan window).
- **1.8 PUSH flow control:** PUSH frames need backpressure too (a flood of watch/status PUSH must not overwhelm subc or the client).
