# Failed health probes and cached supervisor status

## Reader audit

The following source locations were audited before changing the failed-probe
snapshot update (line numbers refer to the pre-change source). Only two
production consumers extract `ModuleStatus.health.status`; neither makes a
lifecycle or routing decision:

- `crates/subc-daemon/src/control.rs:3891`: `handle_supervisor_list` copies the
  cached status into `SupervisorEntry.health` for `supervisor.list`.
- `crates/subc-daemon/src/control.rs:4166`: `handle_supervisor_health` copies it
  into `SupervisorHealthEntry.status` for the cached `supervisor.health` view.
- `crates/subc-daemon/src/supervise.rs:3333`: `status_with_snapshot_lock` copies
  the entire cached health record into `ModuleStatus`; this is the common path
  for `status()` and control snapshots, not a decision on health.
- `crates/subc-daemon/src/supervise.rs:5599`: the test-only `wait_http_health`
  helper reads the cached enum to await HTTP health transitions.

The other relevant paths do **not** read that enum:

| Path | Decision inputs |
| --- | --- |
| `crates/subc-daemon/src/control.rs:1024-1031` | Capability admission uses module id, lifecycle state and enabled flag. |
| `crates/subc-daemon/src/control.rs:3063-3130` | `route.open` uses registration, protocol, warming and readiness. |
| `crates/subc-daemon/src/control.rs:2952-2984` | Refusal logging/classification uses lifecycle state, enabled and live. |
| `crates/subc-daemon/src/control.rs:5039-5066`, `crates/subc-daemon/src/supervise.rs:3432-3446` | Warming uses lifecycle state only. |
| `crates/subc-daemon/src/supervise.rs:3299-3309,1654-1665` | Liveness uses lifecycle state, process-alive, enabled and registration, not health. |
| `crates/subc-daemon/src/supervise_swap.rs:509-549,682-727` | Swap admission uses overlap, enabled, protocol, registration and child presence; candidate health uses a fresh endpoint report (`HealthStatus`), not incumbent cached health. |
| `crates/subc-daemon/src/control.rs:4859-5036` | `ck health <module>` performs a fresh RPC, rendering its report or timeout independently of the cached status. |
| `crates/subc-daemon/src/control.rs:4078-4111`, `crates/subc-daemon/src/supervise.rs:3360-3374` | Provenance uses pid, spawn time/path, executable identity and process start time. |
| `crates/subc-daemon/src/supervise.rs:4745-4788` | Failure alerts and restarts use consecutive failure count and threshold; the alert status is the explicit `unresponsive` literal. |
| `crates/subc-daemon/src/supervise.rs:4699-4715,4799-4844` | Report actions/alerts use the fresh report's `HealthStatus` and the configured action; they do not read cached health. |

No behavioral reader depends on the last successful cached enum. The failure
counter still gates the same restart/alert branch. The first miss changes only
the operator-visible cached health, not the module's lifecycle state.

## Enum and rendering

The existing wire enum (`crates/subc-control/src/lib.rs:2111-2117`) has `Ok`,
`Degraded`, `Failing`, `Unresponsive` and `Unknown`; no wire type changes.
`ck`'s `human_health_status` (`crates/subc-core/src/bin/ck.rs:6397-6402`) renders
`ok`, `degraded`, `failing` and `unknown` literally, and `unresponsive` as
`unhealthy`. Sentence rendering (`:6404-6409`) calls `ok` healthy.

Use **Unknown** for a failed wire probe: the daemon has no current usable
report, so retaining `Ok` would claim a check succeeded when it did not.
`Degraded` and `Failing` are module-reported conditions. `Unresponsive` remains
the threshold-breached classification, not a single missed deadline. The detail
still distinguishes `no-answer`, `lane-dead`, `bad-answer` and misconfiguration.
A successful `handle_health_report` still replaces the status from the report
and resets consecutive failures (`supervise.rs:4688-4696`).

HTTP-probed `protocol: "none"` modules already show failures promptly:
`run_health_probe_cycle` sets `Failing` before invoking the shared failure
handler (`supervise.rs:4288-4293`). Preserve that classification by restricting
the new cached update to the running wire protocol, including across rescans
that change the configured protocol before the next spawn.

## Regression

`unanswered_probe_is_unknown_until_threshold_and_ok_report_recovers` drives real
in-memory wire RPCs and their timeouts through `run_health_probe_cycle`, then
reads `status()`. It checks the first miss, unchanged lifecycle/restart budget
below the threshold, recovery from an `Ok` report and restart scheduling only
at the threshold after a new failure streak. The monitor is stopped so the test
owns the snapshot; no OS process is launched. Existing HTTP transition tests
exercise the separate HTTP behavior.
