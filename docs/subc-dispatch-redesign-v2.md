# subc-core dispatch redesign v2 — mechanism-complete spec

Status: **v2 — RE-GATE NO-GO (6 NO-GO / 2 GO-WITH-CHANGES, 2026-07-15). Trajectory: 5 of 10
v1 blockers verified CLOSED (B3/B5/B6/B7/B8); B2 NOT-CLOSED (B5↔B2 tension: Delivered-before-
send opens a CANCEL-overtake window); new mechanism blockers N1 (two-lock pop/claim gap →
missing-slot panic), N2 (CANCEL forwarded from read loop can overtake the drain's un-issued
send — must serialize module-bound frames through the drain via Sending sub-state), N3 (RAII
guard + explicit release double-releases; credit must be ONE consuming token owned by the
Slot), N4 (corr-uniqueness unenforced → silent slot overwrite leak), N7 (unimplementable
against shipped types: RouteBinding needs Weak<RouteDispatcher>, ForwardingInner needs a
dispatchers map), plus majority items N5/N6/N8/N9/N11. Council archive:
.cortexkit/alfonso/athena/council-subc-dispatch-redesign-v2-regate-b000c2137b81bba2/.
SPIKE LANDED (b235f5be, crates/subc-core/src/dispatch_spike, test-gated, not wired into live
routing): N1/N2/N3/N4/N7 are now closed IN CODE with fail-first-discriminating tests — atomic
pop+claim (one lock), single-writer RouteDrain owning the route's only FrameSink with
cancel-forwards routed through the drain and a select! keeping them credit-free under a
saturated window (the R5-liveness test proves CANCEL(A) reaches the module while B is still
un-acquired), single consuming CreditToken (loom-modeled), corr-uniqueness enforcement, and
concrete Weak<RouteDispatcher>/registry wiring. 720-ordering exhaustive sync-interleaving
harness asserts exactly-one-terminal + credit conservation. RESIDUAL for the production build
(not spike scope): N5 (synthetic-egress full), N6 (async teardown vs shipped sync Drop +
reload quiescence), N8 (merge-1 closed-recheck on both Bound branches), N9 (route_closing SDK
code), N11 (byte-based memory budgets), merge-0 SDK class + fleet verify, live integration.
NEXT: fold the spike's PROVEN shapes back into this spec as normative text, then re-gate
spec+spike together; the production build decomposes as merge-0 (SDKs) → merge-1 (snapshot)
→ merge-2 (dispatch, replacing the spike's registry with real RouteBinding/ForwardingInner
wiring per N7's mapping comments).** Supersedes the mechanism sections of
`docs/subc-dispatch-redesign.md` (v1, gated NO-GO 8/8). v1's architecture was endorsed; this
doc specifies the load-bearing concurrency mechanism the gate found missing. No code until v2
passes re-gate. Verified at source at master `be293a4c`.

Council v1 archive: `.cortexkit/alfonso/athena/council-subc-dispatch-redesign-e26112ccb9170497/`.
This doc closes v1 blockers B1–B10; each section tags the blocker(s) it resolves.

## 0. The defect (unchanged, confirmed)

The per-connection read loop (`server.rs:348-400`) routes each frame to completion before the
next read; a Request awaits `route.flow.acquire()` (`router.rs:465`) and `module_sink.send()`
(`router.rs:491`) inline, and channel-0 runs inline (`router.rs:214`). A saturated route
therefore blocks the whole connection including the CANCEL that would relieve it — cancellation
structurally defeated on a saturated serial route (LOOP R5). Plus one process-wide `RwLock` per
data-frame lookup (`forwarding.rs:846`, LOOP R1) and the duplicate-terminal concurrent
double-release (LOOP R11, narrowed below).

## 1. The core primitive: `RouteDispatcher` (a route-local serial decision point)

Everything hard in v1 (CANCEL limbo, credit leak, DoS scan, queue data race) comes from TWO
actors — the read loop and a drain task — touching one queue with no single serialization
point. v2 introduces exactly that point.

Per live client-side route binding, one `RouteDispatcher`:

```
struct RouteDispatcher {
    inbox: Mutex<RouteInbox>,        // the ONLY shared mutable state; every op holds it O(1), NEVER across an await
    notify: Notify,                  // drain task wakes on push
    flow: Arc<ChannelFlow>,          // the EXISTING per-route credit sem (unchanged: acquire/forget + CAS release)
    module_sink: FrameSink,          // client->module egress (unchanged sink)
    // identity for synthesized frames (client channel/epoch/ver) + module channel/epoch for forwarding
}

struct RouteInbox {
    admission: Admission,            // Open | Closing | Closed  (B9)
    queue: VecDeque<u64>,            // corrs in FIFO order (B3: explicit primitive, NOT mpsc)
    slots: HashMap<u64, Slot>,       // corr -> Slot; O(1) lookup/remove (B6: no scan)
    outstanding: usize,              // delivered-not-terminated count (mirrors flow.in_flight; for drain-side assertions)
}

struct Slot { frame: Frame, state: SlotState }
enum SlotState { Queued, Claimed { cancelled: bool }, Delivered }
```

`RouteInbox` is a plain mutex over maps/deque; every critical section is O(1) (push, pop-front,
get/remove-by-corr, flag set) and holds NO `.await`. The drain task's blocking awaits
(`flow.acquire`, `module_sink.send`) happen OUTSIDE the lock. This is the standard
async-work-queue-with-cancellation shape; it is the single serialization point the v1 sketch
lacked. (B3 resolved: primitive named; no mpsc/scan incompatibility. B6 resolved: `slots`
gives O(1) CANCEL, no read-loop linear scan.)

## 2. The per-corr state machine (B2, B5 — the limbo + insert-order fixes)

A Request corr moves Queued → Claimed → Delivered → (terminal removes it), or is cancelled at
any pre-Delivered point. ALL transitions are decided under `inbox` lock:

**Read loop, on a data Request frame** (non-blocking, O(1)):
```
lock inbox:
  if admission != Open: unlock; synthesize Error{route_closing} to client; return   // B9
  if queue.len()+outstanding >= depth_cap: unlock; synthesize Error{route_backpressure}; return  // B1/B6 (SDK merge-0 makes this retryable)
  slots.insert(corr, Slot{frame, Queued}); queue.push_back(corr)
unlock; notify.notify_one()
```

**Read loop, on a CANCEL frame** (non-blocking, O(1), the limbo-free decision — B2):
```
lock inbox:
  match slots.get_mut(corr):
    Some(Queued)              => remove from queue+slots; unlock; synthesize Error{cancelled} to client   // never acquired credit
    Some(Claimed{cancelled})  => set cancelled=true; unlock; return    // drain rolls back post-acquire (below)
    Some(Delivered) | None    => unlock; forward CANCEL to module_sink (module has it or will; its terminal releases)
```
Because the read loop and the drain task both make their cancel/deliver decision under the SAME
lock, there is NO limbo window: a corr is exactly one of Queued (read loop owns the cancel),
Claimed (read loop sets the flag, drain observes it under lock post-acquire), or Delivered
(module owns the terminal). The v1 "not in queue = delivered/unknown" false dichotomy is gone.

**Drain task** (owns the blocking awaits; one per route):
```
loop:
  corr = { lock inbox; pop_front queue or None }        // if None -> wait notify (or Closing->drain-then-exit)
  { lock inbox; slots[corr].state = Claimed{cancelled:false} }
  match flow.acquire().await:                            // B4: the ONE credit acquire
    Err(ChannelFlowClosed) => handle_closed(corr); continue     // section 4
    Ok(()) => {}
  // credit acquired (flow.in_flight incremented via forget()). Decide under lock BEFORE send:
  lock inbox:
    take Slot; if state==Claimed{cancelled:true} OR admission==Closed:
        unlock; flow.release(); synthesize Error{cancelled} if route-alive; drop; continue   // rollback (B2)
    else: state=Delivered; outstanding+=1                 // INSERT-before-send (B5): corr is now "delivered" pre-send
  unlock
  match module_sink.send(frame).await:                    // B4: send failure arm
    Ok(()) => {}                                          // terminal will release on module->client path
    Err(_) => { lock inbox: outstanding-=1; remove slot; unlock;
                flow.release();                            // release the just-acquired credit (mirrors router.rs:491-496)
                synthesize Error{backend_error} to client if route-alive }   // preserves shipped Error-frame recovery
```

Credit release stays on the module→client terminal path (unchanged direction), now gated by an
exactly-once check (section 3). Because `outstanding`/Delivered is recorded under the lock
BEFORE `module_sink.send().await`, a fast module terminal cannot arrive-and-fail-to-release: the
corr is already Delivered when the module can first see the frame. (B5 resolved.)

RAII: the credit between `flow.acquire` Ok and either Delivered-commit or rollback-release is
held by a scope guard (`AcquiredCredit` that calls `flow.release()` on drop unless `commit()`ed),
so a panic between acquire and send releases the credit. (B4 panic arm.)

## 3. Exactly-once credit release + the R11 rider (B8 — corrected framing)

CORRECTION to v1: shipped `release()` (`forwarding.rs:1702-1731`) ALREADY has a CAS guard that
ignores over-release when `in_flight==0`. So R11 is NOT "trusted vs enforced" broadly — it is
specifically the CONCURRENT-duplicate case: two terminals for the same corr while `in_flight>0`
each pass the `!=0` guard and each decrement, double-releasing.

v2 fix: the module→client terminal path removes the corr from the route's `slots`/`outstanding`
and releases credit ONLY if the corr was still Delivered:
```
on terminal frame (Response|Error|StreamEnd) for corr, module->client path (router.rs:281-309):
  lock inbox:
    if slots.remove(corr) was Delivered: outstanding-=1; release=true else release=false
  unlock
  forward terminal to client (unchanged)
  if release: flow.release()
```
A duplicate terminal finds the slot already gone → `release=false` → forwarded but credit-inert.
This is a genuine change to the release CALL SITE (v1's I3 "release paths untouched" was FALSE —
corrected). Wire behavior to the client is unchanged (both terminals still forward); only the
second's credit effect is suppressed. (B8 resolved; I3/I7 reworded in section 7.)

## 4. `ChannelFlowClosed` disambiguation (B4 — the drain error arm the test demands)

`flow.acquire()` returns `Err(ChannelFlowClosed)` when the sem was closed (`forwarding.rs:1737`
`close()` → reload drain or teardown). The drain task must disambiguate via a per-route
`teardown: TeardownKind` field set by the mutator that closed the flow:
```
handle_closed(corr):
  lock inbox: take slot; kind = self.teardown
  match kind:
    Reloading  => synthesize Error{module_reloading} to client for corr   // matches shipped module_reloading vocab (forwarding.rs:1068)
    Goodbye    => drop silently (client already settled locally on its GOODBYE)
    ConnClose  => drop silently (connection going away)
    None       => // sem closed without a teardown reason: treat as backend_error (defensive)
                  synthesize Error{backend_error} to client for corr
```
The existing test `blocked_flow_control_acquire_wakes_when_module_tears_down`
(`forwarding.rs:~3811`) demands a `backend_error` terminal when a blocked acquire wakes on module
teardown — v2 maps the module-death case (endpoint gone, not a graceful reload/goodbye) to the
`None`/backend_error arm, preserving that test. (B4 resolved; T8 preserved rather than edited.)

## 5. Teardown atomicity — Open/Closing/Closed admission (B9)

Route teardown (GOODBYE, endpoint drain, connection close) is a 3-phase gate, NOT a flush-then-
release:
```
1. lock inbox: admission = Closing            // no NEW enqueues admitted (read loop checks admission, section 2)
2. flow.close() with teardown=<reason>        // wakes any blocked drain acquire into handle_closed
3. cancel_token.cancel()                       // drain task's awaits are select!'d against this token
4. bounded-join the drain task (e.g. 2s); if it doesn't exit, abort() the JoinHandle   // JoinHandle drop != abort
5. lock inbox: admission = Closed; drain queued corrs -> synthesize cancelled/drop per reason
6. epoch-fenced release of the binding (existing path, forwarding.rs:1409-1470) — UNCHANGED predicate
```
Key ordering: admission=Closing (step 1) happens-before the snapshot that removes the binding
(step 6), so a lock-free reader that still sees `Bound` and calls the dispatcher's read-loop push
hits `admission != Open` and gets `route_closing` instead of delivering to a released module —
this closes the v1 "try_push after flush" hole (the admission check, not the flush, is the
barrier). The drain task's `module_sink.send` and `flow.acquire` are `select!`'d against
`cancel_token` so a blocked drain cannot hang connection close (step 4 bounded-join + abort).
The JoinHandle is owned by the teardown path (connection/forwarding table), NOT by the binding,
avoiding the `binding→handle→task→binding` strong cycle. (B9 resolved.)

## 6. Control-plane offload — CLIENT-side only, module publication stays inline (B7)

v1's "whole channel-0 FIFO" was WRONG: it would let a module's bind-ACK (route.open response, a
channel-0 frame on the MODULE connection) be reordered behind the module's immediately-following
reverse-request data frame → dropped as Reserved/Absent, breaking the shipped test
`accepted_route_publishes_route_open_before_immediate_reverse_request` (`router.rs:1078-1102`)
and the wire-v1-final:123-165 barrier.

v2: the ONLY blocking channel-0 op is `route.open` (relays route.bind, awaits the module ack up
to 12s). Offload is scoped to the CLIENT connection's channel-0: a per-client-connection FIFO
control task drains channel-0 frames so a client's `route.open` bind-wait no longer stalls that
client's data frames + CANCELs. MODULE connections are NOT split — their frames (HELLO, bind
acks, catalog.update, data) stay on the single inline read path, preserving the
bind-commit-before-next-frame barrier by construction. Cross-connection: the client's route.open
task awaits the module's ack, and the module's ack + route publication commit happen inline on
the module connection's own read path (unchanged), so publication still happens-before any module
data frame on the new route. Per-client-connection FIFO preserves route.open→route.close ordering.
Control-queue overflow → `Error{control_backpressure}` (retryable, SDK merge-0). (B7 resolved;
Q3 lean corrected: client-side-only offload, never module-side.)

## 7. Snapshot-published forwarding — publish UNDER the write lock (B10)

Data-plane lookups (`lookup_data_route`, `forwarding.rs:846`) move to `ArcSwap<Snapshot>` reads.
The v1 hazard: `ArcSwap::load()` can observe a snapshot published BEFORE a release, then use a
stale `Bound` route → routes into a closed flow (new `backend_error` observable vs today's
`unknown_channel`).

v2 constraints making merge-1 landable standalone:
- The snapshot is rebuilt-and-published INSIDE the existing write-lock critical section for every
  mutation (bind/release/register/cleanup/drain), so publication is serialized with the canonical
  mutation (no publish-after-unlock window).
- Each `RouteBinding` gains `closed: AtomicBool`, set true under the write lock at release BEFORE
  the snapshot that removes it is published. Every data-plane consumer of a `Bound` route
  (`router.rs:281-309` module→client, and the dispatcher push) checks `closed` after loading and
  treats closed-but-still-visible as `unknown_channel` — restoring today's post-release observable
  exactly (no new `backend_error` state). The dispatcher admission=Closing (section 5) is the
  client→module equivalent.
- Read-your-writes: control-plane reads (catalog/status/liveness) STAY on the `RwLock` (not hot,
  want RYW). Only the two hot data-plane lookups move to the snapshot.
- I3/I7 reworded: I3 → "epoch-fence + escalation predicates preserved; the release CALL SITE gains
  an exactly-once `outstanding` gate (R11)." I7 → "module→client wire behavior unchanged;
  duplicate/late-terminal credit accounting intentionally fixed; a `closed` recheck is added to
  the stale-Bound path." I4 → "GOODBYE now flushes queued client→module frames (a raw-wire
  semantic change; SDK clients settle locally first so unaffected)."
(B10 resolved: publish-under-lock + closed-flag makes merge-1 invariant-neutral; the ONE new
field is inert until read.)

## 8. Prerequisite SDK merge-0 (B1 — lands BEFORE any daemon merge)

v1's "zero SDK changes" was FALSE (verified at source): a data-plane `Error{route_backpressure}`
classifies as `outcome_unknown` in TS (`client.ts:781-792,807` — daemon read the frame ⇒
`handedToSocket=true` ⇒ `outcome_unknown` ⇒ managed `call()` RECONNECTS) and `CallError::Module`
in Rust (`consumer.rs:570-579`); the only retryable-code classifier is the closed channel-0
route.open set (`client.ts:1252-1259`, `consumer.rs:3130-3135`). Swift has no data-plane
classifier.

merge-0 (ships and is deployed to consumers BEFORE the daemon dispatch merge):
- Add a data-plane retryable error-code class `{route_backpressure, control_backpressure}` →
  retry-in-place with bounded backoff (NOT reconnect, NOT route eviction) in TS `classifyFailure`
  + managed `call()`, Rust `SubcConsumer::call`, and the Swift client. Parity tests across all
  three (a backpressure Error on a data request → in-place retry, succeeds on a later attempt;
  never reconnects, never evicts the route).
- Until merge-0 is deployed fleet-wide, the daemon MUST NOT emit these codes. Bridge: the daemon
  dispatch merge is gated on merge-0 being live in broca/aft/alfonso-core (verify in their repos —
  NOT in this checkout). Interim safety valve if sequencing slips: per-route bounded BLOCKING
  admission (drain-side, not read-loop) instead of fail-loud — preserves today's backpressure-by-
  wait semantics without the HOL (the wait is in the drain task, off the read loop). Decide at
  re-gate whether to ship the blocking-admission interim or hard-gate on merge-0.

## 9. Invariants (rewritten — the honest deltas)

I1  Per-route Request FIFO preserved (single drain pops in queue order). Cross-route unordered
    (always was).
I2  Credit: acquire exactly-once per delivered Request (drain, in order); release exactly-once
    per terminal via the `outstanding`/`slots` gate (section 3). Window sizes unchanged.
I3  (DELTA) Epoch-fence + escalation predicates preserved; release call site gains the
    exactly-once gate. NOT "untouched."
I4  (DELTA) GOODBYE flushes queued client→module frames (raw-wire semantic change; SDKs settle
    locally first). Relay-to-module + epoch-fence unchanged.
I5  Zero-deserialization preserved: synthesized terminals (cancelled/route_backpressure/
    control_backpressure/module_reloading/backend_error) are all existing `to_error_frame`
    canonical bodies (`router.rs:617`), envelope + code only, no body parse.
I6  BufReader cancel-safety preserved: read loop still cancelled only at connection close; the
    new per-frame hand-off (dispatcher push / control enqueue) is non-blocking O(1), no new await
    between read and hand-off.
I7  (DELTA) module→client wire behavior unchanged; duplicate/late-terminal credit fixed; a
    `closed` recheck added to the stale-Bound path.
I8  Wire: no new frame types/header/bump. New CODES only: route_backpressure, control_backpressure
    (+ reuse cancelled, module_reloading, backend_error). Consumed by merge-0.

## 10. Test plan (re-gate + build gate)

Carries v1 T1–T9, PLUS the blocker-closing tests:
T10 CANCEL limbo: cancel a corr in EACH state (Queued, Claimed-during-acquire, Delivered) →
    exactly one terminal each (synthetic cancelled / rolled-back cancelled / module terminal),
    never two, never zero. Drive the Claimed race with a paused acquire.
T11 Insert-order: fast stub emits terminal immediately on receive; assert credit releases exactly
    once (window returns to full) — proves outstanding-before-send.
T12 Drain error arms: (a) module_sink.send fails → backend_error to client + credit released;
    (b) flow closed via reload → module_reloading; via GOODBYE → silent drop; via conn-close →
    silent; (c) module death mid-block → backend_error (preserves the shipped
    blocked_flow_control_acquire test).
T13 Teardown: GOODBYE with a full queue + a drain blocked on send → admission Closing rejects new
    pushes (route_closing), cancel-token interrupts the blocked send, bounded-join exits, no
    credit leak, no connection-close hang, no task leak.
T14 Bind barrier (B7 regression): the shipped accepted_route_publishes_route_open_before_immediate
    _reverse_request MUST stay green with control offload active; add a module that sends bind-ack
    then an immediate reverse request under load → reverse frame delivered, not dropped.
T15 R11 concurrent duplicate: stub emits TWO terminals for one corr while in_flight>0 → single
    release (window never exceeds cap), both forwarded.
T16 Snapshot stale-read: release a route, then a data frame that loaded the pre-release snapshot →
    `unknown_channel` (NOT backend_error), proving the closed-flag recheck. Read-your-writes on
    control-plane reads.
T17 merge-0 SDK parity: backpressure Error on a data request → in-place retry (all 3 SDKs), never
    reconnect/evict.
T8  Existing suites green UNMODIFIED (any needed edit is a re-review flag).
T9  Perf before/after (Ufuk): loopback throughput + p99, single-serial / 32-window / multi-route
    mixed; lock contention via counters + a snapshot-publish counter.

## 11. Rollout (revised)

Three ordered, separately-gated merges:
- **merge-0 (SDKs):** data-plane retryable class in TS+Rust+Swift + parity tests. Deploy to
  consumers. NO daemon change. (B1 prerequisite.)
- **merge-1 (snapshot forwarding):** ArcSwap publish-under-lock + `closed` flag + recheck.
  Invariant-neutral per section 7; standalone-landable. (B10.)
- **merge-2 (dispatch):** RouteDispatcher + state machine + drain error arms + teardown gate +
  client-side control offload + R11 gate. (B2–B9.)
Each full-gate + its new tests; prod deploy at an explicit window, daemon-first order. Prod stays
on v0.3.0 until then.

## 12. Open questions for the re-gate

Q1' merge-0 hard-gate vs the blocking-admission interim (section 8) — ship interim to decouple
    daemon merge from SDK deploy timing, or hard-gate? Lean: hard-gate (cleaner; consumers are
    our own and fast to bump), interim only if a consumer bump stalls.
Q2' teardown bounded-join timeout (2s?) and whether abort-after-timeout can drop an in-flight
    terminal (acceptable: connection is closing).
Q3' depth caps: keep max(4,2×window)? Byte-based secondary cap per route + per connection to
    close the frame-count-only memory-DoS the v1 gate flagged (large frames × deep queue).
Q4' `slots` HashMap vs a slab/index — corr-uniqueness assumption: a client reusing an in-flight
    corr on one route is a protocol violation; enforce (reject duplicate-corr enqueue as a
    protocol error) rather than silently overwrite a slot (which would leak the overwritten
    corr's credit). Lean: enforce at enqueue.
Q5' snapshot publish granularity: whole-table Arc rebuild per mutation vs per-endpoint shard —
    measure in T9 before optimizing; lean whole-table (mutations rare vs lookups).
