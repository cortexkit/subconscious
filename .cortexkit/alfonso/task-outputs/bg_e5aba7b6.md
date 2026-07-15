---
task_id: bg_e5aba7b6
agent: council-member
session_id: ses_09b6049f6ffeigis2csFJQJ5B7
parent_session_id: ses_09b61906bffeZMC8Uc32l89Pqb
status: completed
completed_at: 2026-07-15T07:29:26Z
---

[assistant] 2026-07-15T07:16:21Z

[assistant] 2026-07-15T07:16:24Z

[assistant] 2026-07-15T07:16:28Z

[assistant] 2026-07-15T07:16:30Z

[assistant] 2026-07-15T07:16:33Z

[assistant] 2026-07-15T07:16:36Z

[assistant] 2026-07-15T07:16:39Z
Swift client: each `routeRequest` runs a per-call while-loop. It does NOT have a "settled pending map" — it just consumes inbound frames in its own loop, matching by corr. The "settle" happens implicitly when the function returns/throws. A late daemon-synthesized `cancelled` terminal arriving after the call returned would just be consumed by the next `nextIngressFrame()` call (in `controlRequest` or the next `routeRequest`) — actually, it's per-conn, so it would get filtered by `frameKey == key` in `routeRequest` because the key is per-corr. But if the late terminal comes for a NEW request with a new corr but a used corr, would it get confused? Each call has its own `key = InFlightKey(channel, epoch, corr)` and discards frames where `frameKey != key` via `continue`. So a late terminal is silently consumed by the loop's `default: continue` branch and the call keeps waiting. This is a LEAK — the call could wait forever. But the `routeRequest` only uses its corr (allocated via `allocateCorr()`) and the frame is rejected if `frameKey != key`. So if the cancelled terminal has a different corr, it's silently skipped; if it has the SAME corr as a CURRENT call, the call throws via the `.error` case. So the late cancelled only matters if the SDK's "client cancel" path already settled (threw / returned) — in which case the per-call coroutine has exited and the next call's loop will see the late frame and skip it.

Wait — in Swift, there's only ONE ingress readloop for the connection (`nextIngressFrame` is called per-control, not per-read). Let me look at how the connection is multiplexed.

[assistant] 2026-07-15T07:16:49Z

[assistant] 2026-07-15T07:16:52Z
This is a SYNCHRONOUS client — it calls `nextIngressFrame` and blocks the current task waiting for the corr's response. There's no concurrent multiplexing here. That means the Swift client is single-threaded blocking; only one in-flight request at a time per Client instance. So a late terminal coming in is fine — it'll be silently dropped by `continue` if the call has already returned.

Now let me look at how TS client handles cancel.

[assistant] 2026-07-15T07:16:55Z

[assistant] 2026-07-15T07:16:57Z

[assistant] 2026-07-15T07:16:59Z

[assistant] 2026-07-15T07:17:02Z

[assistant] 2026-07-15T07:17:06Z
Good. Now I need to understand the existing flow better — let me check if `not_sent` actually corresponds to the request never leaving the socket vs never reaching the module. From the comment, "not_sent" is "request bytes provably never left the local process". This is critical for the new `route_backpressure` claim.

Let me check the `notSentCallError` to see what conditions trigger it.

[assistant] 2026-07-15T07:17:10Z

[assistant] 2026-07-15T07:17:13Z

[assistant] 2026-07-15T07:17:15Z

[assistant] 2026-07-15T07:17:18Z

[assistant] 2026-07-15T07:17:21Z

[assistant] 2026-07-15T07:17:24Z
Now let me look at the existing concurrency tests and look at the design intent claim that "any needed test change is a red flag".

[assistant] 2026-07-15T07:17:27Z

[assistant] 2026-07-15T07:17:29Z

[assistant] 2026-07-15T07:17:31Z

[assistant] 2026-07-15T07:17:35Z

[assistant] 2026-07-15T07:17:39Z

[assistant] 2026-07-15T07:17:42Z

[assistant] 2026-07-15T07:17:44Z

[assistant] 2026-07-15T07:17:47Z

[assistant] 2026-07-15T07:17:50Z

[assistant] 2026-07-15T07:17:52Z

[assistant] 2026-07-15T07:17:55Z
This is the canary test for cancel behavior. Critically, it relies on the module emitting a "cancelled" terminal. The design says: "SDK cancelled-terminal repair from the LOOP regression fix is assumed landed — the design leans on 'module emits a terminal for every delivered Request, including cancelled ones'".

Let me check the fake-aft-stub to confirm.

[assistant] 2026-07-15T07:18:00Z
The fake-aft-stub emits a `cancelled` Error terminal when a Request is cancelled AND the stub has been cancelled. So the design's "module emits a terminal for every delivered Request, including cancelled ones" assumption holds for this stub. But what about real modules like `aft`?

[assistant] 2026-07-15T07:18:04Z

[assistant] 2026-07-15T07:18:06Z
Note: the fake-aft-stub's CANCEL handler `in_flight.remove(&key)` is what claims the cancel. If the request is "in_flight" (delivered to the stub), it sends `cancel_tx.send(())` to signal the request handler. If the request was NOT yet delivered to the stub (i.e., it's still queued at the daemon, awaiting dispatch), then `in_flight.remove` returns `None`, no terminal is emitted by the module. **This is the scenario where the new design's "daemon-synthesized cancelled terminal" is REQUIRED** — the request never reached the module, so the module can't emit a cancelled terminal for it.

Critically, this is already a race that exists TODAY. In current code, if a Request is still in the read loop (i.e., not yet awaiting `flow.acquire`), and a CANCEL arrives... wait, in current code, a single read loop processes frames one at a time. So the only way CANCEL can race with a Request that hasn't been delivered to the module is if the Request is currently being awaited for `flow.acquire()`. In that case, the CANCEL sits behind the read loop and isn't processed. Hence the existing bug (R5). The fix proposed: queue Requests, then CANCEL can inspect the queue.

But there's a NEW race the design introduces: a Request is in the queue, then gets DELIVERED to the module (acquire + send complete), then CANCEL arrives. In that case, the drain task already moved the Request from "in queue" to "delivered". The CANCEL handler in the read loop needs to check "is it in queue OR in outstanding?" If the drain task inserts into `outstanding` on delivery, then the CANCEL handler can check both: not in queue AND not in outstanding → already gone OR unknown; in queue → daemon-synthesize; in outstanding (not in queue) → forward to module.

But what if the drain task is about to deliver but hasn't yet inserted into outstanding, while CANCEL arrives? Then "not in queue" and "not in outstanding" → forward to module. Module then gets the request, then the cancel. The fake-aft-stub's CANCEL handler does `in_flight.remove(&key)` — if the request was just delivered, the request handler may or may not have inserted into in_flight yet. If it hasn't, the cancel is unclaimed, no cancelled terminal emitted. The request handler runs to completion normally and emits a normal terminal. SDK gets a normal response, not cancelled. **This is a behavior change**: in the OLD design, CANCEL would arrive AFTER the request (since the read loop is serial), so the cancel would always arrive after the request is in_flight, claim succeeds, cancelled terminal fires.

Wait, but the OLD design has the bug — CANCEL can't overtake a blocked acquire. So in the OLD design, on a saturated serial route, the user's CANCEL doesn't even get to the read loop. There's no race in the OLD design because all frames are processed serially. In the NEW design, the read loop never blocks, so CANCEL can overtake requests still in the queue. That's the design's whole point. So the new race is: CANCEL arrives just as a Request is moving from "queued" to "delivered" — and this is actually a NEW failure mode for the SDK (no cancelled terminal, but module completes normally).

Actually, let me re-read the design:

> "CANCEL inspects the route's dispatch queue first."
> - Target Request still queued → remove it from the queue and have the DAEMON synthesize the terminal
> - Target not in queue (already delivered, or unknown) → forward CANCEL to the module unchanged

So the read loop's CANCEL handler needs to know: was the Request delivered already? The way to know is the `outstanding` set. If `outstanding.contains(corr)` → delivered. If the request is in the queue, "in queue". If neither → unknown (could be already terminal, or never existed).

The race I described (request being moved from queue to module-sink exactly as CANCEL arrives) is the critical window. In a tokio mpsc, when a sender does `try_send` and the receiver `recv`s, the value moves. If CANCEL arrives between `flow.acquire().await` and the actual delivery to the module-sink, the request is in `outstanding` only AFTER the delivery completes (drain task does `outstanding.insert(corr); module_sink.send(...).await` or `outstanding.insert(corr); module_sink.send(...).await` — order matters).

Wait — the order should be: `outstanding.insert(corr)` BEFORE `module_sink.send(...).await`. Otherwise, a CANCEL could see "not in queue" (just popped) and "not in outstanding" (not yet inserted) → forward to module. Module's CANCEL handler does `in_flight.remove(&key)` → if request just arrived but its handler hasn't done `in_flight.insert(key)` yet, the cancel is unclaimed. Then request handler runs, completes normally, SDK gets a Response, NOT a cancelled error.

This is a SUBTLE race that needs to be addressed by either:
- Insert into `outstanding` before `module_sink.send().await` AND before `flow.acquire().await`? No, `outstanding` is about delivery, not credit.
- Have the CANCEL handler in the read loop poll/peek the queue to atomically "claim" the request from the queue OR check `outstanding`.

This is a real concern. Let me look for related code in the doc.

The doc says (3.3):
> "CANCEL inspects the route's dispatch queue first."
> "Target Request still queued → remove it from the queue and have the DAEMON synthesize the terminal"
> "Target not in queue (already delivered, or unknown) → forward CANCEL to the module unchanged"

So the read loop's CANCEL inspection is: `queue.contains(corr)`. If yes, daemon synthesizes. If no, forward to module. The doc doesn't say to also check `outstanding`. The doc claims:
> "Exactly-once terminals hold: the queued case has exactly the daemon's synthetic terminal; the delivered case has exactly the module's."

But this is FALSE in the race I described. The race: drain task `pop`s Request from queue → `outstanding.insert(corr)` happens AFTER pop → CANCEL handler in read loop does `queue.contains(corr)` → sees false (just popped) → forwards to module. Meanwhile drain task's `outstanding.insert` is racing, possibly not yet committed. Module's CANCEL handler does `in_flight.remove(&key)` → if request was JUST delivered to module but its handler hasn't registered itself in `in_flight` → unclaimed. The request runs to completion. SDK gets a normal Response, not cancelled.

Let me re-read the design's I3 claim:
> "I3 Epoch-fenced release + escalation semantics byte-identical (release paths untouched)."

But the doc then says: "release fires only if `outstanding.remove(corr)` returned true". So release paths ARE touched, contrary to I3.

I3 is **internally contradictory** to the R11 rider section. The design says release is "untouched" in I3, but the R11 rider (3.7) explicitly changes release: "release fires only if `outstanding.remove(corr)` returned true".

Let me look at exactly which release path is referenced.

[assistant] 2026-07-15T07:18:31Z

[assistant] 2026-07-15T07:18:42Z
Now let me look at how the Rust SDK's pending table removes entries.

[assistant] 2026-07-15T07:18:45Z
Now let me check how a connection close triggers drain task shutdown.

[assistant] 2026-07-15T07:18:47Z

[assistant] 2026-07-15T07:18:50Z
Now let me check existing tests to understand how the writer is handled at teardown.

[assistant] 2026-07-15T07:18:54Z
Now let me look at the existing concurrency races tests to verify what the design says is "any needed test change is a red flag".

[assistant] 2026-07-15T07:19:08Z

[assistant] 2026-07-15T07:19:10Z

[assistant] 2026-07-15T07:19:18Z
Now let me verify I1: "At-most-once delivery to module per Request; queue is FIFO per route; no reordering within a route's Requests."

This is critical. With the new design:
- Read loop enqueues into the route's dispatch queue.
- Drain task `recv`s in FIFO order.
- For Request: acquire() → module_sink.send().
- For non-Request: module_sink.send().

The drain task uses `queue.recv()` which is FIFO. ✓

But what about ordering of mixed Request and non-Request frames? Say a Request R1 is in queue, then a CANCEL C1 for R1 arrives at the read loop. The read loop INSPECTS the queue. If C1 is in the queue, daemon synthesizes terminal. If C1 is not in the queue, forward to module.

But CANCEL bypasses credit (R5/R11 claim: "CANCEL bypasses request credits; the request credit returns only on this terminal"). In the NEW design, the drain task processes frames in queue order. So if R1 is queued and then C1 is queued, drain task processes R1 (acquire + send) FIRST, then C1 (send only). This is correct ordering.

What if C1 is queued but R1 is NOT? Then drain task processes C1 first, sends to module. Module sees CANCEL for unknown corr, no-ops. Then R1 arrives, gets queued, processed normally. Module sees Request, no prior CANCEL context, processes. Result: cancel is lost (overtake = cancel lost, request later runs anyway). This is the design's stated concern.

But the design says "CANCEL inspects the route's dispatch queue first" — this is on the READ loop, NOT the drain task. So if CANCEL arrives at the read loop, it inspects the queue. If R1 is in the queue, daemon synthesizes terminal. If R1 is not in the queue, forward to module. If R1 is currently being delivered (popped by drain task but not yet sent to module), the read loop's inspection misses it. This is the race I described.

Let me look for how the design addresses this race. Re-reading 3.3:

> "CANCEL inspects the route's dispatch queue first."
> "Target Request still queued → remove it from the queue and have the DAEMON synthesize the terminal"

The "remove" — is that an atomic "remove if present" from the read loop? If so, there's a window where drain task has already removed it (via `recv()`) and the read loop's `remove` returns false.

The doc says nothing about also checking `outstanding`. The outstanding set is introduced in 3.7 specifically for R11 (credit release double-counting), not for CANCEL classification.

This is a real defect in the design. The race window: drain task `recv()` pops R1 → CANCEL arrives at read loop → queue.contains(R1) = false → CANCEL forwarded to module → module CANCEL handler: in_flight.remove(key) = None (R1's handler hasn't registered yet) → unclaimed → R1 runs to completion, SDK gets normal Response, NOT cancelled.

The fix would be: insert into `outstanding` BEFORE `recv()` pops from queue, OR have CANCEL inspect both queue AND outstanding. The doc explicitly says I2: "Credit: acquire exactly-once per delivered Request (drain task, in order); release exactly-once per terminal (3.7 enforced)." This says acquire happens AFTER pop, not before. The `outstanding` set is about delivery, not queue membership. So the design has a defect.

Wait, let me re-read more carefully:
- "outstanding: HashSet<corr> (insert on delivery to module, remove-once on terminal)"

So `outstanding.insert(corr)` happens when "delivered to module", which is AFTER `module_sink.send()` succeeds. The drain task order:
```
while let Some(frame) = queue.recv():
  match frame.ty:
    Request => { flow.acquire().await; module_sink.send(frame).await; ... outstanding.insert(corr) ... }
```

So between `queue.recv()` and `outstanding.insert(corr)`, the Request is in limbo. The read loop's CANCEL handler doesn't check `outstanding`, only `queue`. So the race window is: `queue.recv()` returned → `outstanding.insert(corr)` not yet happened → CANCEL inspection sees "not in queue" → CANCEL forwarded to module → module's in_flight handler may or may not have registered the Request yet → if not, cancel unclaimed.

This is a **NEW BUG introduced by the design**, and it's specifically the scenario the design was supposed to fix. Let me see if I'm misreading the design.

Actually, re-reading 3.3 more carefully: "CANCEL inspects the route's dispatch queue first." — this is a check. The check happens at the read loop. But there's also a check: "Target not in queue (already delivered, or unknown) → forward CANCEL to the module unchanged". So the design ASSUMES that "not in queue" means "already delivered" — but in the race window, it's actually "in transit between queue and module sink".

The design's invariant claim I2 says "release exactly-once per terminal (3.7 enforced)". I2 is about credit, but the design doesn't address this race for terminal count.

Let me now check: does the test at forwarding.rs:3144 (cancel_before_response_for_cancellable_request_returns_cancelled_error) still pass under the new design? The flow is:
1. Request A delivered to module (FAKE_AFT_DELAY_FROM_BODY=1, so it takes 1ms to respond but actually is parked for 500ms via payload.delay_ms=500).
2. Wait — looking at the payload `delay_ms:500`, the stub waits 500ms before responding.
3. CANCEL arrives while Request is "in flight" (in module's in_flight, not at daemon).
4. The stub's handle_cancel removes from in_flight, claims, emits cancelled error.
5. SDK gets cancelled.

In the new design:
1. Request A sent by client.
2. Read loop enqueues into dispatch queue.
3. Drain task: `recv()` A, `flow.acquire().await` (succeeds), `outstanding.insert(A_corr)`, `module_sink.send(A).await` (succeeds — module ingress queue has space).
4. Stub receives A, inserts into in_flight.
5. CANCEL arrives at read loop. Inspects queue: not present. Inspects outstanding (per design, only queue is inspected): doesn't apply. Forwards to module via `module_sink.send(CANCEL).await` (non-Request, no credit).
6. Stub's handle_cancel: in_flight.remove(A_corr) = Some → claims, emits cancelled.
7. SDK gets cancelled. ✓

So the new design doesn't break this test AS LONG AS the request is delivered to the module BEFORE the CANCEL arrives at the read loop. The race I described requires the request to be in the limbo window (popped from queue, not yet delivered to module). With request pipelining on a serial route (window=1), if a previous request is in flight (acquired credit), the new request is BLOCKED in the queue (waiting for the drain task to pop and `flow.acquire().await` to succeed). The drain task is currently blocked awaiting `module_sink.send()` of the previous request. So the new request sits in the queue.

Now CANCEL for the OLD request arrives. The old request is in the module's in_flight (already delivered), and the new request is in the daemon's queue. CANCEL handler in read loop: queue inspection finds the NEW request, not the old one. If it scans by corr, it won't find the old one (already delivered, not in queue). So it forwards to module. Module's handle_cancel finds old in in_flight → claims. ✓

But for cancel of NEW request: it's in the queue. CANCEL handler scans queue, finds it, removes it, synthesizes cancelled terminal. No credit was acquired (drain task hadn't popped it yet). ✓

What if the drain task has popped the new request and is awaiting `flow.acquire()`? The flow is closed because the module endpoint is reloading? Then drain task is awaiting forever. CANCEL arrives. Queue inspection: new request NOT in queue. Outstanding check: not in outstanding (not yet delivered). CANCEL forwarded to module. Module's in_flight: don't have the new request (never delivered). Cancel unclaimed. **Lost.**

Or, with a saturated serial route: drain task popped old request, has credit, is awaiting `module_sink.send()` of old. New request is in queue. CANCEL for old arrives. Queue inspection: finds new request, not old. Forward to module. ✓ (because old is already in module's in_flight).

CANCEL for new: queue inspection finds new. Synthesize cancelled. ✓

What if module_egress_pressure is the issue and the drain task's `module_sink.send()` is blocked? Then the drain task is in flight on the OLD request, the NEW request is queued. CANCEL for old → queue inspection misses old → forward to module → module's in_flight has old → claims → cancelled ✓. CANCEL for new → queue inspection finds new → cancelled ✓.

So the design seems OK in the steady-state saturate case. The race I described is the simultaneous-pop-and-insert case, which only happens in a very narrow window: drain task has just popped the request, has not yet sent to module. CANCEL arrives at the read loop exactly in this window. Then the request is "neither in queue nor in outstanding nor in module's in_flight". The CANCEL is forwarded to the module. The module may or may not have registered the request in in_flight yet (the stub's `handle_data_request` does `in_flight.insert(key)` immediately). So if CANCEL arrives at module BEFORE `in_flight.insert(key)`, it's unclaimed.

Let me check the stub's handle_data_request:

[assistant] 2026-07-15T07:19:52Z

[assistant] 2026-07-15T07:19:54Z

[assistant] 2026-07-15T07:19:57Z

[assistant] 2026-07-15T07:20:14Z
Now let me look at the fake-aft-stub's `cancellable: true` config — does the test use it?

[assistant] 2026-07-15T07:20:18Z

[assistant] 2026-07-15T07:20:20Z
OK. Now let me look at the connection_loop's actual close path again to understand the teardown sequence.

[assistant] 2026-07-15T07:20:23Z
Now let me check the existing concurrency race test that the design refers to in T8.

[assistant] 2026-07-15T07:20:27Z

[assistant] 2026-07-15T07:20:29Z

[assistant] 2026-07-15T07:20:32Z
Now let me look at the existing flow's response to module terminal / control.

[assistant] 2026-07-15T07:20:36Z

[assistant] 2026-07-15T07:20:38Z
Now let me look at how `release_module_endpoint_routes` is used and what happens to the in-flight requests when a module endpoint goes down.

[assistant] 2026-07-15T07:20:41Z
Now I have enough to dig into the most critical questions. Let me check `release_module_endpoint_routes` — when a module endpoint goes down (drain), this releases routes. But the drain task may still be running and have outstanding requests with acquired credits. If `release_client_route_locked` calls `route.flow.close()`, then the flow semaphore is closed, so any pending `acquire().await` will return `ChannelFlowClosed`. But already-acquired permits — how are they released? They are tracked in `in_flight` (atomic). The design says `outstanding.remove(corr)` is what gates release. If the route is released (drained), the `outstanding` set goes with the route. Any acquired but un-released credits on this route's flow are LEAKED.

Let me check this. When `route.flow.close()` is called:

[assistant] 2026-07-15T07:20:46Z

[assistant] 2026-07-15T07:20:48Z

[assistant] 2026-07-15T07:20:50Z
Let me look at the design's `outstanding` set and the order of operations on `flow.close()`.

Now let me trace through the "module endpoint goes down during in-flight request" case carefully. In the OLD design:
- Request A is in flight: flow.acquire() returned, module_sink.send(A) succeeded, A is in module's in_flight, awaiting terminal.
- Module endpoint drain happens. `begin_module_drain` calls `flow.close()` on all flows for that endpoint.
- Module endpoint teardown: `release_module_endpoint_routes` → `release_module_route_locked` → `route.flow.close()` (idempotent on already-closed semaphores, so this is a no-op).
- A's `in_flight` counter remains > 0. The `outstanding` set would have A in it (per the new design).
- A's terminal never arrives (module is gone). The credit is LEAKED.
- In the OLD design, the `in_flight` atomic counter would just stay > 0 — it's a soft counter, not a hard guarantee. The `release` would log "flow-control over-release ignored" if called when in_flight=0. So a leaked credit is a soft "stale accounting" — not a hard correctness issue, but a perf issue (window never recovers).

In the NEW design with `outstanding` set:
- The route is released → `outstanding` set goes with the route. A's credit is still tracked in `flow.in_flight`. A's terminal will never arrive (module gone). Credit LEAKED.

This is a pre-existing issue (not new to the design), but the new design doesn't fix it. The leak exists today and the design should not regress this.

Actually wait — let me re-read the OLD `release` logic. The semaphore `close()` doesn't drain existing permits. So if `flow.close()` is called while A is in flight, the in_flight counter has 1, semaphore has window permits, but new acquires will fail. The current behavior is: in_flight stays at 1 forever (unless A's terminal arrives, which it won't because module is dead). Window stays at full. New requests on this route can't acquire (semaphore closed). This is the existing behavior.

The new design doesn't change this. The `outstanding` set is per-route-binding, and when the route is released, the set is gone. If A's terminal somehow arrives later (it can't, module is dead), `outstanding.remove(A)` would fail because A is not in the set anymore. So the new design correctly avoids over-release. Good.

But: the new design's drain task might be running, trying to do `flow.acquire().await` on a closed flow. That await fails with `ChannelFlowClosed`. The drain task then... what? In the design:
```
drain_task(route):
  while let Some(frame) = queue.recv():
    match frame.ty:
      Request  => { flow.acquire().await; module_sink.send(frame).await; }
      other    => { module_sink.send(frame).await; }
```

If `flow.acquire().await` fails (channel flow closed), the drain task panics or exits? The design says nothing about this. If the drain task just exits on this error, the queue sender is dropped (senders are owned by the read loop). But if the route is being released, the queue sender SHOULD be dropped (per 3.6 "stop drain task (drop queue sender; task exits when drained)"). The queue's `recv()` would then return `None` and the loop exits. But the `flow.acquire().await` error case isn't covered — the drain task might not exit cleanly if the flow is closed mid-acquire.

This is a potential defect. Let me note it.

Now let me also check the `FrameSink::send` (await) behavior on `module_sink.send().await` if the module is dead — the receiver is dropped, the send fails. In the OLD design, this triggers a `RouterError::backend_with_epoch(...)` and the route is escalated. In the NEW design, the drain task has the same logic. The drain task needs to handle the error and... what? Re-enqueue? Drop? Synthesize error for the client? The design doesn't say.

OK, let me also look at the GOODBYE flow more carefully.

[assistant] 2026-07-15T07:21:07Z

[assistant] 2026-07-15T07:21:10Z

[assistant] 2026-07-15T07:21:41Z

[assistant] 2026-07-15T07:21:43Z

[assistant] 2026-07-15T07:21:45Z

[assistant] 2026-07-15T07:21:46Z

[assistant] 2026-07-15T07:21:48Z

[assistant] 2026-07-15T07:21:50Z

[assistant] 2026-07-15T07:21:52Z

[assistant] 2026-07-15T07:21:54Z

[assistant] 2026-07-15T07:21:56Z

[assistant] 2026-07-15T07:21:58Z
The design is at f3185c89 (master). Now let me check the cancel-related test cases more carefully.

[assistant] 2026-07-15T07:22:01Z

[assistant] 2026-07-15T07:22:03Z
This test is critical: it proves the design's central claim (cancel-bypasses-window) using the OLD design's behavior. In the OLD design, this test currently FAILS on saturated serial routes (R5). Wait — actually it currently passes? Let me re-read.

The test does:
1. Send Request A (cancelled_corr=912, 500ms delay).
2. Wait until A is received by stub.
3. Wait 20ms.
4. Send CANCEL(A).
5. Send Request B (followup_corr=913, 0ms delay).
6. Expect: cancelled error for A, response for B.

In OLD design: read loop processes A first (full route: window=1, A in flight, B's acquire would block). A is in flight, B can't acquire. Then CANCEL(A) arrives. The OLD `handle_bound` awaits `flow.acquire()` for B... wait, B's request is being processed by the read loop, awaiting acquire. The CANCEL sits in the read loop's recv buffer. A's terminal (cancelled) comes back, frees credit. B's acquire succeeds, B is sent. So in OLD design, B is sent AFTER A's terminal. The test passes in OLD design because the read loop is serial.

Wait — actually, the OLD design has the bug. Let me re-read. In the OLD `connection_loop`:
1. Read frame A (Request, type Request).
2. route_for_connection → handle_bound → await flow.acquire (succeeds, A is first) → await module_sink.send (succeeds) → return.
3. Read frame CANCEL(A).
4. route_for_connection → handle_bound (frame_type=CANCEL, no credit) → await module_sink.send (CANCEL sent to module) → return.
5. Read frame B (Request).
6. route_for_connection → handle_bound → await flow.acquire (BLOCKED, A is still in flight) → ...
7. Module receives A, processes 500ms delay. Eventually emits cancelled terminal (because it received the CANCEL).
8. A's cancelled terminal comes back, releases credit. B's acquire succeeds. B is sent.
9. Module receives B, processes 0ms delay. Emits response.
10. Client sees: cancelled(A), then response(B).

So OLD design passes this test BECAUSE the read loop is serial. Each frame is fully processed before the next is read. The CANCEL is processed AFTER A is in flight. CANCEL is sent to module. Module's cancel handler claims and emits cancelled. A's terminal comes back, releases credit. B is sent.

In NEW design:
1. Read frame A. Enqueue into route's dispatch queue.
2. Read frame CANCEL(A). Read loop inspects queue: A is in queue. Daemon synthesizes cancelled terminal. (No CANCEL is sent to module!)
3. Read frame B. Enqueue into route's dispatch queue.
4. Drain task: recv A, flow.acquire() (succeeds), module_sink.send(A). Module receives A. Returns ok. (But module never got a CANCEL, so it'll just process A normally to completion and emit a normal response. WAIT — but the daemon already sent cancelled to the client. So the client will get cancelled (daemon) and ALSO get response (module) for the same corr.)

This is a DOUBLE-TERMINAL defect introduced by the design!

Wait, but the SDK's settle logic silently drops the second terminal. Let me check.

In TS client: `pending.get(key)` returns the pending (if not yet settled), and `this.settle(key, pending, ...)` deletes it. After daemon-synthesized cancelled settles the pending, the module's later response is an orphan terminal. The dispatch logic at line 1046: `if (pending) { ... }` — pending is gone, falls through. Line 1065: `late = this.lateResponses.get(key)` — if a late-response callback was registered, it's invoked. Otherwise line 1083 logs "dropped terminal frame with no waiter". So the second terminal is dropped. ✓

In Rust SDK: same. The pending.remove returns None for the second terminal. Silently bails. ✓

In Swift: depends on whether the call is still active. The cancel call has returned. The new request is in a different call's `routeRequest` loop. The cancelled terminal's frame would be consumed by the next `nextIngressFrame()` call, but `frameKey != key` (the new request's key), so it's silently skipped. ✓

But what if the daemon synthesizes cancelled for the corr, but the module NEVER receives the request and never emits a terminal? Then the SDK has only the daemon-synthesized cancelled. ✓

In the test case I just traced, the daemon synthesizes cancelled BEFORE the drain task has popped A. The drain task then pops A and sends it to the module. The module processes A normally (no cancel was sent to it!) and emits a normal response. The SDK gets: cancelled (daemon), then response (module). The SDK silently drops the response. End state: SDK sees "cancelled", which is correct semantically (cancelled). The request "ran to completion" on the module, but its result is discarded. The module did wasted work, but the wire-level outcome is correct.

Wait — but the design says (3.3):
> "Target Request still queued → remove it from the queue and have the DAEMON synthesize the terminal `Error{code:"cancelled"}` for that corr ... No credit was acquired for a queued Request, so no release. The module never sees either frame."

"the module never sees EITHER frame" — both the Request AND the CANCEL. So the design's intent is: when CANCEL finds the Request in the queue, REMOVE it from the queue, so the drain task never sees it. Then the module never sees the Request. The CANCEL is also not forwarded.

But how is "remove it from the queue" implemented? Tokio mpsc doesn't support "remove a specific item". You'd need to drain the queue, filter, and put back. Or use a different structure (IndexMap, VecDeque, HashMap of corrs → frame). The design says "O(queue) scan, no await" for CANCEL, but doesn't say what data structure. If the queue is a Vec<Frame>, removing an arbitrary item is O(n) (drain, filter, re-push). If the queue is a HashMap<corr, Frame>, it's O(1) but breaks FIFO for the drain task (which needs FIFO).

Actually re-reading 3.3 more carefully:
> "Target Request still queued → remove it from the queue and have the DAEMON synthesize the terminal"

So "remove from queue" is the read loop's action. The drain task is unaware. After removal, the drain task's `recv()` won't see the request. The module never sees it. Good.

But what if the drain task has ALREADY popped the request and is mid-`flow.acquire().await`? Then it's no longer "in the queue" — it's "in flight" in the drain task. The read loop's `queue.contains` returns false. The read loop forwards CANCEL to module. The module receives the Request AND the CANCEL. The module's handle_cancel finds the Request in in_flight, claims, emits cancelled. The module does NOT process the Request (it cancels it via the cancel_tx). So the module does wasted setup work (the `handle_cancellable_data_request` does `send_requested_pushes` BEFORE the cancel-aware `tokio::select!` — that's a side effect that the test at line 3188 explicitly checks for).

So in this case, the module did the request setup (including the response body) but then cancelled. The daemon synthesized a cancelled terminal. Module also emitted a cancelled terminal. The SDK gets TWO cancelled terminals — daemon-synthesized first (from read loop's CANCEL), then module's (from drain task's send). Both have code "cancelled". The second is dropped. End state: cancelled. ✓

But wait — in the NEW design, the read loop doesn't forward CANCEL to the module if the request is in the queue. So if the read loop "removes" the request from the queue and synthesizes cancelled, the drain task doesn't see the request, the module doesn't see the request, the drain task doesn't see the CANCEL. Both are daemon-local actions. The module is unaware. ✓

Now the race I described earlier: drain task popped A from queue (just `recv()` returned), is about to do `outstanding.insert(A_corr)`. CANCEL arrives at read loop. Read loop's `queue.contains(A_corr)` returns false (just popped). Read loop forwards CANCEL to module. But the drain task is still in the race window — it hasn't done `module_sink.send(A)` yet. Module receives CANCEL. Module's in_flight for A: maybe A is registered (if drain task did `module_sink.send(A)` first) or not (if drain task is still between `outstanding.insert` and `module_sink.send`).

Hmm — but the design says the drain task does:
```
Request => { flow.acquire().await; module_sink.send(frame).await; }
```

It doesn't say where `outstanding.insert` happens. Per 3.7:
> "per-route `outstanding: HashSet<corr>` (inserted on delivery to module by the drain task, removed-once on terminal)"

So `outstanding.insert` happens on "delivery to module", which is AFTER `module_sink.send` succeeds. So the order is:
1. `queue.recv()` — Request leaves queue.
2. `flow.acquire().await` — credit acquired.
3. `module_sink.send(frame).await` — Request delivered to module.
4. `outstanding.insert(corr)` — inserted into outstanding.

After step 1 but before step 4, the request is "in transit between queue and module". The read loop's `queue.contains(corr)` returns false. The `outstanding.contains(corr)` returns false. CANCEL is forwarded to module. Module may or may not have registered the request in in_flight yet (depends on module's internal state). If module's in_flight doesn't have the request yet, cancel is unclaimed, and the module processes the request normally to completion.

This is a real defect. The fix: either insert into `outstanding` BEFORE `module_sink.send`, OR have the CANCEL handler check `outstanding` in addition to the queue.

Now let me also verify what the test for cancel_bypasses_full_flow_control_window does.

[assistant] 2026-07-15T07:22:38Z

[assistant] 2026-07-15T07:22:40Z

[assistant] 2026-07-15T07:22:43Z
Let me now look at the BLOCKED test case (line 3811): `blocked_flow_control_acquire_wakes_when_module_tears_down`. The test sends a request, then sends a second request that is BLOCKED on the saturated serial route (window=1), then kills the module. Expects: blocked request gets a `backend_error` (not stuck forever).

In OLD design:
1. Request A sent, drained, in flight, 5000ms delay.
2. Request B sent. Read loop awaits `flow.acquire()` for B (blocked because A holds the only credit).
3. Module is stopped (5 seconds hasn't elapsed). The module's TCP connection dies.
4. Read loop is still blocked on `flow.acquire()`. Wait — actually, the test sleeps 50ms then asserts blocked was NOT received by stub (correct, B is blocked at the daemon). Then `module.stop()` kills the module. Then waits 2 seconds for either Goodbye or backend_error for B.

The test then asserts that B gets a `backend_error`. In OLD design:
- When the module is killed, `module_sink.send()` for the in-flight A's response (which would have been a terminal) will fail. But A hasn't sent a response yet (still in 5s delay).
- The connection's module-side TCP dies. The drain_writer (in the daemon's connection-to-module) sees the module connection close. This triggers `begin_module_drain` for that module endpoint. `begin_module_drain` calls `flow.close()` on all flows for that endpoint.
- The B's `flow.acquire().await` returns `ChannelFlowClosed`. The current `handle_bound` returns `RouterError::backend_with_epoch("flow-control window closed")` (per the code at router.rs:479-484). Wait, but the message says "{err} for route channel {channel}" where err is the ChannelFlowClosed error. So the error message is "flow-control window closed for route channel N".
- The connection_loop receives the RouterError, calls `err.to_error_frame()`. Looking at `to_error_frame` (line 582): Backend variant maps to `error_frame(channel, epoch, corr, "backend_error", message)`. So B gets a "backend_error" frame with message "flow-control window closed for route channel N".

The test asserts `assert_error(&frame, ack.route_channel, blocked_corr, "backend_error")`. So the OLD design passes this test.

In NEW design:
- Request A enqueued. Drain task pops A, acquires, sends to module (succeeds). A in module's in_flight.
- Request B enqueued. Drain task's queue.recv() returns B. Drain task awaits `flow.acquire()` for B. BLOCKED.
- Module dies. `flow.close()` called.
- Drain task's `flow.acquire().await` returns `ChannelFlowClosed`. What does the drain task do?

The design says (3.2):
```
drain_task(route):
  while let Some(frame) = queue.recv():
    match frame.ty:
      Request  => { flow.acquire().await; module_sink.send(frame).await; }
      other    => { module_sink.send(frame).await; }
```

If `flow.acquire()` fails, the design doesn't say what to do. The drain task could:
- Panic (bad).
- Exit the loop (leaving other queued frames unprocessed).
- Synthesize a `backend_error` for B and continue (but the request was Request type, so a cancelled-like daemon synthesis could be made).
- Retry (but flow is closed forever).

The design is **silent on this case**. This is a defect: drain task error handling is undefined.

Let me also check the GOODBYE flow more carefully.

The design says (3.3):
> "GOODBYE for a route: flush its queue (drop queued frames — the client has already settled locally, matching shipped GOODBYE semantics), then proceed with today's epoch-fenced release + relay. Queue flush must precede binding release so no frame can enqueue after flush (both run under the route-teardown path in the drain task's shutdown, 3.6)."

The phrase "queue flush must precede binding release so no frame can enqueue after flush" — this is the design's claim about ordering. But the read loop is enqueueing from a separate task. There's a race:
- Read loop holds the queue sender.
- Drain task is running, popping from queue.
- GOODBYE arrives at read loop. Read loop needs to:
  a) Tell drain task to flush its queue.
  b) Then drop the queue sender (so no more enqueues).
  c) Then release the binding.

But (a) and (b) require coordination between the read loop and the drain task. If the read loop just drops the queue sender directly, the drain task's `queue.recv()` returns `None` and the drain task exits. But the queue still has frames in it (drain task only pops one at a time). Those frames are DROPPED when the queue is dropped. The design says "drop queued frames" for GOODBYE, which is the desired behavior. But the ordering between "drain task pops the last frame" and "queue sender is dropped" is the question.

Actually, the design says "Queue flush must precede binding release so no frame can enqueue after flush". The implication is: flush first (drop queued frames), THEN release. If the queue sender is held by the read loop, dropping it causes the queue to close. The drain task drains the queue before exiting. So the order is: read loop drops queue sender → queue starts closing → drain task drains remaining items → drain task exits → read loop releases binding. The "queue flush" is the drain task draining remaining items. The binding release happens AFTER the drain task has exited.

How does the read loop know the drain task has exited? The drain task is a tokio::task::JoinHandle. The read loop could `await` the handle. But the read loop is busy reading frames; it can't wait for the drain task to exit. There must be some other synchronization.

Actually, the design's "queue flush must precede binding release" is best implemented as: drop queue sender, await drain task exit, then release binding. But the read loop is a single-task per-connection loop. Awaiting the drain task blocks the read loop. The design says the read loop never blocks on per-route work — awaiting drain task exit IS blocking on per-route work.

This is a tension in the design. The "never blocks" promise of the read loop is violated by GOODBYE handling, which inherently must wait for the drain task to drain. The design says "control vs data" is preserved (control task and data drain task are separate), but the GOODBYE itself is a data-plane frame that needs to wait for the data-plane drain task. The design doesn't address this.

Workaround: the read loop doesn't await; it spawns the teardown as a separate task. The teardown task does "drop queue sender, await drain task, release binding". The read loop continues reading frames. But: while the teardown task is in progress, the read loop could try to enqueue more frames into a closing queue (returns error → daemon synthesizes route_backpressure or similar). This is fine for Request frames (already in the read loop), but CANCEL inspection becomes racy: queue might be partially drained, so CANCEL could see "not in queue" but the drain task is mid-drain.

This is getting complex. Let me consolidate the findings.

Now let me check the existing test for "blocked_flow_control_acquire_wakes_when_module_tears_down" — the test expects `assert_error` for the blocked request, with code "backend_error". So the test does expect a terminal for the blocked request. In the OLD design, this is the `RouterError::backend_with_epoch` path. In the NEW design, the drain task must synthesize something equivalent.

OK, let me also look at the connection close teardown. When a TCP connection closes, the read loop returns. The drain task is still running. The connection's `RouterConnection` is dropped, which calls `cleanup_connection`. The `release_client_route_locked` releases the route, which calls `flow.close()`. The drain task is still running, but its `flow.acquire().await` returns `ChannelFlowClosed`. Now what? The drain task must exit. But it might also have frames in the queue. The read loop's outbound `FrameSink` is also being dropped (when `egress` is dropped). The drain task's `module_sink.send()` might still succeed (the module connection is still alive). So the drain task could continue sending frames to the module. The module processes them. But the client connection is gone. The module's response is sent to the client, but the client's FrameSink on the daemon side is dropped. The daemon can't deliver them.

Wait, actually, the module's response is sent via the route's `client_sink`, which is a clone of the egress. When the egress is dropped (in `handle_connection` after `connection_loop` returns, line 244), the client_sink's underlying mpsc is closed. The module's `client_sink.try_send(response)` returns Err, triggering the `escalate_client_delivery_failure` path. This is the OLD behavior.

In the NEW design, the drain task is the one delivering to the module (not the read loop). On the module→client direction, the OLD code does the lookup and `try_send` in the read loop. In the NEW design, the design says (3.2) "Module→client direction is unchanged (it was already non-blocking: lookup + try_send + credit release)". So the module→client direction goes through the read loop. The read loop is what reads the module's response, looks it up, and try_sends to the client. Wait, but the read loop only reads from the CLIENT, not the module. The module's connection has its own read loop on the daemon side.

Let me re-read the design:

> "Module→client direction is unchanged (it was already non-blocking: lookup + try_send + credit release). Only the client→module direction gains queues, because that is the only direction that blocks today."

So the module→client direction is still handled by the read loop? But the read loop reads from the client TCP, not the module TCP. The module TCP is a separate connection. The module's response arrives on the module's connection, and the daemon processes it via... another read loop, or the ForwardBackend::handle? Let me look at how the module→client direction is handled in OLD code.

[assistant] 2026-07-15T07:23:14Z

[assistant] 2026-07-15T07:23:17Z

[assistant] 2026-07-15T07:23:20Z
Important: the module→client direction is handled by `router.route_for_connection(&module_ctx, terminal)` — the module's connection has its own read loop that calls `route_for_connection` for each module frame. So the read loop is per-CONNECTION, not per-client. The module connection has its own read loop.

The design says:
> "Module→client direction is unchanged (it was already non-blocking: lookup + try_send + credit release). Only the client→module direction gains queues, because that is the only direction that blocks today."

This is correct. The module's read loop processes module→client frames via `route_for_connection`, which is non-blocking (lookup_data_route + try_send). The client's read loop processes client→module frames, which USED TO BE blocking (B1+B2) and now becomes non-blocking (drain task).

OK. Now let me look at the connection close + drain task ordering more carefully. The design says:
> "Connection close: existing teardown already releases all routes; that now also tears down all drain tasks. No orphan tasks: task handles owned by the binding entry."

The teardown flow:
1. Client connection closes.
2. `connection_loop` returns.
3. `RouterConnection` is dropped (line 245 in server.rs).
4. `RouterConnection::drop` calls `cleanup_connection` (line 393, 396).
5. `cleanup_connection` releases all routes, which calls `flow.close()` for each route.
6. Drain tasks are still running. Their `flow.acquire()` or `module_sink.send()` will fail.
7. Drain tasks need to exit. But the design says "task handles owned by the binding entry" — when the binding is released, the handle is dropped? Or does the teardown wait for the handle?

The design says (3.6): "stop drain task (drop queue sender; task exits when drained)". So the way to stop the drain task is to drop the queue sender. Who holds the queue sender? The read loop. But the read loop is exiting (connection closed). When the read loop drops its references, including the queue sender, the queue sender is dropped. The drain task's `queue.recv()` returns `None`, and the drain task exits.

But there's a race: the read loop is in the middle of enqueuing a frame when it gets the close signal. If the read loop has already started enqueuing (frame in hand, about to do `queue.send(frame)`), but the queue sender is dropped... actually, the read loop's close-handling is: when `close_receiver` fires, the read loop returns. The next iteration of the loop would have read a frame. If the read loop is between `read_frame` and `queue.send`, the queue send happens before the close is observed? No — the close is observed via select, so if the close fires before the next `read_frame`, the read loop returns. The frame currently in hand (just read) is dropped (Rust drop). The queue send is NEVER called for that frame. So no race in that direction.

But what about frames ALREADY in the queue when the close happens? The drain task is still running. The queue has, say, 3 frames. The read loop's close returns. The read loop's `FrameSink` (egress) is dropped. The read loop's `connection` (RouterConnection) is dropped, which calls `cleanup_connection` → `flow.close()`. The drain task's `flow.acquire()` returns `ChannelFlowClosed` (or succeeds and then `module_sink.send` fails because the module connection is also being torn down... or just because the queue is being closed and the drain task is exiting).

The drain task's `queue.recv()` will return `Some(frame)` for the remaining 3 frames, process them (try to acquire flow — but flow is closed → error). What does the drain task do with the error?

The design is silent. This is a real defect.

Now let me also verify the design's I2 claim: "Credit: acquire exactly-once per delivered Request (drain task, in order); release exactly-once per terminal (3.7 enforced)."

Per exit path:
(a) Delivered + module terminal: drain task does `outstanding.insert(corr)`. Module emits terminal, daemon's module-read-loop processes it via `is_terminal_frame` check, calls `route.flow.release()` (line 308). The release is GATED by `outstanding.remove(corr)` returning true (per 3.7). ✓ Exactly once.

(b) Queued + CANCEL-synthesized: read loop removes from queue, synthesizes cancelled terminal. No credit was acquired (drain task hadn't popped). `outstanding.remove(corr)` would not be called (no terminal to remove from). ✓ No credit, no release. But — what if the drain task pops the request and starts `flow.acquire()` at the same time the read loop removes it from the queue? If the read loop "removes" via a filter-and-repush (O(n) drain), and the drain task is mid-recv... tokio mpsc is a queue, not a set. The read loop would need to call `try_recv` to pop items, check the corr, and either keep or discard. But this is racy with the drain task's `recv()`.

Actually, I think the design's "remove from queue" is ill-defined for a tokio mpsc. You'd need a different data structure (e.g., IndexMap) to support O(1) arbitrary removal while maintaining FIFO for the drain task. The design says "O(queue) scan, no await" — so the read loop drains the queue, filters, and puts back. But this is racy with the drain task's concurrent `recv()`.

This is a real defect: the data structure for the dispatch queue isn't specified, and tokio mpsc doesn't support safe concurrent removal by predicate.

(c) Queue flushed on GOODBYE: drop queued frames, drain task exits. No credit acquired. ✓ But the order of "drop queued frames" vs "binding release" is critical — the design says "Queue flush must precede binding release so no frame can enqueue after flush". But the queue sender is held by the read loop, and the read loop can enqueue at any time. The binding release closes the flow, so any drain task mid-`flow.acquire` will fail. But the read loop is still trying to enqueue. If the read loop's `route_frame` is called after the GOODBYE handler starts, the enqueue could happen on a queue whose sender is about to be dropped. With tokio mpsc, `send` returns Err if all receivers are dropped. So if the queue sender (held by read loop) is the only sender, and the receiver (held by drain task) is dropped, the read loop's send returns Err. But this happens after the drain task has dropped the receiver — which happens after the drain task has exited — which happens after the queue is drained — which happens after the queue sender is dropped.

The race: read loop's `route_frame` is called between the GOODBYE handler starting and the queue sender being dropped. The GOODBYE handler needs to: (1) drop queue sender, (2) wait for drain task to exit, (3) release binding. The read loop is still running concurrently. If the read loop's `route_frame` runs between (1) and the drain task actually exiting, the send might succeed (the receiver is still alive until the drain task exits). So a frame could be enqueued between the GOODBYE handler's drop and the drain task's exit.

Wait — but the GOODBYE handler's "drop queue sender" makes the queue "close" only after the receiver is dropped. If the drain task is still holding the receiver, the queue is not closed; sends succeed. The drain task continues popping frames and processing them. But the binding is being released! The drain task's `flow.acquire` will return ChannelFlowClosed. The drain task needs to exit. But it's still processing frames.

This is a real defect. The design needs a way to atomically "close" the queue so no more sends happen, and to wait for the drain task to fully exit, BEFORE releasing the binding. The read loop cannot enqueue between "queue closed" and "drain task exited" because the read loop's send would fail with Err. So actually:

1. GOODBYE handler: drop queue sender (held by handler itself, not the read loop — wait, who holds the queue sender?).

The read loop holds the queue sender. The drain task holds the receiver. The GOODBYE handler is in the read loop. So the read loop is processing GOODBYE — it has the queue sender in hand. It needs to:
a) Tell the drain task to drain remaining items.
b) Wait for drain task to drain and exit.
c) Then drop the queue sender (so no more enqueues).
d) Then release the binding.

But the read loop holds the queue sender. If the read loop "tells the drain task to drain" by signaling it, the read loop is still holding the queue sender. The signal could be: drop a "drain_now" semaphore, the drain task checks this and exits early. Then the read loop awaits the drain task's join handle. Then the read loop releases the binding.

But the read loop is blocked on `await drain_task_handle`. The design says the read loop never blocks on per-route work. **This is a violation of the design's core promise.**

Alternative: the read loop spawns a teardown task and continues. The teardown task does the drain + release. The read loop continues reading. New requests for this route get "not in forwarding" (because the binding is released by the time the read loop processes them, or maybe still present until the teardown task completes).

This is getting complex. The design doesn't address this, and it's a real correctness concern.

Now let me look at the **PER-connection aggregate cap** claim: 4096 frames. The design says: "Per-connection aggregate cap: sum of queued frames per connection capped (e.g. 4096 frames); overflow → connection-level protocol-error close (a client that floods past every per-route bound is broken; closing is the existing escalation vocabulary)."

This means the read loop must maintain a per-connection counter that increments on enqueue and decrements on... what? On drain task pop? The design doesn't say.

If the counter is per-route, summed across routes, and the drain task decrements on pop, then a "fast" attacker could fill all queues to max, send CANCELs to drain (O(n) each on the read loop), and keep the per-connection counter high. The read loop would close the connection.

But also: the read loop's CANCEL inspection is O(queue) per CANCEL. An attacker with 4 serial routes, each with queue=4, can send 4 CANCELs and force 4 O(4)=O(16) scans. Not a big deal. But with 4096-frame per-connection cap and Serial routes (queue=4), 1024 serial routes, an attacker could send 1024 CANCELs forcing 1024 O(4) = O(4096) scans. The read loop is single-threaded per connection. This is a DoS vector but not catastrophic.

But wait — the CANCEL inspection is O(queue) per CANCEL on the READ LOOP. The read loop is the latency-critical path. An attacker could send 1 CANCEL per socket read, forcing O(4) work per read. With a 64-byte minimal CANCEL frame, that's 64 bytes per O(4) work. Not great but not terrible. The real concern is when the queues are LARGE (StatelessParallel=2048). An attacker fills the queue to 2048, then sends 1 CANCEL. The read loop does O(2048) work. The next read is delayed by 2048 comparisons. That's a real latency hit.

Now let me also check: per the design, CANCEL can ALSO be queued (if the queue has space)? No, the design says: "On full queue (non-Request: CANCEL/GOODBYE): these must never be dropped for capacity — they are queue-INSPECTING (3.3) or queue-FLUSHING (GOODBYE) operations executed by the read loop against the queue structure itself (O(queue) scan, no await), not enqueued behind it."

So CANCEL is NOT enqueued. It's processed by the read loop directly. OK. But this means the read loop's per-frame work for CANCEL is O(queue), which can be 2048 in the worst case. Latency-critical.

Now let me also check the GOODBYE flow. The design says GOODBYE "flushed queue (drop queued frames)". The read loop's GOODBYE handler does an O(queue) scan to drop frames. The read loop is single-threaded per connection. O(2048) work per GOODBYE. Acceptable.

OK, now let me also check the data structure for the dispatch queue. The design says "per-route bounded dispatch queue (depth: 3.5) plus one drain_task". It doesn't specify the type. For FIFO + arbitrary removal, you'd need something like a VecDeque (O(n) arbitrary remove) or a doubly-linked list. The design says CANCEL is O(queue) scan — so it's likely a Vec or VecDeque.

Tokio mpsc is a queue but doesn't support arbitrary removal. The design might use a VecDeque wrapped in a Mutex, or a custom structure. The design is silent on this.

Let me now look at the existing tests for invariants.

[assistant] 2026-07-15T07:24:01Z

[assistant] 2026-07-15T07:24:04Z

[assistant] 2026-07-15T07:24:06Z
Important: the existing test `unknown_channel_cancel_drops_silently_and_connection_survives` (line 3395) cancels a non-existent channel. Expected: no terminal, connection survives. In the NEW design, the CANCEL handler in the read loop inspects the queue for that channel — finds nothing (channel doesn't have a route). What does the read loop do? The design says "Target not in queue (already delivered, or unknown) → forward CANCEL to the module unchanged".

Wait, the read loop's CANCEL handler first looks up the route via `lookup_data_route`. If the route doesn't exist (channel/epoch doesn't have a binding), the lookup returns Absent. The current code (line 247-279) handles GOODBYE; for non-GOODBYE, the Absent case is handled in the broader structure. Let me look at what `route_for_connection` does for an unknown channel + CANCEL.

Actually, looking at the OLD code at router.rs:344:
```
DataRoute::Client(DataRouteState::Absent) => {}
```
This falls through to:
```
if let Some(backend) = self.backends.get(&channel) { ... }
if frame.header.ty == FrameType::Request { ... return error }
Ok(())
```

For a CANCEL on an unknown channel: the data_route lookup returns `DataRoute::Client(DataRouteState::Absent)`. Falls through. `self.backends.get(&channel)` returns None. `frame.header.ty == FrameType::Request` is false (CANCEL). Returns Ok(()). Effectively a silent no-op. ✓

In the NEW design: read loop reads CANCEL for unknown channel. The read loop needs to look up the route to find the dispatch queue. If the route doesn't exist, there's no queue. What does the read loop do? The design says (3.3) "Target not in queue (already delivered, or unknown) → forward CANCEL to the module unchanged". But the route doesn't exist, so there's no module either. The design is silent.

Likely the read loop would do nothing (silently drop the CANCEL). The test `unknown_channel_cancel_drops_silently_and_connection_survives` expects this. The NEW design should preserve this.

Now let me check the GOODBYE race. The design says GOODBYE for a route:
1. Flush queue (drop queued frames).
2. Then release the binding.

If the read loop is in the middle of enqueuing a frame for this route, and the GOODBYE handler is running, the race is:
- Read loop: read frame, classify, call `route_frame(frame)`.
- GOODBYE handler: in another iteration of the read loop, but interleaved with the route_frame call? No — the read loop is sequential. GOODBYE and Request can't be processed concurrently in the read loop. The read loop processes one frame at a time.

Wait, but the read loop is processing one frame at a time. So the read loop's `route_frame(REQUEST)` is atomic with respect to the GOODBYE handler. The GOODBYE handler is `route_frame(GOODBYE)`, which is the next iteration. By the time GOODBYE is being handled, the previous Request has been fully enqueued (or rejected). So there's no race in the read loop itself.

The race is between the read loop's enqueue and the drain task's processing. If the read loop enqueues a frame, then the GOODBYE handler is called (next iteration), the GOODBYE handler flushes the queue (dropping the just-enqueued frame). The drain task never sees it. ✓

But what about a frame that was enqueued BEFORE the GOODBYE? The drain task might have already popped it. The drain task is mid-`flow.acquire()` or mid-`module_sink.send()`. The GOODBYE handler flushes the queue (queue is now empty, after the drain task popped the last one). The GOODBYE handler then drops the queue sender and releases the binding. The drain task is still running. Its `flow.acquire()` returns ChannelFlowClosed (because binding was released → flow.close() called). Drain task needs to exit.

OK so the order is: flush queue → drop sender → release binding → drain task observes flow.close → exits. But the read loop is doing the flush and the release. The drain task's exit is asynchronous. The read loop doesn't wait. The next iteration of the read loop could try to enqueue a frame for this route... but the binding is released, so the route is gone from the snapshot. The next lookup_data_route returns Absent. The read loop does nothing. ✓

But what if the snapshot is STALE (per 3.8)? The read loop could see a snapshot with the route still bound, and try to enqueue. The queue sender was dropped by the GOODBYE handler. The enqueue returns Err. The design says (3.3) "A frame that loads a snapshot before a bind commit sees Absent — identical to today's pre-commit window." But what about a frame that loads a snapshot BEFORE the GOODBYE was processed? It enqueues. The drain task is processing. The GOODBYE is being processed. Flush. Done.

Actually, the design says snapshot publish is on commit. The GOODBYE handler releases the binding and... does it publish a new snapshot? The design says (3.8): "All mutations (bind commit, release, register/cleanup, endpoint drain) keep the existing write lock as the serialization point, apply to the canonical state, then publish a fresh snapshot". So release (including GOODBYE) also publishes a new snapshot. ✓

But there's a window: a frame loaded snapshot S1 (with route bound) just before GOODBYE was processed. The frame tries to enqueue. Enqueue succeeds. Frame is in the queue. Then GOODBYE handler is called, releases binding, publishes S2 (without route). A LATER frame loads S2 and sees no route.

But the frame that loaded S1 and enqueued is in the queue. The GOODBYE handler flushes the queue. The frame is dropped. ✓

What if the frame loaded S1, started enqueuing, but the GOODBYE handler ran first and dropped the queue sender? The enqueue returns Err. The frame is... rejected? The design says "On full queue (Request): the daemon synthesizes `Error{code:"route_backpressure"}`" but this is for a full queue, not a closed queue. The design doesn't address "queue closed" specifically.

If the queue is closed because the GOODBYE handler dropped the sender, the read loop's enqueue should treat it as "route gone" and silently drop the frame (matching the existing channel-gone semantics). The design doesn't say this explicitly. Defect or implicit?

Let me look at the existing channel-gone semantics.

[assistant] 2026-07-15T07:24:31Z

[assistant] 2026-07-15T07:24:33Z
So in OLD code, Absent for client-originated data: falls through, no backend registered, no Request error returned, silent drop. So in NEW design, if the route is released (snapshot no longer has it), the read loop should silently drop the frame. The design implies this but doesn't state it explicitly.

Now let me look at what existing tests exist for "module_tears_down_while_request_in_flight":

[assistant] 2026-07-15T07:24:37Z

[assistant] 2026-07-15T07:24:40Z
Now I have enough context. Let me also check the request closure and the existing test for the `route_closed_during_open` and similar:

[assistant] 2026-07-15T07:24:42Z

[assistant] 2026-07-15T07:24:45Z
Now let me look at the design's data structure and concurrency claims more carefully. Let me also check the design's claim about the `is_terminal_frame` check at `router.rs:281`.

The design says (3.2): "Module→client direction is unchanged (it was already non-blocking: lookup + try_send + credit release)." And (I7): "Module→client direction unchanged (try_send best-effort + escalation)."

The current code at router.rs:281 has:
```rust
let releases_credit = is_terminal_frame(frame.header.ty);
...
if releases_credit {
    route.flow.release();
}
```

This is a credit release. In the NEW design with R11 rider, the release would be gated by `outstanding.remove(corr)`. The design says I7: "Module→client direction unchanged". But the R11 rider CHANGES this: it adds an `outstanding.remove(corr)` check before `flow.release()`. So I7 is CONTRADICTED by the R11 rider.

I7 is FALSE.

OK, let me now also check: the design says I3: "Epoch-fenced release + escalation semantics byte-identical (release paths untouched)." But the R11 rider changes release paths. I3 is FALSE.

Both I3 and I7 are internally contradicted by the R11 rider.

Now let me also check: the design says (3.6) "Connection close: existing teardown already releases all routes; that now also tears down all drain tasks. No orphan tasks: task handles owned by the binding entry."

The teardown of drain tasks: the design says "task handles owned by the binding entry". The binding entry is in the snapshot. When the binding is removed from the snapshot, the entry is dropped, and the task handle is dropped. But dropping a JoinHandle does NOT abort the task; the task continues to run. So "owned by the binding entry" is misleading — the task continues running. The task needs to be EXPLICITLY aborted or signaled to exit.

The design says: "stop drain task (drop queue sender; task exits when drained)". So the queue sender is dropped to signal the drain task to exit. But who drops the queue sender? The read loop, when processing GOODBYE. On connection close, the read loop exits. The read loop's locals (including the queue sender, if the read loop holds it) are dropped. The queue sender is dropped. The drain task's `queue.recv()` returns None. The drain task exits.

But: the read loop might have multiple queue senders (one per route). The read loop holds all of them. On exit, all are dropped. The drain tasks for all routes exit. OK.

But the design says "task handles owned by the binding entry". The binding entry holds the JoinHandle. When the binding is released (by the read loop's GOODBYE handler, or by connection close teardown), the binding entry is removed. The JoinHandle is dropped. The drain task continues running. The queue sender (held by the read loop) is still alive until the read loop exits. When the read loop exits, the queue sender is dropped. The drain task's `queue.recv()` returns None. The drain task exits.

So the actual order: binding released → JoinHandle dropped (task still running) → read loop exits → queue sender dropped → drain task exits. The JoinHandle is dropped before the task is joined. This is fine if the task is just dropped (no resources leak), but if the task has spawned child tasks (it doesn't, per the design), those would leak. Per the design, the drain task doesn't spawn child tasks. So this is OK.

But the design's wording is misleading: "task handles owned by the binding entry" implies that dropping the binding entry tears down the task, which is NOT what happens. The task is torn down by dropping the queue sender (held by the read loop), not by dropping the binding.

This is a documentation/clarity issue, not a correctness issue.

Now let me check the GOODBYE race more carefully. The design says:
> "GOODBYE for a route: flush its queue (drop queued frames — the client has already settled locally, matching shipped GOODBYE semantics), then proceed with today's epoch-fenced release + relay. Queue flush must precede binding release so no frame can enqueue after flush (both run under the route-teardown path in the drain task's shutdown, 3.6)."

"Queue flush must precede binding release so no frame can enqueue after flush" — this is the critical claim. The implementation must:
1. Flush the queue (drop queued frames).
2. Then drop the queue sender (so no more enqueues from the read loop).
3. Then release the binding.

But the read loop is concurrent with the GOODBYE handler. If the GOODBYE handler is in the read loop, it's sequential with the read loop's enqueue. So actually, the GOODBYE handler is the read loop's NEXT iteration. By the time GOODBYE is being processed, the previous frame (if any) has been fully enqueued (or rejected). The GOODBYE handler can:
1. Drop queue sender.
2. Wait for drain task to exit.
3. Release binding.

Wait, but the queue sender is held by the GOODBYE handler (which is the read loop). If the read loop drops the queue sender, then any future enqueue from the read loop (in a NEXT iteration) would fail. But there shouldn't be a NEXT iteration for this route — the route is being released. The snapshot no longer has this route, so the next lookup returns Absent.

But the snapshot publish is on commit. The GOODBYE handler is processing. It hasn't yet released the binding. The snapshot still has this route. A concurrent enqueue from... no, the read loop is sequential. The GOODBYE handler is in the read loop. No concurrent enqueue from the read loop.

But: what if the read loop is processing a frame for ANOTHER route, and a CANCEL for this route arrives next? CANCEL is processed by the read loop's CANCEL handler. The CANCEL handler inspects THIS route's queue. But the queue sender was dropped by the GOODBYE handler. The CANCEL handler can't access the queue (it was dropped with the GOODBYE handler's local). Wait, the queue is owned by the binding entry, not the GOODBYE handler. The queue sender (a clone of the Sender) is held by the read loop. The GOODBYE handler is in the read loop. The queue sender is in the read loop's locals.

This is getting complex. Let me think about ownership:
- Binding entry: owns the queue (Sender + Receiver) and the JoinHandle.
- Read loop: holds a clone of the Sender for each route (to enqueue frames).
- Drain task: holds the Receiver.

For the GOODBYE handler to flush the queue, it needs to access the queue. The queue is owned by the binding entry. The handler is in the read loop. The read loop needs to access the binding entry to get the queue.

This is all internal data structure stuff. The design is silent on the data structure. Let me assume: the read loop holds a map of `RouteKey → (QueueSender, JoinHandle)`. The binding entry (in the snapshot) doesn't hold these — they live in a per-connection data structure (let's call it `RouteDispatchTable`) that the read loop owns.

The GOODBYE handler:
1. Look up RouteDispatchTable by RouteKey.
2. Get the queue sender and join handle.
3. Drop the queue sender.
4. Await the join handle (drain task drains remaining items then exits).
5. Remove the entry from RouteDispatchTable.
6. Call `release_client_route` to release the binding (publishes snapshot).
7. Remove the entry from the dispatch table.

But step 4 awaits the join handle. The read loop is blocked. The design says the read loop never blocks on per-route work. **This is a violation of the design's core promise.**

To avoid blocking the read loop, the GOODBYE handler could:
- Spawn a teardown task that does steps 3-5.
- The read loop continues reading.
- The teardown task is responsible for steps 3-5.

But the teardown task is a child task. The read loop doesn't track it. If the teardown task takes too long, the read loop might process a frame for this route (which is being torn down). The frame would be enqueued into a queue whose sender has been dropped → enqueue fails → frame is... silently dropped? Or sent through a different path?

The design is silent on this. This is a real defect.

Now let me also check: the design says "the only cancellation of an in-progress read remains connection close, and there is still no await between a completed read and hand-off other than the bounded admission path, which never suspends (try_push)".

The "bounded admission path" is the queue full check. The design says it "never suspends (try_push)". So the read loop's hand-off is non-suspending. But the hand-off might be to a queue whose sender is closed (because the drain task is shutting down). The read loop's try_push returns Err. The design doesn't say what happens.

If the read loop's try_push returns Err because the queue is closed, the frame should be silently dropped (matching the channel-gone semantics). The design implies this but doesn't state it.

OK, let me also look at the BufReader cancel-safety claim. The design says (I6): "BufReader cancel-safety: read loop still cancelled only at connection close."

The current code: the read loop is `select!` over `close_receiver` and `read_frame`. If `close_receiver` fires, the loop returns. The `read_half` (BufReader) is dropped when the loop returns (in `handle_connection`, line 243-245, then line 266, the writer is awaited, then the read_half is dropped). The BufReader is drop-safe; its internal buffer is dropped.

In the NEW design: the read loop is `select!` over `close_receiver` and `read_frame`. The route_frame call is non-suspending (try_push). The GOODBYE handler is also non-suspending (according to the design). So the read loop is "read → classify → hand-off (sync, non-blocking)" with no awaits between read and hand-off. The BufReader is still only cancelled at connection close. ✓

But: the GOODBYE handler in the read loop is non-suspending ONLY if the GOODBYE handler doesn't await the drain task. If the GOODBYE handler does await the drain task (to ensure queue flush before release), then the read loop is blocked on the drain task — violating I6.

The design says GOODBYE "flushed queue (drop queued frames)" — this is the read loop's action. If the read loop's GOODBYE handler does the flush by directly accessing the queue (via a reference), it's O(queue) and non-suspending. But then the read loop can't wait for the drain task to finish processing. The drain task might still be processing a frame from the queue (mid-`flow.acquire()` or mid-`module_sink.send()`). The read loop's GOODBYE handler returns immediately, having "flushed" the queue. But the drain task is still running. The binding release happens after the flush. The drain task's `flow.acquire()` will fail (flow closed). The drain task exits. But this is all asynchronous.

The order:
1. Read loop: GOODBYE handler flushes queue (drops remaining items).
2. Read loop: releases binding (flow.close(), publish snapshot).
3. Read loop: returns from route_frame, continues reading next frame.
4. Drain task: pops last item (none, queue was flushed). Wait, `recv()` returns `None`? No — the queue's sender is held by the read loop. The read loop hasn't dropped the sender. So `recv()` blocks until the sender is dropped.
5. Drain task: is still mid-`module_sink.send()` for the frame it had popped before the flush. It returns to the loop, calls `queue.recv()` again. This blocks.
6. Read loop: continues. New frame for this route? The snapshot no longer has this route (published in step 2). New frame is silently dropped (no backend, no error).
7. Eventually, the read loop exits (close_receiver fires or peer closes). The read loop's locals are dropped, including the queue sender. The drain task's `recv()` returns None. Drain task exits.

This is OK functionally. The drain task is "leaked" until the connection closes, but it's not doing any useful work (blocked on `recv()`). When the connection closes, the drain task exits. No permanent leak.

But: what if the connection doesn't close for a long time? The drain task is still running, blocked on `recv()`. It's not doing work, but it's a task. If the connection has many routes, many drain tasks are blocked. This is fine for resource usage (each task is just a `recv()` future, ~small).

OK, so the GOODBYE flow is OK if:
- The read loop's GOODBYE handler does the queue flush (non-suspending O(queue) scan).
- The read loop's GOODBYE handler releases the binding.
- The read loop does NOT await the drain task.
- The drain task is allowed to finish processing its current frame, then blocks on `recv()` until the queue sender is dropped.
- The queue sender is dropped when the read loop exits (connection close).

This is consistent with the design's "queue flush must precede binding release" — the flush happens in the read loop BEFORE the release. The drain task's current frame is allowed to complete (or fail). Its future `recv()` calls block until the connection closes.

But there's a subtle issue: the drain task is still holding the route's `flow: Arc<ChannelFlow>`. When the binding is released, the `Arc<RouteBinding>` (which holds the `Arc<ChannelFlow>`) is dropped. If the drain task is the only holder, the `Arc<ChannelFlow>` stays alive (via the drain task's local reference). The `flow.close()` was called by `release_client_route_locked` (line 1424: `route.flow.close()`). But the close is on the semaphore, not on the Arc. The Arc stays alive until the drain task drops its reference. ✓

The flow semaphore is closed. The drain task's `flow.acquire().await` returns `ChannelFlowClosed`. The drain task needs to handle this error.

**The design doesn't say what the drain task does on `ChannelFlowClosed`.** This is a defect.

Likely behaviors:
- Panic: bad, kills the connection.
- Exit the loop: leaves remaining queued frames unprocessed. The queue sender is still held by the read loop, so `recv()` blocks forever. When the connection closes, the sender is dropped, the drain task exits. OK.
- Synthesize `backend_error` for the frame and continue: the design doesn't say this. The error message would be "flow-control window closed".

The design needs to specify this. **Defect.**

Now let me check the existing test for this:

[assistant] 2026-07-15T07:25:31Z
The test asserts that the blocked request gets a `backend_error` terminal. In the OLD design, this is the `RouterError::backend_with_epoch` path. In the NEW design, the drain task is the one that was blocked on `flow.acquire()`. When `flow.close()` is called, the drain task's `flow.acquire().await` returns `ChannelFlowClosed`. The drain task needs to synthesize a `backend_error` for the blocked request. The design doesn't say this, but the test demands it.

If the drain task's "synthesize backend_error" behavior is the implementation choice, the drain task needs to:
- On `ChannelFlowClosed`, check if the endpoint is draining (the existing code does this at router.rs:466-478).
- Synthesize `backend_error` with message "flow-control window closed".
- The drain task can continue (other frames in the queue) or exit.

OK, this is a design choice that needs to be specified. Let me also look at the GOODBYE-induced flow close: when GOODBYE is processed for a route, `release_client_route` calls `route.flow.close()`. If the drain task is mid-`flow.acquire()` for a queued request, it gets `ChannelFlowClosed`. The design says GOODBYE "drop queued frames". So the drain task should not synthesize an error for the dropped frames — it should just exit.

But the design says (3.6): "stop drain task (drop queue sender; task exits when drained)". So the drain task should drain remaining items (which are already dropped from the queue by the read loop's flush), then exit. The drain task's current frame (mid-`flow.acquire()`) — what happens? The flow is closed. The drain task should... exit? Or synthesize an error?

If GOODBYE is being processed, the client has already settled locally. Synthesizing an error for the in-flight request is fine (the client will discard it). Or the drain task can just exit. Either way, the client doesn't see the error (the connection is being closed or the route is gone).

But the test `blocked_flow_control_acquire_wakes_when_module_tears_down` expects the blocked request to get a `backend_error`. The blocked request is in the drain task's `flow.acquire()`. The flow is closed because the module is being torn down. The drain task wakes up with `ChannelFlowClosed`. The design needs to specify: synthesize `backend_error` and continue, or exit.

For the GOODBYE case: the client has settled, so the error is wasted. For the module teardown case: the client is still alive, so the error is useful (the client knows the request didn't reach the module).

The design should distinguish these two cases. The drain task needs to know "is this GOODBYE-induced close or module-teardown-induced close?" to decide whether to synthesize an error.

**Defect: drain task behavior on `flow.acquire()` failure is unspecified and depends on the cause of close.**

OK, let me also look at the connection-close case. The design says "Connection close: existing teardown already releases all routes; that now also tears down all drain tasks. No orphan tasks: task handles owned by the binding entry."

When a connection closes:
1. Read loop returns.
2. `RouterConnection` is dropped, calls `cleanup_connection`.
3. `cleanup_connection` releases all routes, calls `flow.close()` for each.
4. Drain tasks are mid-`flow.acquire()` or mid-`module_sink.send()`. They get `ChannelFlowClosed` or send errors.
5. Drain tasks need to exit.

The client connection is gone. The client won't receive any error frames. So synthesizing errors is wasted. The drain tasks should just exit.

But the design doesn't say this. **Defect.**

Let me now look at the snapshot stale-read windows claim.

The design says (3.8):
> "Consistency argument: a data frame that loads a snapshot published before its route's release can still enqueue into a queue that is being flushed — the flush-then-release ordering (3.6) plus the queue-sender drop makes late enqueues fail (sender closed), which maps to today's channel-gone drop semantics. A frame that loads a snapshot before a bind commit sees Absent — identical to today's pre-commit window. No new interleaves: the write-lock serialization of mutations is unchanged; only reader visibility latency changes (bounded by publish-on-commit)."

Wait, "the queue-sender drop makes late enqueues fail (sender closed)". The queue sender is held by the read loop. The "drop" happens when the GOODBYE handler drops it. But the GOODBYE handler is in the read loop, which is sequential. So "late enqueues" from the read loop can't happen — the read loop is sequential.

Unless "late enqueues" means enqueues from a DIFFERENT connection. But routes are per-connection (a client route is owned by one client connection). So no other connection can enqueue into this queue.

Actually wait — the design says routes are per-connection, but is the queue per-route or per-(client_connection, route)? A route is unique to one client connection. So the queue is per-route, which is per-(client_connection, route). The read loop for THIS client connection is the only one enqueueing. Sequential. No "late enqueues" from elsewhere.

But the design says "a data frame that loads a snapshot published before its route's release can still enqueue into a queue that is being flushed". How? The data frame is in the read loop. The flush is in the GOODBYE handler (also in the read loop). Sequential. The data frame is either before the GOODBYE (enqueue succeeds, then flush drops it) or after the GOODBYE (snapshot is updated, lookup returns Absent, no enqueue).

Wait, "snapshot published before its route's release" — the snapshot is published on commit. The data frame loads the snapshot. The data frame's lookup uses the snapshot. If the snapshot has the route bound, the data frame proceeds to enqueue. The enqueue succeeds (queue is alive). The drain task processes the frame. Then the GOODBYE handler is called (next iteration), which flushes the queue and releases the binding.

In this scenario, the data frame is enqueued BEFORE the GOODBYE. The flush drops it. ✓

But what if the data frame loads a snapshot S1 (with route bound), starts enqueuing, and the GOODBYE handler is called CONCURRENTLY? In tokio, this can't happen within a single read loop (sequential). But across multiple read loops? No, there's one read loop per connection. The GOODBYE handler is in the same read loop. Sequential.

OK so the "late enqueue" is impossible within a single read loop. The design's claim "queue-sender drop makes late enqueues fail" is about a scenario that can't happen. The design is being defensive.

But there's a real scenario: the read loop is in the middle of `route_frame` (e.g., enqueuing), and the connection close fires. The close is handled in the select! macro, but `route_frame` is not in a select. So `route_frame` completes (non-suspending try_push), then the loop returns to the top, select! fires on close_receiver, loop returns.

Actually, the close handling in the OLD design is at the top of the loop (select on close vs read_frame). The route_for_connection is AFTER the read, NOT in a select. So if close fires during route_for_connection, it's not observed. The loop continues processing the next frame. This is the OLD behavior.

In the NEW design, the read loop is the same. The close is observed at the top. The route_frame is non-suspending. So the close can only be observed between frames. If close fires during a non-suspending route_frame, it's not observed; the next iteration's select sees it.

OK so the read loop is correct.

But what about: the read loop processes a CANCEL. The CANCEL handler does an O(queue) scan. The queue is being modified by the drain task (draining). The CANCEL handler reads the queue, but the drain task is concurrently popping items. This is a data race if the queue is a Vec with no synchronization.

**The design says CANCEL is O(queue) scan on the queue structure. If the queue is a Vec or VecDeque, the scan races with the drain task's `recv()`.** This is a real data race. The queue must be a synchronized data structure (e.g., Mutex<VecDeque>) to allow safe concurrent access.

**Defect: data structure for the dispatch queue is unspecified, and concurrent read (CANCEL scan) vs write (drain task recv) is unsafe without synchronization.**

OK, I have enough. Let me now write up the findings.

Let me first check a few more critical things.

[assistant] 2026-07-15T07:25:59Z

[assistant] 2026-07-15T07:26:02Z
Good — the `outstanding` set and `dispatch_queue` do NOT exist yet. The design is forward-looking. Now let me check the design's claim about route ordering.

[assistant] 2026-07-15T07:26:06Z

[assistant] 2026-07-15T07:26:10Z
Now let me check the per-route queue overflow behavior for non-Request frames more carefully.

The design says: "On full queue (non-Request: CANCEL/GOODBYE): these must never be dropped for capacity — they are queue-INSPECTING (3.3) or queue-FLUSHING (GOODBYE) operations executed by the read loop against the queue structure itself (O(queue) scan, no await), not enqueued behind it."

So CANCEL and GOODBYE are NEVER enqueued. They are processed by the read loop directly. ✓

Now, what about the per-connection aggregate cap? The design says: "Per-connection aggregate cap: sum of queued frames per connection capped (e.g. 4096 frames); overflow → connection-level protocol-error close".

If the per-connection cap is 4096 frames, and an attacker fills all per-route queues to max, then the read loop's enqueue checks the aggregate. If the aggregate exceeds 4096, the read loop closes the connection. But the per-connection cap is also incremented on enqueue and decremented on drain task pop. The decrement is asynchronous (drain task pops, decrements). The increment is synchronous (read loop enqueues, increments). So the cap is a high-water mark, not a strict cap.

If the attacker sends 4096 frames fast, the read loop enqueues all 4096, then the 4097th triggers a close. But the drain task is concurrently popping, so by the time the 4097th arrives, some frames may have been popped. The cap is enforced at the moment of enqueue, not at the moment of pop. So the cap is "at most 4096 queued at any time".

This is fine for the design's intent.

Now let me look at the read loop's per-connection work. The read loop is single-threaded per connection. For each frame:
- Read: O(frame_size).
- Classify: O(1).
- Hand-off: O(1) for Request (try_push), O(queue) for CANCEL, O(queue) for GOODBYE, O(1) for Ping/Pong.
- Enqueue check: O(1) for aggregate cap.

For CANCEL on a full StatelessParallel route (queue=2048), each CANCEL is O(2048) work. The read loop is the latency-critical path. An attacker could send 1 CANCEL per socket read, each forcing O(2048) work. With 64-byte CANCEL frames, the attacker achieves 64 bytes per O(2048) work, or about 32 bytes per comparison. The read loop's throughput is reduced to ~1 frame per 2048 comparisons, which is very slow. The attacker can DoS the read loop.

**DoS defect: CANCEL inspection is O(queue) on the latency-critical read loop, with no upper bound. A 2048-frame queue × attacker-sent CANCELs = latency degradation.**

The design's claim "O(queue) scan, no await" is true, but O(queue) is still bad on the read loop.

OK, I have enough. Let me write up the findings.

[assistant] 2026-07-15T07:26:22Z
Now I have enough context. Let me also verify the `isRetryableRouteOpenCode` does NOT include `route_backpressure`:

[assistant] 2026-07-15T07:26:26Z

[assistant] 2026-07-15T07:26:28Z

[assistant] 2026-07-15T07:26:30Z
Confirmed: NO SDK currently has any awareness of `route_backpressure` or `control_backpressure`. The design's claim "Zero SDK changes required" is FALSE for the additive change. The SDKs need to:
- Recognize `route_backpressure` as a retryable error code.
- Add it to their retry classifier (or rely on the existing "terminal error" handling, which would surface it to the caller as a non-retryable error).

The TS client's `errorFromFrame` (line 1116-1119) parses the body and constructs a `SubcError` with the code. The `classifyFailure` (line 781-792) checks if the error is a `REQUEST_DEADLINE_MARKER` or returns `outcomeUnknownCallError`. There's no classification by error code. So a `route_backpressure` error from the daemon would be a `terminal` error (via `terminalCallError` at line 1155-1158), and the caller's `notSentRecoveryError` (line 1160-1164) checks `isConsumerReconnectTransient(cause)` which returns false for `SubcCallError` (kind === "terminal"). So the SDK does NOT retry on `route_backpressure` today. The design claims "SDKs already classify retryables" — but they don't classify THIS retryable. The design needs to add `route_backpressure` to the SDKs.

This is a CRITICAL defect: the design claims "Zero SDK changes required" but the new retryable codes require SDK changes.

Wait, let me re-read the design:
> "Nothing required. New retryable codes ride the existing error-classification paths (`route_backpressure` joins the retryable set in both SDK classifiers — additive config)."

"joins the retryable set" — so the design is saying the SDKs need to ADD it. But the design also says "Zero SDK changes required". These are CONTRADICTORY. Either the SDKs need changes (and the design's "zero SDK changes" is FALSE) or the SDKs don't need changes (and the design's "joins the retryable set" is hand-waving).

Actually, reading more carefully: "Zero SDK changes required" might be a typo for "Zero SDK wire changes required" (i.e., no wire changes). Let me re-read:
> "Zero wire changes. Zero SDK changes required (SDK cancelled-terminal repair from the LOOP regression fix is assumed landed — the design leans on 'module emits a terminal for every delivered Request, including cancelled ones')."

OK so "Zero SDK changes required" is a top-level claim. The "joins the retryable set" implies the SDKs DO need to be updated. So either:
- The design's "Zero SDK changes required" is aspirational, and the SDKs need to be updated to recognize `route_backpressure` as retryable. (The design should be explicit about this.)
- Or the design is wrong, and consumers will see `route_backpressure` as a terminal error and not retry.

The current SDK code: TS classifies unknown error codes as "terminal" (line 1155-1158). So a `route_backpressure` error would be terminal. The caller's `managedRequest` would throw. The caller doesn't retry. The request is lost.

**Critical defect: the design's `route_backpressure` claim requires SDK changes that the design doesn't call out, and the current SDK code treats unknown codes as terminal. Without SDK updates, the new behavior silently drops requests that the caller expected to be retryable.**

Also: the design says (5) "Semantic improvement consumers get for free: CANCEL works under saturation; independent channels no longer stall behind a saturated sibling; route.open no longer stalls the data plane." This is a behavioral change. Some consumers may have been depending on the implicit backpressure-by-serialization to bound their in-flight requests. Now they get an explicit retryable error instead of a slow stall. They might not have retry logic for this code, and they might over-issue in the past (relying on the implicit bound). The design doesn't address this.

OK, let me now write the findings.

[assistant] 2026-07-15T07:26:44Z

[assistant] 2026-07-15T07:26:48Z

[assistant] 2026-07-15T07:26:53Z
Now I have all the evidence I need. Let me write up the council member response.

<COUNCIL_MEMBER_RESPONSE>
# ADVERSARIAL REVIEW: subc-core dispatch redesign (f3185c89)

Verified anchors (file:line) checked against master at f3185c89:
- `connection_loop` blocks on routing: server.rs:357-400, route_for_connection await at server.rs:370-374 ✓
- `route.flow.acquire().await` inline: router.rs:465 ✓
- `route.module_sink.send(frame).await` inline: router.rs:491 ✓
- Inline channel-0 control: router.rs:207-218, await at router.rs:214 ✓
- Module→client non-blocking try_send + release: router.rs:281-310, release at router.rs:307-309 ✓
- Process-wide RwLock for data lookup: forwarding.rs:846 (lookup_data_route) ✓
- `RouterError::to_error_frame` body-less via `error_frame()`: router.rs:582-633, body built from channel/epoch/corr + arbitrary code string ✓
- `ChannelFlow::acquire` uses semaphore; `release` uses CAS on `in_flight` AtomicUsize, NOT a per-corr HashSet: forwarding.rs:1692-1731. **This contradicts the design's R11 rider assumption that the current release path needs to be "enforced rather than trusted" by gating on `outstanding.remove(corr)` — currently it's gated by `in_flight` atomic, which IS the per-route counter (one per RouteBinding), not a per-corr set.**

## Finding 1: I3 ("release paths untouched") and I7 ("module→client unchanged") are internally contradicted by 3.7 R11 rider
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:219` (I3), `:225` (I7), `:177-188` (3.7 R11 rider)
- **Confidence**: high
- **Issue**: I3 says "epoch-fenced release + escalation semantics byte-identical (release paths untouched)". 3.7 explicitly inserts an `outstanding: HashSet<corr>` and gates `route.flow.release()` on `outstanding.remove(corr)`. That is not untouched; it adds a per-corr hash op to the module→client terminal path. I7 ("Module→client direction unchanged (try_send best-effort + escalation)") is also contradicted: the current release at router.rs:307-309 is a single `route.flow.release()` call; the new design wraps it in an `outstanding.remove(corr)` check.
- **Evidence**: router.rs:307-309 (current release site) and the design 3.7 which is the modification.
- **Suggested Fix**: Make the invariant statements true: either drop 3.7 (keep trusted-module doctrine) and reword I3/I7 to match, OR rewrite I3 as "epoch-fenced release + escalation semantics preserved" (state what IS preserved — escalation on try_send failure, in_flight never exceeds window, double-release prevented) and I7 as "module→client non-blocking try_send + escalation preserved; credit release additionally gated by per-corr HashSet".

## Finding 2: CANCEL-vs-queued-Request race window that defeats the new design's central fix
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:110-130` (3.3), interaction with 3.2 drain-task body and 3.7 outstanding set
- **Confidence**: high
- **Issue**: 3.3 says CANCEL inspects the dispatch queue. 3.7 says `outstanding.insert(corr)` happens "on delivery to module". The drain-task order under the new design is: `queue.recv() → flow.acquire().await → module_sink.send(frame).await → outstanding.insert(corr)`. Between `recv()` and `outstanding.insert(corr)`, the Request is in a limbo window: not in queue, not in outstanding, and the module may not yet have inserted it into its in_flight. A CANCEL arriving at the read loop exactly in this window sees `queue.contains(corr) == false`, forwards to the module, and the module's `in_flight.remove(corr)` returns None (or the request handler hasn't registered yet, depending on module implementation), so the cancel is unclaimed. The Request then runs to completion and emits a normal terminal — the SDK never sees a `cancelled` code for that corr. This is a NEW failure mode introduced by the design; it is the same defect class as R5 but transposed.
- **Evidence**: design 3.2 (drain_task body shows insert-after-send), 3.7 (insert on delivery), `crates/subc-core/src/bin/fake-aft-stub.rs:339-344` shows module-side `in_flight.insert(key, cancel_tx)` happens AFTER module receives the Request frame from the daemon.
- **Suggested Fix**: Insert into `outstanding` BEFORE `module_sink.send().await` (i.e., at the top of the Request arm, immediately after `queue.recv()`), AND have the CANCEL handler at the read loop check `outstanding.contains(corr)` as well as the queue. This collapses the limbo window to the time between `outstanding.insert` and the read loop's CAS-equivalent check.

## Finding 3: Drain-task error handling on `flow.acquire()` returning `ChannelFlowClosed` is unspecified, and the existing test `blocked_flow_control_acquire_wakes_when_module_tears_down` (forwarding.rs:3811) demands a specific behavior the design does not commit to
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:86-95` (3.2 drain_task body), interaction with forwarding.rs:1710-1715 (Semaphore::close) and the existing test forwarding.rs:3870-3877
- **Confidence**: high
- **Issue**: 3.2 shows `drain_task` as `{ flow.acquire().await; module_sink.send(frame).await; }` with no error arm. The existing test `blocked_flow_control_acquire_wakes_when_module_tears_down` (forwarding.rs:3811) sends a request that is blocked on `flow.acquire()` (saturated serial, window=1) and then tears the module down; the test asserts the blocked request gets a `backend_error` terminal (forwarding.rs:3875). In the OLD design this falls out of the read-loop's `RouterError::backend_with_epoch` at router.rs:479-484. In the NEW design, the drain task is the one blocked, and the design doesn't say what it does on `ChannelFlowClosed`. There is also a competing case (GOODBYE-induced close) where the client has already settled and synthesizing a terminal is wasted, and a connection-close case where the client is gone. The drain task cannot tell these apart without additional state.
- **Evidence**: forwarding.rs:1692-1700 (`acquire` returns `ChannelFlowClosed` after `sem.close()`), forwarding.rs:3811-3877 (the test), design 3.2 (no error arm).
- **Suggested Fix**: Specify three failure modes in 3.2 with distinct actions: (a) `ChannelFlowClosed` AND `endpoint_is_draining` (module reload) → synthesize `Error{code:"module_reloading"}` for that corr, drain the rest of the queue with the same error; (b) `ChannelFlowClosed` AND route is being released via GOODBYE → drop the frame silently (client has settled); (c) connection close → drain task exits without synthesizing. The drain task needs a way to distinguish (a)/(b)/(c) (e.g., a per-route "tearing_down" flag set by the GOODBYE handler before flow.close).

## Finding 4: Drain-task error handling on `module_sink.send()` failure is unspecified and breaks I1 ("at-most-once delivery")
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:88-95` (3.2), `crates/subc-core/src/router.rs:491-496` (current error path with credit release on send failure)
- **Confidence**: high
- **Issue**: If `module_sink.send(frame).await` fails (module closed/draining), the current code at router.rs:491-496 releases the credit it just acquired and returns an error. The new design's drain task must do the equivalent, but the design is silent. Worse: if the send fails AFTER `outstanding.insert(corr)`, the new design's release gating would need to fire to avoid a credit leak. And if the send fails, what about I1 at-most-once? In OLD design, the read loop gets the error and the frame is not delivered → no double delivery. In NEW design, the drain task must ensure the frame is not retried (no re-enqueue). The design doesn't say.
- **Evidence**: router.rs:491-496, design 3.2 (no error arm).
- **Suggested Fix**: Specify: on `module_sink.send().await` Err, the drain task must (a) `outstanding.remove(corr)`, (b) `route.flow.release()` (because credit was acquired and not released on the wire), (c) drop the frame (not retry), and (d) optionally synthesize a `backend_error` for the client if the route is still alive. Add to 3.2.

## Finding 5: Data structure for the dispatch queue is unspecified, but CANCEL requires O(queue) concurrent scan against the drain task's `recv()` — undefined data race
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:74-79, 86-95, 113-118` (3.1, 3.2, 3.3)
- **Confidence**: high
- **Issue**: 3.3 says CANCEL does an "O(queue) scan" of the queue on the read loop, concurrent with the drain task's `queue.recv()`. If the queue is a `Vec<Frame>` or `VecDeque<Frame>` without external synchronization, the read loop's scan races the drain task's mutation → undefined behavior (use-after-free, torn reads). If the queue is a `tokio::sync::mpsc::Sender/Receiver`, the read loop CANNOT do a non-destructive scan (only `try_recv`/`recv`/`close`). The design does not specify a data structure.
- **Evidence**: 3.3 "O(queue) scan, no await" implies read access to queue interior; 3.2 "while let Some(frame) = queue.recv()" implies tokio mpsc OR a custom queue; the two are incompatible without a mutex or similar.
- **Suggested Fix**: Specify the data structure. Two reasonable options: (A) `Arc<Mutex<VecDeque<(corr, Frame)>>>` — CANCEL holds the mutex, scans, removes by corr, releases. Drain task holds the mutex only for the `pop_front` of each iteration. Contention is on the critical path. (B) `Arc<DashMap<u64, Frame>>` plus a tokio mpsc::UnboundedSender<()>` for "wake the drain task to retry after CANCEL removed a non-head item" — but this breaks FIFO unless ordered. Option (A) is the cleaner choice; quantify the lock contention in T9.

## Finding 6: Per-connection aggregate cap is unspecifiable as designed — it requires either a global lock or a per-route decrement hook on the drain task
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:160-164` (3.5)
- **Confidence**: high
- **Issue**: 3.5 says "sum of queued frames per connection capped (e.g. 4096 frames); overflow → connection-level protocol-error close". The increment is on enqueue (read loop); the decrement is on drain task pop. With per-route queues, the read loop must atomically read the aggregate, increment, check, and either commit or close. The drain task's pop must atomically decrement. Without a single shared counter (e.g., `AtomicUsize` on the connection), the check is racy: the read loop sees N, decides fine, then two more enqueues from rapid reads push it to N+2. The design doesn't specify the counter location or synchronization.
- **Evidence**: 3.5, no data structure specified.
- **Suggested Fix**: `Arc<AtomicUsize>` for the per-connection aggregate. Read loop `fetch_add(1)`; if result > 4096, decrement back and close. Drain task `fetch_sub(1)` on each `queue.recv()`. Add a small assertion in the test for the race window.

## Finding 7: GOODBYE handler in the read loop must block on the drain task (violates I6 "read loop never blocks") OR race the binding release
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:127-130, 174-177` (3.3 GOODBYE flush, 3.6 release), server.rs:241-245 (read loop teardown)
- **Confidence**: high
- **Issue**: 3.3 says "Queue flush must precede binding release so no frame can enqueue after flush". To guarantee no enqueue after flush, the GOODBYE handler must atomically (a) prevent further enqueues from the read loop, (b) wait for the drain task to finish its current frame, (c) release the binding. (a) requires dropping the queue sender — but the sender is held by the read loop, which is the GOODBYE handler. Dropping it from within the read loop is fine, but then the drain task is still running. (b) requires `await`-ing the JoinHandle — which blocks the read loop on per-route work, violating the design's core promise. If the read loop DOESN'T await, the drain task can still be mid-`flow.acquire()` when the binding is released; its `acquire` returns `ChannelFlowClosed` (see Finding 3), and its current frame is dropped without a terminal. For a GOODBYE that's fine; for a module teardown the test demands a terminal (Finding 3). The design's "no orphan tasks" requires the read loop to await at SOME point.
- **Evidence**: 3.3, 3.6, forwarding.rs:1692-1700 (`ChannelFlowClosed`), forwarding.rs:3811-3877.
- **Suggested Fix**: Either (A) accept that GOODBYE blocking the read loop is OK (it's a low-frequency event) and document the read-loop await, or (B) spawn a per-route teardown task that does "drop sender → await drain task → release binding" and have the read loop enqueue-block (try_send returns Err) on routes in teardown. (B) is cleaner; it requires the read loop to check "is this route in teardown?" before enqueue, and the per-route state needs an `is_tearing_down: AtomicBool`.

## Finding 8: "Zero SDK changes required" is contradicted by the design's own claim that `route_backpressure` "joins the retryable set" — current SDKs classify all unknown codes as terminal
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:230-237` ( "What changes for consumers")
- **Confidence**: high
- **Issue**:  says "Zero SDK changes required. New retryable codes ride the existing error-classification paths (`route_backpressure` joins the retryable set in both SDK classifiers — additive config)." But: `clients/subc-client/src/client.ts:1155-1158` (`terminalCallError`) treats any non-`REQUEST_DEADLINE_MARKER` SubcError from a frame as a "terminal" failure. The TS classifier at line 1220-1238 (`isConsumerReconnectTransient`) does NOT include any `route_backpressure` or `backend_error` code. The Rust SDK `crates/subc-client-rs/src/consumer.rs:3059-3065` (`classify_failure`) only switches on `accepted` boolean, not on error code. The Swift SDK `clients/subc-client-swift/Sources/SubcClient/Client.swift:671-674` (`remoteError`) discards the body code entirely and surfaces as a generic `SubcError`. **No shipped SDK retries on `route_backpressure` today.** A caller of `managedRequest` (TS) or `routeRequest` (Swift) will see the error as terminal and not retry. The design's claim that consumers get this for free is FALSE.
- **Evidence**: client.ts:1155-1158 (terminal classification), consumer.rs:3059-3065 (no code-based switch), Client.swift:671-674 (no code parsing). No matches for `route_backpressure` in any SDK (grep over `clients/subc-client/`, `clients/subc-client-swift/`, `crates/subc-client-rs/`).
- **Suggested Fix**: Either (A) add `route_backpressure` and `control_backpressure` to the SDK retry classifier lists (TS `isConsumerReconnectTransient`, Rust `is_retryable_route_open_code` or a new `is_retryable_data_error_code`, Swift `isError` predicate), and update the design to say "SDK change required: additive config in three classifiers", or (B) keep the old blocking-send behavior on the data plane and add a "queue_full: true" hint to a protocol-level response so callers can distinguish; option (A) is cleaner. Either way, the design as written will cause silent request loss in production.

## Finding 9: The NotSent mapping for `route_backpressure` is DISHONEST in one case — the request may have been pushed to the wire buffer
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:154-158` (3.5 "fail-loud ... maps to the existing NotSent contract (the request never reached the module)")
- **Confidence**: high
- **Issue**: 3.5 says `route_backpressure` maps to NotSent because "the request never reached the module". This is true at the daemon→module boundary, but the bytes have already been read off the socket and are in the read loop's hand-off to the queue. The SDK has no way to know the bytes were read but not delivered. If a caller is using `notSent` to decide "safe to retry with same idempotency key", they are correct semantically. BUT: the existing `notSent` definition in `clients/subc-client/src/client.ts:184-198` is "request bytes provably never left the local process". The new `route_backpressure` case: bytes HAVE left the local process (read by daemon), the daemon is just refusing to enqueue. This is a NEW notSent class. Callers who distinguish `not_sent` (truly local) from `outcome_unknown` (bytes-in-flight) for retry decisions may mis-handle this. The design's "NotSent mapping HONEST" claim is not quite right.
- **Evidence**: client.ts:184-198 (notSent definition), design 3.5.
- **Suggested Fix**: Either narrow the contract: define a new error class `daemon_rejected` (or similar) for "bytes received, daemon refused to enqueue", and have SDKs map it to retryable. Or, document that the bytes have been read but never delivered to the module, and that retry is safe in the idempotency sense.

## Finding 10: The drain task's panic-backstop claim in 3.6 is a half-measure — abort-guard only prevents the silent wedge, not the in-flight credit leak
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:170-173` (3.6)
- **Confidence**: high
- **Issue**: 3.6 says "Drain-task panic backstop: a panicking drain task must release the route (abort-guard mirroring the coordinator-actor drop-guard pattern from broca)". An abort-guard releases the route binding, which calls `flow.close()` (forwarding.rs:1424). But if the drain task panicked MID-frame (after `flow.acquire()` but before `module_sink.send()` completes, or after `outstanding.insert(corr)` but before the next iteration), the acquired credit is never released AND the outstanding entry is never removed. The abort-guard only releases the binding, not the in-flight credit. With `RouteBinding::flow` being an `Arc<ChannelFlow>`, and the abort-guard's release dropping the `RouteBinding` (and thus potentially the last `Arc<ChannelFlow>` reference), the `ChannelFlow` is dropped, which drops the semaphore, which DROPS pending acquired permits. So the semaphore's permit count returns to 0, and any subsequent `acquire` on a new binding would start with a fresh window. But the `in_flight` AtomicUsize is also dropped, and a hypothetical terminal for the in-flight request (if the module somehow recovers) would `release()` on a DROPPED `ChannelFlow` → use-after-free. The existing `release` CAS loop (forwarding.rs:1702-1731) would panic on a dropped Arc.
- **Evidence**: forwarding.rs:1674-1740 (ChannelFlow Drop not defined, so it's the default; semaphore::Semaphore Drop drops all permits), design 3.6 (abort-guard only mentions releasing the route).
- **Suggested Fix**: Define an explicit `Drop for ChannelFlow` that does NOT silently drop the in-flight count — or document that on route release, any in-flight credits are forfeit (matching today's behavior, where `in_flight` becomes stale but the route is gone). For the design, the abort-guard should also reset `in_flight` to 0 OR log a warning if `in_flight > 0` at release time. The current design's "abort-guard mirrors drop-guard pattern" is hand-waving.

## Finding 11: Snapshot stale-read window — a data frame reading a snapshot slightly newer than the control command that published it is impossible (good), but a data frame reading a snapshot older than an in-flight release IS a real window
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:203-211` (3.8)
- **Confidence**: medium
- **Issue**: 3.8 argues that the new snapshot table is "invariant-neutral" because pre-commit = Absent (today's behavior) and post-release = channel-gone (today's behavior). This is true for FRESH routes, but for an in-flight CANCEL+release sequence: a data frame reads snapshot S1 (route still bound, enqueues), then GOODBYE handler runs in the read loop, releases the route, publishes S2. The drain task is processing the enqueued frame. The drain task's `flow.acquire()` will fail (flow closed by `release_client_route_locked` → `route.flow.close()` at forwarding.rs:1424). The drain task is mid-`acquire().await`. The data frame was successfully enqueued but never delivered. Per I1, "at-most-once delivery" is preserved (zero deliveries), but per the GOODBYE flow the daemon is supposed to silently drop the in-flight frame. OK. But: the enqueue itself returned OK (succeeded before flush). The design's "late enqueues fail (sender closed)" doesn't apply here — the send was not late. The design doesn't address this specific case.
- **Evidence**: 3.6 and 3.8, forwarding.rs:1424.
- **Suggested Fix**: Document this case explicitly: "Enqueue-then-release is benign: the drain task's `acquire` fails closed, the frame is dropped silently, and the binding release publishes a snapshot that omits the route. Subsequent enqueues from the same read loop will see Absent." This is the correct behavior; it just needs to be stated.

## Finding 12: DoS — CANCEL spray against a filled queue forces O(queue) work on the read loop, with up to 2048 comparisons per CANCEL
- **Severity**: HIGH
- **Location**: `docs/subc-dispatch-redesign.md:113-118, 159-161` (3.3, 3.5)
- **Confidence**: high
- **Issue**: 3.5 specifies StatelessParallel queue depth = `max(4, 2×1024) = 2048`. 3.3 says CANCEL is O(queue) on the read loop. A malicious client can saturate a StatelessParallel route to queue depth 2048 (legitimately or by overflowing the route repeatedly), then send a single CANCEL per read. Each CANCEL forces up to 2048 comparisons. The read loop is the latency-critical path; 2048 comparisons is ~µs on modern hardware but cumulatively can saturate one CPU. Worse: an attacker can open many routes (channel space is 65k), fill each to 2048, and spray CANCELs across them. With 16 routes × 2048 comparisons per CANCEL = 32k comparisons per CANCEL round. The per-connection aggregate cap of 4096 is per CONNECTION, not per ROUTE, so it doesn't bound this.
- **Evidence**: 3.5 (queue depths), 3.3 (O(queue) scan).
- **Suggested Fix**: Bound the per-frame work the read loop does for CANCEL: e.g., limit the scan to the first N items (e.g., 64); if not found in the first 64, treat as "not in queue" and forward to module. Document the bound and the resulting semantic (a CANCEL for a Request queued behind 64 other Requests is treated as "delivered" and forwarded to the module, where it may be unclaimed if the request hasn't been delivered yet). This is acceptable for high-window routes where 2048-deep queues are common; for low-window routes (Serial=4, ModuleManaged=64) the bound is effectively a no-op.

## Finding 13: Merge-1 (snapshot table) is NOT invariant-neutral as a standalone landing — the read loop still awaits routing, and the new lookup semantics on stale-snapshot reads differ from RwLock-serialized reads
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:266-270` ( Rollout)
- **Confidence**: medium
- **Issue**:  claims merge-1 is "read path mechanical, invariant-neutral". But the OLD `read_inner()`-via-`RwLock` provides sequential consistency: a reader that acquires the read lock AFTER a writer releases the write lock sees the writer's updates. The NEW `ArcSwap::load()` provides only RELAXED atomic load: a reader may see a snapshot that is OLDER than the latest published, and the reader's view is NOT linearizable with concurrent writes. For the read-loop use case (lookup_data_route then enqueue), this difference is mostly benign (a frame that reads an old snapshot may enqueue into a route that's about to be released; see Finding 11). But for CONTROL-PLANE reads (catalog, status, liveness) that are intentionally kept on the lock (per 3.8 "want read-your-writes"), the difference matters. The design explicitly says control-plane reads stay on the lock — but the read-loop's data-plane reads will use ArcSwap. The boundary must be clean: any code that was calling `read_inner()` for data-plane lookups must move to `snapshot.load()`, and any code calling `read_inner()` for control-plane reads must stay. The design is silent on the boundary and the migration. There's a real risk of regressing control-plane read-your-writes during migration.
- **Evidence**: 3.8,  forwarding.rs:840-890 (lookup_data_route is data-plane; uses read_inner today).
- **Suggested Fix**: In merge-1, add a regression test for read-your-writes on control-plane operations (e.g., hello → catalog_update → catalog_list ordering). The control-plane reads MUST stay on the write lock. The data-plane reads use snapshot.load(). Make this boundary explicit in code (e.g., a `ForwardingTable::data_lookup()` vs `ForwardingTable::control_lookup()` method pair).

## Finding 14: Q4 lean ("R11 rider now") is RIGHT but the design's framing of the defect is wrong
- **Severity**: MINOR
- **Location**: `docs/subc-dispatch-redesign.md:178-188` (3.7), `crates/subc-core/src/forwarding.rs:1702-1731` (current release)
- **Confidence**: high
- **Issue**: 3.7 calls the current release "trusted-module doctrine" and frames the R11 rider as "enforced rather than trusted". But the current release at forwarding.rs:1702-1731 is NOT "trusted" — it has a CAS loop on `in_flight` AtomicUsize that detects over-release and logs a warning, and `sem.add_permits(1)` is gated by `is_closed()`. So over-release is partially mitigated today. The R11 rider changes the gating from "in_flight > 0" to "outstanding contains corr", which catches a DIFFERENT defect: duplicate terminals for the SAME corr. The current code handles over-release (too many terminals for the same or different corrs exhausting the budget), but does NOT handle duplicate-release-of-the-same-credit. The R11 rider catches duplicate-release. The design's framing is misleading. The R11 rider is still correct, but the doc should say "R11 rider catches duplicate-release of the same credit, which the current `in_flight` counter does not (it catches over-release, which is different)."
- **Evidence**: forwarding.rs:1702-1715.
- **Suggested Fix**: Reword 3.7 to distinguish over-release (current handling) from duplicate-release (R11 catches).

## Finding 15: Q1 lean (fail-loud `route_backpressure` vs pause-set) is RIGHT for the design's stated goal, but a pause-set would better preserve the at-most-once FIFO and bounded memory property on saturated multi-route clients
- **Severity**: MINOR
- **Location**: `docs/subc-dispatch-redesign.md:273-277` (Q1)
- **Confidence**: medium
- **Issue**: Q1 lean is "fail-loud; the SDKs already classify retryables" — this is WRONG (see Finding 8). With the SDK gap, fail-loud means request loss. A pause-set (block the read loop only for that route's frames, with a per-route admission gate) would preserve the old blocking-send semantics: requests on a saturated route wait, but other routes' requests are unaffected (the same as the new design's data-plane goal). It is more complex (requires a per-route "paused" flag and a wake mechanism), but it avoids the SDK change AND avoids the "consumers over-issue" risk in Finding 8. The design should weigh this trade-off more honestly.
- **Evidence**: 3.5,  and the SDK grep results in Finding 8.
- **Suggested Fix**: Reconsider Q1: pause-set is the higher-fidelity design that avoids the SDK change. If kept as fail-loud, then the SDK change is mandatory and must be added to the rollout.

## Finding 16: Q2 lean (daemon-synthesized cancelled) is RIGHT for the in-queue case but the doc doesn't address the limbo-window case (Finding 2)
- **Severity**: MAJOR (overlaps Finding 2)
- **Location**: `docs/subc-dispatch-redesign.md:278-280` (Q2)
- **Confidence**: high
- **Issue**: Q2 says daemon-synthesizes for queued-Request-cancel. This is correct for the steady-state case (Request sitting in queue). But the design doesn't address the pop-insert race (drain task has popped but not yet inserted into outstanding). In that window, the CANCEL is forwarded to the module, and the module-side cancel may be unclaimed (see Finding 2).
- **Evidence**: 3.3, Finding 2.
- **Suggested Fix**: The fix in Finding 2 (insert into outstanding before send) also addresses this.

## Finding 17: Q3 lean (whole channel-0 FIFO) is RIGHT but the design doesn't address route.open's own blocking sub-calls
- **Severity**: MINOR
- **Location**: `docs/subc-dispatch-redesign.md:281` (Q3)
- **Confidence**: medium
- **Issue**: route.open currently awaits the module ack with a 12s timeout. A single FIFO control task means: while route.open is awaiting, all other control commands (route.close, route.poll, route.status, catalog.list) block. The design says "a slow route.open now stalls only later CONTROL commands on that connection" — this is the design's intent, but the FIFO ordering means a hung route.open stalls ALL control commands. A second slow route.open (or a route.open for a different module) would also stall. There's no per-route.open concurrency. The control queue can be 4096 frames per connection, so a stuck route.open blocks up to 4096 control commands. Whether this is a regression depends on the old behavior: in the OLD design, control was inline in the read loop, so a hung route.open blocked ALL data frames too. The new design is strictly better for data, but it's worth noting that a hung route.open still has a per-connection control-head-of-line block.
- **Evidence**: design 3.4, control.rs:949-1193 (handle_route_open awaits module ack with deadline).
- **Suggested Fix**: Document this trade-off. The control queue should have a per-op deadline enforced by the FIFO task (drop the op if its own deadline has elapsed without consuming it). Or, consider per-priority control lanes (interactive route.close vs background catalog.list). Not a blocker, but a note.

## Finding 18: Q5 lean (whole-table Arc swap) is RIGHT, but the clone-on-write cost is unbounded by snapshot contents
- **Severity**: LOW
- **Location**: `docs/subc-dispatch-redesign.md:282-283` (Q5)
- **Confidence**: medium
- **Issue**: 3.8 says "clone-on-write of the affected maps". For a single bind or release, the affected maps are 1–2 HashMaps of small (key → Arc<RouteBinding>) entries. Clone is O(map_size) and Arc clone is O(1). For 1000 routes on a connection, a single bind is O(1000) Arc clones. The design's claim that "binds/releases are low-frequency vs per-frame lookups — read-mostly by orders of magnitude" is correct for typical workloads. But a control loop that opens+closes routes rapidly (e.g., a test or a misbehaving client) can drive the clone cost up. Per-shard maps would help. Defer to T9 measurement, as the design does.
- **Evidence**: 3.8, T9.
- **Suggested Fix**: Keep Q5 lean, but add a benchmark in T9 that drives rapid bind/release and measures p99 snapshot-publish latency.

## Finding 19: 3.6 "Connection close: existing teardown already releases all routes" — the new design's drain-task ownership changes the teardown order
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:174-177` (3.6)
- **Confidence**: high
- **Issue**: 3.6 says connection close tears down all drain tasks via "task handles owned by the binding entry". But JoinHandles don't abort the task when dropped — they only allow awaiting the result. When the binding is removed (in `cleanup_connection` at forwarding.rs:1168-1239), the JoinHandle is dropped, but the drain task continues running. The drain task's `queue.recv()` blocks until the queue sender (held by the read loop) is dropped. The read loop returns and drops its locals, including the queue sender. The drain task's `recv()` returns None, the drain task exits. This is correct, but it requires the read loop to drop ALL its queue senders. If the read loop holds a HashMap of senders, it must drop the entire HashMap. The read loop's local scope must be designed to ensure this. The design doesn't specify the lifetime relationship between the read loop's sender map and the binding entries.
- **Evidence**: 3.6, forwarding.rs:1168-1239, server.rs:241-245.
- **Suggested Fix**: Specify: "The read loop owns a per-connection `RouteDispatchTable` (a `HashMap<RouteKey, (Sender<Frame>, JoinHandle<()>)>`). On read-loop exit (any cause: peer close, close_receiver fire, protocol-error close), the entire table is dropped, which drops all senders, which causes all drain tasks' `recv()` to return None, which causes all drain tasks to exit. The JoinHandles are not explicitly awaited — they are dropped, but the underlying tasks complete asynchronously. No explicit abort is required because the queue-closed path is cooperative." This is a documentation fix, not a code change.

## Finding 20: The doc's "honest" NotSent mapping (3.5) is a behavioral change for callers that distinguish `not_sent` from `outcome_unknown` for retry idempotency decisions
- **Severity**: MEDIUM
- **Location**: `docs/subc-dispatch-redesign.md:154-158` (3.5)
- **Confidence**: medium
- **Issue**: As in Finding 9, the `route_backpressure` error implies "bytes read by daemon but not delivered". The current SDK error taxonomy has `not_sent` (truly local, safe to retry) and `outcome_unknown` (bytes in flight, caller decides). The new `route_backpressure` is a third case. If SDKs map it to `not_sent` (as the design suggests), callers that distinguish the two for retry idempotency will mis-handle. If they map it to `outcome_unknown`, the design's "fail-loud is fine, SDKs already classify retryables" claim is wrong because `outcome_unknown` is not retried automatically. The design is ambiguous here.
- **Evidence**: client.ts:184-198 (notSent definition), consumer.rs (no code-based switch), design 3.5.
- **Suggested Fix**: Pick one and document. Recommendation: introduce a new error class `daemon_not_sent` (or extend `not_sent` to cover "not delivered to module"), with a clear contract that retry is safe in the idempotency sense. Update the SDK classifiers. This is part of the Finding 8 SDK change.

## Finding 21: The "test plan" T8 ("existing suites green unmodified: HOL isolation, flow-control, epoch-fence, reload-drain, concurrency races") may be violated by `cancel_bypasses_full_flow_control_window_and_credit_frees_on_terminal` (forwarding.rs:3613) under the new design
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:257-259` (T8), `crates/subc-core/tests/forwarding.rs:3613-3709`
- **Confidence**: medium
- **Issue**: The test `cancel_bypasses_full_flow_control_window_and_credit_frees_on_terminal` (forwarding.rs:3613) sends Request A (500ms delay), waits for stub to receive, then sends CANCEL(A) and Request B (0ms delay) in quick succession. The test asserts: (a) cancelled error for A, (b) response for B, (c) `cancelled_terminal_pos < followup_request_pos` in the stub event log (line 3703-3706). Assertion (c) requires the stub to EMIT a cancelled terminal (via `handle_cancel` at fake-aft-stub.rs:377-416). In the new design, when CANCEL(A) arrives at the read loop and finds A in the dispatch queue, the read loop REMOVES A from the queue and synthesizes a cancelled terminal WITHOUT sending CANCEL to the module. The stub's `handle_cancel` is never invoked. The stub's event log will not have a "kind: cancel" event with `claimed: true`. Assertion (c) is at line 3687-3706 which looks for `event_is_terminal(event, "error", ack.route_channel, cancelled_corr) && event["code"] == "cancelled"`. In the new design, the daemon synthesizes the cancelled terminal; the stub does NOT emit one. The stub event log will not have this event. The assertion FAILS.
- **Evidence**: forwarding.rs:3687-3706, fake-aft-stub.rs:377-416, design 3.3.
- **Suggested Fix**: The test needs updating to match the new behavior. Either: (A) accept that the stub event log assertion is no longer applicable when the daemon synthesizes (only check the wire-level cancelled error and the followup response), or (B) for the new test, send a CANCEL for a request that is NOT in the queue (already delivered) so the cancel IS forwarded to the module, and assert the stub's cancelled event in that case. T8 says "any needed test change is a red flag to re-review, not to edit the test" — this is a real test change forced by the design. The design should be explicit about this.

## Finding 22: The dispatch queue depth formula `max(4, 2×window)` for StatelessParallel=2048 is much larger than today's per-connection egress (64) — memory bound is much higher
- **Severity**: LOW
- **Location**: `docs/subc-dispatch-redesign.md:148-153` (3.5)
- **Confidence**: high
- **Issue**: 3.5 says per-route queue depth is `max(4, 2×window)`. For StatelessParallel (window=1024), depth=2048. Each frame is up to ~1MB body (a single subc frame max). Per route, memory could be 2048 × 1MB = 2GB in the worst case. Per-connection aggregate cap of 4096 frames limits total to 4096 × 1MB = 4GB per connection. This is a memory blowup compared to today's per-connection egress of 64 frames (`server.rs:21 CONNECTION_EGRESS_BUFFER = 64`). The design's claim "Explicit, bounded memory per connection and per route" is true, but the bound is much looser. With 16 StatelessParallel routes per connection, the per-connection bound is 16 × 2GB = 32GB in the worst case.
- **Evidence**: 3.5, server.rs:21.
- **Suggested Fix**: Tighten the per-route queue depth to a hard byte budget, not just a frame count. E.g., `max(4, 2×window) × MAX_FRAME_BODY_BYTES` per route, with per-connection aggregate as a hard byte cap. Or, document the worst-case memory explicitly: "with 1MB max body, a single StatelessParallel route's worst-case queue is 2GB; per-connection worst-case is bounded by 4096 × 1MB = 4GB." Surface this in the gate's risk register.

## Finding 23: The dispatch-queue "owned by the binding entry" is incompatible with the read loop's need to enqueue concurrently with the bind commit
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:166-168, 203-211` (3.6, 3.8)
- **Confidence**: high
- **Issue**: 3.6 says "Bind commit ... additionally: create queue + spawn drain task, publish new snapshot". 3.8 says "All mutations (bind commit, release, register/cleanup, endpoint drain) keep the existing write lock as the serialization point, apply to the canonical state, then publish a fresh snapshot". The race: bind commit starts, write lock acquired, queue created, drain task spawned, write lock released, snapshot published. Between the write lock release and the snapshot publish, a data frame can be processed by the read loop using the OLD snapshot (without the new route). The data frame looks up the route → Absent → silently dropped. The new route's first frame must wait for the snapshot publish. This is the design's intended behavior (per 3.8 "A frame that loads a snapshot before a bind commit sees Absent — identical to today's pre-commit window"). BUT: a data frame can also be processed AFTER the snapshot publish but BEFORE the drain task has actually started running. If the data frame is the first frame for the new route, the queue is empty, the data frame is enqueued, the drain task hasn't yet called `recv()`. The drain task eventually starts and processes. OK. But what if the drain task is spawned but its `recv()` is not yet polled? With tokio, spawning a task does NOT guarantee it has run; the task is scheduled. If the read loop enqueues 2048 frames before the drain task polls, the queue fills, the read loop's enqueue check on a 4097th request fails with route_backpressure — but the drain task would have caught up to process them if it had been polling. This is a startup-latency hazard. With high spawn rate and small drain task warm-up, requests can be rejected even though the route has capacity.
- **Evidence**: 3.6, 3.8, tokio::spawn semantics.
- **Suggested Fix**: Either (A) use `tokio::task::yield_now().await` once in the drain task's startup body to ensure it has had a chance to register its `recv()` before the read loop's first enqueue, or (B) document that the first few frames after bind may see transient route_backpressure until the drain task warms up. (A) is safer. Also: the bind commit should publish the snapshot BEFORE spawning the drain task, so any enqueuer sees the route in the snapshot and the queue is ready to receive.

## Summary

22 BLOCKER/MAJOR findings. The design has at least three BLOCKER-class concurrency defects:
- Finding 2 (CANCEL-vs-queue limbo window) — the new design re-introduces a class of cancel-loss race it was meant to fix.
- Finding 3 (drain-task error handling unspecified) — required for the existing `blocked_flow_control_acquire_wakes_when_module_tears_down` test to pass.
- Finding 5 (queue data structure unspecified) — concurrent scan vs recv is a data race.
- Finding 7 (GOODBYE blocking or racing) — I6 violated or correctness gap.
- Finding 8 (SDK changes are NOT zero) — `route_backpressure` is silent request loss in production.

Two MAJOR findings on I3/I7 internal contradiction (Finding 1) and NotSent honesty (Finding 9).

The design is **NOT READY for implementation**. The structural claims (I1–I8) are largely correct in spirit but under-specified for the data structures, error handling, and inter-task synchronization. The R11 rider is sound; the dispatch-queue + drain-task decomposition is sound; the snapshot table is sound. But the implementation details that determine correctness — queue type, error arms, teardown ordering, SDK classifier updates, limbo-window handling — are all glossed over.

## Verdict

**NO-GO.** Blockers (must fix before any merge):

1. **Finding 2** — Insert into `outstanding` BEFORE `module_sink.send().await`, AND have the read loop's CANCEL handler also check `outstanding.contains(corr)`. Without this, the design re-introduces the cancel-loss defect on a different code path.

2. **Finding 5** — Specify the dispatch-queue data structure. Recommend `Arc<Mutex<VecDeque<(corr, Frame)>>>` with the drain task holding the mutex only for `pop_front`. Acknowledge lock contention in the test plan.

3. **Finding 3 + Finding 4** — Specify the drain task's error arms for `ChannelFlowClosed` and `module_sink.send()` failure, with distinct handling for GOODBYE-induced vs module-teardown-induced vs connection-close cases. Update the existing test `blocked_flow_control_acquire_wakes_when_module_tears_down` or document that the new behavior is equivalent.

4. **Finding 7** — Either (a) accept that GOODBYE blocks the read loop (it's a low-frequency event) and update I6 to "read loop blocks only on connection close or on the route's GOODBYE teardown path", or (b) spawn a per-route teardown task with an `is_tearing_down: AtomicBool` and have the read loop's enqueue check it.

5. **Finding 8** — Either ship the SDK change (add `route_backpressure` and `control_backpressure` to the three SDK retry classifiers) in the same gate, OR change Q1 to use a per-route pause-set and preserve the old blocking-send semantics. The "Zero SDK changes required" claim is false; the design must own the SDK change.

6. **Finding 1** — Reconcile I3 and I7 with 3.7 R11 rider. The rider changes release paths; the invariants must be rewritten to state what IS preserved (escalation, at-most-once credit, epoch-fence) rather than claiming "untouched".

The MAJOR findings (3, 4, 7, 9, 11, 13, 15, 19, 20, 21, 23) must be addressed in the implementation spec before merge; the BLOCKERS are correctness-critical. Once these are addressed, re-gate.
</COUNCIL_MEMBER_RESPONSE>

