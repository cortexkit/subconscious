## Finding 1: Module-to-Client Data Frame Race (Stale Snapshot Read)
- **Severity**: critical
- **Location**: `crates/subc-core/src/router.rs:207-218` (read loop routing) and `crates/subc-core/src/forwarding.rs:753` (`commit_route_locked`)
- **Confidence**: high
- **Issue**: In the redesign, control frames (like `RouteBindAck` from the module) are offloaded to a per-connection control queue and processed asynchronously by the control task. However, data frames are processed immediately on the read loop using the lock-free `ArcSwap` snapshot. When a module accepts a bind, it sends a `RouteBindAck` and immediately starts sending data frames. Because the read loop is non-blocking, it will read the subsequent data frames and look them up in the snapshot *before* the control task has processed the `RouteBindAck` and committed the route. This results in the module's data frames being dropped as `Absent` or `Reserved`.
- **Evidence**: In the shipped behavior (`router.rs:207-218`), control frames run inline, ensuring the route is committed before the next frame is read. The redesign breaks this by decoupling control processing from the read loop while keeping data-frame lookups immediate.
- **Suggested Fix**: Process control frames from module connections inline on the read loop (since modules only respond to binds and do not execute slow `route.open` commands, this is fast and safe), or block/buffer data frames on a connection if there is a pending bind control frame for that channel/epoch.

## Finding 2: CANCEL Overtaking Request Race (Cancellation Defeated)
- **Severity**: critical
- **Location**: `crates/subc-core/src/router.rs:465` (`flow.acquire().await`) and the proposed queue-inspection logic
- **Confidence**: high
- **Issue**: If a Request is popped from the queue by the drain task but is still awaiting `flow.acquire().await` or `module_sink.send().await`, it is no longer in the queue. A concurrent CANCEL will not find it in the queue, so the read loop will forward the CANCEL to the module. Because the Request is blocked, the CANCEL frame can arrive at the module *before* the Request frame. The module will drop the CANCEL as an unknown correlation ID, and then process the Request when it eventually arrives, completely defeating cancellation.
- **Evidence**: Shipped behavior at `router.rs:465` and `router.rs:491` shows that Request delivery involves sequential awaits. The redesign's queue-inspection logic only checks the queue structure, missing frames that are mid-delivery.
- **Suggested Fix**: Enqueue CANCEL frames in the same FIFO dispatch queue as Requests. Use a shared `cancelled_corrs: HashSet<u64>` (protected by a local mutex) to mark cancelled correlation IDs. When the drain task dequeues a Request, it checks the set and discards it if marked. If it dequeues a CANCEL, it forwards it only if the Request was already sent. This preserves FIFO ordering and eliminates the race.

## Finding 3: SDK Contract Violation and Connection Teardown on Backpressure
- **Severity**: critical
- **Location**: `clients/subc-client/src/client.ts:781-792` (`classifyFailure`) and `crates/subc-client-rs/src/consumer.rs:579` (`call` error handling)
- **Confidence**: high
- **Issue**: The redesign claims "Zero SDK changes required" and that `route_backpressure` is a retryable error. However, in the TS SDK, because the request bytes were successfully written to the socket, `handedToSocket` is true, so `classifyFailure` will wrap the `route_backpressure` error in a `SubcCallError` with kind `outcome_unknown`. This causes the TS SDK to immediately fail the request, tear down the entire connection, and reconnect. The Rust and Swift SDKs will also fail the request immediately without retrying.
- **Evidence**: TS SDK `client.ts:781-792` classifies any error after write as `outcome_unknown`. Rust SDK `consumer.rs:579` returns `CallError::Module` immediately for any `TerminalFrame::Error` other than `unknown_channel`.
- **Suggested Fix**: Reject the "Zero SDK changes" claim. Modify all three SDKs to explicitly recognize `route_backpressure` (and `control_backpressure`) as retryable `NotSent` errors, preventing connection teardown and enabling proper backoff/retry.

## Finding 4: GOODBYE Ordering Violation (Pipelined Request Drop)
- **Severity**: major
- **Location**: `crates/subc-core/src/router.rs:280-310` (GOODBYE handling)
- **Confidence**: high
- **Issue**: Because the read loop is non-blocking, it can read a GOODBYE frame from the socket immediately after reading Requests, while those Requests are still sitting in the route's dispatch queue. If the read loop immediately flushes the queue and releases the route upon receiving GOODBYE, the pending Requests are discarded. In the pre-redesign, the serial read loop guaranteed that all pipelined Requests were processed before the GOODBYE frame.
- **Evidence**: Shipped behavior processes GOODBYE inline after previous frames complete. The redesign's proposed read-loop flush-on-GOODBYE destroys this ordering guarantee.
- **Suggested Fix**: Enqueue the GOODBYE frame in the route's dispatch queue (with a dedicated slot/capacity so it is never dropped) instead of processing it immediately on the read loop. The drain task will process the GOODBYE frame in order, ensuring all previous Requests are delivered before the route is released.

## Finding 5: Read Loop Blocking on Egress Queue Saturation
- **Severity**: major
- **Location**: `crates/subc-core/src/server.rs:377-399` (sending error frames)
- **Confidence**: high
- **Issue**: When the read loop synthesizes an error frame (e.g., for `route_backpressure` or `control_backpressure`), it must send it to the client. If it awaits `ctx.egress.send().await` inline, and the egress queue is full, the read loop will block. This reintroduces Head-of-Line blocking and violates the non-blocking read loop invariant.
- **Evidence**: Shipped behavior at `server.rs:384-389` awaits `ctx.egress.send` inline.
- **Suggested Fix**: Use non-blocking `try_send` for all daemon-synthesized error frames on the read loop. If `try_send` fails due to egress saturation, escalate and close the connection immediately.

## Finding 6: Late-Insertion Race in Outstanding Set (Credit Leak)
- **Severity**: medium
- **Location**: `crates/subc-core/src/router.rs:491` (`module_sink.send`) and the proposed drain task
- **Confidence**: high
- **Issue**: If the drain task inserts the correlation ID into the `outstanding` set *after* calling `module_sink.send().await`, a fast module response can arrive at the daemon and be processed *before* the insertion occurs. This causes the response to miss the `outstanding` set, leaking the credit and leaving the correlation ID in the set forever.
- **Evidence**: Concurrency window between `send().await` completion and the subsequent insertion in the drain task.
- **Suggested Fix**: Insert the correlation ID into the `outstanding` set *before* calling `module_sink.send().await`. If the send fails, remove the correlation ID and release the credit (only if it was successfully removed, to prevent double-release).

## Finding 7: Drain Task Leak on Route Release
- **Severity**: medium
- **Location**: `crates/subc-core/src/forwarding.rs:1267` (`record_route_release`) and the proposed teardown path
- **Confidence**: high
- **Issue**: When a route is released, the binding entry is dropped, which drops the queue sender. However, the drain task is a spawned Tokio task and will continue running until the queue is empty. If the queue contains many Requests, the drain task will continue to process them and send them to the module, even though the client is gone. This wastes resources and can cause unwanted side effects.
- **Evidence**: Tokio tasks are detached by default and do not abort when their handles or senders are dropped.
- **Suggested Fix**: Explicitly abort the drain task using its `JoinHandle` during route release, or close the queue receiver immediately to discard all remaining frames.

## Finding 8: Denial of Service via CANCEL Spraying
- **Severity**: medium
- **Location**: Proposed read-loop CANCEL queue-inspection logic
- **Confidence**: high
- **Issue**: The proposed design performs an O(queue) scan on the read loop for every CANCEL frame. With a `StatelessParallel` route (window=1024, queue depth=2048) and a per-connection cap of 4096, an adversarial client can fill the queue and spray CANCELs, forcing the read loop to perform up to 8.4 million operations synchronously. This will block the read loop thread, causing a Denial of Service.
- **Evidence**: O(queue) scan on the latency-critical read loop.
- **Suggested Fix**: The `cancelled_corrs` set fix proposed in Finding 2 resolves this by replacing the O(queue) scan with an O(1) set insertion.

## Summary
- **Blocker (Critical)**: 3 (Findings 1, 2, 3)
- **Major**: 2 (Findings 4, 5)
- **Medium**: 3 (Findings 6, 7, 8)
- **Low**: 0
- **Overall Risk Assessment**: High risk. The redesign introduces several critical concurrency races and contract violations that would break client liveness and correctness.
- **Open Questions Verdicts**:
  - **Q1**: Right. Fail-loud is clean and avoids HOL blocking.
  - **Q2**: Right. Synthesizing the terminal in the daemon keeps the module clean.
  - **Q3**: Right. Offloading all channel-0 commands preserves FIFO and prevents read-loop stalls.
  - **Q4**: Right. Enforcing exactly-once credit release is necessary.
  - **Q5**: Right. Whole-table Arc swap is sufficient for low-frequency mutations.

**Verdict**: NO-GO (Blockers: Findings 1, 2, 3 must be resolved before implementation).