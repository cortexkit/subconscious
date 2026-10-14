# subc-mcp Gateway: Feature Scope, Tool-Surface Policy, and Code-Mode Readiness


## 0. The lens: the thin-core invariant

subc-core is a **zero-deserialization opaque-byte router**. It routes frames by
the 17-byte envelope and never parses a tool-call body. Everything that requires
understanding a tool, a schema, a policy, or a payload is a **module** concern.

Tool schemas, policy, and payload-aware features belong to modules or callers, never the opaque-byte core.
## 2. The caller-composed tool-surface policy (the deliverable)

**Goal:** per-agent / per-model / per-session tool-surface customization.


### 2.1 Principle: layers compose to a resolved flat policy

The tool surface is defined by **layers** that deep-merge into one **resolved
flat policy**. The module applies the resolved policy; it never sees the layers.

Precedence (later / more-specific wins), same merge semantics as the config-home
two-tier model (deep-merge by field, `null` deletes an inherited entry,
allow-then-deny within a layer):

```
global  <  harness  <  project  <  agent-role  <  model  <  session
```

- **global** — user baseline: which MCP servers/tools are enabled at all
  (`~/.config/cortexkit/mcp.jsonc`).
- **harness** — per-harness surface (opencode vs pi vs CK-app may differ).
- **project** — per-project (`<root>/.cortexkit/mcp.jsonc`).
- **agent-role** — the logical role the caller assigns (a `research` agent gets
  web tools; a `coding` agent gets edit tools). NOT the process identity (§2.4).
- **model** — the caller, knowing the model, folds a model layer (a weaker model
  gets a smaller surface; a no-vision model drops image tools).
- **session** — the live, most-specific scope; settable/changeable mid-session.

### 2.2 Two composition sites (mirrors config-unification)

- **Owned-harness / CK-app path (full power):** the **caller** (CK app / Alfonso /
  llm-runner) knows agent-role + model + session and reads the config layers, so
  the CALLER composes all six layers and hands the module the resolved flat
  policy. Full per-agent/model/session scoping.
- **Generic-MCP-host path (degraded):** a dumb upstream host (Claude Code, Cursor)
  supplies no agent/model/session policy, so the **module** composes from
  config-home (**static layers only**: global/harness/project). No agent/model
  dynamism beyond the host's own session. This is the same asymmetry as config
  unification: the module reads its own config-home for the static part; the
  caller supplies the dynamic part when it can.

The resolved policy is **caller-owned control input**, not content — so it rides
the request control-plane, never a poison-prone content field (same discipline as
`agent_drop_ids` and the excluded `boundary_present`).

### 2.3 The resolved flat shape (wire, illustrative)

```jsonc
{
  "surface_mode": "full",          // full | search | code  (§3, §4)
  "tools": [                       // the resolved ENABLED set (post allow/deny)
    { "module_id": "aft", "bare_name": "read",
      "exposed_name": "aft_read", "execution_mode": "pure", "enabled": true }
    // ...
  ],
  "overrides": {                   // per-exposed-tool tweaks
    "aft_read": { "description": "…model-facing override…" }
  }
}
```

- The module applies it verbatim: it exposes exactly `tools` (or, under `search`/
  `code`, the meta-surface over that set). Nothing more.
- **Model-agnostic module:** the module never sees `model` — per-model scoping
  happened entirely in caller composition. This preserves both thin-core and the
  model-agnostic router boundary.

### 2.4 Composition with the two primitives we're already building

- **Identity axis = the authenticated principal** (note #410). "Which caller"
  (opencode plugin / runner / facade) is the *principal*, not a policy field. The
  principal gates *trust* (may this caller impose this policy at all); the policy
  gates *surface* (which tools). Orthogonal: principal = who, policy = what.
- **Session axis = `BindIdentity {root, harness, session}`** — already relayed
  per-route. The session layer keys on it. A mid-session policy change is a
  control op on the route (see §2.5).
- **agent-role** is a caller-supplied label, distinct from the principal (one
  authenticated caller can run many agent roles).

### 2.5 Stickiness + refresh (ratified 2026-07: pending-queue with a
configurable drain class)

The active tool surface is **sticky across turns** and is a first-class part of
the render config: byte-stability of the tool list is a cache-stability input
(tools ride the frozen render-config; a reordered or churned tool list busts
the prefix cache from position 0, since the tool block precedes the messages).
Applying a tool-surface change is therefore always HARD-class cache damage —
the design question is only WHEN it applies.

Ratified model: a config-side change (user adds a tool/MCP server, edits
allow/deny) does NOT touch live sessions immediately. It lands in a **pending
tool-surface change** for each affected session and is APPLIED per the
session's refresh mode:

- **`immediate`** — auto-apply on the next pass (busts the prefix now). For
  users who prefer freshness over cache economics.
- **`on-hard` (default)** — the pending change rides the next pass that is
  ALREADY a hard bust (model switch, system-prompt change, Magic Context epoch…): zero
  additional cache damage, the change is free by piggyback. Until then the
  session keeps its frozen surface.
- **`on-soft`** — the pending change rides the next bust pass of ANY class
  (a SOFT m1-delta pass counts). Applies sooner than `on-hard`, at the cost of
  escalating that SOFT into hard-class damage (the tool block busts the whole
  prefix regardless of which region triggered the pass).

Plus the **explicit-activation override**: in every mode the user can force the
pending change NOW (the CK app's "apply tool surface changes" action → a
forced `policy.set` on the session). Under `on-hard`/`on-soft`, explicit
activation is the only way a change applies outside its drain class — the CK
app shows "pending" until then.

MECHANISM NOTE: this is exactly the cache-core deferred-work model (pending
changes queue; a bust pass of sufficient class drains them; a HARD drains
everything) — the tool surface becomes another queued-work producer, not a new
mechanism. The refresh mode is per-session policy (part of §2.3's resolved
shape, `refresh: "immediate" | "on-hard" | "on-soft"`), so an ephemeral
subagent can run `immediate` while the primary runs `on-hard`.

---

## 3. Surface modes

`surface_mode` is how the resolved tool set is EXPOSED to the model. All three
modes operate over the SAME resolved policy (§2) — the policy picks *which* tools;
the mode picks *how*.

- **`full`** — expose every resolved tool as an individual MCP tool. Default.
  Best when N is small.
- **`search`** — expose a small search-and-invoke meta-surface over the resolved tool set; discovery and ranking stay module-side, with thin-core unchanged.
- **`code`** — expose a code-execution surface (§4). Future.

Modes are per-session (part of the policy), so a research agent can run `search`
while a scripted agent runs `code` — same daemon, same modules.

---

## 4. Code-mode constraints (future)

Code mode remains unbuilt; any future callable API must expose exactly the resolved policy's tool set, with the same principal, scoping, and per-tool `execution_mode` restrictions, rather than create a scope bypass.

In-sandbox calls keep their confirmation gates, and a tool's reverse request is relayed to the client with the in-sandbox call waiting for an answer and failing closed if delivery is impossible.

The future code-mode module records each script call's intent/result and each internal tool effect so a crash cannot re-execute a mutating call whose effect was already recorded; this preserves the at-most-once boundary for the outer tool call and its inner effects.

The future code runtime exposes only tool bindings from the resolved policy (§2.3), not ambient filesystem, network or environment access. The module enforces CPU/step, wall-time and memory limits to bound script resource use; the policy retains `surface_mode` and per-tool `execution_mode` without requiring a runtime dependency now.

## 5. subc-core invariants this design must NOT violate (the boundary, restated)

1. subc-core never deserializes a tool-call body (rules out firewall/shadow-scan
   in core).
2. The tool-surface policy is **module/caller state**, never subc-core state.
3. Reliability (circuit breaker / retry / cache / idempotency) is module-side,
   never core.
4. subc-core is model-agnostic — per-model scoping is caller composition only.
5. code-mode's QuickJS runtime is a module; the core just routes its
   consumer-side tool dispatches like any other module-to-module call.

---

## 6. Policy integration requirements

`agent-role` is a free-form caller/app label. The caller supplies the resolved tool policy when binding its session, then updates it through the gateway's `policy.set`; tool-set changes wait for the session's `immediate`, `on-hard`, or `on-soft` refresh condition unless the user explicitly applies them (§2.5).

Search ranks enabled tools by their names and descriptions; ranking from recorded usage is future module work, not a current policy input. Future code execution runs in a dedicated module, while subc-core continues only to route opaque bytes (§5).
