# Build: subc health — Phase 1 (protocol + SDK defaults + generic module-control RPC)

Implement Phase 1 of `docs/specs/subc-health.md` (v2, commit 0b3836a7). The
spec is Oracle-gated and NORMATIVE; where spec and code conflict, the spec
wins. If you find a genuine contradiction the spec missed, STOP and ask.

## Phase 1 scope (this task)

1. **subc-protocol**: `HealthStatus` enum (ok|degraded|failing) +
   `health.check` request/response variants on the module-control shapes
   (tagged exactly like route.bind / RouteBindAck). `detail: Option<String>`
   (skip-if-none), `metrics: Option<serde_json::Value>` (skip-if-none).
   ALSO in the same protocol change: add `description: Option<String>`
   (skip-if-none) to `Tool` in manifest.rs — an unrelated field deliberately
   batched into this one wire bump. Update golden JSON vectors for both.
   Do NOT bump the crate version (the release train handles versions).
2. **subc-client-rs SDK**: default `health()` on ModuleHandler returning
   HealthReport::ok(); serve `health.check` THROUGH THE SAME per-request
   spawn path as data-plane requests (spec §1 L2 — the current inline
   control handling is explicitly NOT acceptable for this op; route.bind
   handling stays as-is). Advertise `health.check` in the HELLO control_ops
   grant (do NOT touch the null-means-baseline set).
3. **TS SubcProvider** (clients/subc-client): same — default health handler
   through the spawned data-request path, optional `health` callback on the
   provider options, control_ops advertisement.
4. **subc-core: generic module-control RPC facility** (spec §4 "new
   machinery"): per-module corr allocator + pending map with deadline +
   response demux by tagged op + cancel on module death. Do NOT migrate the
   route-bind relay onto it (that's a later refactor); it must coexist with
   a concurrent route.bind relay without interference. No prober yet
   (Phase 2) — but expose the facility so control.rs can send a one-shot
   health.check (add a `supervisor.health_probe {module_id}` channel-0 op
   that sends one probe and returns the report, as the Phase-1 proof and a
   diagnostic tool in its own right; supervisor.list is untouched).

Out of scope (Phase 2): the cadenced prober, escalation ladder, health
config block, supervisor.health aggregate op, watchdog, connected_clients.

## Tests (gate for this phase)

- Protocol: golden vectors for health.check req/resp + Tool.description
  round-trip (absent field stays absent — skip-if-none both directions).
- Rust SDK non-vacuity (spec §10): a stub whose data handler is wedged (all
  handler capacity held on a never-resolving request) must ALSO fail to
  answer health.check within a short deadline; a healthy stub answers ok.
- TS SDK: same non-vacuity shape with the fake daemon harness.
- Old-module safety: a module whose HELLO does not advertise health.check —
  assert `supervisor.health_probe` REFUSES to send (typed error
  `health_not_advertised`) and zero health.check frames reach the module.
- RPC facility: response demux by op with a concurrent in-flight route.bind
  relay on the same module connection; deadline expiry produces a typed
  timeout; module death cancels pending probes.
- e2e: real daemon + SDK echo-module — supervisor.health_probe returns ok;
  degraded-reporting stub returns degraded with detail/metrics carried
  verbatim.

## Definition of done

Full workspace cargo test green; clippy -D warnings native AND
x86_64-pc-windows-gnu; cargo fmt clean; bun test + typecheck green in
clients/subc-client; golden vectors updated; comments carry reasons only
(no task refs); commits in logical steps.
