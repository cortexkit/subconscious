# Daemon timeout configuration and pre-auth limits

`$XDG_CONFIG_HOME/cortexkit/subc.jsonc` supports drain and relay timeout keys
at the daemon root as defaults or inside an individual module. The separate
first-registration timeout is per-module only. Drain and relay resolution
happens while the file is parsed:
per-module value wins, then the daemon-wide value, then the built-in default.
An explicit `0` is a value, not an absent setting.

```jsonc
{
  "version": 1,
  "drain_timeout_ms": 45000,
  "route_bind_relay_timeout_ms": 30000,
  "modules": {
    "fast-worker": {
      "program": "/usr/local/bin/fast-worker",
      "drain_timeout_ms": 0,
      "route_bind_relay_timeout_ms": 5000,
      "register_timeout_s": 120
    }
  }
}
```

| Key | What it bounds | Built-in default | `0` |
| --- | --- | --- | --- |
| `drain_timeout_ms` | Time for already-dispatched requests to finish while a module tears down. | 30,000 ms | Accepted: tear the module down now. This is the wedge-bounce action. |
| `route_bind_relay_timeout_ms` | Time for a target module to acknowledge a relayed `route.bind` before the daemon reports `module_timeout`. | 12,000 ms | Refused at both the daemon and module layers. A zero budget makes every bind to that module fail instantly and forever; use `enabled: false` to make a module unreachable. |

The `0` asymmetry is intentional. `drain_timeout_ms: 0` remains a valid
operator action, including as a per-module override; it must not be
"normalized" into a positive-only timeout. `route_bind_relay_timeout_ms: 0`
is a typo wearing a config key, not a useful posture. Its common parse-error
text is carried by `ROUTE_BIND_RELAY_ZERO_MESSAGE`:

```
route_bind_relay_timeout_ms must be greater than 0 (a zero budget fails every bind to the module; to make a module unreachable use enabled: false)
```

At module scope, the error also names the offending module id. At either
scope, it names the key and the `enabled: false` remedy.

## First registration after spawn

`modules.<id>.register_timeout_s` bounds the wait for the first accepted HELLO
of an ordinary supervised `protocol: "subc"` child. It is a per-module integer
from **10 to 3600 seconds**, default **120 seconds**, measured from spawn using
the supervisor's monotonic clock. It is read when supervision starts (daemon
start or rescan adding the module), like the restart policy. Editing it for an
already-supervised module takes effect on the next daemon start.

The deadline applies on boot, start/enable, plain restart, crash respawn, and
slow failed-module retry. On expiry the daemon logs a WARN with module id, pid,
and wait, stops and reaps the child (Unix process-group TERM then KILL after
the existing grace; Windows Job Object termination), and records a crash with
`registration_timeout` in the terminal record's `disposition_detail`. This
reason is retained in the terminal journal and the history shown by
`ck module status` / `ck module terminals`. The normal restart budget and slow
retry apply, even if the child handles TERM by exiting zero.

An accepted HELLO cancels the deadline permanently for that process; a later
disconnect does not re-arm it. Exit, stop, disable, removal and daemon shutdown
also end the wait. `protocol: "none"` children have no registration deadline.
Reload retains its 1-second registration wait, and swap retains its separate
100-second default readiness wait. `route.open` during warm-up is unchanged.

## The crash-restart budget is a rate

`modules.<id>.restart` bounds how often the daemon will replace a module that
keeps crashing. It is per-module only (there is no daemon-wide `restart`
block), and every key is independent: whatever you omit keeps its default.

```jsonc
{
  "version": 1,
  "modules": {
    "flappy-worker": {
      "program": "/usr/local/bin/flappy-worker",
      "restart": { "max_restarts": 3, "window_secs": 600, "backoff_ms": 100, "max_backoff_ms": 30000 }
    }
  }
}
```

| Key | What it bounds | Built-in default | `0` |
| --- | --- | --- | --- |
| `max_restarts` | Replacement processes allowed *within* `window_secs`. | 3 | Accepted: never replace this module. |
| `window_secs` | The span those restarts are counted over. Restarts older than this release their slot. | 600 s | Refused: a zero window holds no crash, so the budget can never be spent and the module restarts forever. |
| `backoff_ms` | Base delay before a replacement spawn. | 100 ms | Accepted: respawn immediately. |
| `max_backoff_ms` | Upper bound for the escalating delay before a replacement spawn. Must be at least `backoff_ms`. | 30,000 ms | Accepted: `0` is valid only when `backoff_ms` is also `0`. |

The budget is a RATE, not a lifetime total, and the distinction is the whole
point of the window. A module that crashed twice yesterday has a full budget
today; a module crashing three times in ten minutes is in a loop and is
stopped. This matters now that modules exit non-zero whenever the daemon's
connection to them drops, since each of those drops spends a unit of the same
budget: under a lifetime total, one flappy hour would stop a healthy module
permanently.

Crash respawn delay escalates from `backoff_ms` by a factor of ten for each
restart already in the window, up to `max_backoff_ms`. This gives peers time to
finish warming during a daemon-wide boot storm: a fixed short delay can make a
module repeatedly probe a peer before it is ready and exhaust its restart budget
while the fleet is still settling.

When the budget refuses a respawn, the module goes to `failed` and both the log
line and the retained terminal record name the limit AND the window:

```
crash budget exhausted: max_restarts=3 within window_secs=600
```

`ck module status` renders the live budget the same way — `restarts 2 of 3 in
10m` — because `2 of 3` alone reads as a lifetime count. An operator restart,
reload, or re-enable hands the whole budget back; `lifetime_restarts` is the
ledger and never moves backwards.

The block is read when a module starts being supervised (daemon start, or a
rescan that adds the module). Like `drain_timeout_ms`, editing it for an
already-running module takes effect on the next daemon start.

## `protocol` declares whether a module speaks subc at all

```jsonc
{
  "version": 1,
  "modules": {
    "nats": {
      "program": "/usr/local/bin/nats-server",
      "protocol": "none",
      "drain_timeout_ms": 10000
    }
  }
}
```

`protocol` is per-module and optional. Absent and `"subc"` are the same value:
every module written before this key existed is a subc module, and there is no
third "unspecified" state. Any other value is refused at parse time, naming the
module and the value, because falling back to `"subc"` on a typo silently
restores the exact supervision the operator was trying to turn off.

`"none"` means the process speaks no subc wire: it never sends `HELLO`, never
registers, never answers `health.check`, and can never serve a route. The daemon
still supervises the PROCESS — spawn, process facts for `supervisor.provenance`,
exit classification, terminal records, and the full crash-restart budget — and
that is the whole reason such a program runs under subc at all.

What changes for a `"none"` module:

| Behaviour | `"subc"` | `"none"` |
| --- | --- | --- |
| Health probing | `health.check` on the configured cadence, escalating on the failure threshold | Suppressed. Silence from a module that speaks no wire is not evidence of anything. |
| `live` | enabled, running, process alive, AND registered | enabled, running, process alive. `ck` renders it as `n/a (no protocol)` rather than a liveness word, because the daemon is asserting less. |
| Teardown | route drain, `route.closed` pushes, per-route GOODBYEs, module GOODBYE, then the drain budget | `SIGTERM`, then the same drain budget, then `SIGKILL`. Read the result in `ck module terminals <id>`: `exit 0` or `exit_signal: 15` is a clean stop, `exit_signal: 9` means the child ignored the signal and the budget ran out. On Windows there is no graceful signal, so teardown is the wait and then the kill. |
| `route.open` | ordinary routing | refused with `module_no_protocol`, which every SDK classifies as terminal rather than retrying |
| Exit 0 the daemon did not ask for | a stop: recorded `stopped`, not restarted | a crash: spends restart budget, respawns with the crash backoff, and ends `failed` (budget named) when the budget runs out |

The last row exists because a stock program usually exits 0 on `SIGTERM`
(nats-server does). A subc-wire module is required to re-raise `SIGTERM`, so a
stray outside signal already reads as a crash; a `"none"` module cannot be made
to, and treating its exit 0 as a stop would leave it down until someone started
it by hand. Exits the daemon itself asked for — `ck module stop`, disable,
restart, reload, swap, a health restart, a drain that ran out of budget, and
daemon shutdown — keep their ordinary handling for both protocols.

The teardown wait is `drain_timeout_ms` — the same key, the same default, and
the same per-restart `--now`/`--drain-ms` overrides. A module with a store to
flush should set it to what that flush actually costs.

`reserved: true` with `protocol: "none"` is refused at parse. `reserved` is
enforced on a module's `HELLO`, and a module that never registers never sends
one, so the pair declares a protection that could never be checked.

A `protocol` change on a running module is a pending-reload difference, like
`program`, `args` or configured `env`: `supervisor.rescan --dry-run` reports it;
after apply, `ck module list` names `spec` as pending until the module restarts.
`ck module status` reports independent path, image and launch-spec verdicts.
The launch-spec comparison covers exactly the configured program, ordered args,
configured env block and protocol. It ignores inherited environment, the
connection file path, module id and launch nonce injected on each spawn. The
status (`match`, `mismatch` or `unavailable`) goes over the control wire, plus
a reason when it is `unavailable`; the digest stays in the daemon. A stopped
module or a running process without a recorded digest reports `unavailable`,
and an older daemon's omitted spec verdict is rendered as `not reported`.

## Keeping a process container across module replacements

```jsonc
{
  "version": 1,
  "modules": {
    "worker": {
      "program": "/usr/local/bin/worker",
      "carry_containment": true
    }
  }
}
```

`carry_containment` is an optional per-module boolean, defaulting to `false`.
Only daemon configuration can enable it; a module cannot request it in `HELLO`
or `catalog.update`. Combining it with `overlap: "safe"` is a config error:
carried containers do not support overlapping blue/green instances.

When enabled, a module's helpers can outlive the module process while a named
replacement is pending: operator restart, reload respawn, health or crash
restart within budget, or a restart after the supervisor explicitly marked a
connection severance as deliberate while the module remains enabled. Generic
connection loss alone does not set that mark. Replacement leaders join the
same container. Stop, disable, removal, a clean exit by a module using the Subc protocol,
exhausted restart budget, failed registration or spawn, supervisor failure or
abort, and daemon shutdown end the container and its helpers. This includes a
stop during backoff, when no module leader is alive. A later start gets a fresh
container. Helpers do **not** survive a daemon restart.

The setting is fixed for each container's lifetime. A config reload applies a
changed value to the next **fresh container**, not the next replacement in an
existing container. In particular, turning the flag off never abandons an
existing container: it is still destroyed at its terminal end. No daemon
restart is required to apply the per-module configuration.

Containment strength is platform-dependent:

- **Windows:** a kernel job contains the module and its descendants, with no
  breakaway. Closing the daemon's handles also kills the job on a daemon crash.
- **Linux with delegated cgroups:** replacements join the existing cgroup.
  Ordinary process-group or session changes cannot escape it. A same-user
  process with write access to the delegated subtree could still move itself
  out, as with ordinary module containment.
- **macOS and Linux without cgroup placement:** cooperative process groups,
  **not a security boundary**. A helper can escape with `setpgid` or `setsid`.
  The first leader is retained unreaped to reserve the group's identity until
  terminal cleanup, costing one zombie per carried module after its first exit.

Opt in only for trusted modules whose helpers stay in their container. Helpers
must use their own stdout/stderr rather than inherit the leader's capture
pipes, and must exit on EOF of their daemon connection.

After a daemon crash, a recorded cgroup is touched only under the daemon's
current trusted delegation. If that root is unavailable, inspection fails, or
members remain, module status reports the retained container and fresh starts
stay blocked. Periodic rechecks lift the block only after the whole subtree is
confirmed empty. An empty cgroup directory awaiting removal does not block.

An unreadable, malformed or unsupported-version `live-children.json` is never
treated as an empty recovery record. The daemon renames it beside the original
as `live-children.json.quarantined.<suffix>`, preserves it, and logs an error.
Every module with `carry_containment: true` is blocked because the identities
of any old helpers are unknown; `ck module status <id>` reports the reason.
The quarantined file itself is the persistent marker: the block survives any
number of daemon restarts, even if a new valid `live-children.json` exists.
Modules without the setting keep their ordinary startup behavior.

There is no online command to discard unknown recovery identities. To clear a
quarantine block safely:

1. Stop the daemon, including its service manager so it cannot restart.
2. Check that no old module processes or helpers remain. If their identities
   cannot be established, reboot the host before continuing.
3. Remove the quarantined file, or move it **out of the daemon run directory**.
   Preserve it elsewhere if it is needed for diagnosis.
4. Start the daemon again.

Removing a quarantine while the daemon is running, or without first resolving
possible survivors, is not a supported clearance and can leave duplicate helpers.

Quarantine also blocks a still-carried container whose configuration flag has
since been turned off. That container remains carried until its terminal end;
an ordinary module with no carried container is unaffected.

Recovery records now use version 2; the new daemon also reads version 1. An old
carried-container record without boot or session evidence blocks only that
module and is never used to authorize a signal. Ordinary version-1 leader
records retain their previous PID/start-time verification.

**Rolling back to 0.39.x:** stop the new daemon cleanly first. A verified clean
shutdown terminates every carried container and rewrites the record with an
empty `children` list; it does not delete the file. Then place and start 0.39.x.
If cleanup retains any unresolved entry, resolve it before rolling back.
If the new daemon crashed, do **not** start 0.39.x directly: start the new
daemon once, let its crash sweep clear or block the old containers, resolve
any remaining block, and stop it cleanly before rolling back. If it cannot
start, use offline cleanup as above instead. The released 0.39.x reader refuses
version 2 for signalling but then replaces it with an empty version-1 record;
it skips recovery of **ordinary leaders as well as carried helpers**. Those
processes can remain running, unknown to the old daemon. Rollback is not
globally fail-closed.

Carried Unix launches stay before exec while the daemon records their complete
identity and confirms cgroup/process-group placement. Admission has a fixed
30-second deadline, allowing slow disk syncs without waiting forever; it is not
a configuration knob. A timeout never starts the module, counts as a failed
spawn, and blocks that module until its child is reaped and the late record
write is durably resolved. Other carried launches can proceed after a known-PID
timeout. Before the child reports its PID, cancellation uses a buffered cancel
byte, not a guessed PID; the launch permit stays held until spawn returns.
If a late admission write or durable removal fails, every carry-enabled module
is blocked by the persistent quarantine marker, with the underlying cause in
`ck module status`. Ordinary modules are unaffected. Use the same offline
clearance steps above for that failed-write quarantine; a timer never clears it.


## Pre-auth limits are not configuration

There is deliberately no `auth_deadline_ms` or
`max_unauthenticated_connections` key. Production `ServerAuth::new` uses
`DEFAULT_AUTH_DEADLINE = 2 seconds` and
`DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS = 256`.

Those values govern separate pre-auth budgets:

1. A connection may wait up to the auth deadline to acquire an
   unauthenticated-handshake slot.
2. Once it has a slot, it receives a fresh full auth deadline for the HMAC
   handshake itself.

The budgets are deliberately independent. Charging queue time to the
handshake deadline would leave a restart-herd connection almost no handshake
time under CPU saturation, recreating the auth failure and restart-budget burn
that the queue prevents.

Do not add configuration paths for these limits. Loosening pre-auth posture is
an attack-surface change, not tuning: the only values worth setting are the
defaults. The two-budget contract was the structural fix for restart herds
under CPU starvation, so it removed the one incident class that might have
created timeout-tuning demand. The two timeout keys above had per-module
operator needs; these limits do not. House rule: prefer fewer knobs and
structural fixes. A bound an operator can widen is a default with extra steps.
