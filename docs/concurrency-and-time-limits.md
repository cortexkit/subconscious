# CortexKit concurrency and time limits

This reference defines common admission and timeout rules in §0 and daemon/SDK limits in §1; module maintainers document their own limits against those rules. It states how much work the fleet admits, how long callers wait, and which refusals they must handle.
Values are quoted from source; each names the constant, so a change to the code shows up as a
difference from this page.

## 0. Rules that hold everywhere

1. **No retry outlives its caller.** Every retry loop stops at the earlier of its own deadline
   and the deadline of the call that started it. A retry that would end after the caller gave up
   does work nobody is waiting for. Owned background work is a separate class (rule 6).
2. **Retryable means no effect.** A retryable refusal is only issued when the request had no
   effect: it never reached the module, or it was rejected before doing anything (for example an
   upstream 400 or 422 on a rewritten body). So retrying it cannot run anything twice. A refusal
   that could follow a partial effect is not retryable.
3. **An unknown refusal code is never treated as retryable.** The component that received it
   does not retry the call, immediately or on a backoff, and never re-sends a mutation because of
   it, so a newer producer cannot turn an old reader into a retry loop. The refusal ends that
   attempt. A later attempt the caller makes on its own (a harness re-sending a turn, a periodic
   read on its fixed schedule) is not a retry by the component and is allowed, as long as the
   refusal itself causes no extra or faster attempts.
4. **Admission limits are per connection or per target, never global,** so one busy client or
   one slow module cannot starve the rest. A process-wide limit that protects a shared machine
   resource (CPU, disk, memory across projects) is allowed, provided it is stated in the owner's
   section, refuses or queues by name, and is never the only thing standing between one busy
   caller and everyone else.
5. **Every refusal names its reason.** A caller, a log reader and an operator must be able to
   tell "at capacity" from "restarting" from "not allowed" without reading source.
6. **Owned background work has its own bound.** Work a module starts for itself after answering a
   request (delivering a deferred command, a reduce, a late result) may outlive that request by
   design. It must be durable or deduplicated so a repeat does nothing twice, bounded by a stated
   budget, and listed in the module's section 3 with that budget.

## 1. Daemon (subc) and SDKs

### 1.1 Opening routes

| Limit | Value | Source |
|---|---|---|
| Pending `route.open` per client connection | 8 | `MAX_PENDING_ROUTE_OPENS_PER_CONNECTION`, subc-daemon server.rs:53 |
| Pending binds per target module | 16 (2 x 8) | `MAX_PENDING_ROUTE_BINDS_PER_TARGET`, server.rs:57 |
| Bind relay timeout (module must answer `route.bind`) | 12 s | `DEFAULT_ROUTE_BIND_RELAY_TIMEOUT`, control.rs:115 |
| Bind breaker: timeouts before it opens | 3 | `DEFAULT_ROUTE_BIND_BREAKER_THRESHOLD`, control.rs:131 |
| Bind breaker cooldown | 20 s | `DEFAULT_ROUTE_BIND_BREAKER_COOLDOWN`, control.rs:150 |
| Required capability settle deadline | 120 s | `CAPABILITY_SETTLE_DEADLINE`, capability_requirements.rs:16 |

A refusal at either open limit is the retryable `target_unavailable`; the per-module one is
logged with reason `target_binds_full`.

**Retryable `route.open` codes:** `module_reloading`, `module_warming`, `target_unavailable`,
`module_timeout` (`is_retryable_route_open`, subc-protocol lib.rs:105-109). Every other code,
including any the reader doesn't know, is terminal. The golden table is
subc-protocol tests/golden/decision_tables.json.

### 1.2 Client connections

| Limit | Value | Source |
|---|---|---|
| Egress bytes queued per client connection | 4 MiB | `CONNECTION_EGRESS_BYTE_BUDGET`, server.rs:38 |
| Largest single frame counted against it | 32 KiB | `CONNECTION_EGRESS_FRAME_CAP`, server.rs:47 |
| Unauthenticated connections | 256 | `DEFAULT_MAX_UNAUTHENTICATED_CONNECTIONS`, server.rs:64 |
| Handshake deadline | 2 s | `DEFAULT_AUTH_DEADLINE`, server.rs:59 |

A client that lets 4 MiB of replies queue up unread is disconnected with a logged warning that
names the connection, its routes and the module whose reply overflowed. Clients must keep reading.

### 1.3 Modules

| Limit | Value | Source |
|---|---|---|
| Drain wait for in-flight requests (restart, reload, stop) | 30 s default, per module config | `DEFAULT_DRAIN_TIMEOUT`, supervise.rs:79 |
| Daemon shutdown, per module | 25 s cap | `CHILD_SHUTDOWN_CAP`, child_roster.rs:265 |
| Restart budget | 3 in 10 min | `DEFAULT_MAX_RESTARTS`, `DEFAULT_RESTART_WINDOW`, supervise.rs:62-68 |
| Restart backoff | 100 ms to 30 s | `DEFAULT_BACKOFF`, `DEFAULT_MAX_BACKOFF`, supervise.rs:63-64 |
| Health probe deadline | 5 s | `DEFAULT_HEALTH_DEADLINE`, supervise.rs:530 |
| Health failures before a module is faulted | 3 | `DEFAULT_HEALTH_FAILURE_THRESHOLD`, supervise.rs:531 |
| Swap candidate ready timeout | 100 s | `DEFAULT_SWAP_READY_TIMEOUT`, supervise.rs:407 |
| Spawn event ring (`supervisor.spawn_subscribe`) | 4096 events | `SPAWN_EVENT_RING_CAPACITY`, supervise.rs:105 |

What a module must do:
- **Exit on EOF of its daemon connection** within its shutdown budget, and re-raise a caught stop
  signal instead of exiting 0.
- **Answer `route.bind` within 12 s.** Slow warm-up belongs behind `ready: false`, which callers
  see as the retryable `module_warming`, not inside `on_bind`.
- **Answer health within 5 s** without disk, locks or subprocesses on the health path. That
  includes any lock a working job can hold, and anything that signals or waits on a job (a
  cancellation, say): read job state from atomics or a lock-free snapshot. A health path that can
  block on a stuck job fails exactly when it is needed: a health report that waits on a lock held
  by a deadlocked job cannot report the deadlock.
- **Never block the task that reads the daemon connection on module work.** That task also
  answers health, accepts `route.bind`, and reads the stop notice; if it blocks, the module refuses
  every new route, ignores its own stop and is killed at the end of the drain window.
- **Finish or give up on in-flight work within its drain window;** long work should detach or
  checkpoint rather than hold the drain.

### 1.4 SDKs

| Setting | Rust (`subc-client-rs`) | TypeScript (`@cortexkit/subc-client`) |
|---|---|---|
| Route-open retry deadline | 90 s, `DEFAULT_ROUTE_RETRY_DEADLINE`, consumer.rs:52 | 90 s, `ROUTE_OPEN_RETRY_DEADLINE_MS`, client.ts:86 |
| Opens in flight per connection | 8, `MAX_ROUTE_OPENS_IN_FLIGHT`, consumer.rs:59 | 8, `MAX_ROUTE_OPENS_IN_FLIGHT`, client.ts:93 |
| Default call timeout | 30 s, `DEFAULT_CALL_TIMEOUT`, consumer.rs:44 | 30 s, `DEFAULT_REQUEST_TIMEOUT_MS`, client.ts:51, for the response wait only |

The 90 s retry deadline covers a full module restart (drain up to 30 s, stop, start, register),
which was measured at 62.5 s once. Both SDKs stop route-open retries at the call's own
deadline when it comes first (rule 0.1). Rust takes `(now + route_retry_deadline).min(call_deadline)`
(consumer.rs:2718), pinned by
`a_call_timeout_shorter_than_the_retry_deadline_ends_the_retries_at_the_call_timeout` (`crates/subc-client-rs/src/consumer.rs`). So its
30 s default call timeout caps the retries, and a caller that wants to ride out a restart raises
both. TypeScript takes the minimum of the retry deadline and `timeoutMs` when the call sets one
(client.ts:1339-1343). A call without `timeoutMs` has no overall deadline: it may retry the open
for the full 90 s, and the 30 s default then bounds only the wait for the response. That is
consistent with rule 0.1, since the caller set no deadline, but it differs from Rust. A TypeScript
caller that needs a bound sets `timeoutMs`. Retry delays are jittered so routes
refused together do not retry in step. The in-flight limit matches the daemon's per-connection
limit, so the SDK queues opens locally instead of having the daemon refuse them.


## 2. Where timeouts must nest

From the outside in, each layer's limit must be larger than the one it waits on, or a retry
at an outer layer repeats work an inner layer is still doing:

- the phone app (outermost client): SubcFed call deadline 300 s, relay authentication
  15 s, polls every 5 s in the foreground and 30 s after two minutes idle, each awaiting its own
  calls so none stack, and busy-session reads every 350 ms (transcript) and 300 ms (display)
  that stop once the session is idle
- Claude Code (outermost, through the Thalamus gateway): retries a 503 about 12 times over
  about 180 s (measured); its own per-request timeout is unmeasured beyond tolerating a 125 s
  stall. The gateway's worst case to first byte is 30 s body read + 120 s transform + 120 s
  upstream headers = 270 s, or 1,650 s with the 1,500 s emergency transform budget. Open: if
  Claude Code gives up before that, the transform keeps running for nobody. The Thalamus maintainer must establish Claude Code's timeout.
- caller's call timeout. A call into Cerebellum that should wait for the user's consent in-call
  needs a timeout above Cerebellum's 120 s consent park, which is longer than the SDKs' 30 s
  default. The park never holds a drain.
  - SDK route-open retry deadline (capped by the caller's timeout)
    - daemon bind relay timeout (12 s)
      - module `on_bind` work
- module drain window (30 s)
  - the longest request a module admits
- daemon shutdown, 25 s per module
  - each module's own shutdown budget (Cerebellum 20 s shared, Broca 20 s ceiling)
