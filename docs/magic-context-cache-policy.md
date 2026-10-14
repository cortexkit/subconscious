# Cache-Policy Core — Design (SPEC #6)

This document fixes the cache-state, per-pass input, byte-replay, and codec contracts.

The Rust cache core binds the CK Message specification in magic-context's `docs/specs/ck-message.md`: §3 defines `CkMessage`, and §5.13.4 requires `Opaque` blocks to be immovable atomic units.

> **Authoritative mechanism.** The cache anchor is boundary presence plus frozen-byte replacement of the covered prefix, never a covered-prefix content fingerprint; an absent boundary reuses frozen bytes this pass and reconciles on the next bust.
> `boundary_present` is an opaque equality token compared with `boundary_id`, not a boolean; `"-"` denotes absence, and `full_array_fingerprint` is the separate whole-array identity for delta and last-known-good validity.
> The old `anchor_fingerprint` and `input_identity` names are superseded by those boundary slots.
> `system_hash` is a content-derived HARD-bust epoch marker, not a `FrozenRenderConfig` member; the codec render-input closed set and observed epoch tuple have distinct roles.
> Cache frozen units are lineage-durable; episode-reset WAL replay state is a separate structure.
> On each pass, replay exact frozen bytes at the stored boundary; absent boundaries defer reconciliation until a bust, all current cache units survive `RunStarted` as lineage state, and the codec reads only the closed `FrozenRenderConfig` inputs listed in “Codec determinism binding”.

**CK#1 integration (post-inversion).** The frozen-set has TWO unit categories: (a) transform
DECISION units (drop / strip / skeleton / synthesized-region / injection — the cache-core's
own frozen render units), and (b) CONTENT units the machinery must treat as atomic+immovable
but never originates — chiefly `Opaque` blocks (CK#1 §5.13.4): native-preserved, never
span-edited, never reordered (immovable under the segmentation-invariance axiom), and
arc-grouped (an `OpaqueArc{kind:Tool|Approval}` is reclaimed as a whole pair). Anchor-validity
covers content order including `Opaque` positions via **boundary-presence** (per the banner
above); the determinism guarantee applies to both categories.


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

- **Observer (opencode/Pi via Magic Context):** does NOT own tools / system / model; it INFERS a bust
  by watching the wire (system-prompt hash change -> HARD). By the time Magic Context sees it, the
  host already busted. Its only lever is *ride it efficiently*. It physically CANNOT defer a
  render-config bust — it is not the author, the change is already on the wire.
- **Author (our harness):** owns every render-config mutation; knows the cause BEFORE the
  wire. This causal authorship is the ONLY thing that makes now/defer/forced possible.

Authored bust sources (our harness): user added a tool / added an MCP / changed an MCP
toolset (expand or shrink) / added or removed an MCP server / head agent added tools to a
hiree. Each is a render-config mutation -> epoch-transition candidate -> policy decision:

- **defer** (default for most): queue the transition; it rides the next HARD bust the loop
  runs anyway (model switch / context fold / another forced change). [Cache invariant:
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
  - **Observer adapter** = generalized-Magic-Context: coarse bust signals derived from host hooks
    (system-prompt hash -> HARD; usage threshold -> fold). No policy lever. Uses only the
    coordinator.
  - **Author adapter** = our harness: rich signals from causal events
    `{ level, source, what, provenance, requested_policy }`. Uses the coordinator AND the
    epoch-policy authority.
- **Layer 2 — render/store adapter (per-harness):** prefix byte rendering + storage. Magic Context ->
  `context.db` + m[0]/m[1] messages. llm-runner -> WAL + sqlite + request renderer.
  Unchanged by this design.

Magic Context = core + observer-adapter (the generalized extract). Ours = core + author-adapter +
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

The bust decision runs EVERY LLM round-trip (Magic Context fires its transform once per step; llm-
runner decides per loop step). A per-step synchronous RPC adds latency and can hang the loop when the service is down; the decision MUST be in-process -> shared LIBRARY, not a service. A module is only ever justified OFF the decision path: config authoring (already
caller-owned) and cross-harness cache observability (a future read-only telemetry sink),
both feeding the in-process decider via pushed epoch state, never a per-pass call.

# Frozen-state integration contract


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

## Frozen-unit requirements and integration

- **Leak A — `decision` must be a byte-COMPLETE payload, not an enum tag.** A rich unit
  (skeleton/edit-marker renders filePath + a 40-char diff-prefix) re-derived from current
  content flips bytes on defer. Fix: the harness renders final bytes at freeze time; the
  core stores `{key, kind, frozen_payload}` where **`frozen_payload` is the EXACT bytes as
  emitted on the bust pass** — never `{id, enum}`, and never "structured inputs to re-render"
  ("Bytes OR complete inputs" reopens the exact re-derivation bug — a defer pass
  re-rendering from inputs can diverge on renderer-version / canonicalization drift). Structured
  inputs MAY be carried only as DEBUG metadata and MUST NOT be rendered on a SOFT+ pass; the
  SOFT+ assert (`replayed bytes == frozen_payload`) only holds if `frozen_payload` is
  authoritative bytes. "Pure render" is necessary but not sufficient; "decision is byte-complete"
  is the other half. (Magic Context's collapse to a single `[dropped N]` worked precisely because the
  payload was made trivial; rich payloads must freeze the payload.)
- **Leak B — the frozen set is RENDER UNITS, not just per-id drops.** A unit is
  `{drop | strip | skeleton | synthesized-region | injection}` — includes whole synthesized
  regions (m[0], m[1] full block bytes) AND deterministic injections (synthetic-todowrite).
  Each is an opaque byte-complete payload the harness produces on bust; the core freezes +
  replays and **never interprets the payload** (stays harness-neutral). Write-back of the
  whole set is **ATOMIC** — `new_state` is ONE value (units + markers + manifest), never
  per-unit, so payloads and their markers cannot be persisted separately.
- **Leak C — "replay every pass" is qualified by BOUNDARY-PRESENCE.** If the user reverts turns or
  the host trims the array, the boundary id the frozen set splices at may vanish. The core's
  FIRST per-pass step is the **boundary-presence check**: is `state.boundary_id` still in the
  live array? PRESENT → splice the covered prefix out at it + replay the frozen bytes. ABSENT
  (a revert removed/crossed the boundary) → **KEEP replaying the frozen bytes THIS pass (SOFT+,
  `reconcile_pending`)** and rematerialize on the NEXT cache-busting pass — NOT a same-pass
  discard/bust (there is no covered-prefix fingerprint, so an in-prefix edit is summarized away,
  never a bust; only an explicit `render_config` epoch change is a HARD bust). **Revert is
  HOST-caused = OBSERVED even for the author harness**, so the boundary-presence check is
  **universal**, not observer-only — the author-side advantage covers *authored* changes but
  not revert.
- **Leak D — the transition must be VERSION-STAMPED CAS, not last-write-wins.** Magic Context shares
  one SQLite store across opencode+pi processes; the shared core must back Magic Context later, so
  `core(prev_state, signal) -> (new_state, action)` stamps `new_state.version =
  prev.version + 1` and defines the compare-version contract; the **harness** does the
  atomic CAS write-back (compare prev-version, swap, idempotent under retry). Single-writer
  (llm-runner) always wins uncontended; multi-writer (Magic Context) retries — same core. This is the
  epoch-CAS primitive already shipped in `cortexkit-lease` + the credential vault. The core
  stays storage-agnostic (stamps + defines compare-version; the harness's store enforces
  atomicity).

## The per-pass core function (converged)

```
core(prev_state, pass_input) -> (new_state, executed_action)
  pass_input = { action, boundary_present, rendered_units? }
        // action: the HARNESS's classification (SOFT+ | SOFT | HARD). The core does NOT
        //   classify a raw signal — trigger causes are a harness-provided predicate: the
        //   observer/author adapter maps its signal {kind,...} -> action, then the core
        //   APPLIES the mechanics of that action. The core is signal-agnostic (it never
        //   sees the signal kind); it returns the executed_action it applied.
        // boundary_present: an OPAQUE live-boundary TOKEN (e.g. "b0" / "-"), compared for
        //   EQUALITY against state.boundary_id — NOT a bool, NOT a content fingerprint. The
        //   harness derives it by findIndex(live array, state.boundary_id): the matched id
        //   token if present, the absent-sentinel "-" if not.
        // rendered_units: byte-complete units the harness rendered for a BUST (empty on defer).
  state      = { version,
                  boundary_id,                    // the coverage descriptor: the id the
                                                 //   covered prefix is spliced out at; CAS-retry
                                                 //   re-splices the SAME id
                 frozen_units: [{ key, kind, frozen_payload, durability_class }] }
                                                 // durability_class: "episode" | "lineage"

  1. BOUNDARY CHECK (Leak C): boundary_present == state.boundary_id ?
        equal  -> covered prefix is spliced out at boundary_id, frozen bytes replace it -> eligible to replay
        absent ("-") -> reuse cache THIS pass (SOFT+, reconcile_pending); the revert reconciles on the
                   NEXT bust (no discard, no content-fingerprint divergence — in-prefix edits are
                   summarized away, never bust)
  2. APPLY the harness-supplied action's mechanics (the core does NOT re-derive the action):
  3a. on BUST (SOFT/HARD): freeze the harness's byte-complete rendered_units into new_state (A/B),
        version = prev.version + 1, drain ALL deferred work into this one bust (coordinator);
        on RunStarted: reset "episode" units, carry "lineage" units forward (advance-only merge)
  3b. on DEFER (SOFT+): replay frozen units VERBATIM (no render call, no rendered_units)
```

> **Binding note (authoritative — the Rust `cortexkit-cache-core` API is the source of truth):**
> the shipped signature is `CoreState::step(&mut self, PassInput) -> StepResult`, where
> `PassInput { proposed: Action, boundary_present: String, rendered_units, queued,
> new_boundary_id, run_started }` and `StepResult { action, reconcile_pending }`. The harness
> supplies the **classified `Action`** (`proposed`), never a raw signal; the anchor input is the
> **`boundary_present` token**, never a content fingerprint. Any consumer spec describing an
> `input_identity` content-fingerprint anchor or an `anchor_fingerprint` state field is on the
> obsolete content-fingerprint model and must migrate to boundary-presence before binding.

Pure function. The harness: renders-on-bust, places-every-pass, CAS-persists. Render runs
ONLY on bust passes -> defer-pass re-derivation is structurally impossible = the bug-class
kill. The state carries the `boundary_id` (coverage descriptor) so a CAS-retry recomputes
boundary-presence against the latest state's boundary and re-splices the same coverage:
on CAS failure, reload latest state, recompute boundary-presence, apply set/advance-only
merges, and retry — never against the stale pre-image.

## Golden-vector schema (harness-neutral)

> Use the boundary-presence schema and schema-version-2 format below. The current schema uses `boundary_id` / `boundary_present`,
> not `input_identity`, and a revert that removes the boundary is reuse-then-reconcile (SOFT+),
> NOT a bust.

```
vector = {
  name,
  render_config,            // opaque { system_hash, tool_set_id, model_key, serializer_profile_id }
  initial_state,            // { version, boundary_id, frozen_units, pending_changes }
  passes: [ { signal, boundary_present, expect_action: SOFT+|SOFT|HARD,
              expect_frozen_set_delta: [{ key, kind, frozen_payload, durability_class }] }, ... ],
  assert: "for every pass i with expect_action == SOFT+: cached_prefix_bytes[i] ==
           cached_prefix_bytes[i-1]; AND replayed bytes == the unit's frozen_payload"
}
```

Bust definition: a wire segment **before the final `cache_control`
breakpoint** changed between two consecutive requests. The byte-stability assert is
**conditional on `expect_action == SOFT+`** (a SOFT/HARD pass is required to change cached
bytes — that IS the bust); a revert that REMOVES the boundary is SOFT+ reuse-then-reconcile
while a `render_config` epoch change is a HARD bust.

Magic Context's first-cut vectors (load-bearing, from its cache-invariant E2E suite): growing-tail
defer (N passes, zero busts), watermark-crossing-an-image on defer (no first-strip),
skeleton-vs-full across a moving window (Leak A), m[1]-on-SOFT/m[0]-frozen, HARD-fold
drains all deferred drops (coordinator), provider-nonce-only change is NOT a bust,
revert-beneath-a-frozen-set (Leak C). Delivered as a single harness-neutral JSON file + a
schema doc = **the migration invariant both Magic Context and the Rust core pin**.


---

# Boundary-presence schema

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
    (treat as already-trimmed) + **reconcile on the next cache-busting pass** (rematerialize m0
    against the live reverted array). The stale-summary window lasts **until the next bust** — it
    is typically one pass, but if only `SOFT+` defer passes follow, `reconcile_pending` persists
    across them (the frozen bytes keep replaying, byte-stable). This is an accepted content-quality
    loss (the summary describes briefly-reverted content), NOT a cache-correctness bug, and the
    existing shipped behavior. (If a future requirement needs faster reconciliation,
    `reconcile_pending` could force the next pass to bust — not v1.)

So: **anchor-validity = boundary-presence + frozen-byte replacement, never a covered-prefix
fingerprint.** `computeRawRangeFingerprint` is a different mechanism on a different path
(historian-snapshot) and MUST NOT be cited as the cache anchor — the codec spec adds a permanent
note splitting the two so the conflation cannot recur.

**Frozen schema (the contract Magic Context emits the vector file to):**
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
    "boundary_present == boundary_id: splice+replay (SOFT+ unless a delta/render_config/epoch forces SOFT/HARD)",
    "boundary_present absent ('-'): KEEP replaying frozen bytes (SOFT+, reconcile_pending), NEVER a blind same-pass rebuild; reconcile on the next cache-busting pass",   // SF2 (boundary-presence model)
    "a render_config/epoch HARD bust ⇒ expect_action == HARD (full invalidation + fresh render + new boundary_id)",
    "across a RunStarted boundary: 'lineage' units (m0/m1 boundary, reasoning-clear watermark) reproduce byte-identical; 'episode' units (none in the cache set today; reserved) would reset"  // B5 cross-episode
  ]
}
```
`signal` is an object with a `kind` member, not a string. A harness that controls configuration changes adds `requested_policy`, `provenance`, and `what` members to that same object; an observer harness needs only the mechanics signal.

**Author layer = additive on top of Magic Context's 7 universal vectors** (nothing Magic Context's harness emits):
- STATE gains an optional `pending_changes` field = queued deferred author transitions
  awaiting the next HARD bust (the observer harness never populates it). Exercised on the
  author side of the coordinator vector: a `requested_policy: defer` signal -> SOFT+ + a
  `pending_changes` delta; a later HARD bust -> all `pending_changes` drain into the fold.
- Author-policy vectors add deferred, immediate, and forced epoch transitions using the same vector format as the universal mechanics.



---

# Vector schema version 2

The schema-version-2 vector format separates mechanics, author-policy, and durability vectors:

```
vector = {
  name,
  layer: "mechanics",                          // author-policy vectors tag "author-policy"; durability vectors tag "durability"
  render_config: { system_hash, tool_set_id, model_key, serializer_profile_id },
  initial_state: { version, boundary_id, frozen_units:[{key,kind,frozen_payload,durability_class,reset_rule}], pending_changes?:[...] },
  passes: [ { signal:{kind,...}, boundary_present, expect_action: "SOFT+"|"SOFT"|"HARD",
              expect_frozen_set_delta:[{key,kind,frozen_payload,durability_class,reset_rule}], expect_pending_delta?:[...] } ],
  asserts: [
    "i>0 & expect_action==SOFT+  ->  cached_prefix_bytes[i] == cached_prefix_bytes[i-1]",
    "replayed unit on SOFT+      ->  rendered_bytes(unit) == unit.frozen_payload",
    "boundary_present absent      ->  reuse-then-reconcile (SOFT+ this pass), never a blind same-pass rebuild",
    "lineage unit across RunStarted -> reproduced bytes == pre-boundary frozen_payload (restart never busts)"
  ]
}
```
First-cut observer signal kinds: `growing-tail | watermark-crossed-image |
skeleton-window-moved | memory-delta | compartment-published | hard-fold-trigger |
provider-nonce-only | revert-or-truncate | idle-ttl-expired`. Author layer adds
`requested_policy | provenance | what` on top.


**Value convention (harness-neutral):** the vector file
emits `boundary_id` / `boundary_present` as **opaque-but-consistent TOKENS, not real hashes/ids.**
The core never *computes* anything — it only *compares* per-pass `boundary_present` against the
stored `boundary_id` for EQUALITY and branches (present -> splice+replay, absent ->
reuse-then-reconcile-next-bust). Opaque tokens (e.g. `"b0"` present across a tail-grow pass,
`"b0"` -> `"-"` when a revert removes the boundary message) test the **core's branch** — the
thing the vector is *for* — without forcing the Rust core to reimplement any harness internals.
Each real harness plugs its own boundary-presence check (Magic Context = `findIndex(info.id === boundary_id)`,
llm-runner = its WAL boundary lookup) behind the same opaque-comparable contract.



---

# Shared golden-vector corpus

The shared test corpus is published in the Magic Context repository (`cortexkit/magic-context`, revision `09b58896`): `docs/cache-policy/cache-stability-golden-vectors.json` contains the expected state transitions, and `docs/cache-policy/cache-stability-golden-vectors.schema.md` defines schema version 2.
**The 9 vectors (schema_version 2):** V1 growing-tail-defer (steady-state zero-bust), V2
post-execute-settle (execute busts once, defers after are byte-stable), V3 frozen-strip-not-
first-applied-on-defer (the single most-broken Magic Context regression), **V4 skeleton-byte-complete-
across-moving-window (Leak-A catch)**, V5 delta-rides-m1-SOFT-m0-frozen, V6 hard-fold-folds-
m1-into-m0-and-drains-deferred (the coordinator), V7 provider-nonce-only-is-not-a-bust, **V8
revert-removes-boundary-reconciles-next-bust (Leak-C catch, boundary-presence model)**, V9
cross-episode-lineage-units-reproduce-byte-identical (the durability / RunStarted twin).


`common_asserts` (top-level): the three locked + "a SOFT/HARD pass may/must change cached
bytes (that is the bust)." `frozen_payload` values are illustrative harness-output samples
(each harness compares its own renderer's bytes to its own payloads); the STRUCTURE (which
unit frozen on which pass, which passes SOFT+) is the invariant.

# Frozen-set durability: lineage-cumulative vs per-episode units (cache-core requirement)


**The durable model has TWO durability classes — but they live in DIFFERENT structures, and
conflating them was a mis-model (corrected; Magic Context's golden-vector regen surfaced it):**

- **The cache FROZEN-SET units are ALL `lineage`.** A frozen render unit (drop / strip /
  skeleton / m0 / m1 / reasoning-clear watermark) compacts a region of the CONVERSATION PREFIX,
  and the conversation prefix is lineage-cumulative — a drop that landed in episode 1 stays
  dropped through episode 5 (it does NOT un-compact at a new `RunStarted`). So there is no
  per-episode cache frozen unit today; every unit is `lineage` (survive + advance-only-merge,
  never reset). All current frozen units are lineage-durable, which is why restarting does not bust the prefix.
- **The `episode`-class state is WAL REPLAY-STATE, NOT cache frozen units.** `run_config` /
  `usage` / `completed_steps` reset at `RunStarted`; `prompt` + the reasoning-clear watermark
  survive (lineage). That reset distinction is llm-runner's durability core (the WAL replay
  state machine, codec §B.4(d)), a SEPARATE structure from the cache frozen-set.

So `durability_class` on a frozen_unit is `"lineage"` for every current unit kind; `"episode"`
is RESERVED (no real episode-class cache unit exists — we do not fabricate one to exercise
`reset_rule`; the reset_rule fires on the WAL replay-state, tested in llm-runner resume
conformance, not the cache golden vectors). The field stays for schema-completeness and so a
future run-scoped frozen unit (if one is ever identified) is expressible without a schema break.

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

> **`system` is NOT a closed-set member** (CK#1 §2.1/§5.11): system
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
   class 1.

**Cross-episode determinism (the lineage-cumulative gate).** Beyond cross-pass stability
(`encode` the same CK twice under frozen config → identical bytes), the codec MUST be
byte-identical across an EPISODE boundary when the FrozenRenderConfig is reproduced from durable
state (new `RunStarted`, config rebuilt from the durable WAL/store). This is the codec-side twin
of the lineage-cumulative frozen-unit durability requirement above: without it a codec passes
every within-run test and busts at the episode boundary (the class that bit the identity-lead).
SPEC #2 §B.4(d) is this test.

---
