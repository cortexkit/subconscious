# subc Continuous Health: Probing, Reporting, Self-Repair (v1)

Layered health for the daemon and its modules. Mechanism lives in subc-core's
Supervisor (the actor that already holds restart authority); judgment stays
module-side (domain health) or user-side (alerts). No rule engine in the
daemon.

## 1. The layers

- **L0 process alive** — exists (Supervisor, restart policy, crash budget).
- **L1 wire responsive** — PING answered by the module SDK's frame loop.
  Subsumed by L2 (a wedged loop fails both); not probed separately.
- **L2 request-path responsive** — a typed `health.check` control request that
  flows through the module's REAL dispatch machinery (spawn-per-request, the
  same path consumer requests take). Catches the alive-but-wedged class: a
  module accepting connections whose handler path is stuck.
- **L3 domain health** — module-defined: stuck runs, failing model loads,
  locked-out credentials. Modules self-report via the same op's reply; subc
  carries the report as a typed shape and never interprets domain detail.

## 2. Wire contract (subc-protocol bump — batch with Tool.description)

New module-control-plane op, daemon → module on the module control channel:

```jsonc
// request
{ "op": "health.check" }
// reply
{
  "status": "ok" | "degraded" | "failing",
  "detail": "<opaque human-readable string, optional>",
  "metrics": { /* opaque JSON object, optional — CK-app/diagnostic fodder */ }
}
```

Rules:
- Reply must be CHEAP (no I/O behind it beyond in-memory state). Deadline is
  enforced by the prober, not the module.
- `status` is the ONLY field subc acts on. `detail`/`metrics` are carried
  opaque (thin core) and surfaced via `supervisor.health` / logs.
- A module that cannot answer within the deadline is L2-unresponsive — that is
  a stronger signal than any self-report and is evaluated first.

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
- Probing uses the existing module control channel; a probe in flight never
  blocks data-plane forwarding (it's an ordinary control request).
- Modules without the SDK default (old builds) fail probes benignly: a module
  that answers data-plane traffic but not health.check would be misclassified,
  so the prober treats an `unknown op`-class error reply as OK-but-unaware
  (probe satisfied at L1½: the dispatch loop answered). Only timeout/silence
  counts as failure. This makes rollout safe with mixed module versions.

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
- Anti-flap: L3-triggered restarts consume the same crash budget as crashes;
  a module that reports `failing` forever does not restart-loop forever.

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
surfacing (the note-#298 resolution: per-module, not global fatal).

## 7. Observability surfaces

- `supervisor.list` rows gain `health: ok|degraded|failing|unresponsive|unknown`
  + `last_probe_ms`.
- New channel-0 op `supervisor.health`: full report per module (status, last
  report's opaque detail/metrics, consecutive failures, last action taken).
- Consumer-population observability: the daemon logs (INFO) connected-client
  count transitions and exposes `connected_clients` in server.describe — a
  fleet that fails to return after a daemon restart becomes visible instead
  of silently absent.

## 8. Daemon self-check

- Internal watchdog task: heartbeats the main accept loop and the router's
  forwarding path (a self-request through a loopback control connection)
  every 60s; a miss logs ERROR with diagnostics. launchd (KeepAlive) remains
  the outer restart layer; the CK app becomes the outside observer later.
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

- SDK: default health.check answered through the dispatch path (test: a
  handler whose data-path is deliberately wedged fails health.check too — the
  probe must NOT be answerable while dispatch is wedged; non-vacuity for L2).
- Prober: wedged-module (stub with stuck handler) detected in N×cadence,
  drain-restart fired, recovery observed; old-module (no health op) treated
  as OK-but-unaware; probe timeout does not disturb in-flight data requests.
- Ladder: on_degraded=restart consumes crash budget (no infinite loop);
  critical module's unresponsive state logs alert-grade.
- Config: schema parse, defaults, unknown fields rejected.
- supervisor.health end-to-end over a live daemon with a degraded-reporting
  stub.
