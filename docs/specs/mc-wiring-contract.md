# MC Module Wiring Contract — own-harness leg

Status: DRAFT (pre-Oracle). Owner: subc (this repo). Consumers: MC (magic-context
module — the transform side), LLMRUNNER (llm-runner — the own-harness producer
side). This is the contract that unblocks WIRING of MC's completed decision
surface (selection, scheduler, boundary/trigger, injection — all built isolated
and differential-golden'd vs TS) into the live transform, and the llm-runner ↔ MC
integration.

Scope: the OWN-HARNESS leg only (llm-runner as producer). The MITM leg (cc/codex)
and the oc/pi plugin leg come later per standing order; where a decision is
leg-dependent this doc pins the own-harness resolution and marks the MITM slot.

It consolidates the five codec-boundary items banked during MC's build waves
(note #414 + wave-2): (1) agent_drop_ids transport, (2) no-omit pairing render,
(3) flat-block granularity + module-derived identity, (4) todo-state capture,
(5) injection placement — plus the transform-request wire additions (usage,
pass-level inputs) ruled during the wave-2 contract pass.

References: magic-context/docs/specs/ck-message.md (CK#1, canonical),
magic-context/docs/specs/codec.md (Spec #2, MC-owned),
docs/specs/mc-plugin-subc-connection.md (Spec #4, this repo),
docs/cache-policy-core-design.md (Spec #6).

---

## 1. The pipeline (own-harness leg)

```
llm-runner (producer)                    MC module (transform)
  canonical messages                       flatten -> CkItem blocks (module ingress)
    -> CkMessage array  ──(subc route)──►  identity assignment (module-owned)
    + control-plane side inputs            classify -> HARD/SOFT/defer (cache-core)
    + pressure inputs                      decision surface (selection/scheduler/
                                             boundary/trigger/injection)
  render(provider wire) ◄──(response)──   [m0, m1] ++ transformed tail (CkItemWire)
```

Division of labor (locked previously, restated):
- **llm-runner** owns the provider render (C7 byte-determinism, its renderer) and
  the durable store (WAL + message identity). It produces CK#1 `CkMessage`s from
  its canonical messages and consumes the transformed array back into its render
  path.
- **MC module** owns the transform: flattening, block identity, cache-stability
  state machine (via cortexkit-cache-core), and the decision surface.
- **subc** routes opaque bytes; nothing here touches subc-core.

## 2. Transform-request wire (the #4 additions)

The transform request grows from `{session_id, render_config, items}` to:

```jsonc
{
  "session_id": "…",
  "render_config": "…",              // unchanged: the frozen render-config hash basis
  "items": [ CkItemWire… ],           // unchanged shape; see §3 for granularity
  "usage": {                          // NEW — caller-owned pressure ground truth
    "current_total_input_tokens": 123456,
    "context_limit_tokens": 200000
  },
  "agent_drop_ids": ["…"],           // NEW — control-plane side input (leg-optional)
  "pass_hint": null                   // RESERVED — see §2.3
}
```

### 2.1 `usage` (pressure — caller-owned)

Provider-reported usage is ground truth the module can never derive (it never
sees the provider response). Rides the request. Feeds the scheduler's 85/95
bands and the emergency `fixedFloor` math. Poison analysis (accepted): a lying
`usage` can only force an early HARD (benign) or delay one (overflow → provider
400 → recoverable; scheduler overflow-detection catches it) — cache-economics
harm at worst, and the caller already owns `items` so it can bust the cache
trivially anyway. Distinct from the module's COMPOSE-side budget (estimator over
its own composed m0/m1/tail), which stays module-internal.

`context_limit_tokens` is the model's context limit as the caller resolved it;
the module derives `ceiling = context_limit × executeThreshold%` with the
threshold from its own config-home (frozen at bind).

### 2.2 `agent_drop_ids` (control signal — caller-owned)

`ctx_reduce` §N§ marks, as flat-item ids (§3 identity). Leg-optional by
construction: empty/absent on legs with no ctx_reduce tool. NEVER a per-item CK
annotation (content-vs-control-signal separation: CK#1 is canonical CONTENT; a
drop mark is a CONTROL signal). The module treats the set as durable add-only,
filtered through frozen_keys.

On the own-harness leg the producer of these ids is llm-runner's harness layer
(the ctx_reduce tool result feeds back as ids of items in the array it sends).
The id vocabulary is therefore the SAME vocabulary as §3 (producer-supplied
stable ids) — no §N§→id mapping lives in the module.

### 2.3 What does NOT ride the request (poison discipline, restated)

- `boundary_present` — module-derived from durable state (poison surface).
- pass class (execute / force / defer) — the module's scheduler DECIDES it (that
  is the point of Unit S); `pass_hint` is reserved as an advisory for a future
  caller-forced-execute UX and is ignored in v1.
- prior_input_sample / has_prior_drop / last_execute_ordinal — durable in
  ModuleMeta (module-owned state).
- smart_drops, thresholds, keep-Ns, reserves — config-home, frozen at bind.

## 3. Flat-block granularity + identity (the load-bearing seam)

### 3.1 Granularity

The transform operates on a FLAT block-granular item space: **one `CkItemWire`
per CK#1 `ContentBlock`**, not per `CkMessage`. `CkItemWire` is EXTENDED with the
typed fields the decision surface reads:

```jsonc
{
  "id": "…",             // stable item id — §3.2
  "ordinal": 42,          // monotonic absolute, never positional
  "bytes": "…",           // the block's faithful bytes (render-input basis)
  "kind": "tool_call",    // NEW — projected 1:1 from ContentKind variant
  "name": "edit",         // NEW — ToolCall.name / ToolResult.tool_name (else absent)
  "file_path": "a.ts",    // NEW — ToolCall.input path keys (edit/write; else absent)
  "provider_executed": false, // NEW — server-tool arcs excluded from reduction
  "arc_id": "…",          // NEW — codec-derived arc grouping (ToolCall+ToolResult+Reasoning)
  "message_ref": {         // NEW — grouping back-reference for BoundaryMsg (wave-2 Unit T)
    "message_id": "…",    // the producer's durable message identity (§3.2)
    "role": "assistant",
    "block_index": 0
  }
}
```

Typed fields are additive; the slice-2/3 reduction mechanics (key on
id/ordinal/bytes) are unchanged. `message_ref` exists because boundary/trigger
group by MESSAGE over the flat vocabulary (Unit T's `BoundaryMsg`) — it is
grouping metadata, not identity.

### 3.2 Identity (module-consumed, producer-anchored on this leg)

Ruling (locked during the selection slice): stable block identity is a
MODULE-OWNED concern, NOT a CK#1 field — CkMessage carries no id by design.
On the OWN-HARNESS leg the module derives ids from the producer's durable
identity, which llm-runner already has:

- **message_id** = llm-runner's durable message identity (the WAL-backed id of
  the canonical message; stable across passes, resumes, and process restarts by
  construction of the WAL).
- **Tool blocks**: `<tool_call_id>#call` / `<tool_call_id>#result` — injective
  despite the shared tool_call_id, stable because tool_call_id is native
  canonical (CK#1 §5.6.1). FINAL (already used in the selection golden).
- **id-less blocks** (Text/Reasoning/RedactedReasoning/Media/Opaque):
  `<message_id>#<block_index>` — stable because a historical message's block
  list is immutable (byte-immutability of the live tail is already a cache
  invariant; a mutated history is a HARD by definition).

The FLATTEN + id-assignment happens at MODULE INGRESS (the module owns reduction
identity, so it owns the projection), consuming the producer's `CkMessage` array.
The producer supplies `message_id` per message; it does NOT compute block ids.

MITM leg (later): no producer-durable id exists → module-assigned first-seen ids
persisted in the module's own store (NOT bare content-hash — byte-identical
blocks collide; first-seen-order disambiguation required). Marked as the open
MITM slot; nothing on this leg depends on it.

### 3.3 Identity invariants (cache-critical)

- Ids are REPRODUCIBLE: same producer history → same ids on every pass and
  after restart (reduction-replay depends on `red:<id>` matching across passes;
  a re-derived id would make a frozen reduction first-apply on a defer = V3
  violation).
- Ids are INJECTIVE within a session (collision would cross-apply reductions).
- The module MUST fail loud on: duplicate ids in one request, a known frozen
  `red:<id>` whose target id vanishes while its message is still live (identity
  drift), or a `message_id` changing its block list (immutability violation).

## 4. Codec render obligations (the two render-side pins)

These bind the codec's render leg (llm-runner's renderer on this leg, the
module's compose on m0/m1):

1. **No-omit pairing**: a reduced ToolCall renders as a syntactically valid
   tool_use and a reduced ToolResult as a tool_result, even when the content is
   `[dropped N]` or a skeleton. NEVER omit a drop-markered block — an orphaned
   tool_use 400s at the provider. Pairing is preserved by construction at the
   reduction layer (in-place content replace); render must not undo it.
2. **Arc atomicity carries to render**: a reduced arc renders with all its
   members present in reduced form (call skeleton + `[dropped N]` result +
   dropped reasoning), or — only where the provider grammar permits omitting a
   COMPLETE arc — the whole arc together. Never a half-rendered arc.

## 5. Injection (Unit I capture + placement — items 4 and 5)

- **Capture (todo-state)**: TAIL-DERIVED on this leg. The current todo view is
  derived from the newest `todowrite` ToolCall.input present in the typed tail —
  no new wire input. (The fallback — a control-plane side input like
  agent_drop_ids — activates only if a leg's tail cannot carry the state; not
  needed on the own-harness leg. MITM slot: cc/codex have no todowrite; the
  injection selector is inert there.)
- **Placement**: the synthetic todo part is composed by the MODULE (it owns m1
  and the tail splice) at the position the TS implementation pins (end-of-tail
  injection unit), and rides the transformed array back as a `synthetic: true`
  wire item — annotation-only, excluded from the cache hash (ruled in slice-2:
  synthetic is a wire-envelope annotation, never hashed). The producer renders
  it as an ordinary user-visible block; it never persists into llm-runner's
  durable store (synthetic items are per-pass output, re-composed each pass).

## 6. Producer contract (llm-runner side — LLMRUNNER to confirm)

Per transform pass, llm-runner:
1. Builds the `CkMessage` array from its canonical messages via its CK#1 codec
   (Spec #2's harness-model leg for llm-runner is trivial: its canonical is
   already CK#1-shaped by design — confirm mapping, esp. provider_executed and
   Opaque carriage).
2. Supplies per-message `message_id` (durable, WAL-anchored) + role + block list.
3. Supplies `usage` from the last provider response (authoritative), and
   `agent_drop_ids` accumulated from ctx_reduce calls since the last pass.
4. Sends the transform request on its MC route (ordinary subc unary call,
   Interactive priority) and awaits the transformed array.
5. Renders the returned `[m0, m1] ++ tail` VERBATIM in array order into the
   provider request (C7: the transformed array IS the render input; no
   re-derivation, no reordering, no filtering — the keystone invariant).
6. On MC-route failure: fail-OPEN for liveness (send untransformed, flag the
   pass) or fail-CLOSED (error the turn)? **OPEN QUESTION — see §8.**

Timing: the transform runs ONCE per provider call, before render, on the
complete outgoing array (system prompt handling stays producer-side per CK#1's
pinned-system rule; MC receives it as part of render_config's hash basis, not as
a reducible item).

## 7. Versioning + evolution

- The transform request/response shapes carry `"v": 1`. Unknown fields ignored
  (no deny_unknown_fields) — additive evolution without breaks.
- The five boundary items resolved here fold back into the MC-owned specs (CK#1,
  codec.md) BY MC after the reverse pass — peer-owns-repo; this doc is the
  contract of record until then.

## 8. Open questions (for the reverse pass + LLMRUNNER)

1. **MC-route failure posture** (§6.6): llm-runner's call: fail-open
   (untransformed pass-through keeps the session alive but busts the cache and
   balloons context) vs fail-closed (turn errors; MC becomes availability-
   critical). Lean: fail-open with a loud flag + single retry, because context
   ballooning is recoverable and a dead turn is worse UX — but this is exactly
   the "MC criticality" question flagged in the daemon-degraded-boot discussion
   (note #298's per-module criticality); decide consciously.
2. **Usage staleness**: `usage` reflects the PREVIOUS provider response; on the
   first pass of a session it is absent. Module behavior on absent usage:
   scheduler runs with pressure=0 (no emergency path) — confirm acceptable.
3. **message_id for the in-flight tail**: the newest user message may not be
   WAL-committed at transform time — LLMRUNNER to confirm the id exists at the
   point the transform request is built (or define a deterministic provisional
   id rule).
4. **Opaque blocks in the flat space**: Opaque is never reduced (immovable unit
   per spec #6) but MUST still carry a stable id (it occupies coverage). The
   `<message_id>#<block_index>` rule covers it — confirm no codec special case.
