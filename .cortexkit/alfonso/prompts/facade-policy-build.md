# Build: MCP-facade tool-surface policy (spec v3)

Implement `docs/specs/mcp-facade-policy.md` (v3, commit 8c8d8ebb) in
`crates/subc-mcp`. The spec is NORMATIVE and two-Oracle-gated — do not
redesign; where the spec and code conflict, the spec wins, and if you find a
genuine contradiction the spec missed, STOP and ask rather than improvising.

## Scope

All changes in `crates/subc-mcp/src/main.rs` (+ its tests / the existing
integration-test files). ZERO changes to subc-core, subc-protocol,
subc-control, or the shim wire (`ShimHello` already carries `harness`).

## Read first

- docs/specs/mcp-facade-policy.md — the whole thing; §3 (schema), §4 (merge
  algorithm), §5 (stickiness/refresh), §6 (search mode), §7 (zero-tool route
  drop), §9 (default-deny) are the build.
- docs/subc-mcp-gateway-design.md §2/§3/§5 — parent design context.
- Existing machinery you extend (do not rewrite): `RawGatewayConfig` /
  `RawProviderConfig` / `RawToolConfig` / `MaybeSet` / `merge_gateway_config` /
  `merge_tool_config` / `read_gateway_config`, `desired_session_from_catalog`,
  the collision pass, `attach_session`, `call_tool_over_route`.

## Build order (each step compiling + tested before the next)

1. **Schema extension** (§3): add `surfaceMode`, `refresh`, `harness` sections,
   object-form tool overrides (untagged bool|null|object). Add
   `deny_unknown_fields` at every level of the raw schema and gate-test that
   existing configs (camelCase `defaultEnabled`, bool overrides) still parse
   unchanged. Reserved refresh values (`on-hard`/`on-soft`) → the documented
   parse error.
2. **Composition** (§4): global top-level → global harness section → project
   top-level (restricted) → project harness section (restricted). The
   restricted project merge is a SEPARATE function (narrowing-only per §4.2:
   enabled/defaultEnabled/override-enabled may only go false; description,
   namespace, refresh dropped with WARN; surfaceMode only full→search). Apply
   `FACADE_DEFAULT_DISABLED` as the pre-merge baseline (§9).
3. **Frozen policy + refresh** (§5): resolve at attach, freeze on the session.
   `immediate`: lazy mtime re-read on request activity, recompute BEFORE
   dispatching the triggering request, update frozen policy, emit
   tools/list_changed. Liveness exception: catalog changes keep mutating the
   served list in both modes (served = frozen_policy ∩ live_catalog — this is
   close to current behavior; make it explicit).
4. **Zero-tool route drop** (§7): `desired_session_from_catalog` excludes
   providers whose resolved callable set is empty.
5. **Search mode** (§6): two meta-tools, outer tools/call accepts ONLY them in
   search mode, private binding table behind `tools_invoke`, one shared
   unknown-tool error path, deterministic search ranking (lexical rank, then
   name), meta-tool name reservation via the existing collision pass (shipped
   whole-attach fail-closed semantics).

## Tests

The spec §10 test list is the gate — implement ALL of it. Follow the existing
integration-test patterns (real daemon + fake-aft-stub + real shim; the
phase1_integration.rs style). Level-triggered sync, no absolute-latency
assertions (repo test norms).

## Definition of done

- Full workspace: cargo test green, cargo clippy --all-targets -D warnings
  clean on native AND --target x86_64-pc-windows-gnu, cargo fmt clean.
- Every §10 test present and non-vacuous (a sabotaged implementation fails it).
- Commit in logical steps with reasons-why comments only (no task/plan refs).
