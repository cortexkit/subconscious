# MC Module Wiring Contract — own-harness leg (v2)

Status: DRAFT v2 — Oracle pass 1 (bg_f359db24) returned NO-GO with 7 blockers;
all folded here. ONE co-design item remains open with LLMRUNNER (§3.2: durable
message identity — two options, decision needed before freeze). Owner: subc.
Consumers: MC (transform side), LLMRUNNER (producer side).

Scope: the OWN-HARNESS leg (llm-runner as producer). MITM and plugin legs later;
leg-dependent decisions pin the own-harness resolution and mark the MITM slot.

Consolidates the five codec-boundary ledger items + the wave-2 wire rulings.
References: magic-context/docs/specs/ck-message.md (CK#1),
magic-context/docs/specs/codec.md (#2), docs/specs/mc-plugin-subc-connection.md
(#4), docs/cache-policy-core-design.md (#6).

---

## 1. The pipeline

```
llm-runner (producer)                    MC module (transform)
  canonical messages + durable ids        flatten -> block-granular items (INTERNAL)
    -> CkMessage array + id sidecar ──►   block identity (module-owned, §3)
    + usage / agent_drop_ids              classify -> HARD/SOFT/defer (cache-core)
    (subc route, Interactive)             decision surface (selection/scheduler/
                                            boundary/trigger/injection)
  render VERBATIM ◄── transformed array   [pinned system ++ m0, m1 ++ tail]
```

- **llm-runner** owns the provider render (C7) and the durable store (WAL +
  message identity). It produces CK#1 `CkMessage`s and renders the returned
  array verbatim (keystone invariant: no re-derivation/reorder/filter).
- **MC module** owns the transform: flattening, block identity, cache-stability
  state machine, decision surface.
- **subc** routes opaque bytes. No subc-core changes anywhere in this contract.

## 2. Transform-request wire

The request carries FULL CK messages (not pre-flattened thin items) plus an
identity sidecar. Rationale (Oracle #8): the decision surface reads full
`ToolCall.input` (edit-marker payloads, ctx_note action, todowrite view) and
message-level grouping (BoundaryMsg role/ordinal); and llm-runner's render side
consumes messages, not flat items — so messages are the wire unit, and the
flatten lives at MODULE INGRESS (module owns reduction identity → owns the
projection).

```jsonc
{
  "v": 1,
  "session_id": "…",
  "render_config": "…",             // frozen render-config hash basis (epoch marker)
  "messages": [                      // FULL CK#1 CkMessages, in order, INCLUDING
    {                                //  leading system message(s) — see §2.1
      "mid": "…",                   // durable per-message identity (§3.2) — sidecar
      "ordinal": 42,                 //  field, NOT part of CK#1 (never rendered)
      "ck": { /* CkMessage: role, content[], origin, provider_extras, meta */ }
    }
  ],
  "usage": {                         // caller-owned pressure ground truth (§2.2)
    "current_total_input_tokens": 123456,
    "context_limit_tokens": 200000
  },
  "agent_drop_ids": ["…"]           // control-plane side input (§2.3)
}
```

### 2.1 System is IN the array (Oracle #1)

Leading `Role::System` message(s) ride the array as ordinary CK content, marked
by position, and MC treats them as PINNED: never summarized, reordered, or
reduced (CK#1's pinned-system rule). Their bytes feed the HARD-bust hash;
`render_config` carries only the epoch marker. The transformed array returns
them verbatim in position, so the producer renders one array with no out-of-band
re-hoisting.

### 2.2 `usage` (pressure — caller-owned)

Provider-reported usage is ground truth the module cannot derive. Feeds the
85/95 bands + emergency fixedFloor. Poison analysis accepted (worst case is
cache-economics harm; the caller owns `messages` anyway). RESTART CONTINUITY
(Oracle #9): the module persists last-seen usage in ModuleMeta; on a pass with
absent/zero usage (first pass, restart) it uses `max(request.usage,
module_meta.last_usage)` so a restart cannot silently exit emergency pressure.
Compose-side budgets remain module-internal (estimator over composed m0/m1/tail).

### 2.3 `agent_drop_ids` (control signal — caller-owned)

ctx_reduce §N§ marks as flat block ids (§3.3 vocabulary — the producer addresses
blocks by `mid#index`, which the harness derives from the same id sidecar it
sent). Leg-optional (absent on MITM). Module treats the set as durable add-only,
filtered through frozen_keys. CLEARING RULE (Oracle #7): the producer clears its
accumulated set only after a transformed response for a request carrying those
ids has been RENDERED; until then every retry re-sends the identical set.

### 2.4 Scheduler input source map (Oracle #9)

Every `SchedulerInputs` field has exactly one source:

| Input | Source |
|---|---|
| current_total_input_tokens, context_limit | request `usage` (+ ModuleMeta max-merge, §2.2) |
| executeThreshold %, TTLs, bands, smart_drops, keep-Ns, reserves | config-home, frozen at bind |
| last_response_time_ms, session timing | request-derived (now_ms) + ModuleMeta |
| prior_input_sample, has_prior_drop (emergency latch) | ModuleMeta (durable) |
| last_execute_ordinal (two-pass watermark) | ModuleMeta (durable) |
| deferred_execute, drain_latch | ModuleMeta (durable) |
| overflow_error_text | request-supplied on the pass AFTER a provider overflow error (producer forwards the provider 400 body verbatim); absent otherwise |
| tail_state / items | derived from `messages` at ingress |
| pass class | module-DECIDED (Unit S) — never caller-supplied |
| boundary_present | module-derived from durable state — never caller-supplied |

## 3. Flattening, granularity, identity (the load-bearing seam)

### 3.1 Flatten at module ingress

MC projects `messages` → flat block-granular items internally: one item per
`ContentBlock`, typed fields (kind, name, file_path via PATH_KEYS, full
`ToolCall.input`, provider_executed, arc grouping, message role/ordinal
back-reference) projected 1:1 from CK#1. The reduction mechanics (slices 2/3)
keep keying on id/ordinal/bytes — unchanged. Nothing pre-flattened crosses the
wire.

### 3.2 Durable message identity (`mid`) — LLMRUNNER co-design, the ONE open item

Oracle #2 (source-verified): llm-runner's prompt model has NO durable
per-message id today — `Message` carries role/content/origin only;
`AssistantMessage.message_id` is dropped at prompt-commit; `RunStarted.input`
is bare `Vec<Message>`. So `mid` must be BUILT. Two options:

- **(a) Explicit durable id on the canonical message** (my recommendation):
  llm-runner adds a `mid` (mint-at-append, WAL-stamped) to its canonical
  message; the renderer provably ignores it (C7 test: render(with mid) ==
  render(without)). Survives store-merge AND full-replay paths by construction
  (it is data, not position), and — decisive — survives LINEAGE FORKING
  (planned from day 1): forked sessions share prefix mids naturally, while any
  positional scheme diverges at the fork point.
- **(b) WAL-projection identity** (`wal_seq/sub_index/slot` computed at
  request-build): no type change, but reproducibility must be PROVEN identical
  across the store-merge and full-replay read paths, and fork semantics get a
  bespoke rule. More fragile under every future prompt-affecting feature.

Freeze gate: LLMRUNNER picks (a)/(b) (or a variant) + commits to the C7 test;
until then §3 is design-final but id-DERIVATION-open.

### 3.3 Block identity: `mid#<block_index>` for ALL blocks (Oracle #3 fix)

`block_id = <mid>#<block_index>` uniformly — including tool blocks.
`tool_call_id` is DEMOTED to pairing/arc metadata only (it stays in the typed
fields for arc grouping and render pairing), because it is NOT session-injective:
Gemini synthesis emits per-turn `call_0`, so `<tool_call_id>#call` collides
across turns and a frozen `red:call_0#call` would cross-apply. The selection
golden's `id#call`/`id#result` strings become `mid#i` — decision logic
unchanged, mechanical golden regen at wiring (as always planned for id strings).

### 3.4 Identity invariants (cache-critical, now ENFORCED not asserted — Oracle #4)

- REPRODUCIBLE: same producer history → same ids every pass and across restart.
- INJECTIVE within a session.
- ENFORCEMENT: at first sight of a `mid`, the module persists its ordered
  block-identity vector (per-block kind + byte-fingerprint). On every later
  pass it fail-CLOSES on drift: changed block list for a live `mid`, duplicate
  ids in one request, or a frozen `red:<id>` whose target vanishes while its
  message is live. Codec/projection version is part of the HARD epoch
  (render_config), so an intentional projection change is a clean HARD, never
  silent identity drift.

## 4. Codec render obligations

1. **No-omit pairing**: reduced ToolCall renders as a valid tool_use, reduced
   ToolResult as a tool_result, even at `[dropped N]`/skeleton. Never omit a
   drop-markered block (orphaned tool_use 400s).
2. **Arc atomicity carries to render**: a reduced arc renders with all members
   present in reduced form, or (only where the grammar permits) the complete
   arc omitted together. Never a half-rendered arc.
3. **Opaque/Media v1 posture (Oracle #10): FAIL-LOUD.** Own-harness v1 rejects
   at ingress any `Opaque` block (and Media beyond llm-runner's supported set)
   with a typed error, rather than carrying a lossy projection. Full CK#1
   Opaque carriage {source, kind, raw, arc} is the MITM-leg prerequisite and
   lands there. (llm-runner's own canonical does not emit Opaque today; this
   gate is a tripwire, not a feature cut.)

## 5. Synthetic injection = a FROZEN RENDER UNIT (Oracle #5 fix)

The synthetic todo part follows the same freeze/replay discipline as every
byte-affecting unit (this is what Unit I's freeze/replay transition already
implements — the v1 draft's "re-composed each pass" wording was wrong):
- Composed ONLY on a bust pass (from the newest todowrite ToolCall.input in the
  typed tail — tail-derived capture, no new wire input on this leg).
- FROZEN with the pass; a defer replays the frozen bytes VERBATIM (never
  re-derives, even if the tail's todo state changed — the change rides the next
  bust; Unit I's Clear outcome handles a bust where no part is built).
- Marked `synthetic: true` on the wire (annotation-only, excluded from the
  cache hash), rendered by the producer as an ordinary block, never persisted
  into the producer's durable store.

## 6. Producer contract (llm-runner side)

Per transform pass, llm-runner:
1. Builds the `CkMessage` array (its canonical → CK#1 mapping; confirm
   provider_executed handling; system message(s) included, §2.1).
2. Supplies per-message `mid` + ordinal (per the §3.2 decision).
3. Supplies `usage` (last provider response), `agent_drop_ids` (accumulated;
   clearing rule §2.3), and — on the pass after a provider overflow error —
   `overflow_error_text` verbatim.
4. Sends on its MC route (unary, Interactive) and awaits the transformed array.
5. Renders the returned array VERBATIM in order (keystone invariant).
6. FAILURE POSTURE (Oracle #6 — fail-open is REJECTED): on MC-route failure,
   ONE retry with the byte-identical request; if that fails, replay the
   LAST-KNOWN-GOOD transformed array IFF full request identity matches
   `{session_id, full_array_fingerprint, render_config, serializer_profile_id}`
   (the #4 LKG rule — a stale LKG never replays against a changed input);
   otherwise FAIL the turn with a typed MC-unavailable error. NEVER send the
   raw untransformed array (it busts the cache once on the failure and again on
   recovery, and can overflow the context the transform exists to manage).
7. RETRY/COMMIT AMBIGUITY (Oracle #7): a lost response after MC's CAS commit is
   safe by construction — the retry is byte-identical (same messages, same
   drop-ids, same usage), and the module's pass evaluation is deterministic +
   idempotent at same inputs (CAS at same version re-serves the committed
   result rather than double-advancing). Producer-side inputs advance only
   AFTER a rendered response (§2.3).

Timing: once per provider call, before render, on the complete outgoing array.

SUBAGENT SESSIONS: a child/subagent session is an ordinary session (own
BindIdentity, own route, own lineage) — MC is ON by default for children
(mirroring the Pi children-with-discovery-ON model); capability gating happens
per-agent fail-closed at the tool-surface layer, never by disabling MC.

## 7. Versioning + evolution

`"v": 1` on request/response; unknown fields ignored (additive evolution). The
resolved boundary items fold back into the MC-owned specs (CK#1, codec.md) by MC
after the reverse pass; this doc is the contract of record until then.

## 8. Open items

1. **§3.2 `mid` derivation** — LLMRUNNER decision (a)/(b) + the C7
   renderer-ignores-mid test commitment. THE freeze gate.
2. MITM-leg slots (module-assigned first-seen identity; Opaque carriage;
   injection inert) — deferred by design, tracked in the ledger.
