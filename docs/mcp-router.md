# MCP Router: scoped tool-surface management

## What exists (build on, do not rebuild)

- **subc-mcp policy engine** (`crates/subc-mcp`): composes the advertised tool
  surface from module manifests at session attach. Global + project config,
  narrowing-only merges, default-deny for agent-internal modules, per-tool
  `ack_only`, tombstones for stale-surface races, `surface_mode: "search"`
  (tools_search/tools_invoke meta-tools), reverse-request relay, spawn
  attestation. Per-server AND per-tool granularity already works — for
  CortexKit's own modules, driven by config files.
- **ck-mcp-stdio-adapter** (subconscious#20, ruled in plexus#4; full contract
  in `docs/mcp-stdio-adapter.md`, which supersedes earlier text here):
  ONE singleton resident adapter module (`mcp-stdio-adapter`, a
  ManagementSurface) owns child lifecycle for ALL configured stdio servers —
  lazy spawn on first call, idle shed (300s default), respawn on demand,
  constructed child environments, claustrum-resolved credentials. It presents
  MCP semantics verbatim (tools/list, tools/call) namespaced by configured
  server name; plexus dispatches by module id over the route plane, owns
  discovery/reconciliation, and governs exposure through its connector
  surface. Child tools are NOT projected into the manifest and subc-mcp does
  not advertise them; the earlier one-module-per-server / manifest-projection
  / catalog.update sketch is retired.

## Scope and composition rules

1. **Scope chain: Global → Workspace → Project → Alfonso.** Persona is not a
   separate scope — an Alfonso derives from a Persona, so persona-declared
   selections materialize at the Alfonso layer as a preset.
2. **Ceiling-narrowing invariant.** Each level sets a ceiling; narrower scopes
   SELECT WITHIN it, never widen. Per-Alfonso "enable GitHub here only" is
   legal iff every wider scope left GitHub in the ceiling: selection within a
   ceiling is not widening.
3. **Granular management surfaces first, app UX later.** The capability (wire
   ops, per-tool granularity) ships before any UI consumes it; most users
   stay at per-server on/off, but the advanced per-tool level must exist.

## Ownership (three-way split, exposure ≠ authority)

- **Plexus — system of record + governance.** The MCP-server registry: command
  lines, versions, per-tool risk classification, grants, drift detection,
  audit. The app's "add MCP server" wizard targets plexus's ManagementSurface.
- **subc — process supervision only.** `subc.jsonc` adapter entries are
  DERIVED from plexus's registry via operator ceremony (`ck` writes config +
  `rescan`). The daemon stays state-free; no registration API grows on it.
- **subc-mcp — exposure.** The scoped policy router decides what an Alfonso
  SEES; plexus governance decides what a call may DO. Collapsing these layers
  puts one policy mistake a step from actuation — keep them separate.

## Security and surface rules

- **Secrets never ride `subc.jsonc` env.** Credential-needing servers get
  resolution at child-spawn through the claustrum seam; adapters are
  spawn-attested reserved modules, so the vault sees `reserved:mcp-<name>`,
  not an anonymous env blob. A third-party server that expects its key in the environment WILL receive
  it in its own child environment -- no seam changes what the child speaks.
  The keepable property is: no at-rest storage in supervisor config,
  resolution at spawn time, and each child receives only its own secret.
  Claiming "no bearer material in argv/env" publicly would be an overclaim.
- **Surface changes apply at session boundaries** where possible. Mid-session
  churn busts prompt caches; tombstones cover the racing edge and must not
  become the normal path.
- **Search mode is the context-economy release valve.** With 19+ servers the
  full surface is hundreds of tools; per-scope selection plus
  `surface_mode: "search"` keeps agent context bounded.

## Effective-surface requirements

For each effective tool-exposure value, resolution returns its winning scope and `resolution: "ceiling"` (see “Scope and composition rules”): a narrower scope may select only what every wider ceiling permits. Wake-policy `resolution: "most_specific"` instead uses the most-specific preference, which may override a wider scope's preference.

A server added by the app wizard is not yet validated: it starts with restricted capabilities, and actions absent from its declared catalog stay quarantined until a review checks its actions and capability policy and promotes the registry entry.

## Not in scope

- App UX/UI (deferred; capability first).
- Remote/HTTP MCP servers — same registry and governance shape, different
  transport face on the adapter; follows the stdio adapter, not designed here.
- The Plexus route-plane transport class (owned by the Plexus module).
