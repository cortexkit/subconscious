# CortexKit concurrency and time limits

Status: draft r1. Section 1 (the daemon and its SDKs) is written by SUBC; each role owner adds a
section for its own module (section 3). This spec says, in one place, how much work each part of
the fleet admits at once, how long it waits, and what a caller should do when it is refused.
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
- **Answer health within 5 s** without disk, locks or subprocesses on the health path.
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
`a_call_timeout_shorter_than_the_retry_deadline_ends_the_retries_at_the_call_timeout`. So its
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

- the phone (outermost client, alfonso-ios): SubcFed call deadline 300 s, relay authentication
  15 s, polls every 5 s in the foreground and 30 s after two minutes idle, each awaiting its own
  calls so none stack, and busy-session reads every 350 ms (transcript) and 300 ms (display)
  that stop once the session is idle
- Claude Code (outermost, through the Thalamus gateway): retries a 503 about 12 times over
  about 180 s (measured); its own per-request timeout is unmeasured beyond tolerating a 125 s
  stall. The gateway's worst case to first byte is 30 s body read + 120 s transform + 120 s
  upstream headers = 270 s, or 1,650 s with the 1,500 s emergency transform budget. Open: if
  Claude Code gives up before that, the transform keeps running for nobody. THALAMUS is
  measuring Claude Code's timeout.
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

## 3. Per-role sections

Each owner adds its module's admission limits, queue bounds and timeouts, with source, and states
how they nest inside section 2.

### 3.1 Linked sections (in their owners' repositories)

- **llm-runner (BROCA):** `broca/docs/concurrency-limits.md`, landing on broca's master. A broca
  run is owned background work (rule 6): per model step, 6 attempts with backoff of 2 to 30 s,
  Retry-After capped at 120 s, and no new wait ending more than 240 s after the first failure.
  Its seal gives 10 s of grace inside a 20 s ceiling, which nests under the daemon's 25 s
  shutdown cap. Open, being fixed in broca: each route-open attempt waits only 10 s, under the
  daemon's 12 s bind relay, so a bind that would succeed at 11 s is reported as failed.
- **Plexus (PLEX):** `plexus/docs/design/concurrency.md`, landing on plexus's main. On EOF
  plexus finishes its current sequential poll pass, measured at up to 31 s in steady state and
  99.6 s during a backfill, so it can outlast the 30 s drain and be killed mid-pass. That is safe
  because each poll commits in one transaction; a kill only delays wakes.

- **Prefrontal (ALF):** `prefrontal/docs/specs/concurrency-prefrontal.md` (prefrontal main,
  a4cbde91d).
- **Callosum (CALLO):** `callosum/docs/concurrency-limits.md` (callosum master, 37267f1),
  including the federation rate buckets (32/s, burst 32, 16 concurrent) and the ledger grace.

- **Magic Context (MC):** `magic-context/docs/architecture/concurrency-limits.md` (magic-context
  master, 5cbcbb7f).
- **Thalamus (THALAMUS):** `thalamus/docs/concurrency-limits.md` (thalamus master, f82a9cc). Its
  section-0 cases are folded into rules 2, 3 and 6 and into section 2.

### 3.3 AFT (inlined from AFT, not published elsewhere)

Values below are production defaults. Paths are relative to `aft/`; a bound called *process-wide* is shared across roots/sessions. These are separate from the daemon/SDK limits in §1.

##### Executor and builds

| Limit / key | Value and bound | Source |
|---|---|---|
| `ExecutorConfig::default().pool_size` | `clamp(available_parallelism - 1, 2, 8)` (fallback CPU count 2); general executor threads, effective range 2–8. | `crates/aft/src/executor/mod.rs:118-135,160-163` |
| `BIND_RESERVE_WORKERS` | 2 additional RouteBind-only threads, total 4–10; no general work uses them. Bind writer promotion after 500 ms (`BIND_PROMOTION_AGE`), other interactive writers after 6 s. | `crates/aft/src/executor/mod.rs:83-100,152-157` |
| `Lane`, `JobClass` | Five lanes: `PureRead`, `SerialLspStatus`, `HeavyInit`, `Mutating`, `MaintenanceCommit`; two classes: `Interactive`, `Maintenance`. Read/actor cap defaults `clamp(pool_size-1,1,4)`; heavy permits `clamp(pool_size-1,2,3)` (effective heavy cap 1–3, leaving a general worker). | `crates/aft/src/executor/mod.rs:32-65,118-133,160-174` |
| `interactive_reserve`, `maintenance_cap` | Reserve 2 general threads when pool ≥4, else 1; maintenance in flight ≤ `max(1, pool_size - reserve)` = 1–6. | `crates/aft/src/executor/mod.rs:175-188` |
| `MAINTENANCE_QUEUE_CAP`, `MaintenanceCoalesceKey` | 512 queued maintenance jobs **per actor**; coalesce queued WatcherDrain/LspDrain/StandingPass/ConfigReload by root and key; at cap remove duplicate drains then refuse if still full. | `crates/aft/src/executor/mod.rs:30,67-79,1452-1507` |
| `DEFAULT_COLD_BUILD_LIMIT`, `GLOBAL_COLD_BUILD_LIMITER` | 2 concurrent cold builds **process-wide** (test build 1024); waiting maintenance/inspect/standing classes share slots, root-aware requests may share a permit. | `crates/aft/src/cold_build_limiter.rs:6-17,31-45,77-90,110-116` |
| `WARM_SEARCH_RELOAD_LIMIT` | 4 concurrent warm search-index parses/verifications **process-wide**, isolated from cold builds. | `crates/aft/src/commands/configure.rs:4295-4302` |
| `BuildDeathBreaker` | Per root/domain/fingerprint: trip at `ZERO_CREDIT_DEATH_LIMIT=3`, `CREDITED_DEATH_LIMIT=6`, or `IN_BUILD_BURN_LIMIT_MS=30 min`; `TRIP_TTL_MS=24 h`; marker heartbeat 5 s/recent 15 s. | `crates/aft/src/build_breaker.rs:14-23,76-83` |

##### Daemon edge and bridge

| Limit / key | Value and bound | Source |
|---|---|---|
| `ROUTE_BIND_DEADLINE` / `RouteBindAck` | 12 s pending bind deadline; successful configure enqueues ack before post-bind maintenance. Relay also expires at 12 s (§1). | `crates/aft/src/subc/mod.rs:153-154,5317-5330,5358-5403` |
| `MODULE_DRAINING_WINDOW_CAP` | `min(daemon-supplied remaining deadline, 120 s)` protects against bogus notices; normally 30 s. End bg_events with `StreamEnd`; detach held bash, settle permission asks/deferred responses, let normal tool calls/binds finish; log held-request census at start/quiescence/deadline/connection end. | `crates/aft/src/subc/drain.rs:1-70`; `crates/aft/src/subc/mod.rs:3862-3880,4292-4350,4692-4710` |
| `LSP_SHUTDOWN_ALL_BUDGET` / graceful index flush | 1.5 s total LSP shutdown; natural stdin EOF waits at most 300 ms for search-index flush thread; callgraph refresh flush follows synchronously. | `crates/aft/src/lsp/manager.rs:37-39,2461-2474`; `crates/aft/src/main.rs:602-623` |
| `ready: false`, `PLAIN_START_WARM_BUDGET`, `SWAP_CANDIDATE_WARM_BUDGET` | Manifest initially unready; 10 s normal start, 90 s swap candidate (incumbent serves). Live-roots query ≤5 s, `catalog.update` flip attempt ≤5 s with 250 ms–10 s backoff. | `crates/aft/src/subc/manifest.rs:356`; `crates/aft/src/subc/readiness.rs:45-69` |
| `ROUTE_OPEN_RELOAD_WAIT_CEILING_MS` | 45 s ceiling, but actual wait budget is min(call's `timeoutMs` or 30 s SDK default, 45 s); retry delay 100 ms–2 s. | `packages/aft-bridge/src/subc-transport.ts:224-263,1740-1767` |
| `MAX_CONSECUTIVE_TRANSPORT_FAILURES`, `LIVENESS_PROBE_TIMEOUT_MS` | After 3 unanswered transport calls probe the same pooled connection for 15 s before dropping it; successful request resets counter. | `packages/aft-bridge/src/subc-transport.ts:205-219,1793-1803,1823-1835` |
| Bridge transport deadline | `DEFAULT_BRIDGE_TIMEOUT_MS=30 s`; per-command overrides 60 s for callers/callgraph/trace*/impact/inspect/grep/glob/search/semantic_search; passive status ≤5 s, with no hang escalation. | `packages/aft-bridge/src/bridge.ts:18-20,872-887`; `packages/aft-bridge/src/command-timeouts.ts:15-27,34-56` |

##### Tool calls, bash, storage, relay, LSP

| Limit / key | Value and bound | Source |
|---|---|---|
| `inspect.diagnostics_timeout_ms` | Default 120 s, clamped 10–600 s; **whole request** shares one deadline, stops work 5 s before terminal; each phase waits ≤ min(half remaining work budget, 60 s). Plugin transport budget adds 30 s. `inspect.tier2_pass_timeout_ms` defaults 600 s, separately configurable without a clamp in resolver (pass budget, not request deadline). | `crates/aft/src/config.rs:21-24,296-314`; `crates/aft/src/config_resolve.rs:1972-1988`; `crates/aft/src/commands/inspect.rs:28-36,50-73`; `packages/opencode-plugin/src/tools/inspect.ts:13,347-350` |
| `FIRST_SEARCH_INDEX_LOAD_WAIT_BUDGET` | 2.5 s first `aft_search` wait for a concurrently loading index. | `crates/aft/src/commands/semantic_search/mod.rs:145-162` |
| `AFT_CALLGRAPH_BUILD_WAIT_MS` | 0 ms default (async `Building`), optional environment milliseconds; inline wait for a cold callgraph build; no production clamp found. | `crates/aft/src/context.rs:3202-3212` |
| `DEFAULT_BG_TIMEOUT`, `foreground_wait_window_ms` | 30 min default command runtime, configurable per bash command by `timeout`; 15 s default foreground auto-promotion window (internal setting); `wait:true` can wait until command exits. | `crates/aft/src/bash_background/registry.rs:68,1894`; `crates/aft/src/config.rs:589-591,750-752`; `crates/aft/src/commands/bash_orchestrate.rs:14,489` |
| `bash.watch_sync_max_ms`, `MAX_WATCHES_PER_TASK` | Plugin sync-watch max default 120 s, clamped 1–1800 s; a watch defaults to 30 s in primary. Up to 8 watches per task. | `crates/aft/src/config.rs:25-27,504-507`; `packages/opencode-plugin/src/tools/bash_watch.ts:74,138`; `crates/aft/src/bash_background/watches.rs:5,91-92` |
| Bash output and watchdog | Final 16 KiB (6 KiB head/10 KiB tail); running preview 8 KiB; raw/structured reply 50 KiB; compression input 10 MiB (4+6 MiB); watchdog tick 500 ms. Foreground orchestration polls every 100 ms **off executor workers** between short read jobs. | `crates/aft/src/bash_background/output.rs:3-34`; `crates/aft/src/bash_background/watchdog.rs:8-14`; `crates/aft/src/subc/mod.rs:160-163` |
| `STEADY_BUSY_TIMEOUT`, deferred `open` | SQLite aft.db busy timeout 5 s; deferred open/migration retries for 10 s, backoff 10–200 ms, per-attempt initialization busy wait ≤100 ms; single-attempt request path uses `TOOL_RETRY_BUSY_WAIT=250 ms` and does not repeat deferred wait. | `crates/aft/src/db/mod.rs:423-427,489-539` |
| `FOLD_BUSY_WAIT`, maintenance cadence | Ledger fold SQLite wait 250 ms and try-lock of process DB mutex; failed folds remain pending for next run, at most once/minute in production. | `crates/aft/src/db/write_ledger.rs:9-14,26-60`; `crates/aft/src/db/mod.rs:460-473` |
| Retention | Bash terminal tasks 30 days; compression raw events 30 days, batches 500, mutation mutex budget 100 ms with 50 ms busy wait, count lock retry 10 ms, skipped sweep retry after 5 s; ledger 7 days; named checkpoints 14 days. | `crates/aft/src/db/bash_tasks.rs:12`; `crates/aft/src/db/compression_events.rs:355-376`; `crates/aft/src/write_ledger.rs:16`; `crates/aft/src/checkpoint.rs:22-23` |
| gh-shim relay | Setup 5 s, each request 30 s; transient refusal sleeps 5 then 10 s, one remint retry, same nonce; exit 86 named refusal, 87 outcome unknown (ordinary upstream failure 1). | `crates/aft/src/gh_shim_relay_client.rs:33-48,589-669`; `crates/aft/src/gh_shim.rs:40-42,6422-6428` |
| LSP | Interactive request 8 s; initialize handshake ≤30 s (caller-owned shorter timeout accepted), shutdown request 5 s standalone; `idle.lsp_ttl_minutes` default 10, clamped 1–10; newly spawned unclaimed child orphan grace 10 s. | `crates/aft/src/lsp/client.rs:23-28,711-731,805-811`; `crates/aft/src/config.rs:33-36`; `crates/aft/src/lsp/child_registry.rs:23-27,42-49` |

##### Nesting against §1/§2

| Inner AFT work / outer limit | Nests? |
|---|---|
| RouteBind 12 s vs daemon bind relay 12 s | **No margin**: AFT's own expiry is equal to relay deadline, so an error generated at 12 s cannot reliably arrive before the daemon's timeout; normal configure+ack must finish earlier (`subc/mod.rs:153-154,5317-5329,5358-5403`). |
| Plain-start warmup 10 s vs SDK 90 s route-open retry / daemon 12 s bind relay | **Yes in intended path**: ready:false refuses opens before bind; swap candidate 90 s runs behind serving incumbent, not inside bind (`subc/manifest.rs:356`; `subc/readiness.rs:45-54`; `subc/mod.rs:5317-5330`). |
| Bridge reload wait min(call budget,45 s) vs SDK 90 s retry and daemon 30 s drain | **Nominally yes for its own delay budget**, but 45 s can cover a 30 s drain plus restart; only retry sleeps, not time inside routeOpen, are subtracted (see violation below). `subc-transport.ts:1740-1767,1978-1983`. |
| AFT drain window vs daemon 30 s drain | **Yes on normal supplied deadline**: uses the daemon's absolute deadline, caps erroneous longer notice at 120 s; streams end immediately and held bash detaches, but ordinary tools may exceed 30 s (`subc/drain.rs:1-70`; `subc/mod.rs:4292-4350`). |
| AFT shutdown vs daemon 25 s child cap | LSP 1.5 s + search flush wait 0.3 s **nest individually**; synchronous callgraph refresh flush has no shown total deadline, so **whole shutdown not proved to nest** (`lsp/manager.rs:37-39,2461-2474`; `main.rs:602-623`). |
| Inspect 120 s default (10–600 s), 30 min bash and 30 s LSP handshake vs 30 s default SDK call | **Not generally nested**: inspect plugin explicitly grants server budget + 30 s transport headroom; long bash plugin requests a larger timeout; ordinary 30 s SDK budget does not cover them (`commands/inspect.rs:28-73`; `packages/opencode-plugin/src/tools/inspect.ts:13,347-350`; `packages/aft-bridge/src/command-timeouts.ts:15-27`). |

##### Violations / unresolved risks against §0

- **Global admissions**: process-wide cold-build 2 and warm-reload 4 permits cross target/root boundaries, contrary to per-target/per-connection only; shared executor workers and maintenance cap also serve multiple roots, though per-actor queues remain bounded (`cold_build_limiter.rs:6-17`; `commands/configure.rs:4295-4302`; `executor/mod.rs:118-188,1452-1469`).
- **Caller deadline can be exceeded on bridge retry**: `reloadWaitedMs` counts only scheduled sleeps, not elapsed time in `routeOpen()`; the open has no per-call `timeoutMs` in its options, so multiple slow bind relays can consume more than the caller's budget before `client.request()` receives the remaining timeout. The proven-absent-route retry also sleeps/reopens without decrementing the request budget (`packages/aft-bridge/src/subc-transport.ts:1746-1767,1823-1825,1873-1878,1978-1983`).
- **Relay retry budget is per attempt, not invocation**: gh-shim gives each relayed request a fresh 30 s, then sleeps 5/10 s on transient refusal or remints once; no aggregate caller deadline wraps `exchange` (`crates/aft/src/gh_shim_relay_client.rs:38-48,564-669`).
- **No definite unnamed refusal identified in the examined admission paths**: maintenance backpressure and bind refusal have codes/messages (`crates/aft/src/executor/mod.rs:1452-1469,2555-2558`; `crates/aft/src/subc/mod.rs:6333-6355`); this is not a proof that every AFT refusal is named.

**Not located as a separate knob:** the brief's “45 s request-deadline cap” is the bridge's *reload-wait* ceiling (`subc-transport.ts:234-247`), not a general request-deadline cap: `client.request` receives the requested timeout or SDK default 30 s (`subc-transport.ts:1823-1825`). No independent LSP process-*spawn* timeout was identified; the initialize handshake has the 30 s bound (`lsp/client.rs:455,711-731`).

### 3.4 Cerebellum (CEREB) (inlined from CEREB, not published elsewhere)

Values below are production definitions, not fixture or example settings. Paths are relative to the Cerebellum repository. A *configured* value has no source-fixed numeric default unless one is stated. Milliseconds are elapsed time, not wall-clock expiry.

| Limit / constant or config key | Value | Source | What it bounds |
|---|---:|---|---|
| Browser consent inline park (`BROWSER_CONSENT_PARK_LIMIT` fallback) | 120 s | `crates/cerebellum/src/lib.rs:4324-4326` | Time the current browser tool call waits for an answer before returning pending; the task-local at `lib.rs:4353-4354` is a test override, not a different production value. |
| Computer consent inline park (`CONSENT_PARK_LIMIT`) | 120 s | `crates/cerebellum/src/computer_grants.rs:70-77` | In-call wait before `consent_pending`; question remains open. |
| Browser grant, profile-root rebind, launch-recovery clear (`PHONE_CONSENT_DEADLINE`) | 30 min each | `crates/cerebellum/src/management.rs:74-82` | Each phone-answerable question's own expiry; the three builders pass it at `management.rs:1254,1278,1324`. |
| Login import question (`ELICITATION_DEADLINE`) | 5 min | `crates/cerebellum/src/login_import.rs:23` | Login import consent answer deadline (`login_import.rs:295`). |
| Login capture question (`ELICITATION_DEADLINE`) | 5 min | `crates/cerebellum/src/login_capture/mod.rs:991` | Capture uses the login-import deadline rather than the captured file lifetime. |
| Computer app grant (`APP_QUESTION_DEADLINE`) | 30 min | `crates/cerebellum/src/computer_grants.rs:79-83` | Question expiry, selected for app grants at `computer_grants.rs:1326-1328`. |
| Computer takeover (`TAKEOVER_QUESTION_DEADLINE`) | 5 min | `crates/cerebellum/src/computer_grants.rs:85-90` | Shorter question expiry for handing over the live pointer. |
| Elicitation (`deadline` parameter) | Required per question | `crates/cerebellum/src/elicit.rs:113-125` | Sent question's advertised deadline and answer wait share the same value (`elicit.rs:132-145,186`); not a fleet-wide default. |
| CDP command (`COMMAND_TIMEOUT`) | 30 s | `crates/cerebellum/src/cdp/mod.rs:17` | Pending command correlation lifetime. |
| CDP pending (`MAX_PENDING_COMMANDS`) | 4,096 | `crates/cerebellum/src/cdp/mod.rs:14` | Commands awaiting replies per transport. |
| CDP queued (`MAX_QUEUED_EVENTS`) | 65,536 | `crates/cerebellum/src/cdp/mod.rs:15` | Events buffered per transport. |
| CDP frame (`MAX_MESSAGE_BYTES`) | 67,108,864 bytes | `crates/cerebellum/src/cdp/mod.rs:13` | One decoded message. |
| Launch readiness (`browser.launch_handshake_deadline_ms`) | Configured positive ms | `crates/cerebellum/src/browser_config.rs:14`; `browser_config.rs:364` | Pipe handshake from release to readiness; handshake constructs the elapsed deadline at `browser_launch_readiness.rs:129-133`. |
| Navigation settle (`browser.navigation_settle_deadline_ms`) | Configured ms > 500 | `crates/cerebellum/src/browser_config.rs:371,375-396` | Absolute navigation settlement deadline; a value at or below the quiet window is rejected. |
| Network quiet (`QUIET_MS`) | 500 ms | `crates/cerebellum/src/browser/mod.rs:80` | Silence required after load before a page settles. |
| Navigation settlement (`SETTLEMENT_MS`) | 30 s | `crates/cerebellum/src/browser/mod.rs:79` | Browser settlement constant; configured navigation deadline remains a separate bound. |
| Browser idle (`browser.browser_idle_timeout_ms`) | Configured positive ms; no code default | `crates/cerebellum/src/browser_config.rs:16,366` | Idle lease before expiry. Idle/exit observer sweep sleeps `min(idle, exit observation, 50 ms)` (`lib.rs:1488-1504`). |
| Termination (`browser.termination_grace_period_ms`) | Configured positive ms | `crates/cerebellum/src/browser_config.rs:15,365` | Graceful browser termination; shutdown may shorten it to fit the shared budget. |
| Exit observation (`browser.browser_exit_observation_deadline_ms`) | Configured positive ms | `crates/cerebellum/src/browser_config.rs:17,367-370` | Browser process exit observation after termination. |
| `Browser.close` (`COMMAND_TIMEOUT`, shutdown plan) | 30 s command correlation; at most remaining graceful budget in shutdown | `crates/cerebellum/src/browser/navigation.rs:1743-1758`; `crates/cerebellum/src/cdp/mod.rs:17`; `crates/cerebellum/src/browser_session.rs:2342-2348` | Sends close, does not await a separate close-specific response timeout; teardown's graceful wait ends at its shared deadline. |
| Shutdown total (`SHUTDOWN_TEARDOWN_BUDGET`) | 20 s | `crates/cerebellum/src/browser_session.rs:681-690` | Shared teardown deadline from first drain, EOF or SIGTERM, not one fresh budget per signal (`browser_session.rs:2300-2317`). |
| Shutdown in-flight wait (`SHUTDOWN_IN_FLIGHT_WAIT_MAX`) | 5 s | `crates/cerebellum/src/browser_session.rs:692-695` | Wait for active browser operations before forcing close. |
| Shutdown cleanup (`SHUTDOWN_CLEANUP_RESERVE`) | 5 s | `crates/cerebellum/src/browser_session.rs:697-706` | End of 20 s for kill/reap, journal/profile cleanup and custody release. |
| Shutdown driver-lock retry (`SHUTDOWN_POLL`) | 5 ms | `crates/cerebellum/src/browser_session.rs:708-711` | Poll for driver lock until the *existing* shutdown plan deadline. |
| Shutdown graceful close window (`ShutdownPlan::starting_now`) | Up to first 15 s, including first 5 s in-flight wait | `crates/cerebellum/src/browser_session.rs:741-749` | Graceful work ends before 5 s cleanup reserve; windows are not additive. |
| Health queue age (`DEFAULT_QUEUE_AGE_STALE_MS`, `health.queue_age_stale_ms`) | 30,000 ms default; configurable | `crates/cerebellum-core/src/config.rs:27-28,91-93` | Oldest queued request before health degrades. |
| Health dispatch heartbeat (`DEFAULT_DISPATCH_HEARTBEAT_STALE_MS`, `health.dispatch_heartbeat_stale_ms`) | 2,000 ms default; configurable | `crates/cerebellum-core/src/config.rs:24-25,89-91` | Age of last dispatch heartbeat. |
| Health in-flight (`derive_in_flight_age_stale_ms`, `IN_FLIGHT_MARGIN_MS`) | `max(30,000 ms wait, configured settle, configured handshake, configured termination grace + exit observation) + 5,000 ms`; 35,000 ms with no browser | `crates/cerebellum/src/lib.rs:116-132` | Longest declared execution cap plus scheduling/handshake margin; startup replaces the core placeholder (`lib.rs:1447-1452`). Not a user-configurable health key. |
| Computer wait (`MAX_WAIT_TIMEOUT_MS`) | 30,000 ms | `crates/cerebellum/src/computer_capability/mod.rs:1643` | Maximum requested `computer.wait` timeout; larger requests refused at `computer_capability/mod.rs:1758-1759`. |
| Wait poll (`WAIT_POLL_INTERVAL_MS`) | 50 ms | `crates/cerebellum-computer/src/dispatch/wait.rs:17-23` | Poll spacing for computer.wait predicates. |
| Wait scoped read (`MAX_SCOPED_READ_MS`) | 500 ms | `crates/cerebellum-computer/src/dispatch/wait.rs:25-26` | Maximum time allotted to one scoped AX read in a wait. |
| Action settle (`maximum_accessibility_tree_settle_interval_ms`, `settle_bound`) | Frozen row value, fallback 500 ms | `crates/cerebellum/src/computer_capability/element_actions.rs:46-57` | Post-action readback window. |
| Action settle poll (`SETTLE_POLL`) | 50 ms | `crates/cerebellum/src/computer_capability/element_actions.rs:38-39` | Spacing of effect checks inside the settle window. |
| AX read (`ReadBounds::default().timeout`) | 4 s | `crates/cerebellum-computer/src/app_state/source.rs:120-126` | One accessibility tree read. |
| AX elements (`READ_BOUND_ELEMENTS`) | 4,000 | `crates/cerebellum-computer/src/app_state/source.rs:116-125` | Tree elements visited per read. |
| AX depth (`ReadBounds::default().max_depth`) | 48 | `crates/cerebellum-computer/src/app_state/source.rs:120-126` | Maximum accessibility traversal depth. |
| Element map (`MAX_SESSION_ELEMENTS`) | 8,000 (2 × 4,000) | `crates/cerebellum-computer/src/app_state/session.rs:28-35` | Retained element IDs per session; absent IDs evicted after 3 complete reads (`session.rs:37-40`). |
| Tree output (`MAX_TREE_BYTES`) | 56 KiB | `crates/cerebellum-computer/src/app_state/render.rs:16-18` | Rendered AX tree bytes in a reply. |
| AX label (`MAX_LABEL_CHARS`) | 200 characters | `crates/cerebellum-computer/src/app_state/render.rs:11-14` | One displayed element label. |
| App target/window identity (`PlatformResolver::new().timeout`) | 10 s | `crates/cerebellum/src/management.rs:204-213` | Window-server app resolution read. |
| App code signing (`CODE_SIGNING_BOUND`) | 2 s | `crates/cerebellum/src/management.rs:115-124` | macOS application code-signing read/requirement. |
| Hardware UUID (`this_machine_uuid`) | 2 s | `crates/cerebellum/src/computer_grants.rs:371-382` | macOS IOPlatformUUID read. |
| Browser verifier (`VERIFIER_DEADLINE`) | 5 s | `crates/cerebellum/src/browser_executable_identity.rs:185-194` | Entire macOS codesign verification, shared across file/slice reads. |
| Backup exclusion (`EXCLUSION_CHECK_DEADLINE`) | 5 s | `crates/cerebellum/src/browser_crash_reports.rs:177-185` | One `tmutil isexcluded` check on launch path. |
| Observer heartbeat (`OBSERVER_HEARTBEAT_BOUND_MS`) | 50 ms | `crates/cerebellum-computer/src/dispatch/seize.rs:201-209` | Own marker must reappear in event tap. |
| Dispatch idle (`OBSERVED_DISPATCH_IDLE_MS`) | 1,000 ms | `crates/cerebellum-computer/src/dispatch/seize.rs:211-220` | Minimum observed user idle before posting input. |
| HID lag / tail drain (`HID_IDLE_TABLE_LAG_MS`) | 100 ms | `crates/cerebellum-computer/src/dispatch/seize.rs:222-230` | Observation continues after last post to catch late event echoes (`seize.rs:486-487`). |
| Fronting cap (`MAXIMUM_FRONTING_BRACKET_INTERVAL_MS`) | 100 ms | `crates/cerebellum-computer/src/platform/containment/mod.rs:15` | Maximum bring-forward bracket. |
| Fronting poll (`FRONTING_POLL_MS`) | 2 ms | `crates/cerebellum-computer/src/dispatch/bracket/mod.rs:35-38` | Checks whether foreground/restore took effect. |
| Identity-to-dispatch (`MAXIMUM_IDENTITY_CHECK_TO_DISPATCH_INTERVAL_MS`) | 5 ms | `crates/cerebellum-computer/src/platform/containment/mod.rs:14` | Maximum gap between process identity check and input dispatch. |
| Capture enumeration (`MAXIMUM_WINDOW_REENUMERATION_INTERVAL_MS`) | 500 ms | `crates/cerebellum-computer/src/capture/mod.rs:15-17` | Time before capture must re-enumerate windows. |
| Capture teardown (`MAXIMUM_CAPTURE_TEARDOWN_INTERVAL_MS`) | 1,000 ms | `crates/cerebellum-computer/src/capture/mod.rs:18-19` | Revocation-to-capture teardown interval. |
| Browser drive journal (`BROWSER_DRIVE_JOURNAL_BYTES`) | 8 MiB | `crates/cerebellum/src/lib.rs:2188` | Browser-drive audit retention capacity. |
| Browser drive entry (`BROWSER_DRIVE_JOURNAL_MAX_ENTRY_BYTES`) | 64 KiB | `crates/cerebellum/src/lib.rs:2189` | Largest single browser-drive journal entry. |
| Computer audit `JournalConfig.journal_retention_bytes` | 8 MiB | `crates/cerebellum/src/computer_capability/mod.rs:269-270` | Computer audit retention capacity. |
| Computer audit `JournalConfig.journal_max_entry_bytes` | 64 KiB | `crates/cerebellum/src/computer_capability/mod.rs:271` | Largest computer audit entry. |
| Launch journal (`LAUNCH_JOURNAL_RETENTION_BYTES`) | 64 MiB | `crates/cerebellum/src/browser_launch_service.rs:1063` | Browser launch audit retention capacity. |
| Launch journal `JournalConfig.journal_max_entry_bytes` | 1 MiB | `crates/cerebellum/src/browser_launch_service.rs:1094-1098` | Largest browser launch audit entry. |
| Browser refusal ring (`DEFAULT_REFUSAL_RECORDS_PER_CALLER`) | 64 records/caller | `crates/cerebellum/src/browser/mod.rs:82` | Persisted recent refusals per caller, not a global request limit. |
| Login capture deletion (`FILE_LIFETIME`) | 10 min | `crates/cerebellum/src/login_capture/mod.rs:46-48` | File is scheduled for unlink at its own expiry; scheduler sleeps until then (`login_capture/mod.rs:712-726`). |
| PNG encoding (`MAX_ENCODED_PNG_BYTES`) | 16 MiB | `crates/cerebellum/src/coordinate.rs:12-15` | Canonical base64 screenshot byte length. |
| Module egress (`EGRESS_BUFFER`) | 64 frames | `crates/cerebellum/src/lib.rs:136` | Internal writer channel capacity; not an admission limit on tool calls. |
| Per browser session driver lock (`LD`) | 1 driver/CDP operation at a time per session | `crates/cerebellum/src/browser_session.rs:785-802` | Serializes pipe writes per session; other sessions/routes can proceed. |
| Route tool admission (`tokio::spawn`) | No explicit numeric module-wide cap found | `crates/cerebellum/src/lib.rs:2654-2669` | Each admitted route request gets its own task and in-flight health guard. |

**Nesting and lifetime.** Section 2's 12 s daemon bind relay must surround module bind work; Cerebellum must not put human consent or browser launch in bind. A tool caller intending to receive an answer in-call must set its call timeout *above* the 120 s browser/computer park (the SDK's section 1 default 30 s is **not** enough); a 90 s SDK route-open retry also stops at its caller's earlier deadline. Browser launch handshake, navigation settle, termination plus exit observation and `computer.wait`'s 30 s feed the derived in-flight health threshold with 5 s slack; this is a **health alarm threshold**, not a request cancellation deadline. The 500 ms network quiet must fit strictly within configured navigation settle. A 500 ms scoped computer.wait read and 50 ms poll are inside its 30 s requested maximum; read/dispatch operations need a caller timeout sufficient for their real work. On drain, Cerebellum's single 20 s shutdown budget (first 5 s for in-flight, graceful close by 15 s, last 5 s reserved) fits within the daemon's 25 s module shutdown cap; it starts immediately on drain/EOF/SIGTERM rather than waiting out the daemon's default 30 s drain. The 120 s consent park also does **not** fit within the daemon's default 30 s drain window, so it does not wait for the human on a stop: a drain notice, SIGTERM, route GOODBYE, closed connection or consumer Cancel withdraws the question and releases the parked call at once (browser: `ConsentShutdown` and the drain/route/cancel arms of `serve_browser_with_consent` in `lib.rs`; computer: the same release in `computer_grants.rs`). The consent question's 30 min/5 min expiry deliberately **does not nest inside** the 120 s in-call park: an answer after `consent_pending` can still be applied without holding the original request. The 10 min captured-file lifetime likewise starts at file creation, not at a caller's tool timeout. `Browser.close` is sent without waiting for its separate 30 s CDP reply deadline during teardown; the shared shutdown plan bounds graceful observation.

**Retries and bounded work.** `UNREADABLE_RETRIES = 5`, `UNREADABLE_RETRY_PAUSE = 20 ms` (`crates/cerebellum/src/launch_recovery.rs:430-475`) retries a transient unreadable process argument read up to five additional times *per PID*. It has no caller-deadline check; the finite retry count does not guarantee that retries end before the calling tool's deadline if many PIDs or a slow read exhaust its time. `copy_store_private` tries at most three recopy passes when cookie-store bytes change (`crates/cerebellum/src/login_import.rs:724-744`), also without a caller deadline around synchronous reads/copies. The shutdown driver-lock poll is explicitly capped by its shared plan (`browser_session.rs:708-711,741-749`). Computer effect polling is capped by its settle bound (`computer_capability/element_actions.rs:50-57,621-631`). The computer `dispatch/retry.rs:80-102` is a retry *decision* about whether an attempt may repeat, not a timed automatic retry loop. Route requests are spawned without a module-wide semaphore (`lib.rs:2654-2669`); browser sessions serialize CDP via per-session driver locks (`browser_session.rs:785-802`). `spawn_blocking` for launch, cancellation waits and shutdown is not guarded by an explicit Cerebellum-specific numeric cap (`lib.rs:1385-1389,2563-2565,4547`); shutdown can itself spawn one OS thread per active session (`browser_session.rs:2332-2339`). No global module cap should be inferred from the daemon's per-connection/per-target limits.

**Gaps / non-guarantees.** **The 120 s consent park is fixed and ignores the caller's own deadline.** A caller whose timeout is at or under 120 s (the SDK default is 30 s; a runner's tool timeout may be exactly 120 s) can time out before the park returns `consent_pending`, losing the ask id. Planned fix, same shape as plexus: honour a host-only `block_until` set by the carrier (its own deadline minus a reply margin) and return `consent_pending` by then; absent, keep 120 s. No production default for `browser_idle_timeout_ms` or other required browser deadlines was found: `browser_config.rs:364-371` requires configured positive values; the short config at `lib.rs:2256-2273` explicitly describes itself as for tests and is not a default. No separate `Browser.close` reply timeout was found (`browser/navigation.rs:1743-1758` sends only); its 30 s CDP pending timeout is not a 30 s shutdown wait. No module-wide in-flight-tool limit, queue-length bound for spawned route handlers, or dedicated `spawn_blocking` concurrency limiter was found at the request spawn sites. The per-PID unreadable retry and synchronous cookie-copy retries have no verified caller deadline propagation; it cannot presently be claimed that they always stop before the calling tool's deadline. No guarantee that blocking filesystem cleanup returns within the shutdown reserve was found (`browser_session.rs:2463-2470`): custody stays held if it overruns. The runtime configuration values and caller-provided tool timeouts are deployment/caller dependent, so no universal numeric launch, idle, navigation, termination or exit deadline is asserted here.
