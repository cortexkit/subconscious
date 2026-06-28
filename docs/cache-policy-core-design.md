# Cache-Policy Core — Design (SPEC #6)

Status: **CONVERGED, pending Oracle gate.** Authored by Alfonso @ subconscious, brokering a
cross-product design (MC + llm-runner + future harnesses). Validated across 5 co-design rounds
with MC (the frozen-set state machine, anchor-validity, the golden vectors) and aligned to the
blessed CK#1 (`magic-context/docs/specs/ck-message.md`). The Rust cache-core binds CK#1 §3
(the `CkMessage` type) + §5.13.4 (`Opaque` as an immovable atomic unit). This doc is the
companion design that the cache-core Rust implementation binds to; it goes through the same
Oracle gate CK#1 did before the build.

**CK#1 integration (post-inversion).** The frozen-set has TWO unit categories: (a) transform
DECISION units (drop / strip / skeleton / synthesized-region / injection — the cache-core's
own frozen render units), and (b) CONTENT units the machinery must treat as atomic+immovable
but never originates — chiefly `Opaque` blocks (CK#1 §5.13.4): native-preserved, never
span-edited, never reordered (immovable under the segmentation-invariance axiom), and
arc-grouped (an `OpaqueArc{kind:Tool|Approval}` is reclaimed as a whole pair). The anchor
fingerprint covers content order including `Opaque` positions; the determinism guarantee
applies to both categories.

## Why this exists

Started from a concrete wart: the llm-runner module-surface makes every `session.send`
carry the full tool list (with schemas) every turn. Tracing it opened a deeper question —
how are tools (and more generally the prompt-cache-affecting "render config") scoped,
pinned, and changed across turns without thrashing the provider prefix cache.

The answer generalizes beyond tools and beyond llm-runner: it is the **cache-bust
coordination** problem MC already solves for opencode/Pi, which we want to (a) generalize
so every harness shares one implementation, and (b) enrich for our own harness where we
have full causal control over render-config changes.

## Scoping model (4 levels -> 1 pinned set)

Tools / MCP tools are scoped at four levels that **compose** into one resolved set:
Global, Harness, Project, Session. Only the **session** pins a concrete schema, derived at
session-start from the composition.

**Composition owner = caller (CK app / Alfonso), not the loop.** [F1 LOCKED] The caller
composes global -> harness -> project -> session (+ hiree manifests from a head agent
hiring a durable hiree) into a **flat resolved policy** (provider + tool-name selection —
NOT schemas) and hands it to the loop at session-start and on refresh. The loop resolves
schemas from the live subc catalog and pins them. Keeps the durable loop generic; keeps
scope/policy semantics where the config and the agent hierarchy live.

## The render-config epoch (SINGLE unit)

A session carries a monotonic **render-config epoch**. The epoch pins the three
prompt-cache HARD markers as ONE unit: **system + tools + model**. [LOCKED: single unit —
all three fully bust the prefix, so there is no value in sub-tracking them per-marker.]

- Every turn in epoch `e` renders the same pinned config -> identical prefix -> cache stays
  warm across turns (stickiness by construction).
- A change opens epoch `e+1`. Any pending HARD changes all drain into the single next
  transition (the coordinator rides one bust, never originates a second).
- The WAL records the transition; C7 per-run freezing is unchanged (each run freezes
  against its epoch's config; resume re-pins the right epoch).

## Refresh policy: now / defer / forced — and why it exists only for authors

Two ways a harness KNOWS a bust happened:

- **Observer (opencode/Pi via MC):** does NOT own tools / system / model; it INFERS a bust
  by watching the wire (system-prompt hash change -> HARD). By the time MC sees it, the
  host already busted. Its only lever is *ride it efficiently*. It physically CANNOT defer a
  render-config bust — it is not the author, the change is already on the wire.
- **Author (our harness):** owns every render-config mutation; knows the cause BEFORE the
  wire. This causal authorship is the ONLY thing that makes now/defer/forced possible.

Authored bust sources (our harness): user added a tool / added an MCP / changed an MCP
toolset (expand or shrink) / added or removed an MCP server / head agent added tools to a
hiree. Each is a render-config mutation -> epoch-transition candidate -> policy decision:

- **defer** (default for most): queue the transition; it rides the next HARD bust the loop
  runs anyway (model switch / context fold / another forced change). [MC invariant:
  deferred work rides the next bust, never forces its own.]
- **now / forced:** deliberately trigger a HARD fold to apply immediately; drain all other
  pending changes into that one bust.
- **provenance -> default policy** (config knob): a user MCP-toggle defaults `defer`; a
  hiree-hired-with-tools defaults `forced-now` (the capability is needed for the work the
  hiree was hired to do — waiting for a fold would be wrong).
- **pending-epoch visibility** (UX seam): deferred changes MUST be observable ("1 tool
  change pending — applies on next fold, or force now"). A deferred tool-add means the
  model literally cannot call the new tool until the epoch flips — fine for a toggle, which
  is exactly why hiree-tools cannot be defer-by-default.

## Architecture: one brain, two adapter seams

- **Layer 0 — shared core (the brain):** SOFT+/SOFT/HARD classification, the ride-the-bust
  coordinator, and the render-config epoch state machine + policy (now/defer/forced). Pure
  decision logic. Rust + TS parity, golden-vector-locked (the cortexkit-store / auth-family
  / wire-codec pattern).
- **Layer 1 — signal adapter (the observed-vs-authored split):**
  - **Observer adapter** = generalized-MC: coarse bust signals derived from host hooks
    (system-prompt hash -> HARD; usage threshold -> fold). No policy lever. Uses only the
    coordinator.
  - **Author adapter** = our harness: rich signals from causal events
    `{ level, source, what, provenance, requested_policy }`. Uses the coordinator AND the
    epoch-policy authority.
- **Layer 2 — render/store adapter (per-harness):** prefix byte rendering + storage. MC ->
  `context.db` + m[0]/m[1] messages. llm-runner -> WAL + sqlite + request renderer.
  Unchanged by this design.

MC = core + observer-adapter (the generalized extract). Ours = core + author-adapter +
richer render/store (the enriched superset). Same brain; ours authors its signals instead
of inferring them, which is what unlocks the policy lever.

## Core input contract (uniform bust-signal, two richness levels)

- Observer emits: `{ level: HARD, source: observed }` — coarse, no policy, "already
  happened."
- Author emits: `{ level: HARD, source: render_config_change, what: tools|mcp|model|system,
  provenance: ..., requested_policy: now|defer|forced }`.

The core classifies and coordinates both identically; the author's extra fields drive the
epoch-policy path the observer never invokes. One contract, two richness levels.

## Why NOT a hot-path module

The bust decision runs EVERY LLM round-trip (MC fires its transform once per step; llm-
runner decides per loop step). A per-step synchronous RPC to a cache-policy daemon would
add latency to every step AND hang the loop if the daemon is down (the host-blocking-hook
hazard we refused for AFT perm-ask). The decision MUST be in-process -> shared LIBRARY, not
a service. A module is only ever justified OFF the decision path: config authoring (already
caller-owned) and cross-harness cache observability (a future read-only telemetry sink),
both feeding the in-process decider via pushed epoch state, never a per-pass call.

## De-risking / sequencing

- **MC is the reference spec.** Its shipped gates (the `transform-postprocess` BUST/VETO
  clauses, frozen-id replay, the disjoint-DB model, the compartment veto) encode hard-won
  invariants. We do NOT rip-and-replace a shipped, battle-tested system.
- Reverse-engineer the core contract FROM MC; extract golden vectors from MC's behavior to
  lock parity.
- Consume the core in **llm-runner first** (greenfield, low blast radius); migrate MC onto
  the shared core later or never (parity held by vectors regardless).
- The render-config-epoch + author-adapter is greenfield, designed fresh. The
  taxonomy/coordinator is a faithful port of MC.

## Open questions for the MC co-design

1. Does the shared core own the epoch STATE representation, or just the DECISION over
   harness-held state? (lean: core defines the state SHAPE, each harness PERSISTS it.)
2. Full SOFT+/SOFT/HARD ladder in the core day-1, or HARD-first for llm-runner? (lean:
   design the contract for the full ladder, implement HARD-first.)
3. TS + Rust parity via golden vectors (our standard), or eventual Rust-only core + WASM/FFI
   consumed by MC?
4. Which MC-specific invariants would the classifier extraction risk breaking — what must
   the contract preserve verbatim?


---

# Co-design with MC — converged contract (rounds 1-2)

Status: **architecture converged; final artifact (golden vectors) pending.** MC (Alfonso @
magic-context) delivered its hard-won invariants and we ran two design rounds. The model
below is materially stronger than the draft above — MC's *post-frozen-id* scars (the bugs
that bit MC *after* it already had the frozen-id discipline) are exactly the traps a clean
model walks into.

## The reshape: hoist the frozen-set state machine INTO the core

The keystone correction: **byte-identical-defer-replay is a Layer-2 (render) property the
classifier does not own.** A perfectly correct classifier can still kill the cache if the
render layer re-derives bytes from moving state (window position, watermark, boundary,
current content). So the contract must protect byte-identity **structurally**, not leave
each harness to rediscover the frozen-id discipline and re-scar.

Resolution — split the frozen-id pattern by who is error-prone:
- **WHEN to freeze** (only on a bust pass) = decision logic -> **CORE**.
- **WHAT is frozen** (the affected render units + their byte-complete payloads) = state ->
  **CORE-owned value**, harness-persisted, replayed every pass.
- **HOW a unit renders to bytes** = the harness produces final bytes **on a bust pass**;
  the core freezes those bytes. **There is NO render call on a defer pass** — defer = place
  the frozen bytes verbatim. This makes defer-pass re-derivation *structurally impossible*.

## The four leaks (MC) and their integration

- **Leak A — `decision` must be a byte-COMPLETE payload, not an enum tag.** A rich unit
  (skeleton/edit-marker renders filePath + a 40-char diff-prefix) re-derived from current
  content flips bytes on defer. Fix: the harness renders final bytes at freeze time; the
  core stores `{key, kind, frozen_payload}` where **`frozen_payload` is the EXACT bytes as
  emitted on the bust pass** — never `{id, enum}`, and never "structured inputs to re-render"
  (Oracle B3: "bytes OR complete inputs" reopens the exact re-derivation bug — a defer pass
  re-rendering from inputs can diverge on renderer-version / canonicalization drift). Structured
  inputs MAY be carried only as DEBUG metadata and MUST NOT be rendered on a SOFT+ pass; the
  SOFT+ assert (`replayed bytes == frozen_payload`) only holds if `frozen_payload` is
  authoritative bytes. "Pure render" is necessary but not sufficient; "decision is byte-complete"
  is the other half. (MC's collapse to a single `[dropped N]` worked precisely because the
  payload was made trivial; rich payloads must freeze the payload.)
- **Leak B — the frozen set is RENDER UNITS, not just per-id drops.** A unit is
  `{drop | strip | skeleton | synthesized-region | injection}` — includes whole synthesized
  regions (m[0], m[1] full block bytes) AND deterministic injections (synthetic-todowrite).
  Each is an opaque byte-complete payload the harness produces on bust; the core freezes +
  replays and **never interprets the payload** (stays harness-neutral). Write-back of the
  whole set is **ATOMIC** — `new_state` is ONE value (units + markers + manifest), never
  per-unit (MC tore the cache when m[1] persisted but markers didn't).
- **Leak C — "replay every pass" is UNSAFE under revert/truncation -> "replay every pass
  WHILE ANCHORED."** If the user reverts turns or the host trims the array, the ids the
  frozen set points at vanish; blind-replay renders against a shifted/absent anchor. Fix:
  the core's FIRST per-pass step is an **anchor-validity check** — does the live input-prefix
  identity still match the identity the frozen set was computed against? match -> replay;
  mismatch -> **discard frozen set + treat as bust (fresh render)**, never blind-replay.
  Critically, **revert is HOST-caused = OBSERVED even for the author harness**, so the
  anchor check is **universal**, not observer-only — the author-side advantage covers
  *authored* changes but not revert.
- **Leak D — the transition must be VERSION-STAMPED CAS, not last-write-wins.** MC shares
  one SQLite store across opencode+pi processes; the shared core must back MC later, so
  `core(prev_state, signal) -> (new_state, action)` stamps `new_state.version =
  prev.version + 1` and defines the compare-version contract; the **harness** does the
  atomic CAS write-back (compare prev-version, swap, idempotent under retry). Single-writer
  (llm-runner) always wins uncontended; multi-writer (MC) retries — same core. This is the
  epoch-CAS primitive already shipped in `cortexkit-lease` + the credential vault. The core
  stays storage-agnostic (stamps + defines compare-version; the harness's store enforces
  atomicity).

## The per-pass core function (converged)

```
core(prev_state, pass_input) -> (new_state, action)
  pass_input = { signal, boundary_present }     // boundary_present: bool, harness derives by
                                                //   findIndex(live array, state.boundary_id) >= 0
  state      = { version,
                 boundary_id,                    // the coverage descriptor (Oracle B2): the id the
                                                 //   covered prefix is spliced out at; CAS-retry
                                                 //   re-splices the SAME id
                 frozen_units: [{ key, kind, frozen_payload, durability_class }] }
                                                 // durability_class: "episode" | "lineage"

  1. BOUNDARY CHECK (Leak C): boundary_present?
        present -> covered prefix is spliced out at boundary_id, frozen bytes replace it -> eligible to replay
        absent  -> reuse cache THIS pass; the revert reconciles on the NEXT bust (no discard, no
                   content-fingerprint divergence — in-prefix edits are summarized away, never bust)
  2. CLASSIFY (signals): SOFT+ | SOFT | HARD
  3a. on BUST: harness renders byte-complete units -> freeze into new_state (A/B),
        version = prev.version + 1, drain ALL deferred work into this one bust (coordinator);
        on RunStarted: reset "episode" units, carry "lineage" units forward (advance-only merge)
  3b. on DEFER (SOFT+): action = replay frozen units VERBATIM (no render call)
```

Pure function. The harness: renders-on-bust, places-every-pass, CAS-persists. Render runs
ONLY on bust passes -> defer-pass re-derivation is structurally impossible = the bug-class
kill. The state carries the `boundary_id` (coverage descriptor) so a CAS-retry recomputes
boundary-presence against the latest state's boundary and re-splices the same coverage (Oracle
B2/SF3): on CAS failure, reload latest state, recompute boundary-presence, apply set/advance-only
merges, retry — never against the stale pre-image. MC confirmed all four of its strip/drop scars die under this, AND it additionally
covers the regions/injections/revert/concurrency cases that bit MC *after* frozen-id — a
strictly stronger contract than MC currently ships.

## Golden-vector schema (harness-neutral; MC is extracting the first cut)

```
vector = {
  name,
  render_config,            // opaque { system_hash, tool_set_id, model_key } — no harness specifics
  initial_state,            // opaque epoch/frozen-set value, carries `version`
  passes: [ { signal, input_identity, expect_action: SOFT+|SOFT|HARD,
              expect_frozen_set_delta: [{ key, kind, frozen_payload }] }, ... ],
  assert: "for every pass i with expect_action == SOFT+: cached_prefix_bytes[i] ==
           cached_prefix_bytes[i-1]; AND replayed bytes == the unit's frozen_payload"
}
```

Bust definition (MC's, ported): a wire segment **before the final `cache_control`
breakpoint** changed between two consecutive requests. The byte-stability assert is
**conditional on `expect_action == SOFT+`** (a SOFT/HARD pass is required to change cached
bytes — that IS the bust); a revert pass MUST classify as bust, never SOFT+.

Three fields I asked MC to add before freezing the schema: (1) per-pass `input_identity`
(makes the revert/anchor case expressible + testable), (2) `frozen_payload` on each unit
(direct test for Leak A — "stable but wrong" vs mere drift), (3) `version` on state (no
concurrent vector in the first cut, but the field present means a two-writers vector adds
later with no schema break).

MC's first-cut vectors (load-bearing, from its cache-invariant E2E suite): growing-tail
defer (N passes, zero busts), watermark-crossing-an-image on defer (no first-strip),
skeleton-vs-full across a moving window (Leak A), m[1]-on-SOFT/m[0]-frozen, HARD-fold
drains all deferred drops (coordinator), provider-nonce-only change is NOT a bust,
revert-beneath-a-frozen-set (Leak C). Delivered as a single harness-neutral JSON file + a
schema doc = **the migration invariant both MC and the Rust core pin**.

## Remaining open (before build)

- MC confirms the Leak-A simplification (it freezes FULL block bytes and replays, does NOT
  re-render on defer — I believe yes from `cached_m1_bytes`, awaiting confirm).
- MC delivers the first-cut vector file (it is mid-release-train; this is its next work
  item).
- Ufuk sign-off on this converged contract before any code.

In parallel (contract-shaping, NOT building): the Rust core state value
`{version, anchor_fingerprint, frozen_units}` + pass-input `{signal, input_identity}` + the
pure per-pass function, so the harness consumes MC's vector file unchanged when it lands.


---

# Round 3 — Leak-A confirmed at source, schema frozen

**Leak-A confirmed (MC, at `inject-compartments.ts:258-292`):** on a defer pass MC does NOT
re-render m[0]/m[1] — it replays `cached.injection` verbatim; the only per-pass work is
re-splicing the frozen boundary against the freshly-rebuilt message array (a TRIM to a
frozen boundary id, not a content re-render). Re-render happens only on a bust pass. So the
simplification IS MC's reality, and the contract version is *stronger*: MC enforces
"defer returns cached" by **convention**; our model enforces it **structurally** (the core
never calls render on defer — there is no defer-path code that *could* re-derive). The
discipline becomes an invariant.

**Anchor semantics — CORRECTED to the shipped mechanism (Oracle B1, MC source-verified).**
An earlier draft of this section bound anchor-validity to a CONTENT fingerprint over the covered
prefix (MC's `computeRawRangeFingerprint` shape). That was a CONFLATION: `computeRawRangeFingerprint`
is the HISTORIAN in-flight snapshot validator (a deliberately length-based check that the
runner's raw read matches the trigger's fire-decision; a content-quality residual, NOT the cache
anchor). The actual SOFT+ cache mechanism (shipped, `inject-compartments.ts:258-292`) has **NO
fingerprint over the covered prefix at all**:

- The covered prefix is **REPLACED** by the frozen m0/m1 bytes (the `<session-history>` payload).
  It is summarized away, not re-validated.
- Defer-pass validity = **BOUNDARY-PRESENCE ONLY**: `findIndex(m => m.id === boundary_id)` over
  the live array, then `splice(0, cutoff+1)` out the covered prefix and prepend the frozen bytes.
- **Consequences (this is the whole Leak-C truth, milder than the old framing):**
  - an in-prefix **content edit** (any size, same-length or not) → the prefix is summarized away
    → **intentional lossiness, NOT stale-cache**. There is NO collision surface because there is
    no hash on the covered region. (The old "(b) content within covered prefix → bust" case does
    not exist — in-prefix edits never bust.)
  - boundary id **present** in the live array → splice succeeds against the same frozen bytes →
    **replay**.
  - boundary id **removed/moved** by a revert → `findIndex < 0` → **reuse the cache THIS pass**
    (treat as already-trimmed) + **reconcile on the next BUST pass** (rematerialize m0 against the
    live reverted array). A one-pass stale-summary window — content-quality, not cache-correctness,
    and the existing shipped behavior; NOT a new hazard.

So: **anchor-validity = boundary-presence + frozen-byte replacement, never a covered-prefix
fingerprint.** `computeRawRangeFingerprint` is a different mechanism on a different path
(historian-snapshot) and MUST NOT be cited as the cache anchor — the codec spec adds a permanent
note splitting the two so the conflation cannot recur.

**Frozen schema (the contract MC emits the vector file to):**
```
vector = {
  name,
  render_config,                  // opaque { system_hash, tool_set_id, model_key, serializer_profile_id }
  initial_state: { version, boundary_id,
                   frozen_units: [{ key, kind, frozen_payload, durability_class }] },  // "episode" | "lineage"
  passes: [ { signal, boundary_present, expect_action,
              expect_frozen_set_delta: [{ key, kind, frozen_payload, durability_class }] }, ... ],
  asserts: [
    "for every pass i>0 where expect_action==SOFT+: cached_prefix_bytes[i] == cached_prefix_bytes[i-1]",
    "for every replayed unit on a SOFT+ pass: replayed_bytes(unit) == unit.frozen_payload (EXACT, not re-rendered)",
    "every pass with boundary_present==false: expect_action != SOFT+ (reuse-then-reconcile, never a fresh SOFT+ replay against a moved boundary)",
    "a whole-frozen-set anchor mismatch ⇒ expect_action == HARD (full invalidation, never SOFT/SOFT+)",   // SF2
    "across a RunStarted boundary: 'episode' units reset; 'lineage' units (m0/m1 boundary, reasoning-clear watermark) survive byte-identical"  // B5 cross-episode
  ]
}
```
My one pre-freeze ask (the only non-additive item): **`signal` is an OBJECT `{ kind, ... }`,
never a bare string** — the author layer extends it with `requested_policy` + `provenance` +
`what`, and string->object is breaking while object+fields is additive.

**Author layer = additive on top of MC's 7 universal vectors** (nothing MC's harness emits):
- STATE gains an optional `pending_changes` field = queued deferred author transitions
  awaiting the next HARD bust (the observer harness never populates it). Exercised on the
  author side of the coordinator vector: a `requested_policy: defer` signal -> SOFT+ + a
  `pending_changes` delta; a later HARD bust -> all `pending_changes` drain into the fold.
- I author the AUTHOR-POLICY vectors (defer/now/forced epoch transitions) as an additive set
  on top of MC's 7. MC's 7 = universal mechanics (anchor/freeze/replay/classify/coordinator);
  mine = the author-side policy lever. Same file format.

**Status:** architecture + schema converged and frozen (modulo MC's ack of `signal`-as-object).
Remaining: MC emits the first-cut vector file (its next work item; release train cleared);
I shape the Rust core (per-pass function + state value) against the frozen schema in parallel;
then layer the author-policy vectors. **Ufuk sign-off on this converged contract before any
build.** The vector file is the migration invariant both MC and the Rust core pin.


---

# Round 4 — schema final, vector file in flight

MC accepted `signal`-as-object and added a `layer` tag (`"mechanics" | "author-policy"`) so
the universal vectors and the additive author-policy set stay explicitly separated. Final
frozen schema:

```
vector = {
  name,
  layer: "mechanics",                          // author-policy vectors tag "author-policy"
  render_config: { system_hash, tool_set_id, model_key },
  initial_state: { version, anchor_fingerprint, frozen_units:[{key,kind,frozen_payload}], pending_changes?:[...] },
  passes: [ { signal:{kind,...}, input_identity, expect_action: "SOFT+"|"SOFT"|"HARD",
              expect_frozen_set_delta:[{key,kind,frozen_payload}], expect_pending_delta?:[...] } ],
  asserts: [
    "i>0 & expect_action==SOFT+  ->  cached_prefix_bytes[i] == cached_prefix_bytes[i-1]",
    "replayed unit on SOFT+      ->  rendered_bytes(unit) == unit.frozen_payload",
    "input_identity diverges over covered prefix  ->  expect_action in {SOFT,HARD}, never SOFT+"
  ]
}
```
First-cut observer signal kinds: `growing-tail | watermark-crossed-image |
skeleton-window-moved | memory-delta | compartment-published | hard-fold-trigger |
provider-nonce-only | revert-or-truncate | idle-ttl-expired`. Author layer adds
`requested_policy | provenance | what` on top.

**Anchor reference impl confirmed (MC source):** `computeRawRangeFingerprint`
(`read-session-true-raw-tokens.ts:661`) hashes `ordinal:id:parts.length:partContentFingerprint`
per message over `[start,end)` — raw CONTENT only, NEVER tag/drop/strip state. That property
is *why* a tail drop/strip cannot false-bust (tag state isn't hashed) while an in-prefix
content retract diverges -> the two anchor sub-cases fall out for free. It's the reference
impl for the anchor field.

**Value convention (load-bearing for harness-neutrality):** the vector file emits
`anchor_fingerprint` / `input_identity` as **opaque-but-consistent TOKENS, not real hashes.**
The core never *computes* a fingerprint — it only *compares* per-pass `input_identity`
against the stored anchor for EQUALITY and branches (match->replay, diverge->bust). Opaque
tokens (e.g. `"cov:A"` stable across a tail-grow pass, `"cov:A"` -> `"cov:A-reverted"` on an
in-prefix retract) test the **core's branch** — the thing the vector is *for* — without
forcing the Rust core to reimplement MC's exact hash. Each real harness plugs its own stable
fingerprint (MC = `computeRawRangeFingerprint`, llm-runner = its WAL-message fingerprint)
behind the same opaque-comparable contract.

**Status: contract + schema fully frozen.** MC is extracting the 7 mechanics vectors (from
its cache-invariant E2E suite) into the frozen JSON + a one-page schema doc; it will ping a
readable path. Remaining gates: (1) MC's vector file lands, (2) **Ufuk sign-off on this
converged contract**, then the build (Rust core against the frozen schema -> llm-runner
consumes first -> author-policy vectors layered -> MC migrates later). I shape the Rust core
(per-pass function + `{version, anchor_fingerprint, frozen_units, pending_changes}` state +
`{signal, input_identity}` pass-input) against the frozen schema in parallel; consumes MC's
file unchanged when it lands.


---

# Round 5 — vector file SHIPPED + verified. Contract complete.

MC shipped the golden-vector file (cortexkit/magic-context @ `09b58896`), independently
verified at source against the frozen schema — exact match:
- `docs/cache-policy/cache-stability-golden-vectors.json` — **8 `mechanics` vectors**
  (MC added V2 post-execute-settle from the A2 E2E case; additive), all opaque tokens
  (`cov0`/`cov0@r1`/`cov1`, `sys0`/`tools0`/`m0`), **zero real hashes** -> the Rust core
  passes by branching correctly, never by porting MC's fingerprint.
- `docs/cache-policy/cache-stability-golden-vectors.schema.md` — the one-page contract:
  SOFT+/SOFT/HARD action model, frozen-render-unit byte-completeness, anchor-content-over-
  coverage, opaque-token rule, the two-source coordinator, and the stated axioms.

**The 8 vectors:** V1 growing-tail-defer (steady-state zero-bust), V2 post-execute-settle
(execute busts once, defers after are byte-stable), V3 frozen-strip-not-first-applied-on-
defer (the single most-broken MC regression), **V4 skeleton-byte-complete-across-moving-
window (Leak-A catch)**, V5 delta-rides-m1-SOFT-m0-frozen, V6 hard-fold-folds-m1-into-m0-
and-drains-deferred (the coordinator), V7 provider-nonce-only-is-not-a-bust, **V8 revert-
within-covered-prefix-discards-and-rebuilds (Leak-C catch)**.

**V4 + V8 are the two to wire first** — they encode the exact two leaks the clean model
would have walked into (byte-complete payload, anchor-content-over-coverage). A consumer
that greens those two has the two scar-forced refinements correctly implemented.

`common_asserts` (top-level): the three locked + "a SOFT/HARD pass may/must change cached
bytes (that is the bust)." `frozen_payload` values are illustrative harness-output samples
(each harness compares its own renderer's bytes to its own payloads); the STRUCTURE (which
unit frozen on which pass, which passes SOFT+) is the invariant.

## Status: CONTRACT COMPLETE — awaiting Ufuk design-before-code sign-off

Everything is locked and verified. The build sequence on sign-off:
1. Rust cache-policy core (per-pass fn + `{version, anchor_fingerprint, frozen_units,
   pending_changes}` state + `{signal, input_identity}` pass-input, opaque-token equality),
   green against MC's 8 `mechanics` vectors (V4 + V8 first).
2. llm-runner consumes it (the author harness: caller hands flat policy -> module resolves
   schemas from the subc catalog -> pins the per-session-sticky render-config epoch). This
   resolves the original Swift tool-calling wart structurally (no per-turn schema shipping).
3. Author-policy vector layer (`pending_changes` + defer/now/forced epoch vectors,
   `layer:"author-policy"`) appended to MC's file; sent to MC to review the two-source
   coordinator drain.
4. MC migrates onto the shared Rust core later (parity held by the same vector file) when
   MC-under-subc lands — retiring MC's permanent TS-opencode/TS-pi parity tax.

The vector file is the **migration invariant** pinned on both sides. No crate code is
written until Ufuk signs off the contract.


---

# Frozen-set durability: lineage-cumulative vs per-episode units (cache-core requirement)

Surfaced during the CK#1 reconcile (LLMRUNNER, the reasoning-retention durability twin of
FrozenRenderConfig). A requirement on the cache-core's durable frozen-set model, banked here
so it is designed in, not retrofitted.

**The frozen-set has TWO durability classes of unit, and the durable store (Edge B: cache-
DECISION state persists) MUST distinguish them:**

- **Per-episode units** (most drops/strips/skeletons): scoped to one run/episode. Reset at a
  new `RunStarted` boundary, like `run_config` / `usage` / `completed_steps`.
- **Lineage-cumulative units**: persist across turns AND across episode boundaries (a clear
  that landed in episode 1 must stay frozen through episode 5). Examples:
  - the **frozen reasoning-clear watermark** (the `clearedReasoningThroughTag`-style advance-
    only watermark from the retention invariant);
  - likely the **m0/m1 compartment boundary** (the compaction boundary is lineage-scoped, not
    run-scoped).

**Why this is load-bearing (the two bust modes it prevents):**
- **Crash-resume:** if a lineage-cumulative watermark is in-memory or a per-run frozen value,
  a mid-lineage crash re-derives it from scratch → the clear/boundary lands at a different
  position → busts the very cache it exists to protect.
- **Cross-episode:** if it is per-run-frozen (resets at each `RunStarted`), the older-turn
  clear un-freezes at every episode boundary → re-derive bust per episode (the same class as
  the §8.1 identity-lead cross-episode bust).

**Requirement:** lineage-cumulative frozen units MUST live in durable lineage replay state
(WAL-recorded on the owned leg, the durable store on the module leg), survive `RunStarted`
boundaries, and be reproduced byte-identically by resume + cross-episode replay — gated by the
same crash-cut + cross-episode byte-identity discipline as the existing resume gates. This is
the reasoning-retention twin of FrozenRenderConfig durability: same freeze discipline, a new
lineage-scoped field. The owned-leg implementation rides the cache_tiers + cache-core build
(not the B2 reconcile build); this note ensures the durable frozen-set model treats lineage-
cumulative units as a first-class class from day one.

**Shipped reference implementation (proof-of-existence, not theoretical):** MC's plugin leg
already does exactly this — `clearedReasoningThroughTag` is persisted durably in `session_meta`,
survives restart, and is reproduced across the conversation lifetime (advance-only watermark,
clear-on-bust, replay-identical-on-defer). So the owned-leg WAL-durable watermark has a working
pattern to mirror, not a design to invent.


---

# Codec determinism binding (the cache-core ↔ codec seam, SPEC #2 §B)

The codec spec (#2) §B binds the codec layer to this cache-core's frozen-set/byte-stability
contract. Stated here so both specs assert the same thing (single source of truth for the seam):

**The FrozenRenderConfig is a CLOSED set.** `encode(ck)` (a wire codec's render) MUST be a pure
function of `(CkMessage, FrozenRenderConfig)` and MUST read NOTHING byte-affecting outside it.
The closed set, frozen at run-start and reproduced from durable state on resume:
`{ target wire family, model/wire_model_id, resolved tool set, tool_choice, generation params,
response_format, cache-policy/breakpoint config, the frozen reasoning positional bits
(is_last_assistant_turn / merge-group membership), the target-native alias map basis,
provider_options, serializer_profile_id }`. A closed enumeration (not an open "etc.") is what
makes "the codec introduces no new bust input" CHECKABLE rather than aspirational.

> **`system` is NOT a closed-set member** (corrected per CK#1 §2.1/§5.11, Oracle B4): system
> bytes are CK CONTENT (leading `Role::System` messages), not a render input — listing them
> here would double-represent them (the content anchor already covers a system change). `encode`
> DERIVES the top-level system field FROM those messages (§A.5). `serializer_profile_id`
> (§6.1 quirk seam: clear-shape / segmentation guard / residual) IS a member — it is a
> non-CK byte-affecting render input.

**The four bust-input classes a codec MUST freeze-or-exclude:**
1. Nonces/timestamps in any emitted field (the identity-lead class) — frozen or stripped.
2. Non-deterministic iteration order — every map a codec serializes (provider_extras, tool
   input JSON object keys, nested Opaque-summary structures) MUST be canonical-ordered, or two
   logically-equal requests diverge on key order = a silent bust.
3. The per-request alias map — looks stateful, MUST be a pure fn of `(canonical_id, frozen
   target family)`; never a counter/RNG.
4. Id-less tool-call id synthesis (Gemini optional ids, CK#1 §5.6.1 r2 / codec §A.4) — MUST be
   a pure fn of `(message ordinal, part ordinal, tool name, hash(input))`, NEVER a clock/counter
   (the Pi `google.ts` `Date.now()` bug). This is a distinct synthesis trap, not a sub-case of
   class 1 (Oracle SF1).

**Cross-episode determinism (the lineage-cumulative gate).** Beyond cross-pass stability
(`encode` the same CK twice under frozen config → identical bytes), the codec MUST be
byte-identical across an EPISODE boundary when the FrozenRenderConfig is reproduced from durable
state (new `RunStarted`, config rebuilt from the durable WAL/store). This is the codec-side twin
of the lineage-cumulative frozen-unit durability requirement above: without it a codec passes
every within-run test and busts at the episode boundary (the class that bit the identity-lead).
SPEC #2 §B.4(d) is this test.

---

# Round 4 — Oracle gate hardening (bg_992d1730, Ufuk-directed self-gate)

The cache-core spec went through the same adversarial Oracle gate CK#1 did. It found 5 blockers
+ 3 should-fixes — all spec-tightening (the architecture held; no structural rethink). Resolutions:

**B4 — FrozenRenderConfig was not closed against CK#1.** FIXED above (Codec determinism binding
section): `system` removed (it's CK content, double-represented), `serializer_profile_id` added.
The early epoch text that pinned only `system + tools + model` is superseded by the closed set
(those three were a first-cut gloss; the closed set is authoritative).

**B3 — `frozen_payload` "bytes OR complete inputs" reopened the re-render bug.** FIXED above
(Leak A): `frozen_payload` is EXACT emitted bytes only; structured inputs are debug-only, never
rendered on SOFT+. The golden-vector schema doc is corrected on MC's side (same change).

**SF1 — id-less synthesis is a 4th bust-input class.** FIXED above (now 4 classes, not 3).

**B5 — lineage-cumulative vs per-episode durability was required but not represented/tested.**
The frozen-vector STATE shape gains, per unit, `durability_class: "episode" | "lineage"` + b a
`reset_rule`: an `"episode"` unit resets at `RunStarted`; a `"lineage"` unit survives episode
boundaries and merges advance-only. The reasoning-clear watermark AND the m0/m1 boundary are
`"lineage"` (resolving the earlier "likely" → REQUIRED, matching codec §B.4(d)). Add a
crash-resume + `RunStarted` cross-episode golden vector (the durable-state twin of the within-run
defer vectors). Field name/values synced identically with MC's golden-vector schema.

**SF2 — anchor-mismatch action.** A whole-frozen-set anchor mismatch ⇒ HARD (full invalidation +
fresh render), never SOFT+ and never a narrower SOFT rebuild (a per-unit-anchored narrower SOFT
is only valid if per-unit anchors prove it safe — not in v1). The schema assert is tightened to
"input_identity diverging over covered prefix ⇒ expect_action == HARD."

**SF3 — CAS-retry semantics (one normative sentence).** On a CAS write-back failure: reload the
latest state, recompute `input_identity` against THAT state's stored coverage descriptor, and
apply set/advance-only merges before retrying. Never retry against the stale pre-image.

**B1 + B2 — anchor mechanism (pending MC source-confirm, then rewrite).** The Oracle flagged the
anchor as length-based (→ silent stale cache) and coverage-not-in-state. MC source-verified that
the cited `computeRawRangeFingerprint` is the HISTORIAN in-flight snapshot validator (deliberately
length-based, content-quality residual only), NOT the SOFT+ cache anchor — so this spec's line
binding the anchor to `computeRawRangeFingerprint` was a CONFLATION (my error). Pending MC's
confirm of the exact replay-vs-bust validity mechanism (boundary-presence + frozen-byte
replacement, vs a covered-prefix fingerprint), the anchor section is rewritten to state the
correct mechanism + B2 (the boundary/coverage descriptor lives in core state, not just a
fingerprint). Held until MC confirms; then re-gate.
