# Cache-Policy Core — Design Notes (working, pre-co-design with MC)

Status: **working draft, NOT locked.** Authored by Alfonso @ subconscious, brokering a
cross-product design (MC + llm-runner + future harnesses). To be validated against MC's
accumulated cache-stability invariants before any build.

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
  core stores `{key, kind, frozen_payload}` (bytes or complete inputs), **never
  `{id, enum}`**. "Pure render" is necessary but not sufficient; "decision is byte-complete"
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
  pass_input = { signal, input_identity }       // input_identity = opaque prefix fingerprint
  state      = { version, anchor_fingerprint, frozen_units: [{ key, kind, frozen_payload }] }

  1. ANCHOR CHECK (Leak C): input_identity vs prev_state.anchor_fingerprint
        mismatch -> bust (discard frozen set)
  2. CLASSIFY (signals): SOFT+ | SOFT | HARD   (anchor-mismatch forces a bust class)
  3a. on BUST: harness renders byte-complete units -> freeze into new_state (A/B),
        version = prev.version + 1, drain ALL deferred work into this one bust (coordinator)
  3b. on DEFER (SOFT+): action = replay frozen units VERBATIM (no render call)
```

Pure function. The harness: renders-on-bust, places-every-pass, CAS-persists. Render runs
ONLY on bust passes -> defer-pass re-derivation is structurally impossible = the bug-class
kill. MC confirmed all four of its strip/drop scars die under this, AND it additionally
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
