# MC-under-SUBC + the shared cache/message foundations — consolidated design (council draft SKELETON)

Status: **SKELETON for review.** Consolidates three converged threads into one
council-reviewable artifact. Locked decisions are stated tersely as stubs; the full prose
is filled section-by-section once Ufuk approves the structure + blesses the canonical. Open
forks are marked **[OPEN]**. Companion source artifacts (to be folded in / cited):
- `docs/cache-policy-core-design.md` (this repo) — the cache-policy core contract + per-pass function.
- `magic-context/docs/cache-policy/cache-stability-golden-vectors.{json,schema.md}` — the 8 mechanics vectors.
- `magic-context/docs/cache-policy/ck-message-field-inventory.md` — the CK Message lossless contract.
- `magic-context/docs/cache-policy/mc-subc-migration-map.md` — what collapses/relocates/survives.

---

## 0. The thesis (one paragraph)
MC (the cache-stability transform) moves from an in-process OpenCode/Pi plugin to a stateful
SUBC Rust module with ONE harness-agnostic transform over a canonical `CK Message`. Most of
MC's current code is state-reconstruction + multi-writer-defense + per-harness-divergence
machinery that exists only because MC is a stateless guest re-handed a fresh array each pass
over a shared DB across two message models. A single stateful daemon + one canonical message
+ the shared cache-policy core deletes that machinery and centralizes the cache discipline.

## 1. The three layers being unified
- **Cache-policy core** (shared library, Rust+TS golden-parity): the SOFT+/SOFT/HARD
  classifier + ride-the-bust coordinator + frozen-set state machine + render-config epoch.
- **CK Message** (the canonical representation): one message model; encoders/decoders at
  every edge; CK = superset, llm-runner's canonical = the byte-affecting projection.
- **MC-under-SUBC** (the migration): MC becomes a daemon module; the transform relocates,
  the self-inflicted machinery collapses.

## 2. Cache-policy core (LOCKED — full contract in cache-policy-core-design.md; this summary UPDATED to the Round-4 boundary-presence mechanism)
- Render-on-bust / replay-verbatim-on-defer = the structural bug-class kill (no defer-pass render).
- Frozen RENDER UNITS with byte-complete (EXACT-bytes) payloads; atomic version-stamped CAS state.
- **Anchor-validity = BOUNDARY-PRESENCE** (is `boundary_id` still in the live array?) + frozen-byte
  replacement of the covered prefix — there is **NO content fingerprint over the covered prefix**
  (the old "content-fingerprint / in-prefix-retract = bust" model is SUPERSEDED). An in-prefix edit
  is summarized away (not stale-cache); a revert that removes the boundary reuses-then-reconciles on
  the next bust. `boundary_id` / `boundary_present` are opaque equality tokens, not hashes.
- 9 golden vectors (schema_version 2): 8 mechanics (V4 byte-complete + V8 revert-removes-boundary)
  + V9 cross-episode lineage. Cache frozen units are ALL `lineage`.
- **`full_array_fingerprint` (spec #4 delta/LKG whole-array staleness) is DISTINCT from the cache
  anchor (boundary-presence)** — the old `array_fingerprint == input_identity` equation was a
  conflation, now split. `computeRawRangeFingerprint` = historian snapshot validator, NOT the anchor.

## 3. CK Message canonical (LOCKED pending Ufuk blessing)
- **[DECISION — Ufuk]** CK SUPERSET, llm-runner canonical = byte-affecting PROJECTION.
  Keystone conformance invariant: `render(project(ck)) == render(project(strip_harness_flags(ck)))`.
- 3-bucket field model: (a) canonical CONTENT verbatim, (b) downstream TYPED POLICY (reusable
  decide_* via decision/emitter seam), (c) LOOP/DURABILITY invariant (ToolPairing = synthesize_hard_stop).
- Provider quirks = a DOWNSTREAM pass; MC-core stays PROVIDER-NEUTRAL (freezes CK-level units).
  Reuse seam: `shape()->Value` whole-body (owned/MITM) + per-message `decide_*` (plugin gap-fill,
  assembly stays in shape(), never lifted into the plugin consumer).
- Reasoning-strip = ONE composable `decide_reasoning` (keep iff is_last_assistant_turn AND NOT
  after_first_of_merge_group; merge term gated by serializer_profile, always-false on non-merging paths).
- **[DECISION — Ufuk]** Image/File = bucket-(a) content in CK day one; owned-path RENDER deferred
  to FileUploads; owned-path image input is fail-loud-partial in v1 (projection undefined on image → never a silent drop).
- step_context = render-PARAM (rides run-config), never a CK content field.

## 4. Serializer healing profiles + owned-path residual (LOCKED — verified)
- `quirk_work(provider, path) = provider_requirement − serializer_healing(provider, profile)`.
  A quirk is never f(provider) alone; `serializer_profile` is a required protocol field.
- Profiles (ground-truth verified): Pi = heals universally (residual zero); OpenCode @ai-sdk =
  anthropic+bedrock-only (MC carries the [dropped]/merge residual); OC-V2-core = universal.
- llm-runner 4-row map (verified at source + live): Row 3 never-merge ✅ all 5 (the structural
  win — whole merged-assistant interleaved-thinking class absent), Row 4 verbatim sigs ✅ all 5.
  Two byte-safe gaps to close: G1 empty-text-drop (defensive, byte-identical on all goldens),
  G2 reasoning_content-required (net-new default-off policy; verified live NOT a current bug).
- => owned-path residual = ZERO is achievable + bounded (close G1+G2 in the policy build).
  Two net-new shared typed policies total: EmptyContent + ReasoningContentRequired.

## 5. The daemon↔plugin boundary (Edges B–G LOCKED; A = plugin-delta [OPEN, measurement-gated])
- **B durability:** daemon persists cache-DECISION state + frozen_units, NOT content; content
  reconstructs from the harness full-array hand-off; restart = full-array + durable frozen-set → byte-identical.
- **C serializer_profile:** required request field → healing-coverage table → residual.
- **D daemon-down:** fail-open-raw REJECTED; cache-last-known-good + abort-on-anchor-divergence;
  health via subc Ping/Pong + connection-liveness + clean Error frame (not a hang).
- **E injection/delivery:** daemon owns transform-time injections (frozen-units); plugin keeps out-of-band delivery.
- **F per-session actor:** daemon serializes per-session (subc coordinator precedent; no cross-process leases).
- **G compaction-marker:** plugin-path concern the daemon never sees (store-marker injection drives
  live compaction because OpenCode/Pi re-read store each turn). MITM uses a different live mechanism
  (in-memory virtual marker + request-rewrite) — RESOLVED, see §6.
- **A delta-vs-always-full — [OPEN, MITM-gated]:** ALWAYS-FULL is the forced baseline (MITM has
  no delta channel — we intercept the full provider request). Delta is a PLUGIN-path perf add-on,
  decided later from a ~550msg/~1MB subc round-trip measurement. Build always-full first.

## 6. MITM harnesses (Claude Code, Codex) — VIABLE (MC exploration complete)
**Both harnesses fully viable.** Settled empirically via a base-URL-redirect logging proxy.

WIRE FORMATS + REDIRECT (note the per-row evidence grade):
- Claude Code [SELF-VERIFIED — request bodies directly captured through the proxy]: Anthropic
  `/v1/messages`, plain HTTP. Redirect = `ANTHROPIC_BASE_URL=http://host:port`. Also carries a
  server-side `context_management` directive (`clear_thinking_*`) — we PASS IT THROUGH unchanged in
  MITM (do NOT strip; see §6b — Anthropic owns the tail thinking-clear, residual-zero).
- Codex [HEADROOM-DOCUMENTED + consistent with observed behavior; TO CONFIRM end-to-end at build
  with a WS-capable proxy]: OpenAI Responses `/v1/responses`, **WebSocket by default** (raw-WS, note
  #346). Redirect = top-level `openai_base_url` in `~/.codex/config.toml` + a custom
  `[model_providers.headroom]` table with `supports_websockets=true` (the built-in `openai` provider
  can't be overridden directly). NOT self-captured end-to-end — the recipe comes from headroom's
  `wrap.py` + its `handle_openai_responses_ws` relay (production prior art) + note #346; a wrong-key
  redirect attempt (`chatgpt_base_url`) caught only backend housekeeping, consistent-with-but-not-a-
  direct-WS-capture. If Codex uses HTTP under some auth config, the recipe shifts — the build re-confirms.

THE KEYSTONE FINDING (the architectural fact, proven 3 ways): **store-marker injection does NOT
drive LIVE compaction on MITM harnesses** — it's resume-boundary-only. Claude Code + Codex build
the live wire from an IN-MEMORY array (store = append-only write-through, read only at resume),
unlike OpenCode/Pi which rebuild the wire from store each turn. Proof: (1) live PTY probe — mid-
session disk edit was NOT recalled, in-memory value was, on both; (2) binary recon — no session-
file watch; (3) claude-code source — compaction is `setMessages(getMessagesAfterCompactBoundary)`
on the in-memory array scanning an in-memory `compact_boundary` streaming-event, NOT the JSONL
marker; the only disk read is `loadConversationForResume` (only at session-load — startup / `--print`
/ `--resume` — never per-turn). **This is WHY request-
rewrite is mandatory for MITM, not a choice** — and it cleanly explains the plugin/MITM split.

THE DUAL DESIGN (locked):
- **LIVE compaction** = in-memory VIRTUAL compaction marker held by the MITM daemon +
  REQUEST-REWRITE of every intercepted outbound request to the compacted form (m0/m1 + post-
  boundary tail). Works because the harness sends full history on the wire and we own the wire.
- **RESTART persistence** = ALSO inject the real store marker (`isCompactSummary` / `compacted`)
  so resume loads compacted, not full history. (Both native compactions verified: Claude `/compact`
  87→5 msgs; Codex `compacted` 231503→21545 tokens. Both disableable via `DISABLE_AUTO_COMPACT` /
  `auto_compact_token_limit`.)
- **DISABLE native auto-compaction** so the harness doesn't fight the virtual boundary.
- So live-compaction mechanism differs by integration position, SAME CK transform: plugin = store-
  marker injection (harness re-reads each turn); MITM = in-memory virtual marker + request-rewrite.

## 6b. The MITM module (daemon topology — Alfonso/subc owns)
A daemon-supervised subc module that: (a) hosts the local HTTP/WS endpoint the harness is
redirected to, (b) **DECODES** the intercepted provider request → CK, (c) runs the MC transform
(consumes the cache-policy core), (d) produces the rewritten wire (see byte-fidelity below),
(e) forwards to the real provider + streams the response back.

**Two decoder FAMILIES (the bounding insight):**
- (a) HARNESS-MODEL decoders (MessageV2→CK, AgentMessage→CK) on the PLUGIN leg, keyed by HARNESS.
- (b) PROVIDER-WIRE decoders (Anthropic-wire→CK, OpenAI-Responses→CK) on the MITM leg, keyed by
  PROVIDER WIRE FAMILY, **1:1 with llm-runner's renderers** — each WireFamily gains `decode()`
  beside `render()` (the families become bidirectional; LLMRUNNER-owned work). N-providers, not
  N-harnesses: a 3rd/4th MITM harness on an existing provider wire = ZERO new decoder. The MITM
  leg never touches a harness message model — it sees only provider wire.

**Byte-fidelity on the MITM leg — pass-through, NOT a full round-trip (the risk-bounding design):**
The per-turn MITM output is `[ frozen m0/m1 PREFIX (our renderer, byte-identical replay = cache-core
frozen-unit) ] + [ post-boundary TAIL passed through VERBATIM ] + a clean SPLICE`. We REPLACE the
pre-boundary region with frozen m0/m1 and PASS THROUGH the tail — we never re-render the tail with
llm-runner's renderer, so we never require two serializers to byte-agree. `decode(wire)→CK` is
therefore needed only for (i) the anchor DECISION over the pre-boundary region and (ii) SUMMARIZING
that region into m0/m1 — NOT for reproducing the tail. So `decode` need only be a faithful
STRUCTURAL inverse over the summarized region, not a perfect byte-inverse of render.
- **MITM provider-quirk residual = ZERO:** the harness IS the provider serializer (Claude Code emits
  valid Anthropic wire; Codex valid OpenAI-Responses), so no tail message needs quirk-FIXING → pure
  pass-through is viable for quirks.
- **The MITM byte-fidelity contract (v1)** = SPLICE-VALIDITY (valid message-sequence + tool-call/result
  pairing across the boundary — boundary must NOT cut a tool arc → orphaned tool_result 400s — +
  signed-thinking continuity, = MITM Q3) + frozen-prefix replay + tail pass-through. NOT "decode is a
  perfect render-inverse." Strictly less risk.

**v1 vs v2 scope (honest framing — MITM v1 is the COMPACTION HALF of MC, not full MC):**
Pure prefix-compaction + verbatim tail captures the historian/compartment half (replace old history
with m0/m1) but NOT the TAIL-RECLAIM half — MC also drops/compresses SPENT TOOL OUTPUTS in the
working window (smart-drops: superseded edits → edit_marker; emergency drops ≥85% oldest-first; the
ctx_reduce machinery). Those are tail message mutations (a tool_result's content → `[dropped §N§]`),
and on a 2M-token session a big chunk of the savings is tail-reclaim, not just prefix-compaction.
- **MITM v1 = prefix-compaction-only + verbatim tail.** The byte-fidelity contract above holds
  EXACTLY, zero tail-mutation risk, and it's the bulk of long-session savings. Ship first.
- **MITM v2 = tail reclaim via SURGICAL BYTE-SPAN edits.** Dropping a tool output = locate that
  tool_result's content span in the harness's OWN wire bytes and splice `[dropped §N§]` in place;
  every other byte stays harness-emitted/verbatim. We STILL never re-render the tail with llm-runner's
  renderer — we edit specific spans within the harness's bytes. Cost: decode must track BYTE OFFSETS
  for mutation-target spans (offset-aware decode), beyond structurally decoding the summarized prefix.
  Cache-wise identical to a plugin-leg reclaim: the edit busts from that point and replays
  byte-identically on defer — a surgical `[dropped §N§]` span IS a frozen unit, so tail-reclaim fits
  the cache core UNCHANGED; the only new thing is offset-tracking in the wire decoder.

**Two tail items (neither forces a full re-render):**
- Claude Code's request carries `context_management:{edits:[{type:clear_thinking_*,keep:all}]}` (a
  server-side thinking-clear directive). On rewrite we PASS IT THROUGH — harmless on our thinking-free
  m0/m1 prefix; on the tail it's the harness's own directive. Do NOT strip it (stripping changes behavior).
- Tail reasoning-clearing is DELEGATED to Anthropic via that directive on the Claude Code leg → MC
  needs no tail reasoning-clearing there (another "residual = zero because the harness owns it"). Codex:
  confirm separately, same expectation.

- **headroom** (`~/Work/OSS/headroom`, Apache-2.0 Rust+Python, verified) = REFERENCE prior art, NOT
  a fork/dep. Shipping context-compression proxy wrapping claude/codex/cursor/aider/copilot via
  wire-rewrite, with a Codex `/v1/responses` WS relay + WS→HTTP fallback (`headroom-proxy`) and the
  exact `ANTHROPIC_BASE_URL` redirect recipes. Build the MITM module subc-NATIVE in Rust with OUR
  cache core; STUDY headroom's WS transport. De-risks the WS mechanic, nothing more.

## 7. What collapses / relocates / survives (from the migration map)
- COLLAPSE: empty-text sentinels, sentinel-vs-splice drop split, CAS/delta multi-writer defense,
  per-harness PARITY.md duplication.
- RELOCATE: §N§ re-prefix (→ incremental), all per-strip watermark tables (→ one frozen-set),
  the ~53ms hot path (→ held incremental array), lease subsystem (→ in-process serialization).
- SURVIVE: reduction/scheduler decisions, the visible `[dropped §N§]` placeholder, usage/boundary/
  overflow/historian-failure state, the compaction marker (plugin-path only).

## 8. The three load-bearing risks (honest relocation — many shallow → few deep)
1. Delta-anchor correctness (golden-vector anchor-validity).
2. Encode/decode byte-fidelity (the keystone projection invariant).
3. Restart recovery (reconstruct canonical + frozen-set byte-identically).
All three are centralized + golden-vector-testable — the good trade.

## 9. Open decisions for the council to pressure-test
- The CK-superset / projection split + the keystone invariant (is it sufficient + checkable?).
- The MITM dual-design (in-memory virtual marker + request-rewrite for live; store-marker for
  restart; native-compaction disabled) — validate the keystone in-memory-vs-store finding holds
  and the request→CK decoder is the right net-new boundary.
- The delta-vs-always-full fork (plugin-path only; gated on the ~550msg/1MB measurement; MITM is
  always-full by nature so the always-full path is non-optional regardless).
- The 3-load-bearing-risk concentration (is trading many shallow defenses for 3 deep invariants right?).

### Remaining empirical opens (do NOT gate the council — flagged in-doc)
- **Edge A plugin-delta:** the subc full-array round-trip measurement (Alfonso/subc owns; runs
  on request). Decides delta-as-plugin-optimization; does not affect MITM or owned paths.

---

### Build sequence (post-blessing)
cache-policy core (Rust, V4+V8 first) → CK canonical (one representation) → llm-runner
serializer G1+G2 + bidirectional WireFamily (shape() exposure + decode()) → MC transform port →
MITM module v1 (host endpoint + structural provider→CK decoder + prefix-compaction + verbatim tail,
headroom-referenced WS) → plugin/MITM/owned adapters → MITM v2 (offset-aware decode + tail-reclaim
surgical span edits).
