# subc-core dispatch redesign — non-blocking read loop, per-route dispatch, snapshot-published forwarding

Status: DESIGN — Athena gate pending. No code until the gate passes.
Motivation: LOOP escalations R1 + R5 (+R11 rider), 2026-07-15. Verified at source at master `9b995560`.

## 1. The defect class (verified at source)

The per-connection read loop (`server.rs:348-400 connection_loop`) reads one frame, then
**awaits its full routing to completion** (`router.route_for_connection`) before reading the
next frame. Anything that routing awaits therefore blocks the ENTIRE connection — all other
channels, and every frame behind the blocked one in the socket. Three blocking awaits exist
inside that routing path today:

- **B1 — route credit** (`router.rs:465 route.flow.acquire().await`): a Request on a route
  whose flow-control window is fully in-flight blocks until a module terminal frees a credit.
  Serial modules have window=1. Consequence (LOOP R5, confidence 0.99): a client that
  pipelines request B behind long-running request A on a saturated route cannot CANCEL A —
  the CANCEL frame sits unread in the socket behind B's blocked acquire, and A's credit only
  frees when A completes on its own. **Cancellation is structurally defeated exactly when it
  is most needed.** Independent channels on the connection stall too.
- **B2 — module ingress** (`router.rs:491 route.module_sink.send(frame).await`): a full module
  ingress queue blocks the client read loop the same way.
- **B3 — inline control** (`router.rs:214 handle_control_frame(ctx, frame).await`): channel-0
  commands run inline; `route.open` relays route.bind and awaits the module ack (bind timeout
  12s). A route.open against a warming/busy module blocks every data frame and every CANCEL
  on that connection for up to 12s. Same class, control-plane instance.

Additionally (LOOP R1): every data-frame lookup takes one process-wide `std::sync::RwLock`
(`forwarding.rs:846 lookup_data_route` via `read_inner`), both directions, with write-side
cleanup holding the same lock across whole-store scans. This is the throughput ceiling and
was a consciously-accepted tradeoff of the forwarding refactor (`2859b8b5`); this redesign
retires it.

Related documented intent being superseded: `server.rs:172-178` ("intentionally keeping
inbound dispatch serial"), memory #7022 (backpressure-by-serialization). Both described the
v1 simplicity choice; this design replaces serialization-as-backpressure with explicit
bounded queues and fail-loud admission.

## 2. Goals / non-goals

GOALS
1. The read loop never blocks on per-route or per-command work: CANCEL, Ping, GOODBYE and
   frames for other channels are always read and acted on promptly, regardless of any one
   route's saturation.
2. Preserve at-most-once forwarding, per-route credit accounting (window sizes unchanged:
   Serial=1, ModuleManaged=32, StatelessParallel=1024), epoch-fenced release, and the
   GOODBYE/teardown semantics exactly as shipped.
3. Data-plane route lookup becomes lock-free for readers (snapshot-published table);
   binds/releases/cleanup stay strictly serialized on the write side.
4. Explicit, bounded memory per connection and per route; overload fails loud (typed,
   retryable) rather than wedging silently.
5. Zero wire changes. Zero SDK changes required (SDK cancelled-terminal repair from the LOOP
   regression fix is assumed landed — the design leans on "module emits a terminal for every
   delivered Request, including cancelled ones").

NON-GOALS
- No request shedding / priority scheduling beyond the existing flags (negative space of the
  wire-v1-final spec holds).
- No change to writer-side egress (bounded FrameSink + drain task + try_send best-effort for
  module→client data is already correct).
- No cross-connection fairness work (per-connection loops are already independent tasks).

## 3. Design

### 3.1 Read loop = read, classify, hand off (never await route work)

`connection_loop` becomes:

```
loop {
  frame = select { close => exit, read_frame(buf_reader) }   // unchanged, BufReader preserved
  match classify(frame):
    Channel0        => control_queue.push(frame)              // 3.4
    DataFrame(ch)   => dispatch.route_frame(frame)            // 3.2 — sync, non-blocking
}
```

`route_frame` performs the snapshot lookup (3.3, lock-free) and enqueues into the per-route
dispatch queue (3.2). Its only await-free failure mode is queue-overflow admission (3.5).
The BufReader cancel-safety invariant (R4 Oracle) is preserved: the only cancellation of an
in-progress read remains connection close, and there is still no await between a completed
read and hand-off other than the bounded admission path, which never suspends (try_push).

### 3.2 Per-route dispatch queues, one drain task per live route

Per live (client-side) route binding: a bounded FIFO `dispatch_queue` (depth: 3.5) plus one
`drain_task` owning ALL awaits that used to live in the read loop:

```
drain_task(route):
  while let Some(frame) = queue.recv():
    match frame.ty:
      Request  => { flow.acquire().await; module_sink.send(frame).await; }   // B1+B2 moved here
      other    => { module_sink.send(frame).await; }                          // non-Request: no credit (unchanged)
```

Credit release stays where it is today (module→client terminal forwarding path releases the
route credit; that path was and remains non-blocking try_send + release). Exactly-once
acquire/release accounting therefore moves intact: acquire happens exactly once per Request,
in queue order, in one task; release happens exactly once per terminal (see 3.7 for the R11
rider that makes "exactly once" enforced rather than trusted).

Module→client direction is unchanged (it was already non-blocking: lookup + try_send +
credit release). Only the client→module direction gains queues, because that is the only
direction that blocks today.

Drain tasks are spawned at bind commit and torn down at release (3.6). One task per live
route bounds task count by live-route count (already bounded by channel space per connection).

### 3.3 CANCEL semantics under queueing (the one real semantic decision)

Today CANCEL forwards immediately (no credit) — but only if the read loop is unblocked. With
queues, a CANCEL whose target Request is STILL QUEUED (not yet delivered to the module) must
not simply overtake it: the module drops CANCEL for unknown corrs (`handle_cancel` no-ops),
so overtake = cancel lost, request later runs anyway.

Rule: **CANCEL inspects the route's dispatch queue first.**
- Target Request still queued → remove it from the queue and have the DAEMON synthesize the
  terminal `Error{code:"cancelled"}` to the client for that corr (canonical error frames from
  the daemon already exist: `RouterError::to_error_frame`; this adds no body-parsing — the
  daemon knows ty/channel/epoch/corr from the envelope alone). No credit was acquired for a
  queued Request, so no release. The module never sees either frame.
- Target not in queue (already delivered, or unknown) → forward CANCEL to the module
  unchanged; the module emits the cancelled terminal (SDK repair guarantees this), which
  releases credit on the way back. Exactly-once terminals hold: the queued case has exactly
  the daemon's synthetic terminal; the delivered case has exactly the module's.
- GOODBYE for a route: flush its queue (drop queued frames — the client has already settled
  locally, matching shipped GOODBYE semantics), then proceed with today's epoch-fenced
  release + relay. Queue flush must precede binding release so no frame can enqueue after
  flush (both run under the route-teardown path in the drain task's shutdown, 3.6).

### 3.4 Channel-0 control offload (B3)

Per connection: one bounded control queue + one control drain task executing commands in
FIFO order (per-connection control ORDER is preserved — clients await each control response
before depending on it, and the SDKs single-flight route.open, so FIFO per connection is
sufficient). The read loop enqueues; the control task runs `handle_control_frame` and sends
responses through the existing egress. A slow route.open now stalls only later CONTROL
commands on that connection, never data frames or CANCELs. (Parallel control execution is
rejected: route.open → route.close orderings on the same connection must not reorder.)
Control queue overflow: fail-loud typed error frame `control_backpressure` (retryable) —
control commands are low-volume; a full queue signals a broken client.

### 3.5 Bounded admission — replacing backpressure-by-serialization

The old implicit bound (read loop blocks → socket fills → client TCP-backpressured) is
replaced explicitly:

- Per-route dispatch queue depth: `max(4, 2×window)` frames (Serial=4, ModuleManaged=64,
  StatelessParallel=2048). Rationale: enough to keep the pipe full across credit turnaround,
  small enough that per-route memory is bounded by depth × max frame size in the worst case;
  typical frames are far below cap and the queue holds refs to already-read frames (no new
  copies).
- On full queue (Request): the daemon synthesizes `Error{code:"route_backpressure"}`
  (retryable, new canonical code) for that corr immediately. Fail-loud beats both silent
  wedge (today's behavior) and unbounded buffering. SDK classification: retryable-in-place,
  maps to the existing NotSent contract (the request never reached the module).
- On full queue (non-Request: CANCEL/GOODBYE): these must never be dropped for capacity —
  they are queue-INSPECTING (3.3) or queue-FLUSHING (GOODBYE) operations executed by the
  read loop against the queue structure itself (O(queue) scan, no await), not enqueued
  behind it. Ping/Pong stay on the read loop path as today (cheap, no route).
- Per-connection aggregate cap: sum of queued frames per connection capped (e.g. 4096
  frames); overflow → connection-level protocol-error close (a client that floods past every
  per-route bound is broken; closing is the existing escalation vocabulary).

### 3.6 Route/task lifecycle

- Bind commit (existing forwarding write-lock path) additionally: create queue + spawn drain
  task, publish new snapshot (3.8).
- Release (GOODBYE / teardown / endpoint drain): flush queue, stop drain task (drop queue
  sender; task exits when drained), then existing release path. Drain-task panic backstop:
  a panicking drain task must release the route (abort-guard mirroring the coordinator-actor
  drop-guard pattern from broca) — a dead drain task with a live binding would recreate the
  silent wedge this design kills.
- Connection close: existing teardown already releases all routes; that now also tears down
  all drain tasks. No orphan tasks: task handles owned by the binding entry.

### 3.7 R11 rider — enforced exactly-once credit release (fold-in, cheap here)

LOOP R11: duplicate module terminals double-release credit (aggregate counter can exceed
window; in_flight undercount). With per-route structure in place, the module→client terminal
path gains a per-route `outstanding: HashSet<corr>` (inserted on delivery to module by the
drain task, removed-once on terminal): release fires only if `outstanding.remove(corr)`
returned true. Cost: one hash op per Request delivery + one per terminal, confined to the
route's own state (no global lock — unlike the pre-redesign fix that made this escalation-
expensive). This retires R11 as a rider instead of a separate design. The "trusted module"
doctrine (release comment) is preserved as documentation of intent; the set makes it
enforced rather than trusted.

### 3.8 Snapshot-published forwarding table (R1)

Data-plane lookups (`lookup_data_route`, both directions) move to a lock-free snapshot read:

- `ArcSwap<ForwardingSnapshot>` (or equivalent Arc swap + atomic load) holding the immutable
  route maps needed by the data plane: `client_to_module`, `module_to_client`,
  `endpoint_by_connection`, reserved/slot-epoch marks — exactly the fields `lookup_data_route`
  touches today (forwarding.rs:840-880).
- All mutations (bind commit, release, register/cleanup, endpoint drain) keep the existing
  write lock as the serialization point, apply to the canonical state, then publish a fresh
  snapshot (clone-on-write of the affected maps; binds/releases are low-frequency vs
  per-frame lookups — read-mostly by orders of magnitude).
- Read-side change is mechanical: `read_inner()?` → `snapshot.load()` in the data-plane
  lookups ONLY. Control-plane reads (catalog, status, liveness) stay on the lock — they are
  not hot and want read-your-writes.
- Consistency argument: a data frame that loads a snapshot published before its route's
  release can still enqueue into a queue that is being flushed — the flush-then-release
  ordering (3.6) plus the queue-sender drop makes late enqueues fail (sender closed), which
  maps to today's channel-gone drop semantics. A frame that loads a snapshot before a bind
  commit sees Absent — identical to today's pre-commit window. No new interleaves: the
  write-lock serialization of mutations is unchanged; only reader visibility latency changes
  (bounded by publish-on-commit).

## 4. Invariants preserved (checklist for the gate)

I1  At-most-once delivery to module per Request; queue is FIFO per route; no reordering
    within a route's Requests. (Cross-route ordering was never guaranteed.)
I2  Credit: acquire exactly-once per delivered Request (drain task, in order); release
    exactly-once per terminal (3.7 enforced). Window sizes unchanged.
I3  Epoch-fenced release + escalation semantics byte-identical (release paths untouched).
I4  GOODBYE: flush-then-release; late frames drop; relay-to-module unchanged.
I5  Zero-deserialization: daemon still never parses bodies. Synthetic terminals
    (cancelled/route_backpressure) are envelope-only + canonical error body, an existing
    daemon vocabulary (RouterError::to_error_frame).
I6  BufReader cancel-safety: read loop still cancelled only at connection close.
I7  Module→client direction unchanged (try_send best-effort + escalation).
I8  Wire: no new frame types, no header changes, no protocol bump. New error CODES only
    (canonical JSON error bodies): "cancelled" (daemon-synthesized case), "route_backpressure",
    "control_backpressure".

## 5. What changes for consumers (SDKs)

- Nothing required. New retryable codes ride the existing error-classification paths
  (`route_backpressure` joins the retryable set in both SDK classifiers — additive config).
- Semantic improvement consumers get for free: CANCEL works under saturation; independent
  channels no longer stall behind a saturated sibling; route.open no longer stalls the
  data plane.

## 6. Test plan (gate-relevant, all against real daemon)

T1  THE R5 regression test: serial route, A in-flight (handler parked), B pipelined, CANCEL(A)
    → A's cancel reaches the module while B queued; A terminal (cancelled) arrives; B then
    delivers and completes. Fails against today's daemon (CANCEL unread) — fail-first proof.
T2  Independent-channel progress: saturate route X (window full + queue non-empty), drive
    request/response on route Y same connection → Y latency unaffected (structural assert:
    Y completes while X still saturated).
T3  CANCEL-overtake: CANCEL for a still-queued Request → daemon-synthesized cancelled
    terminal, module never receives the Request (module-side assert), credit accounting
    balanced (route usable at full window after).
T4  GOODBYE flush: saturated queue + GOODBYE → queue flushed, release clean, no leaked
    credit, drain task exited (no task leak — assert via task handle).
T5  Backpressure fail-loud: flood one route past queue depth → route_backpressure errors for
    overflow corrs, in-window requests unaffected, no memory growth beyond caps.
T6  route.open-under-warm-module no longer stalls data plane (control offload): bind ack
    delayed 5s; concurrent data frames on other routes complete meanwhile.
T7  R11 rider: duplicate terminal from a misbehaving module (stub) → single release
    (window never exceeds cap), second terminal forwarded but credit-inert.
T8  Existing suites green unmodified: HOL isolation, flow-control, epoch-fence, reload-drain,
    concurrency races (they encode the invariants; any needed test change is a red flag to
    re-review, not to edit the test).
T9  Perf evidence (Ufuk requirement): loopback throughput + p99 latency before/after on the
    counters build — single-route serial, 32-window concurrent, and multi-route mixed. Lock
    contention measured via the existing drop/route counters + a snapshot-publish counter.

## 7. Rollout

Feature-flag-free single cutover on master (pre-release product, no coexistence per house
rule), but staged in two reviewable merges: (1) snapshot-published forwarding (R1) — read
path mechanical, invariant-neutral; (2) dispatch queues + control offload (R5/B1-B3 + R11
rider). Each merge full-gate + the new tests; prod deploy only at the next explicit deploy
window with the usual daemon-first order.

## 8. Open questions for the gate (with leans)

Q1  Queue-full Request policy: synthesize route_backpressure (lean) vs block the read loop
    only for that route's frames via a per-route pause-set (more faithful to old semantics,
    more complex, partially reintroduces HOL for interleaved same-route frames). Lean:
    fail-loud; the SDKs already classify retryables.
Q2  Daemon-synthesized cancelled terminal for queued-Request-cancel (lean: yes, 3.3) vs
    forward-both and require SDK tombstones for early CANCEL (rejected: adds unknown-corr
    state to every module = DoS surface).
Q3  Control offload scope: whole channel-0 FIFO task (lean) vs offload only route.open.
Q4  R11 rider now (lean: yes, cheap here) vs keep trusted-module doctrine.
Q5  Snapshot publish granularity: whole-table Arc swap per mutation (lean; mutations are
    rare) vs per-shard maps (only if publish cost measures hot in T9).
