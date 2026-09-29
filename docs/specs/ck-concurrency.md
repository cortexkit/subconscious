# CortexKit concurrency and time limits

Status: draft r1. Section 1 (the daemon and its SDKs) is written by SUBC; each role owner adds a
section for its own module (section 3). This spec says, in one place, how much work each part of
the fleet admits at once, how long it waits, and what a caller should do when it is refused.
Values are quoted from source; each names the constant, so a change to the code shows up as a
difference from this page.

## 0. Rules that hold everywhere

1. **No retry outlives its caller.** Every retry loop stops at the earlier of its own deadline
   and the deadline of the call that started it. A retry that would end after the caller gave up
   does work nobody is waiting for.
2. **Refused means not sent.** A retryable refusal is only ever issued before the request reached
   the module, so retrying it cannot run anything twice. A refusal that could follow a partial
   send is not retryable.
3. **Unknown codes are terminal.** A reader that meets a refusal code it doesn't know treats it
   as terminal, never as "retry": a newer producer cannot turn an old reader into a retry loop.
4. **Limits are per connection or per target, never global,** so one busy client or one slow
   module cannot starve the rest.
5. **Every refusal names its reason.** A caller, a log reader and an operator must be able to
   tell "at capacity" from "restarting" from "not allowed" without reading source.

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
- **Answer health within 5 s** without disk, locks or subprocesses on the health path.
- **Finish or give up on in-flight work within its drain window;** long work should detach or
  checkpoint rather than hold the drain.

### 1.4 SDKs

| Setting | Rust (`subc-client-rs`) | TypeScript (`@cortexkit/subc-client`) |
|---|---|---|
| Route-open retry deadline | 90 s, `DEFAULT_ROUTE_RETRY_DEADLINE`, consumer.rs:52 | 90 s, `ROUTE_OPEN_RETRY_DEADLINE_MS`, client.ts:86 |
| Opens in flight per connection | 8, `MAX_ROUTE_OPENS_IN_FLIGHT`, consumer.rs:59 | 8, `MAX_ROUTE_OPENS_IN_FLIGHT`, client.ts:93 |
| Default call timeout | 30 s, `DEFAULT_CALL_TIMEOUT`, consumer.rs:44 | (to confirm) |

The 90 s retry deadline covers a full module restart (drain up to 30 s, stop, start, register),
which was measured at 62.5 s once. The Rust SDK's 0.22.0 changelog states that
the retries still end at the call's own timeout when that comes first (rule 0.1), so the 30 s
default call timeout caps them and a caller that wants to ride out a restart raises both. That
is recorded from the changelog, not yet re-read in code. Retry delays are jittered so routes
refused together do not retry in step. The in-flight limit matches the daemon's per-connection
limit, so the SDK queues opens locally instead of having the daemon refuse them.

Open for this section: confirm in code, for both SDKs, the default call timeout and that
route-open retries stop at the caller's deadline.

## 2. Where timeouts must nest

From the outside in, each layer's limit must be larger than the one it waits on, or a retry
at an outer layer repeats work an inner layer is still doing:

- caller's call timeout
  - SDK route-open retry deadline (capped by the caller's timeout)
    - daemon bind relay timeout (12 s)
      - module `on_bind` work
- module drain window (30 s)
  - the longest request a module admits

## 3. Per-role sections

Each owner adds its module's admission limits, queue bounds and timeouts, with source, and
states how they nest inside section 2. Open slots:
- llm-runner (BROCA): run admission, concurrent sessions, model request timeouts and retries.
- Prefrontal (ALF): wake and effect dispatch, relay limits, `delegation_not_registered` wait.
- AFT: tool call admission, bash and background task limits, drain behaviour.
- Plexus (PLEX): vendor call rate limits and retry, stored approval execution window.
- Cerebellum (CEREB): consent waits, takeover grants, input dispatch.
- Callosum (CALLO): federation rate buckets (32/s, burst 32, 16 concurrent), ledger grace.
- Magic Context (MC), Thalamus (THALAMUS), and any other module that admits work.
