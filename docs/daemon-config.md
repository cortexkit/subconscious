# Daemon timeout configuration and pre-auth limits

`$XDG_CONFIG_HOME/cortexkit/subc.jsonc` has two timeout keys that are
deliberately configurable. Both may appear at the daemon root as defaults or
inside an individual module. Resolution happens while the file is parsed:
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
      "route_bind_relay_timeout_ms": 5000
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
