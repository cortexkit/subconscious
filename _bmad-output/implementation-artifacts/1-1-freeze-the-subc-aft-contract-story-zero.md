# Story 1.1: Freeze the subc⟷AFT contract (Story Zero)

Status: ready-for-dev

<!-- Note: Validation is optional. Run validate-create-story for quality check before dev-story. -->

## Story

As the **subc and AFT teams**,
I want the **subc⟷AFT wire envelope + capability manifest + command set frozen and co-signed in a shared Rust types crate, validated against a recorded real-AFT trace**,
so that **every later story (and the fake-AFT stub) builds against a contract that won't be re-cut underneath it — the single highest silent-risk in the plan is retired before any dependent code exists**.

> **This is Story Zero: a SPEC + CONTRACT story, not a feature build.** Its output is a frozen, co-signed contract crate + conformance fixtures + decisions, not running daemon behavior. Stories 1.0/1.2–1.11 and all of Epic 2 depend on it. It folds in **Spike A** (recorded real-AFT trace).

## Acceptance Criteria

1. **AC1 — Shared `subc-protocol` crate exists and both sides compile against it.**
   **Given** the locked 17-byte envelope (architecture §4.8) and the AFT command contract,
   **When** the crate is published with: (a) the envelope types, (b) the JSON-RPC method/param/result shapes for the command set, (c) the capability-manifest schema (`tool_provider` role for v1),
   **Then** subc and a minimal AFT build both compile against it, and the crate is the single source of truth (no duplicated wire definitions).
   **And** the crate's repo home + cross-team versioning mechanism (semver + HELLO capability negotiation) are decided and documented.

2. **AC2 — Frozen-prefix invariant is encoded and documented.**
   **Given** the envelope spec,
   **Then** `len` (u32 @ offset 0) and `ver` (u8 @ offset 4) are documented as fixed-meaning/position-forever; the crate's decode reads the 5-byte prefix first and dispatches header length by version; little-endian; `len` = body bytes after the 17-byte header.

3. **AC3 — Command→frame-shape mapping (trimmed gate).**
   **Given** a representative ~8–10 command subset exercising every frame-shape CLASS,
   **When** each is mapped to its concrete frame sequence,
   **Then** the mapping covers: single req/resp (e.g. `read`, `outline`), STREAM_DATA/STREAM_END (e.g. large `read`/callgraph dump), bulk-lane (embedding vectors — boundary vs `len`-u32), PUSH (bash completion, `status_changed`), and CANCEL (cancel-mid-scan). The full ~67 commands are NOT enumerated (would contradict the versioned-envelope extensibility) — the subset proves each class.

4. **AC4 — Spike A: recorded real-AFT trace, and the stub conforms to it.**
   **Given** today's NDJSON AFT (no Leg 1 needed),
   **When** a trace is captured for the AC3 command mix (a read, a callgraph cold-build, a bash-with-PUSH, a cancel-mid-scan),
   **Then** the trace is stored as a conformance fixture, and the contract crate ships a conformance suite that the (forthcoming, Story 1.5) fake-AFT stub MUST match in frame sequence + timing-class. Real-AFT-through-subc conformance is deferred to Story 1.9 (already Leg-1-gated) — this AC resolves the dual-conformance/Leg-1 contradiction by using a recorded trace, not a live Leg-1 round-trip.

5. **AC5 — Per-command scope table, co-signed with AFT.**
   **Given** the command set,
   **When** every command is tagged session-scoped / project-scoped / both,
   **Then** the table is the validated input to dual-scope identity (Story 1.7); AFT-Alfonso co-signs it. Baseline: undo/backup/bash-tasks/checkpoints = session; watcher/index/LSP/callgraph = project (verify per-command, surface ambiguities like inspect/diagnostics cache, LSP-per-file).

6. **AC6 — Per-user tenancy decided.**
   **Given** the per-user daemon decision,
   **Then** the socket path scheme (`$XDG_RUNTIME_DIR/subc.sock`, fallback `/tmp/subc-$UID.sock`), perms (0600), and "no fixed TCP port in v1" are documented as the input to Story 1.0 (bootstrap/singleton).

7. **AC7 — AFT co-sign.**
   **Given** the frozen manifest + command set + scope table,
   **Then** AFT-Alfonso has reviewed and co-signed them (peer coordination); divergences are resolved before the freeze is declared final.

## Tasks / Subtasks

- [ ] **T1: Stand up the `subc-protocol` crate** (AC1, AC2)
  - [x] Decide repo home: **monorepo** at `subconscious/` (Cargo workspace); AFT path-deps `crates/subc-protocol` (git-dep = CI/cross-machine upgrade). Versioning = semver + HELLO capability negotiation.
  - [x] Define envelope struct + `FrameType` enum + `Flags` bitfield + `encode`/`decode_header`, little-endian, frozen-prefix-aware (`crates/subc-protocol/src/lib.rs`; 10 unit tests green — round-trip, all frame types, pure-header, flags, layout, + 5 malformed-rejection cases). Satisfies AC2.
  - [ ] Define JSON-RPC method/param/result types for the command set (mirror AFT's `{id,command,params,session_id}` → tri-state result, reshaped to JSON-RPC 2.0)
  - [ ] Define the capability-manifest schema (`tool_provider`: tools, identity_scope, concurrency, emits_push, sub_supervises; bindings). **NO cardinality field** — all v1 modules are singletons (topology decision); AFT self-demuxes by project_root.
- [ ] **T2: Command→frame-shape mapping** (AC3)
  - [ ] Pick the ~8–10 representative commands covering each frame class
  - [ ] Document the frame sequence per command; draw the bulk-lane vs `len`-u32 boundary
- [ ] **T3: Spike A — recorded real-AFT trace + conformance suite** (AC4)
  - [ ] Capture NDJSON from today's AFT for: `read`, callgraph cold-build, bash-with-PUSH, cancel-mid-scan
  - [ ] Store as fixtures; write the conformance suite the fake-AFT stub (Story 1.5) must pass (frame sequence + timing-class)
- [ ] **T4: Per-command scope table** (AC5) — tag all commands session/project/both; resolve ambiguities; get AFT co-sign
- [ ] **T5: Tenancy spec** (AC6) — socket path scheme + perms + no-TCP-port, as Story 1.0 input
- [ ] **T6: AFT co-sign** (AC7) — send frozen manifest + command set + scope table to AFT-Alfonso (peer `AFT`); incorporate feedback; declare freeze final

## Dev Notes

**This story produces a contract, not daemon behavior.** The 17-byte envelope is already locked — do NOT redesign it; encode it faithfully.

- **Envelope (architecture §4.8, locked):** `len` u32@0 (body bytes after header; 4 GiB cap, large data streams via bulk lane) · `ver` u8@4 · `type` u8@5 (REQUEST/RESPONSE/PUSH/STREAM_DATA/STREAM_END/ERROR/CANCEL/PING/PONG/HELLO/HELLO_ACK/GOODBYE) · `flags` u8@6 (bit0 BINARY · bits1-2 PRIORITY passive/interactive/background · bit3 LAST · 4-7 reserved) · `channel` u16@7 (route = (component,session); 0 = subc) · `corr` u64@9. Little-endian. CANCEL/PING/PONG/GOODBYE are pure-header (len=0). **Frozen-prefix rule:** `len`@0 + `ver`@4 fixed forever.
- **AFT command contract source:** `~/Work/Projects/CortexKit/aft/.alfonso/plans/aft-module-command-contract.md` (~67 commands; current wire `{id,command,params,session_id}` → tri-state `{id,success,...}`; JSON-RPC reshape is mechanical). Groups: lifecycle, read, edit, search, imports, refactor, callgraph, inspect, lsp, safety, bash(+PUSH), git_conflicts, filters, db_state.
- **Manifest schema (architecture §4.3–4.7):** v1 exercises only `tool_provider`. AFT declares: `concurrency: module_managed` (it owns the reader-writer executor), `emits_push: true`, `sub_supervises: true`, `identity_scope: [session, project]`. `mutates` is per-tool metadata (observability only; subc never acts on it).
- **Dual-scope identity (AC5):** session-scoped = undo/backup/bash-tasks/checkpoints; project-scoped = watcher/index/LSP/callgraph. This table is the contract Story 1.7 implements.
- **Why the recorded-trace approach (AC4):** real AFT mutates on its read path (RefCells), lazy-loads models, streams PUSH unpredictably (architecture §6.4). A stub written to an idealized contract would be a tautology; a stub matching a recorded trace is evidence. Capture from today's NDJSON AFT so no Leg 1 is needed.
- **Concurrency interlock (architecture §6):** the crate must carry everything the delivery contract needs (corr-id, channel, cancel, flags-priority) so subc ships concurrency day one and AFT grows into it (Leg 1 serial → Leg 2 concurrent) without a contract change.

### Project Structure Notes

- New crate `subc-protocol` (repo home TBD in T1). Shared by subc-core and AFT (and later the TS plugin client, which mirrors the framing in TS).
- No subc daemon code in this story — types, fixtures, mapping docs, decisions only.

### References

- [Source: docs/subc-core-architecture.md#4.8 The wire envelope & versioning]
- [Source: docs/subc-core-architecture.md#4.3-4.7 Capability registration / roles / bindings]
- [Source: docs/subc-core-architecture.md#6 Concurrency & the delivery contract]
- [Source: _bmad-output/planning-artifacts/epics.md#Story 1.1 + v1 Hardening (Story Zero) + Council additions]
- [Source: ~/Work/Projects/CortexKit/aft/.alfonso/plans/aft-module-command-contract.md]
- [Source: .decision-log.md — frozen envelope, per-user tenancy, dual-scope split, Spike-A-into-Story-Zero, dual-conformance fix]

## Dev Agent Record

### Agent Model Used

Alfonso (subc-core) — Story Zero implementation

### Debug Log References

### Completion Notes List

- Ultimate context engine analysis completed — comprehensive developer guide created.
- **2026-06-18:** Monorepo initialized (`git init` + Cargo workspace). `subc-protocol` crate created with the **envelope codec** (locked §4.8): `EnvelopeHeader` encode/`decode_header`, `FrameType`, `Flags`/`Priority`, frozen-prefix decode discipline, typed `DecodeError`. 10 tests green. **AC2 done; AC1 partial** (envelope types in; JSON-RPC command shapes + manifest schema remain). Tenancy (AC6) decided (per-user socket, 0600, no TCP port). Topology decided: AFT = singleton, no manifest cardinality field.
- **Remaining:** JSON-RPC command body types + manifest schema (rest of AC1); command→frame-shape mapping (AC3); Spike A recorded real-AFT trace + conformance suite (AC4, needs real AFT); per-command scope table (AC5, folds in AFT's forthcoming substrate inventory); AFT co-sign of manifest+commands+scope (AC7). Envelope already AFT-signed-off (no change).
- **Baseline commit:** pending Ufuk's go-ahead (repo has no commits yet; needed before mason delegation).

### File List

- `Cargo.toml` (workspace root) — new
- `crates/subc-protocol/Cargo.toml` — new
- `crates/subc-protocol/src/lib.rs` — new (envelope codec + tests)
- `.gitignore` — new
