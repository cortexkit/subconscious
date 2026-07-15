---
task_id: bg_904cb9dd
agent: council-member
session_id: ses_09b6049f7ffei1OC6ZFFOywwBS
parent_session_id: ses_09b61906bffeZMC8Uc32l89Pqb
status: completed
completed_at: 2026-07-15T07:20:18Z
---

[assistant] 2026-07-15T07:13:57Z

[assistant] 2026-07-15T07:14:01Z

[assistant] 2026-07-15T07:14:04Z

[assistant] 2026-07-15T07:14:06Z

[assistant] 2026-07-15T07:14:10Z

[assistant] 2026-07-15T07:14:12Z

[assistant] 2026-07-15T07:14:14Z

[assistant] 2026-07-15T07:14:18Z

[assistant] 2026-07-15T07:14:20Z

[assistant] 2026-07-15T07:14:21Z

[assistant] 2026-07-15T07:14:23Z

[assistant] 2026-07-15T07:14:26Z

[assistant] 2026-07-15T07:14:31Z

[assistant] 2026-07-15T07:14:33Z

[assistant] 2026-07-15T07:14:41Z

[assistant] 2026-07-15T07:14:42Z

[assistant] 2026-07-15T07:14:44Z

[assistant] 2026-07-15T07:14:47Z

[assistant] 2026-07-15T07:14:49Z

[assistant] 2026-07-15T07:14:50Z

[assistant] 2026-07-15T07:14:55Z

[assistant] 2026-07-15T07:14:57Z

[assistant] 2026-07-15T07:14:59Z

[assistant] 2026-07-15T07:15:00Z

[assistant] 2026-07-15T07:15:03Z

[assistant] 2026-07-15T07:15:06Z

[assistant] 2026-07-15T07:15:08Z

[assistant] 2026-07-15T07:15:10Z

[assistant] 2026-07-15T07:15:12Z

[assistant] 2026-07-15T07:15:16Z

[assistant] 2026-07-15T07:15:18Z

[assistant] 2026-07-15T07:15:23Z

[assistant] 2026-07-15T07:15:25Z

[assistant] 2026-07-15T07:15:27Z

[assistant] 2026-07-15T07:15:34Z

[assistant] 2026-07-15T07:15:37Z

[assistant] 2026-07-15T07:15:40Z

[assistant] 2026-07-15T07:15:42Z

[assistant] 2026-07-15T07:15:46Z

[assistant] 2026-07-15T07:15:48Z

[assistant] 2026-07-15T07:15:53Z

[assistant] 2026-07-15T07:15:54Z

[assistant] 2026-07-15T07:15:56Z

[assistant] 2026-07-15T07:16:00Z

[assistant] 2026-07-15T07:16:02Z

[assistant] 2026-07-15T07:16:05Z

[assistant] 2026-07-15T07:16:08Z

[assistant] 2026-07-15T07:16:11Z

[assistant] 2026-07-15T07:16:15Z

[assistant] 2026-07-15T07:16:19Z

[assistant] 2026-07-15T07:16:22Z

[assistant] 2026-07-15T07:16:27Z
**Analyzing Hunt Items**

I've gained a solid grasp of the client implementations. Now, I'm diving into the 10 hunt items. I'm starting with missed interleaves. My plan is to enumerate concurrency windows and, for each, either prove its safety against a specific existing semantic or flag it. The focus is on bind-commit vs snapshot-publish vs first scenarios.

[assistant] 2026-07-15T07:16:31Z

[assistant] 2026-07-15T07:16:36Z

[assistant] 2026-07-15T07:16:38Z

[assistant] 2026-07-15T07:16:42Z

[assistant] 2026-07-15T07:16:44Z

[assistant] 2026-07-15T07:16:46Z

[assistant] 2026-07-15T07:16:50Z

[assistant] 2026-07-15T07:16:54Z

[assistant] 2026-07-15T07:16:56Z

[assistant] 2026-07-15T07:16:58Z

[assistant] 2026-07-15T07:16:59Z

[assistant] 2026-07-15T07:17:02Z

[assistant] 2026-07-15T07:17:06Z

[assistant] 2026-07-15T07:17:08Z

[assistant] 2026-07-15T07:17:10Z

[assistant] 2026-07-15T07:17:15Z
**Analyzing Route Binding**

I've been examining the `complete_pending_relay` function's role post-`route.bind` request. If accepted, `commit_route_locked` inserts the route and sends a `route_open_frame`. Now, the redesign involves replacement, so I am going to delve deeper into the nature of the replacement in the design.


**Investigating Snapshot Publication**

I'm now focusing on the implications of the `ArcSwap<ForwardingSnapshot>` within the redesign. I see that `commit_route_locked` still handles mutations, locking and updating the canonical state before publishing a fresh snapshot. My concern was a race condition between the snapshot publication and the client receiving the `route_open_frame`. I confirmed the sequence: publication *then* the frame is sent. My next thought involves the client receiving the frame and attempting to send data before reading the new snapshot.


**Confirming Ordering Guarantees**

I've been analyzing the causal relationships to ensure the read loop sees the new snapshot. Thread A publishes the snapshot and sends the `route_open_frame`. The client then receives the frame and subsequently sends data. Thread B's read loop loads the snapshot. I've confirmed that the memory operations are ordered due to socket write/read synchronization. The read loop is therefore guaranteed to see the snapshot. Now, I'm considering the module's role in this flow.


**Addressing Data Frame Visibility**

I'm now grappling with a crucial edge case: the module's data frame visibility concerning the route snapshot. I realized the module *might* send data before the client gets the `route_open_frame`. This could mean the daemon's read loop drops the data if the snapshot isn't yet published. It’s problematic that the module sends a `route.bind` response before the snapshot is updated.


**Clarifying Race Condition**

I'm now revisiting the potential race conditions, specifically comparing pre-redesign behavior. I've confirmed that the module's `route.bind` response, being a control frame on channel 0, is handled serially via `handle_control_frame`, which calls `complete_pending_relay` and commits the route. Consequently, data frames sent after the response are guaranteed to be read after the route is committed in the pre-redesign. This clarifies a critical ordering guarantee.

[assistant] 2026-07-15T07:17:22Z

[assistant] 2026-07-15T07:17:24Z

[assistant] 2026-07-15T07:17:26Z

[assistant] 2026-07-15T07:17:28Z
**Analyzing Error Handling**

I've been examining the error handling strategy within this snippet. Specifically, the `if let Err(err)` block. It seems we're gracefully managing outbound failures and signaling routing issues via `on_route_gone`. However, I'm now considering the implications of returning the original error versus handling it internally.


**Identifying Race Condition**

I've zeroed in on a race condition. The daemon's read loop can drop module data frames due to a stale forwarding table snapshot, a correctness defect! It arises because module data frames can arrive before the route is committed after the `RouteBindAck`. The client-to-module path is safe because of the route-open response. I'm now thinking about how to resolve this critical issue.


**Uncovering Concurrency Issue**

I've just confirmed the core problem: data frames are overtaking control frames. The `RouteBindAck` is processed asynchronously, while data frames immediately hit the snapshot lookup within the read loop. This means the read loop will see stale route information. Data frames are dispatched directly, while `RouteBindAck` processing is delayed; a major issue! I'm now documenting this discovery.
I'm also exploring other concurrency windows, such as `GOODBYE` flush versus concurrent enqueue operations. The design doc indicates a flush must precede binding release, ensuring no enqueues after the flush operation.


**Tracing Teardown Sequence**

I've been tracing the teardown sequence to understand a potential race condition. Specifically, I'm analyzing the interactions between the read loop, the forwarding table snapshot, and the drain task. The question is this: after flushing the queue, could a frame still be enqueued? I've outlined the steps, focusing on snapshot publication versus queue sender closure. I'm now certain any read loop thread that loads the snapshot after the publication will see `Absent` and drop or reject the frame. I'm clarifying the exact sequence to confirm the drain task will correctly handle all remaining frames, including any late enqueues, to ensure no data is lost.


**Clarifying Flushing Logic**

I'm now diving into the flush operation during route teardown, and I see a potential race. I'm focusing on the interaction between snapshot updates and queue flushes. I've re-examined the shutdown path, and found the read loop might enqueue after the flush if the snapshot isn't updated first, so I must re-check this ordering. I'm exploring the implications of dropped senders.


**Confirming Teardown Correctness**

I've been re-evaluating the route teardown and queue-flushing behavior. I've been tracing the sequence and confirming data loss is not possible. I'm now clear that any read loop that loads the forwarding table snapshot after the update will not enqueue any frames. If a read loop loads the table *before* the update, those frames may be enqueued. However, if we drop the sender *before* or *during* the flush, then any attempt to enqueue will fail with an error. Alternatively, if we call `close()` on the channel, then any attempt to send also fails with an error. So data will not be lost.


**Refining Drain Task Behavior**

I'm now zeroing in on the drain task's behavior during teardown. My central question is: does the task *process* or *discard* enqueued frames when it exits? The design indicates that it exits when drained, which implies processing, not discarding. Closing the channel prevents new enqueues. I need to clearly define the expected behavior for proper flushing. Also, the read loop is safe as it can't enqueue anything once the sender is closed, which is safe. My current plan is to examine the impact of the sender closing on existing enqueued frames to be very sure the proper semantics are applied, and no frames are lost.


**Clarifying Request Discarding**

I'm now investigating the implications of a `GOODBYE` message on queued requests. My focus is the need to discard these requests, not process them, and the impact on credit accounting. The critical question: should we synthesize errors? The client-sent `GOODBYE` implies local settlement and a closed route. I'm now certain this means we should discard the pending requests, and error responses are not expected. I must verify this is true for various scenarios.


**Verifying Goodbye Handling**

I'm now investigating a potential race condition involving a request in the queue, a client-sent `GOODBYE`, queue flushing, and request dropping. The intended behavior is confirmed: requests in the queue are dropped upon `GOODBYE`. I'm verifying the module handling of delivered requests and the implications for credit accounting. Specifically, I'm analyzing the `outstanding` set and the client-side credit management, which are destroyed upon route release. I've concluded that this does not create any credit leaks.


**Analyzing Cancel Behavior**

I'm now investigating a race condition: a `CANCEL` frame racing with a queued request. I'm focusing on two scenarios, "CANCEL wins" and "Request-delivery wins".  Specifically, I'm examining how credit accounting and queue management interact when a `CANCEL` arrives and could "win" (request still queued). I'm evaluating the need for a thread-safe queue with arbitrary item removal support. `tokio::sync::mpsc` doesn't support removal, so I will have to find a new solution.


**Clarifying Cancel Delivery**

I'm now investigating a crucial race: a `CANCEL` frame arriving concurrently with a queued `Request`. I'm focused on when the `CANCEL` arrives and how the module receives each frame. The read loop *does not enqueue* the `CANCEL` message; it sends it directly to the module, potentially overtaking the queued request in the process. This means the module could receive the `CANCEL` message before the queued request.


**Resolving Cancel Transmission**

I'm now zeroing in on the mechanics of `CANCEL` frame transmission, specifically avoiding `module_sink.send(frame).await`. The read loop shouldn't block, so enqueuing is out, but that means `CANCEL` must be sent without await. However, if this is done using a non-blocking `try_send`, what happens if that fails and the ingress queue is full? The design says no dropping of `CANCEL` messages! I'm now exploring the design doc to solve how the `CANCEL` can be sent without awaiting.



**Refining Cancel Logic**

I'm now refining how to handle `CANCEL` frames. I see the design indicates non-request frames like `CANCEL` never get dropped due to capacity, but they also don't enqueue. The read loop transmits `CANCEL` directly to the module via a non-blocking operation such as `try_send`. However, this raises questions about how the read loop handles a `try_send` failure and what happens if a `CANCEL` races with a request in a mid-delivery state, which can lead to it being ignored. I'm focusing on ensuring CANCEL frame integrity in the face of congestion and concurrency.



**Addressing Cancellation Races**

I'm now focused on the "CANCEL overtakes Request" race. I've confirmed cancellation can be defeated as the `CANCEL` gets dropped at the module because the correlation ID is unknown! The request is blocked waiting for the flow credit. The `CANCEL` bypasses this block. Thus the module can get the `CANCEL` *before* the request! So I must resolve this critical race condition.

**Analyzing Cancel Race**

I'm now looking at the concurrency defect where CANCEL can be defeated. Specifically I'm verifying the module will drop the CANCEL. I'm exploring the implications. The read loop removes the request. The drain task never sees it and then drops the cancel. I've re-examined the design, and now realize that my approach had not accounted for the cancellation being lost.


**Verifying Terminal Semantics**

I'm now zeroing in on duplicate terminal handling, particularly when a duplicate `CANCEL` is sent. I've been tracing the scenarios where the read loop and the drain task might interfere. The read loop *does* remove the request and synthesize a `cancelled` terminal, even if the drain task is also involved. I'm focusing on the `outstanding` set's role and how it handles credit releases. The design says a release only fires once, so I must verify the exact terminal forwarding behavior and client SDK's duplicate terminal handling.

[assistant] 2026-07-15T07:17:45Z

[assistant] 2026-07-15T07:17:47Z

[assistant] 2026-07-15T07:17:50Z

[assistant] 2026-07-15T07:17:55Z
**Analyzing Concurrency Concerns**

I've zeroed in on the use of `std::sync::RwLock` and its synchronous nature. It looks like acquiring the write lock synchronously within `Drop` is the current point of consideration. However, I'm now wrestling with the implications of a panic in the drain task, which runs on a Tokio thread. This could affect the abort-guard's behavior. The panic leads to `Drop` being called, which then attempts to acquire the lock.


**Addressing Shutdown Sequence**

I'm now focusing on the connection-close versus drain-task shutdown sequence. I'm exploring the implications of the connection teardown process and how it relates to releasing routes. I am concerned that, if the connection teardown does not wait for the drain task to exit, there might be a race condition when the drain task tries to access resources. The drain task owns the `module_sink` and `flow`, and the connection shutdown needs to ensure these are properly handled.


**Clarifying Frame Flow**

I'm now tracing the frame flow when a client connection closes. I've realized the drain task, client-to-module, might continue sending to the module even after the client disconnects. This is because the module connection could still be active. I'm focusing on the interaction between the connection shutdown, the route binding removal (and subsequent sender drop), and the drain task's lifecycle to ensure a graceful shutdown.


**Prioritizing Queue Discarding**

I've realized that sending requests to the module after the client disconnects is wasteful and potentially harmful. I'm now focused on immediately discarding queued requests on connection closure. I'm investigating how to achieve this: dropping the route binding and its associated drain task. I need to ensure the drain task exits immediately to prevent leaks or unwanted execution. I'm looking at how the `JoinHandle` behaves to determine whether I need an explicit abort. I'm also confirming that late enqueues after the queue sender is dropped will simply fail, and the read loop should handle this without issue. Finally, I'm starting to consider credit accounting across different exit paths for each task.


**Verifying Credit Accounting**

I'm now diving into credit accounting in various scenarios. I've confirmed credit balance with a request processed normally, ensuring exactly-once acquire and release. I've also verified correct credit accounting when a request is cancelled in the queue and when the queue is flushed due to a GOODBYE. I'm focusing on the connection death scenarios, considering requests in queue and those in-flight but not terminated by the module, to ensure there are no credit leaks.


**Examining Mutex Protection**

I'm now zeroing in on the `outstanding` set's thread safety. It's clear that due to concurrent access from the drain task and the module connection's read loop, the set needs protection. My current thought is that it must be a `Mutex<HashSet<u64>>` because the `RouteBinding` is shared. The lock operations are expected to be fast since they're confined to O(1) ops. There is no risk of deadlock.



**Addressing Race Condition**

I've pinpointed a critical race condition. The terminal response from the module can arrive before the drain task inserts the correlation ID into the `outstanding` set. This means the credit won't be released. I'm now exploring how to make sure the credit is released. It seems I must insert the `corr` before send completes.


**Confirming Atomic Operations**

My current thinking revolves around ensuring the atomic nature of correlation ID management and credit release. I've concluded that inserting the correlation ID into the `outstanding` set *before* calling `module_sink.send(frame).await` is crucial for correctness. The atomic insertion ensures that even if `send` fails or the terminal arrives concurrently, the credit is managed correctly. Now, I'm verifying the design doc and analyzing edge cases with these atomic operations.


**Analyzing Double Release Risk**

I'm now deeply concerned about the potential for a double credit release. It looks like a terminal response from the module arriving *before* the drain task completes `send.await` can cause issues. The drain task might attempt to release credit even if the module has already done so, or the module might release it before the task knows it is needed. I'm focusing on ensuring atomic operations to prevent this. I think the problem revolves around the order of operations in the send loop.

[assistant] 2026-07-15T07:18:07Z

[assistant] 2026-07-15T07:18:09Z

[assistant] 2026-07-15T07:18:11Z

[assistant] 2026-07-15T07:18:14Z

[assistant] 2026-07-15T07:18:24Z

[assistant] 2026-07-15T07:18:28Z

[assistant] 2026-07-15T07:18:32Z
**Analyzing Egress Blocking**

I've zeroed in on the blocking potential within `FrameSink::send`, specifically how `self.tx.send(frame).await` ties into the egress queue. The read loop's call to `ctx.egress.send(error_frame).await` presents a clear blockage risk if this bounded `mpsc::channel(EGRESS_BUFFER)` fills. I'm now exploring the implications.


**Investigating Backpressure Concerns**

I'm now deeply considering the implications of `try_send` versus `send` for error frames within the read loop, specifically in scenarios of a full egress queue. Dropping frames seems reasonable if the client isn't reading, implying saturation. But, this still feels like a violation of the goal for a non-blocking read loop. I'm focusing on `route_frame` and its potential to synthesize error frames and reintroduce blocking, which I'll pursue next.

[assistant] 2026-07-15T07:18:43Z

[assistant] 2026-07-15T07:18:48Z

[assistant] 2026-07-15T07:18:50Z

[assistant] 2026-07-15T07:18:55Z

[assistant] 2026-07-15T07:19:00Z

[assistant] 2026-07-15T07:19:07Z

[assistant] 2026-07-15T07:19:10Z

[assistant] 2026-07-15T07:19:17Z
**Analyzing Error Recovery**

I've been examining the "not_sent" error handling. Specifically, I'm focusing on the `reconnectAfterDrop` function and the potential for a `reconnectErr`. The goal is to ensure robust error recovery when managed calls fail to send. A failure in reconnect recovery now throws a new error type.


**Investigating Frame Handling**

I'm now diving into how the client's `readLoop` processes `ERROR` frames, particularly when the error code is `route_backpressure`. I'm tracing the flow from `dispatch` to `rejectPending`, and further to the `classifyFailure` function. It seems the code path might be different than previously thought; specifically, whether the `classifyFailure` function is even called given the current context. I'm focusing on the condition where the socket remains active.


**Unraveling Error Propagation**

I've just confirmed the critical issue: an `ERROR` frame with `route_backpressure` leads to an `outcome_unknown` error. The `classifyFailure` function incorrectly assumes a connection drop, even when the socket is active. This causes immediate reconnect scheduling and connection tear-down, preventing retries. This behavior diverges significantly from the expected handling of backpressure situations and may need urgent fixing. I'm now verifying the exact control flow to confirm the code's behavior.


**Clarifying SDK Error Handling**

I've been looking at the implications of a `route_backpressure` error in both the TS and Rust SDKs. It seems both currently fail the request immediately, without retrying. The TS SDK also tears down the connection, making the issue even more problematic. I've confirmed that the existing code does not handle this error as retryable, which goes against the documentation. I need to understand how to correctly classify `SubcCallError` as `not_sent` in the TS SDK, which is still confusing me, to enable a more appropriate retry strategy.



**Confirming SDK Modifications**

I've confirmed that the TS SDK's `classifyFailure` *must* be modified to correctly handle `route_backpressure`, `control_backpressure`, and `cancelled` errors. The current logic in `classifyFailure` will lead to immediate connection tear-down and prevent retries, despite the bytes having been written to the socket. This forces us to re-evaluate the "zero SDK changes" requirement. The Swift client will need similar, but independent, modification.


**Evaluating Error Behavior**

I've been analyzing the Swift client's behavior with `ERROR` frames. It also immediately throws `SubcError` upon receiving `route_backpressure`, confirming it does not retry, as expected. I've re-examined the honesty of the `NotSent` mapping in overflow cases, confirming it holds. However, I've identified that implicit backpressure reliance could now lead to over-issuing and unexpected behavior. Clients previously relying on blocking for flow control may now receive a flood of `route_backpressure` errors instead. This highlights a critical behavioral shift.


**Analyzing Retry Logic**

I'm now considering the implications of not implementing a backoff strategy when retrying. It appears the client might enter a hot retry loop, overloading the daemon. The shift in control flow from blocking to immediate errors demands careful backoff logic to prevent denial-of-service. I need to be sure the new behavior of a flood of `route_backpressure` errors is handled.

[assistant] 2026-07-15T07:19:25Z

[assistant] 2026-07-15T07:19:27Z
**Analyzing Route Closure**

I've been examining the route closure logic, specifically the handling when `opts.drain` is set. It seems like the system drains the unary requests on a handle, then immediately fails the handle with a `route_closed` error, and lastly cleans up by deleting the live route.

[assistant] 2026-07-15T07:19:31Z

[assistant] 2026-07-15T07:19:34Z

[assistant] 2026-07-15T07:19:36Z

[assistant] 2026-07-15T07:19:42Z
**Verifying Route Closure**

I've hit a small snag. It seems there's no explicit `route.close` command. Now, I understand a route is actually closed by sending a `Goodbye` frame across the route's channel. I'm double-checking this in `crates/subc-core/src/router.rs` to confirm my understanding.

[assistant] 2026-07-15T07:19:48Z

[assistant] 2026-07-15T07:19:53Z

[assistant] 2026-07-15T07:19:56Z
**Investigating Cancellation Flow**

I'm now focusing on how the client deals with a cancelled request's response. The client sends a CANCEL frame upon `cancel(handle, corr)`. I'm tracing where the pending request remains and how the `readLoop` processes the "cancelled" error response. This helps me understand the complete lifecycle of a cancellation scenario.


**Analyzing Cancellation Flow**

I've confirmed the client's expectation: cancellation errors settle pending promises. The `cancel` function only sends a CANCEL frame; promise rejection relies on receiving a terminal error, typically "cancelled". The client's lifecycle of handling requests relies heavily on the daemon sending the terminal frame for a cancelled request to properly resolve the promise. I have analyzed how the promise is settled and verified that there is exactly one terminal that is sent. If the client receives a second terminal, it is safely ignored.

\n\n

**Exploring Snapshot Stales**

I'm now examining stale ArcSwap snapshot windows in forwarding table lookups. Pre-commit scenarios map to `Absent` drops, mirroring previous behavior. Post-release scenarios map to `channel-gone` drops. I've considered scenarios where a read loop enqueues a frame on a route, after releasing a route. The frame is either flushed or the enqueue fails due to queue closure which is consistent with existing semantics. The key observation is that stale snapshots can occur, but existing logic handles them safely, preventing new observable states.


**Uncovering Epoch Invariants**

I've been meticulously examining the behavior of released routes and the potential for stale snapshots. My thinking led me to focus on epoch fencing and how it prevents cross-epoch pollution. Even with stale snapshot reads, the system correctly discards stale frames, thanks to epoch checks. I've also verified this protection extends to the module-to-client path, where the client handles potential inconsistencies gracefully. However, a race condition has emerged, where the read loop might drop a data frame after a bind, presenting a new broken state.


**Quantifying Potential Vulnerability**

I am now focusing on a potential Denial of Service (DoS) vulnerability. By opening a route with a large queue and subsequently sending a flood of CANCEL frames, the read loop is forced to scan a very long queue for each CANCEL, leading to a large number of operations on a latency-critical path. This could allow a single client to saturate the CPU and cause a Denial of Service. My calculations indicate the potential for a large number of operations, which is performed in the critical read loop.


**Evaluating Cancellation Performance**

I am now performing calculations related to the number of operations generated by flooding a route with cancels after setting the queue depth to 4096. This quickly jumped to 8.4 million, which could block the read loop, potentially starving it and other connections. I'm exploring an optimization: Instead of the read loop doing an O(queue) scan, I am now investigating whether we can offload the CANCEL handling to a drain task by enqueuing the CANCEL operations in the dispatch queue.


**Refining Cancellation Strategy**

I'm now refining the cancellation approach to address the issue. I've realized that the initial method would cause CANCEL to be unable to cancel requests already queued. To solve this, I'm proposing a `cancelled_corrs` set on the route. The read loop now performs an O(1) insert on `cancelled_corrs` when a CANCEL frame arrives. The drain task then checks if the request's correlation ID exists in the `cancelled_corrs` set to efficiently determine whether the Request is cancelled, providing a synthesis terminal if so.


**Validating Request Lifecycle**

I am now verifying the request lifecycle with the `cancelled_corrs` set. The read loop now efficiently handles CANCEL frames with O(1) inserts into `cancelled_corrs`, and enqueues CANCEL frames for the drain task. The drain task then checks for cancelled correlation IDs to synthesize a terminal if needed, which prevents the O(queue) scan and handles the race correctly. This approach ensures safety and solves the key problems, in a brilliantly simple way.


**Confirming Same-Thread Invariance**

I've confirmed the crucial invariant: on a single thread, the read loop is guaranteed to see a consistent snapshot following a control frame mutation. The write lock ensures atomicity, and the read loop's immediate next iteration reflects the updated forwarding table. This eliminates stale reads within the same thread. Now, I must check the impact of other threads.


**Assessing Cross-Thread Safety**

I'm verifying cross-thread safety now, specifically considering a data frame read on Thread B. The potential stale snapshot window for Thread B is equivalent to the RwLock behavior. Module connection on Thread C follows the RwLock analogy as well. So, the merge-1 approach remains invariant-neutral. I must now assess each lean in the open questions section.


**Prioritizing Resource Management**

I'm now focusing on how to prevent memory growth or complex buffer management. I see that blocking the read loop would cause Head-of-Line blocking, which is not desirable. Failing-loud is clean, bounded, and maps nicely to the SDK's `NotSent` retry contract. Synthesizing cancellation terminals in the daemon is the right call for safety and efficiency. Also, offloading all control commands to a FIFO task is correct.


**Analyzing Control Flow**

I'm now examining the implications of strict FIFO ordering for control commands on the daemon, specifically per-connection. My analysis confirms that enforcing exactly-once credit release in the daemon is more robust, providing a cleaner way to enforce the invariant. Additionally, whole-table Arc swaps for snapshot publishing are the right choice for their low-frequency nature.


**Synchronizing Data Frames**

I'm now focused on synchronizing data frame processing with control frame acknowledgments. Data frames might be dropped as `Absent` or `Reserved` due to the read loop's processing *before* a `RouteBindAck`. I'm considering fixes: pausing the read loop, buffering data frames, or forcing the daemon's control task to process synchronously. The optimal solution is for the module to await confirmation before sending data frames, or synchronize the `RouteBindAck` processing.


**Evaluating Route Acknowledgments**

I'm now diving into the implications of processing `RouteBindAck`. Processing it inline allows the module to read next frame right away, but could cause contention. I have focused on ensuring data frames are not dropped due to an uncommitted route. The daemon's control task now queues data frames or waits for the `RouteBind` acknowledgement.


**Implementing Inline Control Processing**

I'm now implementing a new approach: the module connection's read loop will process control frames inline. This allows `RouteBindAck` to commit the route *before* the next data frame is read. This simple fix provides a clean solution, because module connections don't run slow commands. It will prevent data frames from being dropped. I'm also addressing the race where a CANCEL overtakes a Request.

\n\n


**Addressing SDK and GOODBYE**

I'm now focusing on two key problems. First, the SDK now violates its contract, treating `route_backpressure` as a terminal error, leading to connection teardown. Second, a GOODBYE frame can cause the premature discard of pending requests in the redesigned system. I propose the SDK should treat `route_backpressure` as a retryable error, and enqueue GOODBYE frames into the route's dispatch queue.


**Refining Egress Handling**

I'm now focused on addressing read loop blocking on egress queue saturation.  My new approach involves using `try_send` for daemon-synthesized error frames.  Failing to send results in immediate connection closure, which maintains the non-blocking read loop invariant. This keeps the design efficient and safe.
<COUNCIL_MEMBER_RESPONSE>
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
</COUNCIL_MEMBER_RESPONSE>

