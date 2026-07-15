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