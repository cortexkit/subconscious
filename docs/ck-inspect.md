# Read-only process inspection

`ck-inspect` is an optional supervised module, id `inspect`. It declares no tools,
only two management queries, `module.resources` and `module.profile`, and
`flow-scopes/v1` with both names in `flow_operations`. Build it with
`cargo build -p ck-inspect` and configure it in `subc.jsonc`:

```json
{
  "version": 1,
  "modules": {
    "inspect": { "program": "/absolute/path/to/ck-inspect", "args": [], "env": {} }
  }
}
```

The module exits when its daemon connection ends. Route admission uses only the
daemon's bind stamp: a `Principal::Direct` operator is admitted, as is a scope with
both `owner_authorized: true` and a `flow_id`. Other routes are refused as
`not_permitted`. Body fields cannot grant access or select a PID. The module does
not relay arbitrary control operations.

## Queries and CLI

`ck module resources` sends `{"method":"module.resources","params":{}}` over a management route
to `inspect`. A successful reply is the standard management envelope,
`{"result": <value>}`, and a refusal is an Error frame carrying its code and
reason. The resources value is always an object:

```json
{"modules":[{"module_id":"worker","state":"running","spawn_generation":3,"pid":123,"resources":{"status":"measured","memory_bytes":4096,"memory_kind":"resident_set","cpu_user_ms":100,"cpu_system_ms":20}}]}
```

`module_id`, `state`, `spawn_generation` and `resources` are copied from a fresh
`supervisor.list`. Optional fields absent from the daemon stay absent. `pid` is
joined from `supervisor.spawn_snapshot` only when its generation matches the list
entry; a mismatch omits PID rather than pairing unrelated observations.

`ck module profile <id> [--seconds N]` sends
`{"method":"module.profile","params":{"module_id":"worker","seconds":1}}`. The default CLI
window is one second. The profile value, inside `{"result": ...}`, is:

```json
{"module_id":"worker","pid":123,"seconds":1,"method":"sample","threads":[{"name":"worker","samples":90,"busy_samples":50,"stacks":[{"count":50,"frames":["work","main"]}]}],"idle_threads":2,"truncated":false}
```

`method` is an open string. New public Rust result structs are non-exhaustive and
have constructors. The whole reply, envelope included, stays within 16 KiB.
`--json` prints the value inside `result`; `ckni --json` adds the
`ck.module.resources/v1` or `ck.module.profile/v1` schema. Plain output prints all
values from the bounded result. Missing or stopped `inspect` is a one-line CLI
refusal (exit 1) pointing to `ck setup` or daemon config. This workspace does not
add an installer component; manual configuration is sufficient.

## Measurements and bounds

- Only the supervisor-spawned process is inspected, not its children, just like
  the daemon's resource counters. The PID and spawn generation come from a fresh
  spawn snapshot, never from the request. A second snapshot after capture must
  agree, or the result is discarded as `module_restarted`. Profiling cannot
  distinguish a daemon restart with coincidentally identical process identities;
  loss of the daemon connection ends the inspection module itself.
- macOS runs `/usr/bin/sample <pid> <seconds> 10 -file <output>` without a shell,
  using a fresh private 0700 temporary directory, removed after parsing. The
  interval is 10 ms. Counts describe sampled positions, not CPU milliseconds.
  For each thread it returns the top three exclusive stacks whose leaf is not a
  known wait, at most 12 frames, leaf first, and `busy_samples`, the number of
  samples whose leaf was not a wait. Addresses and source locations are
  stripped. Rust legacy and v0 names are demangled without hash suffixes. A
  frame in a stripped binary, which `sample` prints as `???`, is reported as
  `image+0xOFFSET`, the offset from the image's load address, unless a matching
  separate dSYM supplies its name (see below). Thread labels are retained
  as `sample` prints them.
- A known wait is a leaf blocked in the kernel: `__psynch_cvwait`,
  `__psynch_mutexwait`, `__ulock_wait`, `__ulock_wait2`, `kevent`, `kevent64`,
  `kevent_id`, `mach_msg`, `mach_msg2_trap`, `__semwait_signal`,
  `semaphore_wait_trap`, `semaphore_timedwait_trap`, `__workq_kernreturn`,
  `read`, `__read_nocancel`, `recvfrom`, `__recvfrom`, `recvmsg`, `__recvmsg`,
  `accept`, `__accept`, `connect`, `__connect`, `select`, `__select`, `pselect`,
  `__pselect`, `poll`, `__poll_nocancel`, `__sigsuspend`, `__wait4` or `__waitid`,
  including `_suffix` variants such as `_nocancel`.
  A macOS thread whose every sampled leaf is a wait is idle: it's counted in
  `idle_threads` and not listed. Runtime ancestors of a sleeping leaf do not
  make it busy.
- Linux reports `method: "proc_threads"`: thread CPU milliseconds from differences
  in `/proc/<pid>/task/*/stat` user+system ticks, with names from `comm`. Threads
  present at both endpoints with the same start time are compared; exited, newly
  created or reused thread ids are omitted. Zero-tick threads count as idle.
  No stacks are returned. Windows is unsupported.
- The window must be an integer from 1 through 10. Invalid requests run nothing.
  One capture runs at a time per `inspect` process; contenders are refused, never
  queued. A module has a 60-second cooldown after an attempted capture completes
  (including failure/cancellation), using a monotonic clock. No measurements or
  decisions are retained; only the cooldown timestamps survive a reply.
- The sampler deadline is `seconds + 5` seconds. On macOS, timeout/cancellation
  kills the **sampler's** process group, never the inspected process; Linux
  reads `/proc` in-process. At most 8 MiB of sampler output is read for parsing.
  Replies keep at most eight threads, ordered by `busy_samples` on macOS or CPU
  on Linux, so the busiest threads survive the limits. The serialized profile body is at most 16 KiB;
  `truncated` is true when data is omitted, including stack/thread limits.

Inspection never signals, restarts, or changes the target process. On macOS,
`/usr/bin/sample` briefly pauses the target's threads at each sample (every
10 ms) to read their stacks; on Linux the module only reads `/proc`. When the
deadline passes or the call is cancelled, the module kills the sampler's own
process group, never the target.
It records no history and sets no high-CPU policy. A flow may periodically read
resources and request a bounded profile when its own policy requires one.

### Separate debug symbols on macOS

A module can keep named frames in profiles of stripped binaries by retaining each
build's debug symbols at:

```text
<data home>/cortexkit/<module_id>/dsym/<UUID>/<anything>.dSYM/Contents/Resources/DWARF/<debug file>
```

The data home is the same resolved data home used for the module's other paths.
Use the module id from the profile request (for example, `aft`, not the executable
name). The UUID is the image's Mach-O `LC_UUID`, written as uppercase hexadecimal
with no dashes. The UUID directory must contain exactly one `*.dSYM` bundle, and
its `Contents/Resources/DWARF/` directory must contain exactly one file. The DWARF
file's name need not match the executable. Lowercase or dashed UUID directory
names are not recognized.

The profiler reads image load addresses and UUIDs from `sample`'s `Binary Images:`
section. For unresolved `image+0xOFFSET` frames with a matching bundle, it invokes
`/usr/bin/atos` without a shell, at most once per image and with at most 256
distinct addresses per image. Symbolization uses only the time remaining in the
existing `seconds + 5` deadline; an overrun kills only the symbolizer process.
Names lose image/source suffixes and Rust symbol hashes, just like other sampled
frames. Stacks that become identical are merged before stack ranking and limits,
then the existing 16 KiB reply budget is applied.

Missing or ambiguous debug files, a missing or failed symbolizer, timeouts, and
addresses the symbolizer cannot resolve leave `image+0xOFFSET` intact. None of
these conditions fails a profile. Linux `proc_threads` profiles are unchanged.

## Typed refusals

All refusals are terminal for the call: none is retried in place. They are unknown
or terminal under the protocol's existing `is_retryable_route_open` predicate;
that predicate is not a general query retry loop. Query errors carry
`detail.class: "terminal"` and a short `detail.reason`, never sampler stdout or
stderr. Bind rejections use the SDK's typed error frame.

| Code | Meaning | Class |
| --- | --- | --- |
| `invalid_request` | Bad query, caller-supplied PID, or seconds outside 1..10 | terminal |
| `not_permitted` | Bind is neither direct operator nor authorized flow | terminal |
| `unknown_module` | ID absent from the supervised roster | terminal |
| `not_running` | No live process in the spawn snapshot | terminal |
| `busy` | Another capture is active in this inspect process | terminal |
| `rate_limited` | Cooldown; `detail.retry_after_ms` is time remaining | terminal |
| `module_restarted` | PID/generation changed or disappeared during capture | terminal |
| `unsupported_platform` | OS has no profiling backend | terminal |
| `sampler_failed` | Capture failed or exceeded its deadline; static short reason | terminal |
| `roster_unavailable` | Daemon cannot provide list or spawn snapshot | terminal |

A watch flow should skip `busy`, `rate_limited`, `not_running` and
`module_restarted`, then try on its next normal tick, not immediately retry.
