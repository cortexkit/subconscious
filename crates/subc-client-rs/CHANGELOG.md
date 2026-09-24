# Changelog

## 0.19.1 — 2026-09-24

- Add `SubcConsumer::spawn_snapshot`, the typed `supervisor.spawn_snapshot` call: it returns the
  daemon's `SpawnSnapshot` (live processes with their spawn generations, and the cursor), and a
  daemon refusal as `CallError::Module` with its code. `SpawnSnapshot`, `LiveSpawn` and
  `SpawnCursor` are re-exported from `subc_client_rs::consumer`, so a caller needs no direct
  `subc-control` dependency.

## 0.18.7 — 2026-09-24

- Require subc-protocol 0.25.2. Since 0.18.5 this crate calls
  `error_codes::is_established_route_dead`, which first appears in 0.25.2, but it still accepted
  0.25.0, so a consumer locked at 0.25.0 or 0.25.1 got a compile error instead of an upgrade.

## 0.18.6 — 2026-09-24

- `open_route_with_admission_facts` (and its `_and_options` form) now keeps the daemon's refusal
  code and detail, like the plain route open: read them with `CallError::route_open_refusal()`.
  Before, a refused admitted open became an uncoded `NotSent`, so a caller could not tell
  `module_warming` (retry shortly) from `admission_facts_not_permitted` (configuration).

## 0.18.3 — 2026-09-23

- `PolicyResolver` releases a subject's state once its verdicts expire: expired
  cache entries are removed, and the subject's resolver route is closed and its
  `policy.subscribe` task aborted. Before, both grew with every subject seen.
- Add `PolicyResolver::footprint` and `PolicyResolverFootprint` (entries, held
  routes, running subscription tasks).

## 0.7.2 — 2026-08-24

- Add capability-addressed provider resolution from the catalog capabilities mirror.
- Add deterministic plural resolution, singular ambiguity/unprovided errors, and local identifier validation.
