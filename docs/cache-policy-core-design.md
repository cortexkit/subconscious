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
