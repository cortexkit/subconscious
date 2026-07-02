# subc-mcp Gateway: Feature Scope, Tool-Surface Policy, and Code-Mode Readiness

Status: DESIGN (pre-review). Owner: subc. Reviewed against `mcp-gateway`
(github.com/mikkoparkkola/mcp-gateway, cloned to `~/Work/OSS/mcp-gateway`) as
prior art — a mature Rust MCP gateway that solves the same problem with the
opposite architecture (fat single-binary gateway vs our thin-core + module OS).

This doc does three things:
1. Maps every mcp-gateway feature to a subc disposition (provide / existing
   module / **won't touch**), so the thin-core boundary is explicit and durable.
2. Specifies the **caller-composed tool-surface policy** — the real deliverable:
   per-agent / per-model / per-session tool-surface customization.
3. Designs the policy so **code-mode** (QuickJS: the agent writes code that
   calls/composes tools) slots in later as a *surface mode*, with zero rework.

---

## 0. The lens: the thin-core invariant

subc-core is a **zero-deserialization opaque-byte router**. It routes frames by
the 17-byte envelope and never parses a tool-call body. Everything that requires
understanding a tool, a schema, a policy, or a payload is a **module** concern.

So the disposition question for every gateway feature is not "do we want it" but
"**where does it live**": the subc-mcp gateway module, an existing module
(credentials / llm-runner / alfonso-routing), the CK app, or **nowhere in subc**
(a deliberate non-goal because it would fatten the core or require body
inspection). mcp-gateway puts ~all of this in one process; we distribute it.

---

## 1. Feature disposition map

### 1.1 subc-mcp gateway MODULE provides (MCP-facing, thin-core intact)

| mcp-gateway feature | subc disposition |
|---|---|
| Meta-tool discover-on-demand (`gateway_search` + `gateway_invoke`, small surface vs N tools) | **Provide** — the optional search surface; real context win (~150 tok/tool saved). See §3 `surface_mode: Search`. |
| `surfaced_tools` (which tools are always visible) | **Provide** — part of the resolved tool-surface policy (§2). |
| Namespacing + per-tool enable/disable | **Already designed** (subc-mcp gateway config). Folds into §2. |
| Tool-surface scoping (allow/deny globs) | **Provide via caller-composed policy** (§2) — NOT self-service profiles. |
| `autotag`, differential descriptions, search ranking | **Provide (module)** — quality-of-life for the search surface. All module-side. |
| `transition` (predict next tool) | **Optional module** — low priority; usage-learning, not correctness. |
| Server→client elicitation/sampling/roots relay (`proxy.rs`) | **= gateway primitive #3** (note #413). Reverse-lane is emergent in subc-core; relay logic is module-side. Their `destructive_confirmation.rs` is a working reference for AFT bash-permission — but they fail-**open** on undeliverable; we fail-**closed** (keep our decision). |

### 1.2 Existing OTHER modules already own these

| mcp-gateway feature | Our home |
|---|---|
| `secrets` (`{keychain.X}`/`{env.X}`/`{oauth:provider}`), OAuth 2.0 PKCE | **cortexkit-credentials** vault (shipped; richer). |
| Cost/usage/token-savings stats, `cost_governance` budgets | **llm-runner** (reads authoritative `Usage`; never estimates) + router domain. |
| Model routing | **alfonso-routing**. |

### 1.3 subc-core — essentially nothing new

The only core-adjacent item is the reverse-request lane for elicitation
(primitive #3), which is **already emergent** in the router (source-verified:
`route_for_connection` forwards any frame type module→client; flow-control credits
only the client→module Request, so no deadlock). No wire rebuild. The relay
*logic* is module-side.

### 1.4 Deliberate NON-GOALS (won't touch in subc-core)

These are the boundary. They stay out of subc-core because they either fatten the
router or require body deserialization we structurally refuse.

- **In-gateway reliability stack** — circuit breaker, kill switch + error budget,
  retry, health check, response cache, idempotency cache. In our model each is
  **module-side** (a module owns its own failsafe) or **caller-side**. subc-core
  stays a router.
- **Body-inspection features** — `firewall`, `shadow scan`, prompt-injection
  detection, anomaly detection, credential redaction. These require **parsing the
  tool-call body**, which violates zero-deserialization routing. If ever wanted,
  a dedicated inspection **module** sits in the data path — never subc-core.
- **Product-surface features** — `control_plane` RBAC, TrustCard/TrustLab,
  `marketplace`/`registry`, embedded web UI, `key_server`/OIDC, mTLS, `webhooks`,
  `playbooks`, `transform` pipeline, OpenAPI import, MCP-server `validator`.
  These belong to the **CK app** + module ecosystem, evaluated case-by-case as
  modules or app surface — not subc-core.
- **`code_mode` (their sequential-chain version)** — we do the richer QuickJS
  version as a surface mode (§4), module-side.
- **`session_sandbox`, `tunnel` (tailscale)** — orthogonal; tunnel overlaps our
  **federation** design, where we chose Noise over a tailscale-style overlay.

---

## 2. The caller-composed tool-surface policy (the deliverable)

**Goal:** per-agent / per-model / per-session tool-surface customization.

**Why mcp-gateway's model is only a subset:** their `routing_profile` is
**session-scoped but agent-self-chosen** (the agent calls `gateway_set_profile`);
their per-API-key `allowed_tools` is **static per credential**; they have **no
model dimension at all** (the gateway never knows the model). We want
externally-imposed, model-aware, identity-bound scoping — a superset.

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
  ALREADY a hard bust (model switch, system-prompt change, MC epoch…): zero
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
- **`search`** — expose a tiny meta-surface (`search` + `invoke`) and let the
  model discover tools on demand. The one genuinely stealable idea from
  mcp-gateway: ~150 tok/tool of context saved, "unlimited" practical tool count.
  Pure module logic; thin-core intact. The optional search tool Ufuk asked for.
- **`code`** — expose a code-execution surface (§4). Future.

Modes are per-session (part of the policy), so a research agent can run `search`
while a scripted agent runs `code` — same daemon, same modules.

---

## 4. Code-mode readiness (QuickJS — design now, build later)

**The vision (Ufuk):** all tools callable via QuickJS; the agent writes code to
call and **compose** multiple tools/MCP-tools in one turn (control flow, fan-out,
intermediate transforms) instead of N separate tool-call round-trips.

This is the real Cloudflare-style Code Mode — strictly richer than mcp-gateway's
`gateway_execute` (which is only a declarative sequential `{tool,args}` chain, no
code). We design the policy so it slots in as `surface_mode: code` with **zero
rework**, and build it later.

**Reference model (TanStack AI code-mode + Cloudflare):** the model gets a single
`execute_typescript`-style tool and writes a self-contained program (loops,
conditionals, `Promise.all` parallelism, data transforms) run in an isolated
sandbox; a query that took 4 LLM round-trips completes in ~2 with far less
context. Two design specifics worth adopting:
- **Pluggable isolate driver.** TanStack ships V8-isolate (native, fastest) and
  QuickJS-WASM (portable, no native deps) drivers behind one contract. Our
  Rust equivalent: start with **`rquickjs`** (QuickJS, portable, embeddable, no
  native/V8 build) and keep the isolate behind a trait so a V8-isolate backend
  can drop in later where throughput matters. Driver choice is a module-internal
  detail — invisible to subc-core and to the policy.
- **Typed bindings.** The injected tools are TYPED so the model writes valid code.
  Our source of those types is the **resolved policy's per-tool schema** (§2.3):
  the code-mode module generates the TS type surface from exactly the enabled
  tool set. So the typed API is a projection of the same policy — reinforcing §4.2
  requirement 1 (code-mode is not a scope-bypass; the callable surface == the
  policy).

### 4.1 Why it fits thin-core cleanly

The QuickJS runtime lives in a **module** (the subc-mcp gateway module, or a
dedicated `code-mode` module). subc-core is unaffected — it still routes opaque
bytes. The code-mode module is BOTH a served provider (it exposes an
`execute_code` tool) AND a **consumer** (it opens routes to the real tool modules
and dispatches each in-sandbox tool call back through subc). This is exactly the
module-to-module consumer path we already support (memory 7298/7300).

```
model → execute_code(js) → [code-mode module: QuickJS runtime]
                              │  tool bindings = the resolved policy's tool set
                              └─ each ck.tools.aft_read(...) → subc → aft module → result → back into JS
```

### 4.2 The readiness requirements the policy must already satisfy (why we design now)

1. **The callable API == the resolved policy's tool set.** The JS runtime exposes
   exactly the tools in the resolved tool-surface policy (§2) as callable
   functions — no more. So code-mode is a *projection of the same policy*, not a
   new access surface. Allow/deny, per-agent/model/session scoping, and the
   authenticated principal's trust all apply unchanged. **This is the load-bearing
   reason to design the policy first:** code-mode must not become a scope-bypass.
2. **execution_mode is enforced at the binding, not just the tool.** A `mutating`
   tool called from inside JS is still `mutating`. The sandbox must carry each
   binding's `execution_mode` so fencing/idempotency (llm-runner ToolPlane
   semantics) and any confirmation gate still fire per-call — even when the call
   originates inside a code block. So `execution_mode` must be in the resolved
   policy per tool (it already is, §2.3).
3. **Elicitation composes through the code frame** (primitive #3). A tool called
   inside JS that needs mid-execution HITL confirmation raises the reverse-request
   lane from the tool module → the code-mode module must relay it to the client
   and block the *in-sandbox* call on the answer (fail-closed on undeliverable,
   same floor as §1.1). So the code-mode module is an elicitation *relay point*,
   not just a dispatcher. Design note, not a v1 blocker.
4. **Determinism / durability boundary.** Code-mode execution is a single logical
   "tool call" from the loop's view — its intent + result must fence like any
   other (at-most-once). But an in-sandbox call sequence has *internal* effects.
   Decision for the build: the code-mode module owns an **internal effect log**
   (which in-sandbox tool calls ran) so a crash mid-script doesn't silently
   re-run mutating in-sandbox calls — the same intent-log discipline llm-runner
   uses, applied one level down. Flagged for the code-mode build, not now.
5. **Sandbox constraints.** QuickJS with no ambient I/O (no fs/net/env inside the
   JS) — the ONLY capability is the injected tool bindings. CPU/step limits, wall
   timeout, memory cap. Standard sandbox hardening; module-side.

### 4.3 What we do NOW (readiness, not implementation)

- Keep `surface_mode` in the policy as `full | search | code` (§2.3, §3).
- Keep `execution_mode` per-tool in the resolved policy (§2.3) — the code binding
  needs it.
- Keep the tool-surface policy as the single source of "which tools" so code-mode
  is a projection, not a bypass.
- Nothing else. No QuickJS dependency, no runtime, until we build it.

---

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

## 6. Decisions (ratified by Ufuk, 2026-07-02)

1. **Composition home** — as leaned: caller composes the dynamic layers on the
   owned-harness path; module composes static-only (config-home) on the
   dumb-host path. The module never composes agent/model layers.
2. **`agent-role`** — free-form string. Roles are a caller/app concept.
3. **Policy delivery** — rides the session bind + `policy.set` on the GATEWAY
   MODULE's own control surface (subc-control stays generic). Mid-session
   changes follow the ratified pending-queue refresh model in §2.5: config
   changes never bust a live session's cache implicitly; they apply per the
   session's refresh mode (`immediate` | `on-hard` default | `on-soft`) or on
   the user's explicit "apply tool surface changes" activation from the CK app.
4. **Search-mode ranking** — start pure name/description match; usage-weighting
   later as a module-side store.
5. **Code-mode home** — a dedicated `code-mode` module, built LAST; the policy
   shape is code-mode-ready now (§4.3), nothing else moves early.
