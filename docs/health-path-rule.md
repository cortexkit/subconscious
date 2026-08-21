# Health-Path-Rule v3

The fleet doctrine for module health checks. Canonical text; previously this
lived only in the SUBC seat's memory, which violated its own spirit (a rule one
machine can read is not a rule). v1 → v2 added the lock-set clause; v2 → v3
added the no-subprocess clause after two fleet-wide outages in one day
(2026-08-08).

## The rule

A module's `health.check` reply path must have **nothing to queue behind**:

- no live store / disk / network / keychain reads,
- no blocking locks,
- no synchronous downstream probes (cached gauges only),
- **no subprocess exec.**

Health replies come from a dedicated **insulated lane**. The module internally
self-monitors dispatch health, deriving status **mechanically** from signals
the real dispatch path stamps — a monotonic heartbeat, oldest-queued-request
age, oldest-in-flight age, read as atomics — never from its own opinion.
A wedged dispatch loop stops stamping → the stamp goes stale → the insulated
lane replies promptly with "unresponsive: heartbeat Ns stale". The reply is
always fast; the *content* carries the bad news.

## Why subprocess exec is its own clause (v3)

A lock contends with your own work, so its worst case is bounded by what your
module is doing. A subprocess exec queues behind the **host's slowest shared
resource** — outside your process entirely, and precisely the resource that is
degraded under the conditions the probe exists to survive. The health reply
execs into the exact fault it is supposed to report on.

Proof case: aft's `build_health_report` called `artifact_cache_key()` per live
root (33 of them) on the channel-0 reply path; that function spawns `git` per
unmemoized root with 3 retries plus backoff. On a host intermittently blocking
fresh-binary exec for 12s+, the 5s deadline was unreachable: 33 roots × 12s
serial = 396s before retries. Result: near-continuous probe failure from the
moment the binary answered — three kills in eight minutes against a four-hour
healthy base rate on the previous build.

## The lock-set clause (v2)

A health-snapshot read must take **only locks never held across expensive or
mutating work**, or none. Audit by comparing the health snapshot's lock set
against the hold set of the module's expensive paths. If they intersect,
channel-0 **times out** instead of fast-failing to `busy`.

Diagnostic: a `health.check` that times out rather than fast-failing with
`live=false` means the request reached the module and the frame-loop or a lock
was held — `live=false` is the daemon's cheap supervisor verdict and always
answers.

## Degraded vs dispatch-impaired

`degraded` must mean **dispatch-impaired only**, never "a background index is
still building". Borrowed/warming state is informational detail, not a
degraded trigger. Freshness and liveness ages are computed **live at probe
time**, never frozen into snapshots; downstream gauges are cached, never
probed synchronously.

## Daemon-side restart policy (for reference)

- Lane death / process exit / probe transport timeout → restart fast.
- Reported-unresponsive (stale heartbeat) → restart per threshold.
- Reported-degraded (slow but moving) → report/alert, no kill.

The daemon emits typed `module_warming` to distinguish post-respawn warm-up from
reloading.
Note: the daemon schedules a module's **first** probe at cadence+jitter
(~30s), so a freshly restarted module reads `unknown` for at least 30s
regardless of readiness — that is not a failed boot.

## Separate hazard — do not conflate

`ck health <module-id>` (the one-shot CLI probe) uses its **own** hardcoded
5s timeout in `control.rs`, independent of the module's configured supervisor
deadline. That command can report `module_timeout` on a module the supervisor
considers healthy. To judge supervisor behaviour, read the supervisor's own
view: `ck module status <id>` (failures, last_action, restart budget).

## Building a new module

The `subc-client-rs` SDK's default `ModuleHandler::health()` self-identifies
as unimplemented ("no health implementation; inherited default") — replace it
with an insulated-lane implementation before carriage. The reference
implementations: engram (dedicated supervision control connection), subc-mcp
(module-mode health connection isolated from client traffic).
