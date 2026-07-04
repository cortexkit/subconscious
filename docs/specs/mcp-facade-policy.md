# MCP-Facade Tool-Surface Policy — Implementation Spec (v1)

Implements §2/§3 of `docs/subc-mcp-gateway-design.md` for the generic-MCP-host
path. Scope: `crates/subc-mcp` only. subc-core is untouched (thin-core §5 of the
design doc holds).

## 1. What v1 builds

1. **Static-layer policy composition** (module-composed): the facade resolves
   its tool surface from config-home files at session attach.
2. **Per-session sticky surface + pending-changes queue** with the drain
   classes that are honest on this path (§4).
3. **`surface_mode: "search"`** — the discover-on-demand meta-surface.
4. **Per-tool description overrides** (model-facing description rewrite).

Explicit non-goals for v1: `code` mode (QuickJS), caller-composed dynamic
layers (agent-role/model/session — no trusted caller exists on this path;
`mcp:generic` principals are untrusted and MUST NOT impose policy), any
`policy.set` wire op (no CK app yet; config files are the only writer, same
read-only-consumer model as every other module).

## 2. Layers and composition (facade path)

Three static layers, deep-merged (later wins, `null` deletes, allow-then-deny
within a layer):

```
global   ~/.config/cortexkit/mcp.jsonc          (top-level keys)
harness  same files, `harness.<client>` section (keyed by MCP clientInfo.name)
project  <root>/.cortexkit/mcp.jsonc            (top-level keys, then its own harness section)
```

- The harness key is the `clientInfo.name` the host sends at MCP `initialize`
  (e.g. `"claude-code"`, `"codex"`), lowercased. The shim already receives it;
  it must be forwarded to the module in the attach payload.
- An unknown/absent clientInfo.name simply means the harness layer contributes
  nothing (global+project only). Not an error.
- Project layer keeps the existing trust posture: it may only NARROW the
  surface (deny/disable); privileged grants (enabling a module the global tier
  disabled) are global-tier-only. Same per-tier-file trust model as config
  unification.

Resolved flat shape (persisted per session, from design doc §2.3):

```jsonc
{
  "surface_mode": "full",            // "full" | "search"
  "refresh": "on-attach",            // §4
  "tools": [ { "module_id", "bare_name", "exposed_name", "execution_mode", "enabled" } ],
  "overrides": { "<exposed_name>": { "description": "…" } }
}
```

## 3. Stickiness

The surface is resolved ONCE at shim-session attach and frozen for that
session. Config edits during a live session do NOT change the served surface;
they land in a pending change (§4). This mirrors the frozen-render-config
discipline: a stable tool block is a cache-stability input for the host's own
provider prefix cache.

## 4. Refresh / drain classes — honest v1 set

The ratified design names `immediate | on-hard | on-soft`. `on-hard`/`on-soft`
require a bust-class signal source, which exists on the owned path (cache-core
pass classes) but NOT here: the facade never sees the host's provider passes.
Pretending otherwise would be a dead knob. v1 therefore ships:

- **`on-attach` (default)** — pending changes apply at the next shim-session
  attach (new conversation/process). Zero mid-session cache damage ever; the
  natural MCP analogue of "ride the next hard bust" (a fresh session IS a cold
  cache).
- **`immediate`** — pending changes apply on the next config re-read tick: the
  module updates the session surface and emits `notifications/tools/list_changed`
  through the shim. The host re-fetches; its next request pays the prefix bust.
  For freshness-over-economics users and ephemeral use.

`on-hard`/`on-soft` are RESERVED words in the schema (rejected with a clear
"requires a bust-signal source; not available on the MCP path" error, so the
future owned-path/ai-proxy integration can claim them without a breaking
change).

Pending-change detection: mtime-based config re-read (the same mechanism MC
uses for its config), evaluated lazily on request activity — no watcher thread.

## 5. `surface_mode: "search"`

When `search`, the facade exposes exactly two MCP tools instead of the resolved
list:

- `tools_search { query: string, limit?: number }` → ranked matches over the
  resolved set: `[{ name, description, input_schema, execution_mode }]`.
  Matching is name+description substring/token match (no embedding dependency —
  the resolved sets are small enough that lexical is honest).
- `tools_invoke { name: string, arguments: object }` → validates `name` is in
  the resolved set (fail-closed `unknown_tool` otherwise, including for tools
  that exist in the catalog but are policy-disabled — indistinguishable from
  absent), then routes exactly as a direct call.

The resolved policy still gates everything: `search` changes exposure, never
membership. `tools/list` under `search` returns only the two meta-tools.

## 6. Trust invariants (restating, load-bearing)

- The facade path takes NO policy from the wire. Hosts and agents cannot widen
  their own surface (an agent asking `tools_invoke` for a policy-disabled tool
  gets `unknown_tool`).
- The principal story is unchanged: the facade's binds are `reserved:subc-mcp`;
  provider modules keep their own policy mapping (AFT: forced-restrict,
  bash-deny). Facade policy NARROWS on top of that; it never grants.
- Collision handling stays fail-closed (existing behavior).

## 7. Interlock: MC dual-envelope + ctx_reduce via facade (separate contract)

The facade will front MC's agent-facing tools eventually, but `ctx_reduce`
exposure through the facade is GATED on the session-identity mapping design:
shim sessions are process-ephemeral while MC's drops queue keys on the durable
wire session-id (ai-proxy's key). Routing an agent's ctx_reduce from a shim
session into the right ai-proxy-session queue needs the project_root →
active-session resolution (Mode-4 territory). v1 of THIS spec therefore ships
with `magic-context.*` default-disabled in the facade's global config, and the
MC-side dual-envelope work is limited to routing hygiene (typed envelope
dispatch + fail-loud unknown shapes). The convergence design is its own
follow-up.

## 8. Tests (gate)

- Composition: global-only; global+harness override; project narrows; project
  attempts-to-grant is dropped; null-deletes; unknown clientInfo.
- Stickiness: mid-session config edit does not change served surface
  (`on-attach`); `immediate` emits list_changed and serves the new surface;
  pending survives module restart (config is the durable source — recompute on
  attach, no persisted queue state needed).
- Search mode: search finds enabled tools only; invoke routes and returns real
  results over the live daemon; policy-disabled and nonexistent tools are
  indistinguishable (`unknown_tool`); tools/list shows exactly the two
  meta-tools.
- E2E: real daemon + fake-aft-stub + real shim, both modes, per the existing
  conformance-test pattern.
