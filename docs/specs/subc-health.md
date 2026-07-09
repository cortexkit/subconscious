# subc Continuous Health: Probing, Reporting, Self-Repair (v1, amended)

Layered health for the daemon and its modules. Mechanism lives in subc-core's
Supervisor (the actor that already holds restart authority); judgment stays
module-side (domain health) or user-side (alerts). No rule engine in the
daemon.

## 0. v2 doctrine amendment (2026-07-09, supersedes the §1 L2 honesty mechanism)

Ruling after a production false-positive kill: under machine-wide CPU
saturation, a busy-but-healthy module missed 3×5s probes on the dispatch
path, the prober escalated to a health restart, and the respawn warm turned a
15s load spike into a multi-minute route-unavailable outage.

The generic split, for every module:

- **subc detects total wreckage only.** The daemon's probe answers one
  question: can this module reply at all? A module that cannot answer a
  probe on its supervision lane is dead-or-wrecked and gets restarted. The
  daemon never infers "dispatch is slow" from probe latency — that inference
  is what converted load spikes into kill decisions.
- **Modules own an internal health runner.** `health.check` is served from a
  dedicated insulated lane/thread (the subc-mcp supervision-connection
  pattern) so the reply is prompt even when the tokio workers are starved.
  Honesty is structural, not transport-borne: the insulated reporter is a
  MECHANICAL mapper over signals stamped by the real dispatch path — a
  monotonic dispatch-loop heartbeat plus oldest-queued-request and
  oldest-in-flight ages, read as atomics. It never forms its own opinion. A
  wedged dispatch stops stamping; the stale stamp makes the prompt reply say
  `unresponsive: dispatch heartbeat Ns stale`. The v1 requirement that the
  probe reply itself ride the per-request spawn path is RETIRED (it was the
  false-dead vector); the non-vacuity property it protected is preserved by
  the stamped-signal contract instead: a wedged data path MUST surface as a
  stale heartbeat in the report.
- **Restart policy keys on the split**: lane death / process exit / probe
  transport timeout → restart fast (total wreckage). Reported-unresponsive
  (stale heartbeat) → restart per existing threshold (the module itself
  attests its dispatch is wedged). Reported-degraded (slow but moving) →
  report/alert only, never kill.
- **`module_warming`**: post-respawn route-unavailability is a typed,
  retryable state distinct from `reloading`, so clients distinguish
  retry-soon from stuck. Modules shrink the window with lazy root/domain
  warm-on-bind rather than upfront warm.

Adoption is incremental: modules move to the insulated-lane + stamped-signal
shape as they touch health code (AFT first; its dispatch-liveness gauges
already carry the queue/in-flight ages). Until a module adopts, the v1
dispatch-path probe semantics below remain its contract, with the §5 ladder
softened by the restart-policy split above.

## 1. The layers

- **L0 process alive** — exists (Supervisor, restart policy, crash budget).
- **L1 wire responsive** — PING answered by the module SDK's frame loop.
  Subsumed by L2 (a wedged loop fails both); not probed separately.
- **L2 request-path responsive** — a typed `health.check` control request that
  flows through the module's REAL data dispatch machinery. Catches the
  alive-but-wedged class: a module accepting connections whose handler path is
  stuck. IMPLEMENTATION REQUIREMENT (not current behavior): today both SDKs
  handle module-control requests INLINE in the frame loop while data requests
  are spawned per-request — an inline health answer would be dishonest (a
  wedged handler pool would still answer). The SDKs MUST route `health.check`
  through the same per-request spawn path as data-plane requests, and the
  non-vacuity test (§10: wedged data handler ⇒ health.check also unanswerable)
  gates the claim.
- **L3 domain health** — module-defined: stuck runs, failing model loads,
  locked-out credentials. Modules self-report via the same op's reply; subc
  carries the report as a typed shape and never interprets domain detail.

## 2. Wire contract (subc-protocol bump — batch with Tool.description)

New module-control-plane op, daemon → module on the module control channel:

```jsonc
// request (ModuleControlRequest, tagged like route.bind)
{ "op": "health.check" }
// reply (ModuleControlResponse, tagged)
{
  "op": "health.check",
  "status": "ok" | "degraded" | "failing",
  "detail": "<human-readable string>",   // optional; absent when omitted (never null)
  "metrics": { /* JSON object */ }        // optional; opaque to the core; ≤16 KiB
                                          // serialized or the prober truncates it
                                          // from supervisor.health (status still acts)
}
```

Rules:
- Tagged exactly like the existing module-control shapes (two-implementer
  determinism). Timestamps anywhere in this feature are Unix ms.
- Reply must be CHEAP (in-memory state only). Deadline is enforced by the
  prober, not the module.
- `status` is the ONLY field subc acts on. `detail`/`metrics` are carried
  opaque (thin core) and surfaced via `supervisor.health` / logs.
- A module that cannot answer within the deadline is L2-unresponsive — that is
  a stronger signal than any self-report and is evaluated first.
- The op is advertised via the HELLO `control_ops` grant (§4); it is never
  part of the baseline set.

`Tool.description: Option<String>` rides the same protocol release (unrelated
field, batched to avoid two wire bumps).

## 3. SDK defaults (subc-client-rs ModuleHandler + TS SubcProvider)

The SDK serves `health.check` by default THROUGH THE SAME dispatch path as
data-plane requests (per-request spawn), replying `{status:"ok"}`. Every
module inherits L2 on its next rebuild with zero code. Optional override:

```rust
async fn health(&self) -> HealthReport { HealthReport::ok() }
```

Modules override to report domain health (L3): alfonso-core reports stuck-run
counts, synapse reports model-load states. The default answer must NOT be
served from the reader loop directly — routing it through the normal dispatch
path is the entire point (it proves the path consumers use).

## 4. Prober (subc-core Supervisor)

- Per-module cadence (default 30s, config-overridable), jittered so probes
  don't synchronize. Probe deadline default 5s.
- `consecutive_failures >= threshold` (default 3) ⇒ L2-unresponsive ⇒ the
  module enters `Unresponsive` state and the configured action fires.
- Probes do NOT consume data-plane flow-control credits (they are control
  frames, not route Requests). They DO share the module connection's socket
  egress queue — the guarantee is "no credit consumption", not total
  isolation.
- ROLLOUT IS CAPABILITY-GATED, never unknown-op-probed: today's modules
  deserialize module-control ops into a closed enum and reply with an error
  (or worse, a TS module treats it as an unexpected-request failure) — probing
  an old module could destabilize it. `health.check` is an OPTIONAL advertised
  module-control op: new SDKs advertise it in the HELLO `control_ops` grant
  list; it is NOT added to the null-means-baseline set (old modules sending
  `control_ops: null` must not appear to support it). The prober only probes
  modules that advertised the op; everything else reports health `unknown`
  (OK-but-unaware) and is never sent the op. Mixed fleets are safe by
  construction.
- NEW MACHINERY REQUIRED: the daemon's only daemon→module request today is the
  route.bind relay, whose correlation tracking and response parsing are
  route-bind-specific. The prober needs a small GENERIC module-control RPC
  facility in subc-core: per-module corr allocator, pending map with deadline,
  response demux by tagged `op`, cancel on module death. The route-bind relay
  can migrate onto it later; v1 only requires health.check to use it.

## 5. Escalation ladder (mechanism, not policy language)

```
L2 timeout ×N            → drain-restart (existing supervisor.reload lever)
restart fails to clear   → existing crash budget → Disabled + loud surface
L3 "degraded"/"failing"  → per-module configured action (closed set):
                            report | restart | alert
```

- `report` (default): carried in supervisor.list/health, logged. The module
  is expected to remediate internally (ALF's sweep/re-home precedent).
- `restart`: for modules whose degraded state is known restart-curable.
- `alert`: loud surfacing for CRITICAL modules (per-module criticality — MC
  failing breaks context infra and must not be quiet; quota failing degrades
  quietly). v1 alert = ERROR-level daemon log line + status in
  supervisor.health; the CK app subscribes later.
- Anti-flap: health-triggered restarts INCREMENT the module's restart counter
  (today's explicit restart paths RESET it — the health path must not reuse
  them as-is, or `on_failing: restart` loops forever). A health restart
  consumes crash budget exactly like a crash; budget exhaustion lands in
  Disabled + alert-grade surfacing.
- Drain-restart on an UNRESPONSIVE module is bounded: forwarding drain waits
  on in-flight counts only up to the existing `drain_timeout`, then
  force-releases routes, and child shutdown waits-with-timeout then kills.
  Worst case ≈ drain_timeout + child kill timeout; the spec accepts that
  wedged in-flight requests are force-torn-down (their consumers get the
  standard route-gone GOODBYE).

Explicit non-goals for v1 (out, by decision): time-windowed conditions
("degraded >5min"), trend analysis, cross-module correlation, any rule
engine. Config maps (module, status) → action from the closed set; nothing
smarter lives in the daemon.

## 6. Config (subc.jsonc, per module, all optional)

```jsonc
"modules": {
  "alfonso-core": {
    "health": {
      "cadence_ms": 30000,
      "deadline_ms": 5000,
      "failure_threshold": 3,
      "on_degraded": "report",   // report | restart | alert
      "on_failing": "alert",
      "critical": true            // folds in the parked per-module criticality flag
    }
  }
}
```

Absent = defaults (30s/5s/3, report/report, critical:false). `critical:true`
additionally escalates L2-unresponsive and spawn-failure to `alert`-grade
surfacing (per-module criticality, not global fatal).

Parser note: daemon_config's existing convention IGNORES unknown fields
(forward-compat) — the health block follows that convention (no global
posture flip); values inside a PRESENT health block are validated (bad enum,
non-positive cadence ⇒ config parse error, fail-loud at boot).

## 7. Observability surfaces

- `supervisor.list` rows gain `health: ok|degraded|failing|unresponsive|unknown`
  + `last_probe_ms`.
- New channel-0 op `supervisor.health`: full report per module (status, last
  report's opaque detail/metrics, consecutive failures, last action taken).
- Consumer-population observability: the daemon maintains an authenticated-
  connection counter (new — no such counter exists today), logs (INFO) count
  transitions, and exposes `connected_clients` in server.describe. VISIBILITY
  ONLY: it makes a fleet that fails to return after a daemon restart
  observable; the repair for that class lives client-side (the 0.3.2
  reconnect classifier).

## 8. Daemon self-check

- Internal watchdog task: every 60s, a loopback self-request (the daemon
  authenticates to itself with its own key — sound, and accept spawns a task
  per connection so the probe cannot deadlock the loop it watches) drives a
  channel-0 `server.describe`. SCOPE: this proves accept + auth + control
  dispatch; it deliberately does NOT prove the data-plane forwarding path
  (that would need a resident test module — out of v1). A miss logs ERROR
  with diagnostics. launchd (KeepAlive) remains the outer restart layer; the
  CK app becomes the outside observer later.
- Connection-file integrity check rides the watchdog (file present, 0600,
  parses, matches the live port/key).

## 9. v2 direction (banked, not built)

- LLM-assisted diagnosis (user-consented): on `failing`/`unresponsive`, hand
  the health history + module log tail + supervisor state to a model for a
  root-cause hypothesis and suggested action, surfaced in the CK app —
  diagnosis stays ADVISORY; actions remain the closed set. Requires the
  health history to exist: v1's supervisor.health is deliberately shaped so
  its reports can be ring-buffered later without a wire change.
- Health-history persistence (ring buffer → store) and the CK app health
  panel ride the same v2.

## 10. Tests (gate)

- SDK (both Rust and TS): default health.check answered through the per-
  request spawn path (non-vacuity: a handler whose data-path is deliberately
  wedged — e.g. all handler permits held — must fail health.check too).
- Prober: wedged-module (stub with stuck handler) detected in N×cadence,
  drain-restart fired within drain_timeout bounds, recovery observed;
  NON-ADVERTISING module is never sent the op and reports health unknown
  (assert zero health.check frames on its wire); probe timeout does not
  disturb in-flight data requests.
- Generic module-control RPC: response demux by op, deadline expiry, cancel
  on module death, no interference with a concurrent route.bind relay.
- Ladder: on_failing=restart increments restart count (no reset), exhausts
  crash budget into Disabled (no infinite loop); critical module's
  unresponsive state logs alert-grade.
- Config: health block parse + validation errors fail boot; absent block =
  defaults; unknown fields inside follow the parser's ignore convention.
- supervisor.health end-to-end over a live daemon with a degraded-reporting
  stub; oversized metrics truncated without losing status.
