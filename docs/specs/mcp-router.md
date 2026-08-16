# MCP Router: scoped tool-surface management

Status: DRAFT r1 — settled direction from Ufuk (2026-08-15 chat), pre-gate.
Seats for the design round: SUBC (custody, this doc), PLEX (governance + registry),
ALF (agent scope, app management surface), entorhinal owner (policy store custody).

## What exists (build on, do not rebuild)

- **subc-mcp policy engine** (`crates/subc-mcp`): composes the advertised tool
  surface from module manifests at session attach. Global + project config,
  narrowing-only merges, default-deny for agent-internal modules, per-tool
  `ack_only`, tombstones for stale-surface races, `surface_mode: "search"`
  (tools_search/tools_invoke meta-tools), reverse-request relay, spawn
  attestation. Per-server AND per-tool granularity already works — for
  CortexKit's own modules, driven by config files.
- **ck-mcp-stdio-adapter** (subconscious#20, ruled in plexus#4; full contract
  in `docs/specs/mcp-stdio-adapter.md`, which supersedes earlier text here):
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

## Settled by Ufuk (2026-08-15)

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

- **PLEX — system of record + governance.** The MCP-server registry: command
  lines, versions, per-tool risk classification, grants, drift detection,
  audit. The app's "add MCP server" wizard targets plexus's ManagementSurface.
- **subc — process supervision only.** `subc.jsonc` adapter entries are
  DERIVED from plexus's registry via operator ceremony (`ck` writes config +
  `rescan`). The daemon stays state-free; no registration API grows on it.
- **subc-mcp — exposure.** The scoped policy router decides what an Alfonso
  SEES; plexus governance decides what a call may DO. Collapsing these layers
  puts one policy mistake a step from actuation — keep them separate.

## Hard rules carried into the design round

- **Secrets never ride `subc.jsonc` env.** Credential-needing servers get
  resolution at child-spawn through the claustrum seam; adapters are
  spawn-attested reserved modules, so the vault sees `reserved:mcp-<name>`,
  not an anonymous env blob. Stated precisely (PLEX's correction, adopted):
  a third-party server that expects its key in the environment WILL receive
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

## Open for the round (not settled)

- **Policy store custody.** Lean: scope-tree documents (Global/Workspace/
  Project) in entorhinal — it owns the workspace/project tree — with the
  Alfonso layer contributed by prefrontal (it owns agent identity and
  Persona). Alternative: single custody in one module. The round decides;
  the composed READ path at session attach must stay cheap and
  offline-tolerant either way.
- **Effective-surface resolution semantics.** Where composition happens
  (subc-mcp at attach, as today, extending the existing global+project
  resolver) and what the wire op for "resolve effective surface for scope S"
  looks like — the app needs it for preview ("what would this Alfonso see").
  PINNED from #scheduled-wakes (CKIOS): the op emits, beside each effective
  value and its winning scope, a RESOLUTION-KIND discriminator
  (`resolution: "ceiling"` here; the wakes ladder emits
  `resolution: "most_specific"`). Same field name in both contracts. Two
  ladders share the four scope names with deliberately different rules —
  wake policy is preference (a narrower scope may quiet what the workspace
  wanted loud), tool exposure is security (a narrower scope may never see
  what the workspace denied) — and most rows agree most of the time, so a
  renderer teaching the wrong mental model ships invisibly. The typed
  discriminator makes that failure a type error instead of a user-education
  problem.
- **Manifest-at-HELLO vs lazy enumeration** for adapters whose children are
  expensive to boot-spawn even once.
- **Registry provenance (PLEX finding 1).** plexus's catalog is file-driven:
  `retire_absent_manifests` retires anything active in the store but absent
  from `catalog/*.jsonc` -- correct for a deleted manifest, and it would
  silently withdraw a wizard-added server on the next boot. The registry
  needs a provenance distinction (file-reviewed vs operator-added) before
  the sweep can tell a withdrawal from an addition it has never seen.
- **Wizard entries are unreviewed vendors (PLEX finding 2).** The settled
  propose-vs-classify rule applies unchanged: pasting `npx -y @foo/mcp` is
  not a review, so wizard-added servers enter capability-restricted with
  undeclared actions quarantined, and only a review promotes them. The
  registry extends plexus's model iff wizard entries land unreviewed.
- **Migration**: current global/project config files become the Global and
  Project layers of the store; no coexistence period (clean cutover per house
  preference).

## Not in scope

- App UX/UI (deferred by Ufuk; capability first).
- Remote/HTTP MCP servers — same registry and governance shape, different
  transport face on the adapter; follows the stdio adapter, not designed here.
- The plexus route-plane transport class (owned by PLEX from plexus#4).
