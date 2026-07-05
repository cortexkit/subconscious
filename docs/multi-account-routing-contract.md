# Multi-account + quota-aware routing — cross-module contract (design, pre-build)

Status: design locked with Ufuk; pre co-design with ALF (router) + QTA (quota) + a
multi-module Oracle gate before build. Spans three modules: the credential vault
(cortexkit-credentials), the quota module (ai-provider-quota), and the router
(alfonso-routing). Brokered here (subc owns the cross-module integration contracts).

Grounded in the PROVEN reference: the anthropic-auth OpenCode/Pi plugin
(~/Work/Projects/CortexKit/anthropic-auth/packages/core/src/{accounts,routing,killswitch}.ts).
This contract carries that model forward (not a stripped version) onto the subc planes.

## 0. The load-bearing boundary: data vs policy

- **QTA = raw per-`(provider, account)` quota DATA. Never policy.** It serves each
  window's `remainingPercent` + `resetsAt` + a freshness/unknown flag, per account,
  faithfully. It NEVER pre-combines accounts, picks a primary, applies a threshold, or
  decides fail-closed. (Contaminating the data with policy would destroy the router's
  ability to apply the user's strategy.)
- **Router = ALL policy.** Usability gates (killswitch + quota-min + fail-closed),
  selection strategy, ordering, the reactive-fallback status set. The router reads QTA's
  raw windows and computes "quota pressure" per its user-configured strategy.

QTA answers "how much is left on each account." The router answers "which account (and
which provider) do we use, and when do we route around."

## 1. The candidate unit is `(provider, account)` — the key generalization

A logical model request does NOT resolve to one account; it resolves to an **ordered
list of `(provider, account)` candidates**. This unifies two fallback dimensions under
ONE contract:
- **multi-account** — the same provider, N accounts (`anthropic:work`, `anthropic:personal`).
- **multi-provider-same-model** — N providers serving the same logical model
  (e.g. an open model via DeepSeek-direct vs OpenRouter vs Fireworks; or Claude via
  Anthropic vs Bedrock vs Vertex).

The router orders candidates across BOTH dimensions by quota pressure + strategy. The
contract carries `(provider, account)` from day 1, so multi-provider fallback is a later
POPULATION change in the router (add cross-provider candidates), NOT a contract change.
Design the seam right now; phase what fills it.

## 2. credential_id convention

`credential_id = "<provider>:<account>"` — `provider` = the canonical catalog provider
id (router maps model → provider → account-set); `account` = a user label
(`work`/`personal`/…). Provenance stays the record's separate `source` field;
`refresh_adapter` stays an explicit record field. So the id is purely the selection key.
(The legacy `opencode:anthropic` becomes `anthropic:<account>`.)

## 3. Vault (cortexkit-credentials) — additions

- **`list-accounts` (non-secret enumeration).** Returns `[{ credential_id, provider,
  account_label }]` — ids + labels ONLY, NEVER tokens or handles. Used by BOTH the
  router (discover the account-set for a provider) and the future CK-app account picker.
  Safe because it leaks only non-secret metadata; a handle is still required to GET a
  token. (Resolves "how does the router discover accounts" = vault is the source of
  truth, not duplicated config.)
- **`display_label` column** on the record (the picker label; CKCRED already flagged it).
- Everything else: ZERO vault change. Multi-account is N rows under the free-form
  `credential_id` PK, each with its own OAuth/refresh/handles, per-id single-flight
  refresh. The vault was account-agnostic by construction.

## 4. QTA (ai-provider-quota) — per-account quota data

(QTA-reacted at source; the four items below are folded from its react.)

- `usage.get` returns a window set per **`(provider, account)`** instead of one per
  provider. Each window: `{ window_name, remaining_percent, resets_at, fresh: bool }`
  (provider-generic window set — Anthropic has `five_hour`+`seven_day`, others differ;
  QTA reports each provider's real windows, never assumes a fixed pair).

- **TWO credential-source classes (QTA's correction — the contract must NOT assume every
  provider is a vault consumer):**
  - **vault-sourced** (genuine multi-account, handle-per-account): the OAuth set
    (codex/claude/gemini/grok) + the API-key set (elevenlabs/llmproxy/warp/… ). These
    have a token the vault holds N of → QTA is a vault consumer for them. **The actual
    multi-account demand is the OAuth set** (Ufuk: "several Claude/OpenAI accounts" =
    claude + codex); the API-key set is multi-account-CAPABLE, populate-later.
  - **machine-local** (single implicit account, NOT a vault consumer, NO handle): the
    browser-cookie cohort (cursor/factory/mimo/opencode/opencodego/amp/ollama),
    antigravity (probes the one running editor), jetbrains (local IDE XML). These source
    from local machine state, not a vault token — structurally ONE account (`account =
    "local"`), EXEMPT from the vault-consumer path. The `fresh` flag still applies (dead
    cookie / stopped editor → `fresh:false`).

- **The read path must be NON-BLOCKING (QTA's Q4 correction — this was wrong in my draft).**
  QTA's cache today is a LAZY pull-through (a miss synchronously fetches ALL providers
  inline), so `route.select` on a cold/expired cache WOULD block on N HTTP fetches — Q4 is
  NOT satisfied today. v2 build item (DESIGN, not free): flip to a **background-refresher**
  (the reference `startBackgroundRefresh` 60s+jitter tick warms per-`(provider,account)`)
  + a **cache-only read path** (miss/stale → last-known windows marked `fresh:false`,
  NEVER an inline fetch). The router reads the cache; it never triggers a live fetch on
  the hot path.

- **Per-account quota-fetch backoff = TRANSIENT-AWARE (carry the reference, don't
  reinvent):** exponential (`60s · 2^min(retryCount-1,6)`, cap 15m) for TRANSIENT
  (429/≥500), fixed 5m for non-transient; honor the `429 Retry-After` header when present;
  **stale-but-still-relevant** — a cached window whose `resets_at` hasn't passed is STILL
  served (marked `fresh:false`), never blanked on a single 429. QTA does NOT decide
  fail-closed — it reports `fresh`, the router decides.

- **The `fresh` field is an ADDITIVE wire change → multi-module gate.** ProviderUsage /
  RateWindow has no freshness field today, and its window shape has TWO consumers: the
  EXISTING alfonso pace-model quota consumer (single-account aggregate, already cut over)
  AND the new per-account router. So `fresh` must be serde-default / backward-compatible
  (same class as the `resets_at`-optional change already shipped) so it doesn't break the
  existing consumer. Coordinate the shared-shape change with ALF (its extractor reads the
  window).

- NO thresholds, NO ordering, NO combination. Pure per-account data.

- **Cold-start / absent-data invariant (pinned during the refresher spike):** an
  absent `(provider, account)` in QTA's output means "no data yet" (pre-first-sweep
  or never fetchable), and the router MUST treat it as unusable-for-ranking — never
  as zero, healthy, or implicitly-full quota. QTA never fabricates a placeholder
  window to avoid this; the router react implements against absence directly.

- **Retry-After handling (graduation-scoped):** the spike ships class-based bounded
  backoff only; honoring `429 Retry-After` lands with the structured fetch-error
  change at graduation, and when it lands the value MUST be clamped (an unbounded
  upstream-controlled delay could suppress refresh for hours — never honor it raw).

## 5. Router (alfonso-routing) — selection + the policy model

### 5a. The candidate-list contract (resolves the reactive-reroute fork → option A)
`route.select(model, …)` returns an **ordered list of candidates**:
`[{ provider, account, credential_id, /* handle ref */, reason }]`, in strategy order.
The consumer (llm-runner) WALKS the list:
- try candidate 0; on a **reactive fallback status** (default `[401, 403, 429]`,
  configurable) move to candidate 1; etc.
- report outcomes back (`mark_used` / `mark_failed{status}`) so the router's quota view
  + the account's reactive state stay fresh.

Chosen over one-at-a-time re-ask because it keeps the reactive retry tight (no
per-fallback round-trip to the router mid-request) while policy stays router-side (the
router pre-computes the order; llm-runner just walks it). It also naturally extends to
emergency swaps across providers (§1) — the walked list can span providers.

### 5b. The proven policy model (carried in full from the reference)
Two INDEPENDENT gates, both on `remaining_percent` per window:
- **Killswitch (hard block)** — block the request even if the API would accept it.
  Default `5h ≥ 5% / 7d ≥ 10%`; per-account overrides (a `main` default + per-account).
  A safety stop, not a route-around.
- **Quota-min (soft)** — "prefer not to route here, fall to another." Filters the usable
  set.

Two TRIGGERS:
- **Preemptive** — quota below threshold → route around BEFORE trying the account.
- **Reactive** — the real request returns a fallback status (`[401,403,429]`) → mark +
  next candidate (§5a).

Safety defaults (carry them):
- **`fail_closed_on_unknown = true`** — an unknown/stale window (`fresh:false`) → treat
  the account as UNUSABLE (don't gamble). Router applies this on QTA's `fresh` flag.
- **all-candidates-killed `retry_after`** = earliest `resets_at` across every candidate's
  windows (so the caller knows when to retry rather than hard-failing).

### 5c. Selection strategies (pluggable seam, config-selected)
The strategy is a **pluggable policy** chosen by user config — built pluggable from day 1
even though one ships first:
- **`emergency_fallback`** (DEFAULT) = the reference `main-first`: ordered preference,
  main account until its window gates out, then secondaries. (= Ufuk's "secondary as
  emergency.")
- **`fallback_first`** = the reference `fallback-first`: burn fallbacks first to PRESERVE
  main's quota. (Carry it; the plugin has it.)
- **`combined_pool`** (NEW — no reference precedent, design carefully) = Ufuk's "combined
  quota as 2 accounts": pressure = combined headroom; load-balance to the account with
  the most room; the budget is the SUM, requests DISTRIBUTE across accounts.
- (more later — the seam is the point.)

**Invariant:** the unit of selection is ALWAYS one account per request (a single request
can't split across tokens). `combined_pool` means "sum the budget, distribute the
requests"; `emergency_fallback` means "use main's token until its window gates out." The
strategy only decides WHICH single account; QTA stays "report each window," the router
stays "pick one (ordered)."

## 6. Phasing

- **v1 (basic multi-account):** vault `list-accounts` + the `<provider>:<account>`
  convention + the router candidate-list contract with explicit/default account
  selection (the list may be 1 candidate). No quota policy yet. Independent of the vault
  cutover EXCEPT QTA-as-vault-consumer.
- **v2 (quota-aware):** QTA per-`(provider,account)` windows + the router's
  `emergency_fallback` policy (pluggable seam, the full §5b gate model). Gated on the
  vault being the live cred source.
- **v3 (later):** `combined_pool` + `fallback_first` strategies; multi-provider-same-model
  candidate population (needs the model→providers catalog mapping; models.dev partly
  provides it). NO contract change — the §1 candidate unit already carries it.

## 7. Division of labor + build sequence

- **CKCRED (vault):** `list-accounts` + `display_label` column. Small; the vault is
  otherwise done.
- **QTA (ai-provider-quota):** per-`(provider,account)` `usage.get` + vault-consumer
  token fetch + per-account quota backoff. The genuine data build. QTA is idle now.
- **ALF (router):** the candidate-list `route.select` shape + the pluggable policy seam +
  `emergency_fallback` + the reactive `mark_used`/`mark_failed` feedback. ALF owns the
  router.
- **Consumer (llm-runner):** walks the candidate list + reports outcomes — a LATER
  integration once llm-runner's own build settles (not now).

Sequence: this contract → ALF + QTA react on their module surfaces → fold in → Oracle-gate
the converged multi-module design → build (QTA per-account data + ALF policy seam +
CKCRED list-accounts can go in parallel, separate repos, no contention with the in-flight
llm-runner/vault flagships).

## 8. Open questions for co-design + the Oracle

1. **Handle delivery to QTA + router.** QTA (vault consumer) needs a handle per account;
   the router needs to know which handle maps to which candidate. Does the router pass a
   handle ref in the candidate, or does the consumer resolve `credential_id` → handle from
   its own config? (Leans: the consumer holds the handles; the candidate carries
   `credential_id` and the consumer maps it to a handle it already holds. Confirm.)
2. **Reactive state home.** Where does the just-failed-account / backoff state live — the
   router (so a subsequent `route.select` excludes it) or the consumer's walk (transient
   per-request)? The reference keeps it durable (per-account `nextRetryAt`). Lean: router
   holds the durable per-account reactive state (it already persists route state); the
   consumer's walk is the in-request retry.
3. **`combined_pool` exact semantics** (v3) — load-balance by most-headroom vs
   round-robin vs weighted; defer detailed design to v3 but don't foreclose.
4. **Quota freshness vs request latency.** QTA's per-account fetch + backoff must keep the
   router's `route.select` fast (cache-served, never blocking on a live quota fetch in the
   hot path). Confirm the router reads QTA's CACHED windows (latest-wins), never triggers a
   synchronous fetch on the selection path.
