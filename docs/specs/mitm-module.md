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
  full history. **Write ordering (Oracle SF): persist MC's frozen-set state FIRST, THEN write the
  harness marker, stamped with the `mc_state_version` + session id.** The MC durable boundary and
  the harness store marker are two different stores; on a resume mismatch (marker version ≠ MC
  state, or a corrupt/half-written marker) treat it as a HARD bust/reconcile — rematerialize against
  the live array, never blindly replay a frozen set whose marker can't be confirmed against MC's
  state. A half-written marker is thus always safe (it loses the marker, falls back to a fresh
  fold, never replays stale frozen bytes).
- **DISABLE native auto-compaction** (`DISABLE_AUTO_COMPACT` / `auto_compact_token_limit`) so
  the harness doesn't compact on its own and fight the virtual boundary.

**Codex / OpenAI-Responses server-side chaining — HARD GATE, FAIL-CLOSED ON THE PRINCIPLE
(Oracle, CK#1 §5.12.5).** A chaining-capable provider can omit history the server holds. We CANNOT
compact hidden server-side history — there is nothing on the wire to rewrite, and keeping those
residual fields would double-count (hidden server history + our m0/m1 prefix). The gate is an
INVARIANT, NOT a denylist — enumerating OpenAI's server-state fields is non-convergent (each API
revision adds another: `previous_response_id`, then `conversation`, then
`input[*].type:"item_reference"`, …):

> **The MITM module compacts a request ONLY if it is provably SELF-CONTAINED FULL-INPUT — the
> entire conversation is on the wire and the request references NO server-held state. Otherwise it
> forwards the request UNTOUCHED (no compaction).** Fail-closed: an unrecognized or new field is
> treated as possible server-state → no compaction, never a guess that it's safe.

This is checked on **EVERY outbound request — every WS `response.create` event, not just connection
setup**. Known server-state references that violate self-contained-full-input (any one present ⇒ do
NOT compact):
- `previous_response_id` (present/non-null) — chains onto a stored prior response.
- `conversation` (present/non-null) — a stored conversation whose items are PREPENDED; persists
  independently of the 30-day response TTL, so it's hidden history even with `store:false`.
- any `input[*]` item of `type:"item_reference"` (`{id, type:"item_reference"}`) — a direct
  reference to a server-held input item.
- `store` not forced `false`/equivalent — leaves the response object server-stored for chaining.
The disable-native-compaction step (above) should also force `store:false` + drop
`conversation`/`previous_response_id`/`item_reference` where the harness config allows. If Codex
cannot be driven into provable self-contained-full-input mode, Codex MITM is INFEASIBLE (Anthropic,
which never chains, is the clean first MITM leg regardless). **The self-contained-full-input
PREDICATE is the contract; the field list is the known-instances note, not the gate.**

**Boundary authority (resolves the §2/§8 "stateless per call" vs "in-memory virtual boundary"
tension).** MC is the SOLE authority for the compaction boundary + frozen-set. The MITM module is
stateless-authoritative: it MAY cache only an EPHEMERAL `(session_id, mc_state_version, boundary)`
mirror for the in-flight request, and MUST refresh/validate it against MC every transform pass
(carry `mc_state_version`; on mismatch, re-fetch). The MITM mirror is never authoritative — it
cannot become a split-brain second source of boundary truth.

**Provider-wire `boundary_id` derivation (cache-core requires boundary-PRESENCE, not just
whole-message presence).** The cache core's anchor is `boundary_id` / `boundary_present` (a stable
boundary descriptor matched in the live array), so the MITM provider-wire `decode` MUST produce a
**stable boundary descriptor per provider message-array element** for MC to splice against — NOT
merely assert "whole messages." For a wire with a stable native item/message id (Anthropic message
objects), that id IS the descriptor. For a wire WITHOUT a stable native id, the codec MUST define a
deterministic descriptor (e.g. a content-derived stable element key), or MITM compaction is
INFEASIBLE for that wire (documented per-family, like the Codex full-input precondition). Without a
stable boundary descriptor the cache core cannot do boundary-presence, so this is a hard per-wire
requirement, not a should.

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
A boundary is **splice-safe** iff it is a provider-wire **message-array element boundary** that
ALSO satisfies all of the following (Oracle: role+arc validity alone is insufficient — a boundary
can be role-valid and arc-valid yet still cut a provider-rendered ASSEMBLY UNIT and reflow
neighboring blocks):
- the boundary must NOT cut any correlated ARC. This is the general form (CK#1 §5.13.3
  `OpaqueArc{kind, id, role}`): a standard tool_use↔tool_result pair, AND a provider
  `OpaqueArc` of kind `Tool` (server_tool_use↔result) OR `Approval`
  (mcp_approval_request↔response). `synthesize_hard_stop` groups an arc and MUST keep both
  halves on the same side of the boundary — an orphaned result (standard or Opaque) 400s.
  Arc-grouping survives the MITM compaction boundary (CK#1 §5.13.3).
- the boundary must NOT split a CK **render ASSEMBLY GROUP or SEGMENT** (CK#1 §5.12): a
  consecutive-assistant merge-group, a user+tool combined group, or a thinking-segment boundary
  (`moveToolUseBlocksToEnd`). Splitting one makes the raw tail no longer byte-spliceable OR
  reflows neighboring `tool_use` blocks across the cut = a different, possibly-invalid request.
- signed-thinking continuity across the boundary (a replaced/compacted earlier assistant
  turn must not strand a signed thinking block the next turn's validation needs);
- valid message-sequence/role ordering after the splice, AND whole-message presence (the
  boundary falls BETWEEN provider message-array elements, never mid-element).
Conformance: splice-safety tests for assistant merge-groups, user+tool assembly, thinking-boundary
tool-use reflow, and whole-message presence — not only the arc/signature/role checks.

### v2 — tail reclaim via surgical byte-span edits (extension)
MC also reclaims SPENT TOOL OUTPUTS in the working window (smart-drops, emergency drops,
ctx_reduce) — tail mutations, a big chunk of long-session savings. v2 does these as
**surgical byte-span edits**: locate the tool_result's content span in the harness's OWN
wire bytes, splice `[dropped §N§]` in place, every other byte verbatim. STILL never a full
tail re-render. The only new mechanism is **offset-aware decode** (byte-offset tracking for
mutation-target spans). Cache-wise identical to a plugin-leg reclaim — a surgical
`[dropped §N§]` span IS a frozen unit, so it reuses the cache core unchanged.

**Span-edits target STANDARD `ToolResult` LEAF content spans ONLY — never an `Opaque` block,
including a NESTED one.** CK#1 §5.13.4 makes `Opaque` ATOMIC wherever it appears — top-level OR
nested. The trap (Oracle): a standard typed `ToolResult.output` of kind `Content(Vec<ResultBlock>)`
can CONTAIN a `ResultBlockKind::Opaque` (CK#1 §5.6.2) — so a byte-span replacement over the WHOLE
tool-result content would overwrite the nested Opaque's bytes, violating atomicity. Therefore the
v2 offset map MUST expose **leaf spans** (the `Text` / `Json` output regions), NOT the parent
`ToolResult` span, and a span-edit edits ONLY non-Opaque leaf spans, preserving every nested
`Opaque` byte range whole. A whole Opaque server-tool result (and its whole arc) is either
summarized into the m0/m1 prefix or passed through verbatim — never span-mutated. (Shared finding:
this is the same atomic-Opaque rule the §F provider-wire encode binds — a nested Opaque is re-emitted
whole, never flattened. One rule, three consumers: encode, MITM v2 span-edit, cache-core unit.)

## 7. Transport (HTTP and WS)

**Header handling after a body rewrite (Oracle: "preserve original headers" is invalid once the
body changes).** The rule is **preserve AUTH-relevant provider headers verbatim; RECONSTRUCT
transport headers**:
- preserve verbatim: `Authorization` / `x-api-key` / the harness's auth + provider-API headers
  (e.g. `anthropic-version`, `anthropic-beta`) — the auth/semantic dimension (§9).
- recompute/rewrite: `Content-Length` (the body changed), `Host` / `:authority` (must target the
  REAL provider, not localhost), and strip hop-by-hop headers (`Connection`, `Transfer-Encoding`,
  `Keep-Alive`, etc.). Reject or normalize a COMPRESSED request body (`Content-Encoding`) — decode
  it before rewrite, re-encode or drop the header to match the emitted body.
- **Auth-safety caveat:** this assumes a HEADER/token auth scheme (Anthropic, OpenAI — true for
  both targets). If a provider ever used a SIGNED-BODY scheme (a body HMAC/signature header), a
  body rewrite would invalidate the signature → MITM is infeasible for that provider without
  re-signing, which we cannot do without its key. Both v1 targets are token-auth, so this is a
  documented non-issue today, flagged so a future signed-body provider isn't silently broken.

- **Claude Code (HTTP):** a local HTTP server; read the request body, decode/transform/
  rewrite, forward to `api.anthropic.com` with the header rule above, stream the SSE response
  back to the harness.

**Response-side rules (the response is NOT a blind passthrough).** Stream the provider response
back but: strip hop-by-hop RESPONSE headers; never forward a stale `Content-Length` (the stream is
proxied, not buffered); PRESERVE `Content-Type: text/event-stream` + the provider's
request-id/rate-limit headers (the harness reads them); do NOT auto-follow a cross-origin redirect
carrying the auth header; and do NOT retry the upstream once any response byte has been written
back to the harness (a retry-after-partial-write would duplicate/corrupt the SSE stream). The SSE
body itself passes through verbatim (we do not rewrite the response), but the event framing
(`data:` chunks, ping/keepalive events, the terminal `[DONE]`/`message_stop`) must be relayed
intact — a dropped terminator hangs the harness.
- **Codex (WS):** a WS relay with a WS→HTTP fallback — TERMINATE the harness WS and INITIATE a
  fresh upstream WS handshake (WS handshake headers cannot be blindly forwarded). **Reference
  headroom** (`~/Work/OSS/headroom`, Apache-2.0): its `headroom-proxy` has a working
  `/v1/responses` WS relay + WS→HTTP fallback — study the transport solution, build subc-native
  in Rust with OUR codecs/cache core. Do NOT fork or depend.
- Session identity is derived from the request (harness/provider-specific — e.g. a session
  header or a stable conversation id); the MITM module keys its per-session MC route on it.

## 8. Restart recovery

The MITM module is stateless per call, so its OWN restart is trivial (next request re-decodes
full). The stateful piece is MC (the frozen-set + the virtual boundary), recovered per SPEC
#5. MC's persisted state includes the **byte-complete frozen payloads of ALL cache-core
frozen_units** (the m0/m1 synthesized regions AND any v2 span-edit `[dropped §N§]` units — all
`lineage`-class per the cache-core durability model) + the boundary id + the state version — NOT
just a boundary pointer — so the rewritten prefix is byte-identical across a daemon restart (a
pointer alone would force a re-render and risk drift). The harness-side store marker (§5) covers a
HARNESS restart/resume (loads compacted, not full), with the MC-first write ordering above.

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
