# Build: subc health — Phase 2 (prober, escalation ladder, config, observability)

Implement Phase 2 of `docs/specs/subc-health.md` (v2, commit 0b3836a7),
building on the merged Phase 1 (master e84107e2: protocol health.check +
HealthReport, SDK defaults through the per-request dispatch path, generic
module-control RPC facility in forwarding.rs, supervisor.health_probe one-shot
op with capability gating). The spec is Oracle-gated and NORMATIVE; on genuine
contradiction, STOP and ask.

## Phase 2 scope

1. **Cadenced prober** (subc-core, Supervisor-adjacent): per-module probe loop
   for modules that advertise health.check — cadence default 30s (jittered),
   deadline default 5s, consecutive-failure threshold default 3. Reuses the
   Phase-1 RPC facility. Modules not advertising: health state `unknown`,
   never probed. Prober lifecycle follows the module: starts on registration,
   stops on death/disable, resets failure count on re-registration.
2. **Escalation ladder** (spec §5): threshold breach ⇒ Unresponsive ⇒
   drain-restart via the supervisor (bounded by existing drain_timeout).
   HEALTH-TRIGGERED RESTARTS INCREMENT the module restart counter (do NOT
   route through the counter-resetting explicit restart paths — spec §5
   anti-flap; budget exhaustion lands in Disabled + ERROR-grade log).
   L3 status actions from config: report (default) | restart | alert.
   `alert` = ERROR-level daemon log naming the module, status, and detail.
3. **Config block** (daemon_config.rs, spec §6): per-module `health` object
   {cadence_ms, deadline_ms, failure_threshold, on_degraded, on_failing,
   critical}. Absent = defaults. Values in a PRESENT block are validated
   fail-loud at boot (bad enum / non-positive numbers = config parse error);
   unknown fields inside follow the parser's existing ignore convention.
   critical:true escalates L2-unresponsive and spawn-failure surfacing to
   alert-grade.
4. **Observability** (spec §7): supervisor.list rows gain
   `health: ok|degraded|failing|unresponsive|unknown` + `last_probe_ms`
   (Unix ms). New channel-0 op `supervisor.health` returning per-module:
   status, last report detail/metrics (opaque, metrics >16KiB truncated),
   consecutive_failures, last_action + last_action_ms. Golden vectors for
   the new/changed control shapes.
5. **connected_clients** (spec §7): authenticated-connection counter,
   INFO log on transitions, exposed in server.describe. Golden vector update.

Out of scope (explicitly): the daemon self-watchdog (§8 — ships separately),
any health-history persistence, LLM diagnosis (v2 direction).

## Design constraints

- The prober must live where restart authority lives: it acts through the
  existing Supervisor levers (drain-reload path), never a parallel kill path.
- No rule engine: the action mapping is a closed enum lookup, nothing more.
- Probe failures and data-plane traffic must not interact beyond the spec's
  stated guarantee (no credit consumption).
- Test timing discipline (repo norm): level-triggered polling on
  subc-observable state, 10s setup guards, NO absolute-latency assertions.
  Prober cadences in tests are injected small values, never sleeps tuned to
  real defaults.

## Tests (gate)

- Wedged-module e2e: stub advertising health.check whose handler wedges (the
  Phase-1 fake-aft-stub already grew health hooks — extend) ⇒ prober detects
  after N consecutive failures ⇒ drain-restart fires ⇒ module recovers ⇒
  health returns to ok. Assert restart counter incremented (not reset).
- on_failing=restart flap: stub that always reports failing ⇒ restarts until
  crash budget exhausts ⇒ Disabled, alert-grade log, no further restarts.
- on_degraded=report: degraded stub is NOT restarted; supervisor.health
  carries its detail verbatim.
- Non-advertising module: never probed (zero health frames on its wire),
  health=unknown in supervisor.list.
- critical:true: unresponsive module logs alert-grade (assert log line).
- Config: valid block parses; bad enum / zero cadence fails boot loudly;
  absent block = defaults.
- supervisor.health + supervisor.list health fields over a live daemon;
  connected_clients moves with client connect/disconnect.
- Golden vectors for all new/changed control shapes.

## Definition of done

Full workspace cargo test green; clippy -D warnings native AND
x86_64-pc-windows-gnu; cargo fmt; golden vectors updated; comments carry
reasons only; logical commits.
