# SPEC #3 — The subc MITM module

Status: **DRAFT for review.** Owner: Alfonso @ subconscious (daemon/subc). Part of the
MC-under-SUBC spec set (index: `docs/mc-subc-and-cache-foundations.md`). Depends on:
SPEC #1 (CK Message), SPEC #2 (codecs / bidirectional WireFamily), the cache-policy core
(`docs/cache-policy-core-design.md`), and the MC subc module (SPEC #5).

## 1. Purpose

Let MC's cache-stability transform apply to harnesses we do NOT control the loop of —
Claude Code and Codex — by intercepting their outbound provider requests, rewriting the
request to a compacted form, and forwarding to the real provider. The harness keeps driving
its own loop and consumes the provider response unchanged; it never knows the request was
rewritten.

This is the third integration position. The three:
- **owned** (llm-runner): we drive the loop, render CK → wire ourselves, use our own auth.
- **plugin** (OpenCode/Pi): the harness drives; a plugin shim hands CK to the MC module
  each turn; the harness serializes the (transformed) array (SPEC #4).
- **MITM** (Claude Code/Codex): the harness drives; we sit on the WIRE between harness and
  provider, rewrite the request, forward with the harness's OWN auth (this spec).

## 2. The load-bearing resolution: MITM is NOT llm-runner-the-service

The re-encode + forward stage is the **MITM module itself**, NOT a route to llm-runner. Two
reasons, both structural:

1. **Auth.** The intercepted request already carries the harness's own auth (Claude Code's
   subscription/OAuth token; Codex's ChatGPT auth). We forward with THOSE headers. Routing
   through llm-runner would inject llm-runner's *vault* auth → wrong identity, wrong billing.
   MITM is a transparent proxy on the auth dimension.
2. **Proxy, not loop.** llm-runner DRIVES an agentic loop (decides the next step, owns the
   turn). In MITM the *harness* drives the loop; we only rewrite the request body. llm-runner
   has no role.

What IS reused from llm-runner: its **WireFamily codec as a shared LIBRARY** —
`render(CK → provider wire)` for the m0/m1 prefix + the structural `decode(provider wire →
CK)` (SPEC #2's bidirectional WireFamily). Code reuse, never service routing. The MITM
module links the codec lib; it does not open a route to the llm-runner module.

## 3. Topology

```
harness (Claude Code / Codex)
   │  outbound provider request  (base-URL redirected to the local endpoint)
   ▼
┌─────────────────── subc MITM module (this spec) ───────────────────┐
│  1. receive raw provider-wire request (HTTP or WS)                   │
│  2. decode(provider wire → CK)            [SPEC #2 codec lib]        │
│  3. route CK → MC module, get transform decision back  [via subc]   │
│  4. re-encode: render m0/m1 CK → wire [codec lib] + splice tail      │
│  5. forward rewritten request → REAL provider  [harness's OWN auth]  │
│  6. stream provider response back to the harness  [verbatim, v1]    │
└─────────────────────────────────────────────────────────────────────┘
```

- The MITM module is a **daemon-supervised subc module** (entry in `subc.jsonc`), provider-
  AWARE (it owns the codecs + the wire), and **stateless per call** (it decodes the full
  intercepted request each turn — MITM is always-full by nature, no delta).
- **MC is a separate subc module**, provider-NEUTRAL + STATEFUL (holds the frozen-set +
  durable store). The MITM module is a CONSUMER of MC: it route.opens to MC and sends the
  decoded CK, receives the transform decision (the frozen m0/m1 + the boundary). This keeps
  the provider-aware proxy and the provider-neutral transform cleanly separated (MC never
  sees provider wire; the MITM module never holds cache state).
- Per-session: keyed by the harness session (derived from the request — see §7). The MC
  module serializes per-session (its coordinator).

## 4. Redirect mechanism (per-harness)

The harness is configured to send its provider calls to the MITM module's local endpoint.

- **Claude Code** [SELF-VERIFIED]: `ANTHROPIC_BASE_URL=http://127.0.0.1:<port>`. Anthropic
  `/v1/messages`, plain HTTP. The request carries a `context_management` directive
  (`clear_thinking_*`) — **pass it through unchanged** (Anthropic owns the tail thinking-clear;
  stripping changes behavior).
- **Codex** [HEADROOM-DOCUMENTED, TO CONFIRM end-to-end at build]: OpenAI Responses
  `/v1/responses`, **WebSocket by default**. Redirect = top-level `openai_base_url` in
  `~/.codex/config.toml` + a custom `[model_providers.<name>]` table with
  `supports_websockets=true` (the built-in `openai` provider can't be overridden directly).
  Confirm the exact recipe with a WS-capable proxy at build (a wrong-key attempt caught only
  backend housekeeping — consistent-with-but-not-a-direct-WS-capture).

The local endpoint config (port, which harness/provider it fronts) lives in the MITM
module's config (`~/.config/cortexkit/<mitm-module>.jsonc`, read-only consumer per the
config-home convention).

## 5. The compaction model (live vs restart) — the keystone finding

**Store-marker injection does NOT drive live compaction on MITM harnesses** (proven 3 ways:
live PTY probe, binary recon, claude-code source — they build the live wire from an
in-memory array, store is read only at resume). So:

- **LIVE compaction = request-rewrite.** The MITM module holds an in-memory virtual
  compaction boundary per session; every intercepted request is rewritten to the compacted
  form (m0/m1 prefix + post-boundary tail). Works because the harness sends full history on
  the wire and we own the wire.
- **RESTART persistence = store-marker injection.** ALSO write the harness's native store
  marker (`isCompactSummary` / `compacted`) so a resume loads from the compacted state, not
  full history.
- **DISABLE native auto-compaction** (`DISABLE_AUTO_COMPACT` / `auto_compact_token_limit`) so
  the harness doesn't compact on its own and fight the virtual boundary.

## 6. Byte-fidelity contract

### v1 — prefix-compaction + verbatim tail (ship first)
The rewritten request is:
```
[ frozen m0/m1 PREFIX ]  +  [ post-boundary TAIL passed through VERBATIM ]
   + [ request-level RESIDUAL fields spliced back ]  +  a clean SPLICE
```
The `residual` is `DecodedRequest.residual` (SPEC #2 §4): request-level JSON the summarizer
must not lose but does not interpret (unknown/forward-compat top-level fields, e.g. Claude
Code's `context_management` directive). The MITM re-encode is therefore `{frozen m0/m1
prefix} + {verbatim tail bytes} + {residual top-level fields}` — no full round-trip.
- The m0/m1 prefix is MC's synthesis, rendered CK → provider wire by the codec lib, and is a
  cache-core **frozen unit** (byte-identical replay across defer passes).
- The tail is the harness's own emitted bytes, **passed through unchanged** — we never
  re-render it with the codec, so the two serializers never need to byte-agree.
- `decode(wire → CK)` is therefore needed only for (i) the cache-core anchor DECISION over
  the pre-boundary region and (ii) SUMMARIZING that region into m0/m1 — NOT for reproducing
  the tail. So decode is a faithful STRUCTURAL inverse over the summarized region only.
- **MITM provider-quirk residual = ZERO** (the harness IS the provider serializer → no tail
  message needs quirk-fixing → pure pass-through is viable).

**The v1 byte-fidelity contract = SPLICE-VALIDITY + frozen-prefix-replay + tail-passthrough.**
Splice-validity:
- the boundary must NOT cut any correlated ARC. This is the general form (CK#1 §5.13.3
  `OpaqueArc{kind, id, role}`): a standard tool_use↔tool_result pair, AND a provider
  `OpaqueArc` of kind `Tool` (server_tool_use↔result) OR `Approval`
  (mcp_approval_request↔response). `synthesize_hard_stop` groups an arc and MUST keep both
  halves on the same side of the boundary — an orphaned result (standard or Opaque) 400s.
  Arc-grouping survives the MITM compaction boundary (CK#1 §5.13.3).
- signed-thinking continuity across the boundary (a replaced/compacted earlier assistant
  turn must not strand a signed thinking block the next turn's validation needs);
- valid message-sequence/role ordering after the splice.

### v2 — tail reclaim via surgical byte-span edits (extension)
MC also reclaims SPENT TOOL OUTPUTS in the working window (smart-drops, emergency drops,
ctx_reduce) — tail mutations, a big chunk of long-session savings. v2 does these as
**surgical byte-span edits**: locate the tool_result's content span in the harness's OWN
wire bytes, splice `[dropped §N§]` in place, every other byte verbatim. STILL never a full
tail re-render. The only new mechanism is **offset-aware decode** (byte-offset tracking for
mutation-target spans). Cache-wise identical to a plugin-leg reclaim — a surgical
`[dropped §N§]` span IS a frozen unit, so it reuses the cache core unchanged.

**Span-edits target STANDARD `ToolResult` content ONLY — never inside an `Opaque` block.**
CK#1 §5.13.4 makes `Opaque` ATOMIC: a provider-native block (server_tool_use, web_search
result, etc.) is NEVER partially edited. So v2 reclaim of an Opaque server-tool result is
not a span-edit inside it — it is whole-block: the WHOLE Opaque (and its whole arc, both
halves) is either summarized into the m0/m1 prefix or passed through verbatim in the tail,
never span-mutated. Only a standard typed-core `ToolResult.output` content region is
span-reclaimable. This keeps Opaque opacity + arc validity intact under v2.

## 7. Transport (HTTP and WS)

- **Claude Code (HTTP):** a local HTTP server; read the request body, decode/transform/
  rewrite, forward to `api.anthropic.com` preserving the original headers (auth + the
  `context_management` field), stream the SSE response back verbatim.
- **Codex (WS):** a WS relay with a WS→HTTP fallback. **Reference headroom**
  (`~/Work/OSS/headroom`, Apache-2.0): its `headroom-proxy` has a working
  `/v1/responses` WS relay + WS→HTTP fallback — study the transport solution, build
  subc-native in Rust with OUR codecs/cache core. Do NOT fork or depend.
- Session identity is derived from the request (harness/provider-specific — e.g. a session
  header or a stable conversation id); the MITM module keys its per-session MC route on it.

## 8. Restart recovery

The MITM module is stateless per call, so its OWN restart is trivial (next request re-decodes
full). The stateful piece is MC (the frozen-set + the virtual boundary), recovered per SPEC
#5 (durable store → reconstruct frozen-set; the boundary is also persisted so the rewritten
prefix is stable across a daemon restart). The harness-side store marker (§5) covers a
HARNESS restart/resume (loads compacted, not full).

## 9. Auth (explicit)

The MITM module **forwards the harness's own auth headers verbatim** and injects NONE of its
own. It does not read, store, or substitute the harness's credentials — it is a transparent
proxy on auth. (This is also why the credentials vault is NOT involved on the MITM path; the
harness owns its provider identity.)

## 10. Dependencies

- **SPEC #1 (CK Message):** the canonical the codecs decode to / render from.
- **SPEC #2 (codecs):** the bidirectional WireFamily, consumed as a LIBRARY (render m0/m1 +
  structural decode + the offset-aware decode for v2).
- **SPEC #5 (MC module):** the transform the MITM module routes CK to.
- **cache-policy core:** the frozen-set + anchor discipline MC applies (the m0/m1 frozen
  units, the splice boundary as an anchor).
- **headroom:** WS transport REFERENCE (not a dep).

## 11. Open items

- **Codex WS recipe** — confirm end-to-end at build with a WS-capable proxy (the redirect key
  + `supports_websockets` table); the HTTP fallback is the safety net.
- **v2 tail-reclaim** — offset-aware decode is the one net-new decoder capability; scoped as
  a v2 extension, not v1.
- **Session-identity derivation** per harness — the exact stable key from the request
  (header vs conversation id) — confirm at build for each harness.
- **Codex's `context_management` analog** — confirm whether Codex has an equivalent
  server-side directive to pass through (Claude Code does; Codex TBD).
