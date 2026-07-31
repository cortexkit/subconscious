# MCP-Facade Tool-Surface Policy — Implementation Spec (v2)

Implements §2/§3 of `docs/subc-mcp-gateway-design.md` for the generic-MCP-host
path. Scope: `crates/subc-mcp` only. subc-core is untouched (thin-core §5 of the
design doc holds).

v2 incorporates the Oracle gate findings (bg_284e68fb): harness identity moved
off `clientInfo.name` onto the existing trusted `--harness` flag, a normative
raw-config schema + monotonic project-tier merge, search-mode dispatch pinned to
a private binding table, meta-tool name reservation, the catalog-liveness
exception, zero-tool provider route drop, and a concrete default-deny mechanism.
v3 folds the re-gate (bg_03eb1252): schema aligned with the shipped
`RawGatewayConfig` spelling and shapes, explicit null semantics, unknown-field
rejection stated as an implementation requirement, project-tier `refresh`
dropped, collision semantics corrected to the shipped whole-attach fail-closed,
and monotonicity defined over the provider-callable set.

## 1. What v1 builds

1. **Static-layer policy composition** (module-composed): the facade resolves
   its tool surface from config-home files at session attach.
2. **Per-session sticky surface + pending-changes queue** with the drain
   classes that are honest on this path (§5).
3. **`surface_mode: "search"`** — the discover-on-demand meta-surface.
4. **Per-tool description overrides** (global/harness tiers only; §4.3).

Explicit non-goals for v1: `code` mode (QuickJS), caller-composed dynamic
layers (agent-role/model/session — no trusted caller exists on this path;
`mcp:generic` principals are untrusted and MUST NOT impose policy), any
`policy.set` wire op (no CK app yet; config files are the only writer, same
read-only-consumer model as every other module).

## 2. Harness identity (v2: trusted flag, not client claim)

The harness layer keys on **`ShimHello.harness`** — the existing `--harness`
CLI flag the USER writes into their own MCP client config
(`subc-mcp shim --harness claude-code`), defaulting to `DEFAULT_HARNESS`.

Why not MCP `clientInfo.name`: (a) it is not available at attach — the shim
sends `ShimHello` and the module attaches the session BEFORE MCP `initialize`
flows; (b) it is host-claimed, and a host claiming an unknown name would dodge
a restrictive harness profile. `--harness` is user-authored config (the same
trust origin as the config files themselves), attach-time available, and
host-unspoofable. No wire change needed — the field already exists.

## 3. Raw config schema (normative)

Both files (`~/.config/cortexkit/mcp.jsonc` global, `<root>/.cortexkit/mcp.jsonc`
project) share one schema. This EXTENDS the existing `RawGatewayConfig`
(version + providers) — existing fields keep their exact semantics.

```jsonc
{
  "version": 1,
  "surfaceMode": "full",             // "full" | "search"; absent = "full"; null = reset to absent
  "refresh": "on-attach",            // "on-attach" | "immediate"; absent = "on-attach"; null = reset
                                     // "on-hard" | "on-soft" = RESERVED: parse error with
                                     // "requires a bust-signal source; not available on the MCP path"
  "providers": {
    "<module_id>": {
      "enabled": true,               // tri-state: absent | null (delete tier's setting) | bool (existing)
      "namespace": "aft",            // exposed-name prefix (existing)
      "tools": {
        "defaultEnabled": true,      // existing (camelCase, matches shipped RawToolConfig)
        "overrides": {
          // Existing bool shorthand AND the new object form are both valid
          // (serde untagged). bool = enable/disable; null = delete this
          // override entry (existing semantics, preserved).
          "<bare_name>": false,
          "<other_name>": {
            "enabled": false,        // absent = inherit defaultEnabled; null INVALID inside object
                                     // (delete the whole entry with the null shorthand instead)
            "description": "…"       // NEW: model-facing description override; absent = provider
                                     // manifest description; null INVALID (omit instead)
          }
        }
      }
    }
  },
  "harness": {                       // NEW: per-harness overlay sections
    "<harness_name>": {
      "surfaceMode": "search",       // may override top-level
      "refresh": "immediate",
      "providers": { /* same provider schema */ }
    }
  }
}
```

Spelling and shape are NORMATIVE and match the shipped `RawGatewayConfig` /
`RawToolConfig` (camelCase `defaultEnabled`; overrides accept the existing
`bool | null` shorthand, extended with the object form via untagged deserialize).

Unknown fields: rejected — an IMPLEMENTATION REQUIREMENT of this spec, not
current behavior (the shipped Raw structs do not carry `deny_unknown_fields`;
the build must add it at every level of this schema and gate-test it). This is
what makes the reserved `refresh` values and future keys fail loud instead of
silently no-oping. `harness` sections with names not matching the session's
harness are ignored (not validated against a registry — free-form,
lowercase-compared).

## 4. Composition algorithm (normative, monotonic)

### 4.1 Order

```
1. global top-level
2. global harness.<h> section
3. project top-level          (narrowing-only, §4.2)
4. project harness.<h> section (narrowing-only, §4.2)
```

Steps 1-2 use the existing later-wins merge (`merge_gateway_config` semantics:
`Missing` no-op, `Null` deletes/resets, `Value` sets). Both are user-authored
files in the user's home — full trust, may grant or narrow.

### 4.2 Project tier is narrowing-only (v2, closes the grant hole)

The project file is in-repo = untrusted (same per-tier-file trust model as
config unification). Its merge is a RESTRICTED operator applied AFTER the
global baseline is fully composed:

- `enabled`: may set `false`. `true` and `null` are DROPPED (with a WARN log
  naming the field) when the baseline has the provider disabled — a project
  cannot enable a provider the user disabled, and cannot null-delete a deny.
  Setting `true` on an already-enabled provider is a no-op (allowed, harmless).
- `tools.default_enabled`: may set `false`; `true`/`null` dropped unless the
  baseline already has it `true`.
- `tools.overrides.<t>.enabled`: may set `false`; `true`/`null` dropped unless
  already enabled in the baseline.
- `tools.overrides.<t>.description`: DROPPED at project tier entirely (WARN).
  An in-repo file rewriting model-facing tool descriptions is a prompt-injection
  channel (Oracle finding; same class as AFT dropping privileged fields from
  project config).
- `namespace`: DROPPED at project tier (renaming affects collision handling and
  model-facing names — identity-adjacent, global-only).
- `surfaceMode`: project may set `"search"` (strictly narrowing exposure);
  `"full"` when the baseline says `"search"` is DROPPED (widening).
- `refresh`: DROPPED at project tier entirely (WARN). Re-gate finding: letting
  a project set `on-attach` under a global `immediate` would delay the user's
  own revocations (a global disable would not apply until next attach) — an
  in-repo file must not weaken revocation latency. `refresh` is global/harness
  tier only.

The result is monotone over the PROVIDER-CALLABLE SET: for every provider
tool, callable(project applied) ⊆ callable(global baseline), and no
model-facing string is project-controlled. (`surfaceMode: "search"` changes
the literal MCP tool list to the two meta-tools; monotonicity is stated over
what is invokable through whatever surface is exposed, not over literal
`tools/list` names.)

### 4.3 Description overrides

Global/harness tiers only (§4.2). Applied to the MCP `Tool.description` served
to the host. The schema is never overridable (schema comes from the provider
manifest verbatim — existing behavior).

## 5. Stickiness, refresh, and the liveness exception

### 5.1 Frozen policy at attach

The RESOLVED POLICY is computed once at shim-session attach (the existing
`attach_session` → config read → `desired_session_from_catalog` flow already
does this — v1 formalizes it) and frozen for the session.

### 5.2 The liveness exception (explicit, Oracle S5)

POLICY is frozen; LIVENESS is not. Catalog changes (provider registers,
provider GOODBYE/dies) continue to mutate the served surface immediately and
emit `tools/list_changed`, exactly as today. Rationale: a dead provider's tools
are not servable regardless of policy, and a newly-live provider that the
frozen policy enables was always intended to be present (its absence was an
outage, not a decision). The frozen object is the POLICY (which tools WOULD be
exposed); the served list is `frozen_policy ∩ live_catalog`.

### 5.3 Refresh modes

Config-file edits during a live session do not change the frozen policy;
they become a PENDING change applied per the session's `refresh` mode:

- **`on-attach` (default)** — applies at the next shim-session attach (new
  conversation/process). Zero mid-session CONFIG-POLICY churn (catalog
  liveness still moves the served list, §5.2); the MCP analogue of
  "ride the next hard bust" (a fresh session IS a cold cache). Requires no
  persisted queue: config is the durable source; recompute at attach.
- **`immediate`** — the module re-reads config lazily on request activity
  (mtime check, no watcher thread). On change: recompute the policy, update
  the session's frozen policy in place, emit `tools/list_changed`. The host
  re-fetches and its next request pays the prefix bust. Sequencing note: the
  recompute happens BEFORE dispatching the request that triggered the mtime
  check, so a call to a just-disabled tool fails closed (`unknown tool`) even
  within the triggering request.

`on-hard` / `on-soft` remain reserved (§3) for the owned-path integration.

## 6. `surface_mode: "search"`

When the resolved mode is `search`:

- `tools/list` returns EXACTLY two tools: `tools_search` and `tools_invoke`.
- **The outer `tools/call` dispatch accepts ONLY those two names** (Oracle S6).
  The resolved per-tool bindings move to a PRIVATE table reachable only through
  `tools_invoke` — a direct `tools/call` on a resolved tool name returns the
  same error as a nonexistent tool. No bypass.
- `tools_search { query: string, limit?: number }` → ranked matches over the
  resolved enabled set: `[{ name, description, input_schema, execution_mode }]`.
  Lexical matching (name + description substring/token). Deterministic order
  (rank, then name) — the result feeds model context.
- `tools_invoke { name: string, arguments: object }` → looks up the private
  table; on hit, routes exactly as a direct call (same translate-free
  RouteToolCallRequest path, same error envelopes). On miss — including
  policy-disabled, catalog-dead, and never-existed — returns the identical
  `invalid_params("unknown tool '<name>'")` error (§8: "indistinguishable"
  means this exact single error path, not a family of similar messages).

### 6.1 Meta-tool name reservation (Oracle S7)

`tools_search` and `tools_invoke` are RESERVED exposed names in every mode.
The collision pass treats a provider tool resolving to either name as a
collision, and collisions keep the SHIPPED semantics: the ATTACH FAILS CLOSED
with an error naming the colliding exposed name (the existing machinery aborts
the session on any exposed-name collision; this spec does not change that —
the fix is the user renaming the provider namespace). The v2 text claiming
per-tool exclusion was wrong about the shipped behavior and is retracted.

## 7. Zero-tool providers get no route (Oracle S9)

If a provider's resolved callable tool set is empty (all tools policy-disabled
or provider disabled), `desired_session_from_catalog` EXCLUDES it: no route is
opened, so it can never send reverse requests into the session. The reverse
relay's surface is thereby policy-bounded: only providers with at least one
exposed tool hold a route. (Today an all-tools-filtered provider still gets a
route; v2 closes that.)

## 8. Trust invariants (restated, load-bearing)

- The facade path takes NO policy from the wire. Hosts and agents cannot widen
  their own surface. Policy-disabled and nonexistent tools are served by ONE
  shared error path.
- Project tier is narrowing-only and controls no model-facing string (§4.2).
- Principal story unchanged: facade binds are `reserved:subc-mcp`; provider
  modules keep their own policy mapping (AFT: forced-restrict, bash-deny).
  Facade policy narrows on top; it never grants.
- Collision handling stays fail-closed; meta-tool names reserved.

## 9. Default-deny for agent-internal modules (Oracle S10)

The facade ships a built-in constant `FACADE_DEFAULT_DISABLED: &[&str] =
&["magic-context", "llm-runner"]` — modules whose tool surfaces are
agent-internal control planes, not host-facing tools. Semantics and ORDERING
(load-bearing, because the shipped default is allow-by-absence via
`unwrap_or(true)`): the constant is applied as the PRE-MERGE BASELINE — for
these module IDs, absence means `enabled: false` instead of the default-true —
BEFORE the global tier merges. The GLOBAL tier may explicitly enable them
(`"magic-context": { "enabled": true }`); the project tier cannot (§4.2).
This is a facade-local default, not a manifest change — revisit as a manifest
capability flag (`facade_exposable`) if the list grows.

Rationale for the two entries: `magic-context`'s `ctx_reduce` requires the
shim-ephemeral → durable-session mapping that does not exist yet (the Mode-4
convergence design); `llm-runner`'s session ops are consumer APIs, not tools.

> STATE CLAIM, STALE AS OF 2026-07-31. The mapping SHIPPED: the shim carries a
> wrapper-minted conversation token (`CK_INSTANCE_TOKEN`) and binds the session
> verbatim, and `ctx_reduce` runs in production under `mode: "ack_only"` with
> the reduction applied downstream from the completed provider response.
> `magic-context` is enabled at the global tier in the live config, which is
> exactly the escape hatch the paragraph above describes.
>
> The DECISION is unchanged and still correct: these module IDs are
> baseline-denied, and only the global tier may enable them. What rotted is the
> OBSERVATION the decision was written around — a spec is frozen to preserve
> reasoning, and reasoning does not rot the way state claims do. Marked here,
> where a reader meets the stale sentence, rather than in a note at the end
> that the misled reader never reaches.
>
> `llm-runner` was also renamed to `broca` and no longer appears in the daemon
> config under that name. The entry is kept as written because it is a
> historical record of a gated decision; the current module ID is `broca`.

## 10. Tests (gate)

- Composition: global-only; global harness-section override; project narrows;
  project attempts-to-grant dropped WITH warn; project description-override
  dropped; project namespace dropped; project refresh dropped; null-deletes at
  global tier; null-at-project-tier dropped when it would widen; unknown
  harness name ignored; reserved refresh values rejected with the documented
  error; UNKNOWN FIELDS rejected at every schema level; bool-shorthand and
  object-form overrides both parse; existing configs (bool overrides,
  camelCase defaultEnabled) parse unchanged.
- Stickiness: mid-session config edit does not change served surface
  (`on-attach`); `immediate` recomputes + emits list_changed + just-disabled
  tool fails closed within the triggering request; provider death/rejoin still
  mutates the served list in BOTH modes (liveness exception).
- Search mode: tools/list = exactly the two meta-tools; direct tools/call on a
  resolved name fails with the unknown-tool error; tools_search returns only
  enabled tools, deterministic order; tools_invoke routes a real call over the
  live daemon; disabled/dead/nonexistent are one error path.
- Reservation: provider tool colliding with tools_search/tools_invoke is
  excluded fail-closed with an error log.
- Zero-tool provider: gets no route; sends no reverse requests.
- Default-deny: magic-context absent from surface by default; global-tier
  enable exposes it; project-tier enable does not.
- E2E: real daemon + fake-aft-stub + real shim, both modes, per the existing
  conformance-test pattern.
