# cache_tiers — provider prompt-cache policy for the owned harness

Status: FROZEN v2 — Oracle pass 1 (bg_0907ba8d) BLOCKed with 2 spec/code
mismatches + 4 revisions, all folded; confirm re-gate (bg_1ba10d70) verified
all 6 closed at source and returned SHIP (notably: render_config_blob flows
llm-runner → transform request → MC's render_config_changed, so the §2
epoch-coherence claim is code-true). Handed to LLMRUNNER for the renderer half.
Owner: subc (design) + LLMRUNNER (render/runtime half). Product decisions ratified
by Ufuk 2026-07-02 (breakpoint shape, smart TTL defaults, prewarm banked,
provider research resolved).

The `cache_tiers` field has existed in llm-runner's `RunConfig` since Phase 2,
always `Null`. This spec gives it a shape. It is the missing arm of the 8799
exit gate (cache_read retention) and the cache-economics payoff of the MC +
cache-core + llm-runner stack: MC guarantees byte-stable prefixes; cache_tiers
makes providers actually CACHE them.

## 1. Model: three provider cache classes

Per the 2026-07 provider survey (librarian, official docs):

| Class | Providers | Mechanism |
|---|---|---|
| **Markers** (explicit inline breakpoints) | Anthropic (`cache_control`, max 4, 5m/1h), Bedrock Converse (`cachePoint`, Claude-family, 5m default / 1h some models) | Client places breakpoints; write premium (1.25x/2x), read 0.1x |
| **Hints** (implicit prefix cache + routing/retention knobs) | OpenAI (`prompt_cache_key` + `prompt_cache_retention: in-memory\|24h`), xAI (`x-grok-conv-id` / `prompt_cache_key`), Fireworks (`prompt_cache_key`/affinity), Groq (auto, 2h idle), DeepSeek (auto disk, hit/miss pricing) | No placement; the runner supplies a stable per-session key and, where offered, a retention knob |
| **None / separate flow** | Gemini current API (implicit only, no knob; legacy `cachedContent` objects are a different flow — out of scope), Mistral/Cerebras (no official doc — treat unknown) | Nothing to emit |

Corrections to prior beliefs, folded: OpenAI has NO announced explicit
breakpoints (GPT-5.6 rumor unconfirmed — the design stays ready via the Markers
class but nothing is built for it); OpenAI's real lever is retention
(`in-memory` ≈ 5–10 min observed — matching the measured "10m actual" — up to
1h; `24h` extended retention, default for non-ZDR orgs since 2026-05-29).

## 2. The frozen shape

THE FREEZE BOUNDARY (the load-bearing correction from the Oracle gate): the
render path is `render(&FrozenRenderConfig, &CallOptions)` and can see NOTHING
else — so the resolved cache policy MUST live INSIDE the provider's
`FrozenRenderConfig` blob, not as a sibling `RunConfig` field the renderer
cannot reach. Resolution happens at admission (before freeze); the resolved
policy is folded into the frozen blob; render reads it from there. This one
placement buys three invariants at once:
1. C7 purity — resume re-renders from the same frozen blob, markers
   byte-identical by construction (drift-detection already compares the blob).
2. MC epoch coherence — the transform's HARD classifier keys on
   `render_config_changed`; because the policy is part of the render-config
   hash, a TTL/placement change is AUTOMATICALLY a HARD epoch on the MC side.
   A policy change can never bust the provider cache while MC replays frozen
   bytes believing nothing changed.
3. Ambient-read exclusion — the renderer never consults a live defaults table
   (the no-ambient rule, compile-enforced by the existing signature).

The legacy `RunConfig.cache_tiers` field (Null since Phase 2) records the
PRE-resolution policy inputs for observability only — never read by render or
resume logic; LLMRUNNER may keep it Null and derive observability elsewhere
(its call at implementation).

Shape of the resolved policy (inside FrozenRenderConfig; field layout is the
family's own — this is the semantic content):

```jsonc
// RunConfig.cache_tiers (was Null) — resolved BEFORE freeze from
// (provider capability class) x (TTL defaults table) x (per-run overrides)
{
  "version": 1,
  "class": "markers" | "hints" | "none",

  // markers class only
  "markers": {
    "syntax": "anthropic_cache_control" | "bedrock_cache_point",
    "ttl": "5m" | "1h",              // resolved, not a table — see §4
    "max_breakpoints": 4,
    "lookback_blocks": 20            // provider prefix-check window (§3 bridge)
  },

  // hints class only
  "hints": {
    "cache_key": "<stable per-session value>",   // see §5 derivation
    "retention": null | "in-memory" | "24h"      // OpenAI only today
  }
}
```

Resolution inputs and precedence (mirrors the `execution_mode` ownership
precedent — module-owned defaults, caller downward-only override):
1. Provider capability class: from the catalog/provider-spec (static fact).
2. TTL defaults table: llm-runner config-home (`llm-runner.jsonc`,
   `cache.provider_ttl` map) — ships with smart defaults (§4), user-editable.
3. Per-run override: `CallOptions.cache` (session role / explicit ttl) — the
   caller (chat app, Alfonso, historian) may only choose among
   provider-supported values; unsupported ⇒ typed config error at admission
   (fail loud, never silently degrade).

## 3. Marker placement (Anthropic/Bedrock) — the ported hybrid algorithm

Ported from the proven anthropic-auth hybrid mode (transform.ts), RE-GROUNDED
for the owned harness (the Oracle gate caught the v1 drift here): in the
plugin, MC-in-opencode merged its regions into content BLOCKS of messages[0];
on the owned leg the wiring contract returns m0 and m1 as TWO SYNTHESIZED
USER MESSAGES (`[m0, m1] ++ tail`) which the anthropic family renders as two
separate wire messages. So the plugin's split-prefix block special-case does
NOT port — the owned placement is message-granular:

4 slots, spent as:

1. **System tail anchor** — after coalescing instruction tail blocks into one
   block (byte-identical text must never flip merged/split layouts and move
   the breakpoint). Skipped when the bridge (4) claims the slot.
2. **m0 anchor + m1 anchor** — the LAST cacheable block of `messages[0]` (=m0)
   and of `messages[1]` (=m1). With MC on these are exactly the synthesized
   regions; with MC off the same rule anchors the first two real messages —
   graceful degradation with zero mode detection. (Anchoring the last block of
   a single-block synthesized message is trivially block 0.)
3. **Latest user boundary** — the last user message with **index > 1** (never
   m0/m1 themselves); the moving tail anchor.
4. **Bridge anchor** — the step-aware nuance: the provider's prefix check has a
   bounded lookback (`lookback_blocks`, 20 for Anthropic). When a tool-heavy
   turn puts more than `lookback_blocks` content blocks between the previous
   user boundary and the latest one, the previous marker falls outside the
   window and the read misses. Detect (`distance > lookback_blocks`) and spend
   slot 1 on the PREVIOUS user boundary instead of the system tail. GUARD
   (ported from the reference's `previous.index > 1`): the bridge candidate
   must itself be a user boundary at **index > 1** — m0/m1 are never bridge
   targets (they already carry anchors; bridging onto them would waste the
   system slot on the first live tail turn).

Discipline (all ported): markers only on cacheable content blocks (never
message objects, never thinking/redacted_thinking — 400s); strip all inbound
markers first (single authority); placement is a pure function of (frozen
policy, rendered prompt) — deterministic across resume by construction. Note
the within-step determinism argument: a resume re-render of the SAME step
rebuilds the SAME message array (C7's existing guarantee), so the moving
boundary and bridge detection see identical inputs — no new determinism
surface is introduced by position-dependent placement.

## 4. TTL policy — smart defaults + session role

Measured reality (Ufuk): Anthropic 1h cache ≈ 2.6x cheaper than 5m in real
primary-agent use despite the 2x write premium. The hybrid economics:

- **Session role** is supplied on `CallOptions.cache.role: "primary" |
  "ephemeral"` (default `primary`) and consumed at ADMISSION ONLY: it is a
  resolution input, resolved into the frozen policy before freeze, and IGNORED
  on resume (the frozen blob is authoritative — same rule as `auth_selection`).
  A caller supplying a different role on a resume changes nothing (C7).
  Ephemeral = subagent-class sessions with no >5m idle gaps: 5m tier (cheaper
  writes, sufficient). Primary = interactive/long-lived: 1h tier. The CALLER
  owns the role (it knows what it's spawning); llm-runner applies the table.
- **Defaults table** (shipped, user-overridable per provider/model):
  anthropic: primary=1h, ephemeral=5m; bedrock-claude: same where the model
  supports 1h, else 5m (capability-gated, fail-closed to 5m); openai:
  retention=24h where the org/model allows, else in-memory; groq/deepseek/xai/
  fireworks: nothing to set (auto), cache_key hint only; gemini: none.
- MC's scheduler consumes TTL predictions for its boundary/timing predicates
  (Unit S). The defaults table is the SHARED vocabulary: llm-runner passes the
  resolved TTL to MC on the transform request (a new optional field,
  `cache_ttl_ms`) so both layers reason from the same number. VERSION-SKEW
  RULE, stated honestly: the field is wire-additive (deserialize-tolerant per
  the MC contract), but an MC that hasn't adopted it yet falls back to its OWN
  per-provider TTL table — a bounded, benign divergence (scheduler timing
  predictions slightly off; zero correctness/byte impact, the transform output
  doesn't depend on it). The single-table goal is reached when MC adopts the
  field, tracked as an MC follow-up — not a deploy-ordering constraint.

## 5. Hints class — cache_key derivation

`cache_key` must be stable across a session's runs (that's the point). Derive
as `fnv1a64` over the length-prefixed BindIdentity triple
(`len(project_root) ‖ project_root ‖ len(harness) ‖ harness ‖ len(session) ‖
session` — length-prefixed so field boundaries can't alias). Stamped at
freeze; a fork (new session id) naturally gets a new key. OpenAI's
`prompt_cache_key`, xAI's `x-grok-conv-id` header, Fireworks' `prompt_cache_key`
all take the same value.

HONEST SCOPE: this is a ROUTING HINT, not a security boundary. A collision (or
any cross-session key reuse) degrades HIT RATE only — providers match on the
actual prefix bytes under the key, so a colliding key cannot serve another
session's content. FNV-1a-64 unsalted is therefore adequate; do not treat the
key as secret or collision-proof, and do not claim isolation from it. (True
cache isolation, where offered, is a different field — e.g. Fireworks'
`prompt_cache_isolation_key` — out of v1 scope.)

## 6. What this deliberately does NOT do (v1)

- **Prewarm/cachekeep** — banked (note #428): a runtime idle-pinger is module
  behavior, costs real money, and 1h-tier primaries mostly obviate it. Revisit
  with real cache_read telemetry.
- **Gemini explicit cachedContent objects** — a separate stateful cache-object
  flow (create/reference/expire lifecycle), not inline rendering; own design if
  ever wanted.
- **OpenRouter passthrough markers** — OpenRouter relays upstream
  `cache_control` for Anthropic-family routes; v1 keys the class off the
  RESOLVED wire family, so an anthropic-family-via-openrouter route gets
  markers for free. No OpenRouter-specific work.
- **Dynamic tier switching mid-run** — the tier is frozen per run; a policy
  change is an epoch (C7).

## 7. Open item — config ownership (#5, needs a ruling)

Where the defaults table lives is settled (llm-runner config-home). Open: who
sets the per-run ROLE for sessions spawned by other modules (Alfonso spawning
subagents via llm-runner, MC's historian firings)? Proposal: the historian is
`ephemeral` by construction (MC sets it on its session.send — one-shot, no
gaps); Alfonso's router stamps role from its own task model when it dispatches.
Both are caller-side one-liners on an existing surface. The alternative (a
central policy service) is rejected as indirection with no second consumer.

## 8. Verification plan

- Golden: placement algorithm unit-golden per arm (m0/m1 anchors, fallback,
  moving boundary, bridge-trigger at exactly lookback+1, marker-on-thinking
  never) + C7 determinism (same frozen config + prompt ⇒ byte-identical
  markers, across resume).
- Live (the 8799 pending arm): multi-turn primary session on real Anthropic —
  assert `cache_read_input_tokens > 0` from turn 2, cache-hit ratio across a
  defer run, a HARD fold busting exactly once, and the 1h-vs-5m write-premium
  visible in usage. This lights up the last gate arm.
