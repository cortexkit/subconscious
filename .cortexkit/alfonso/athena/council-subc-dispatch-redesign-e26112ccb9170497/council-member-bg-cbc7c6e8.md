## Finding 1: CANCEL can be lost after dequeue but before module delivery
- **Severity**: BLOCKER
- **Location**: Dispatch queue CANCEL semantics / queued→delivered boundary
- **Confidence**: high
- **Issue**: The design treats “not in queue” as “already delivered or unknown.” That misses the drain-task limbo where a Request has been popped from the queue but is still waiting on `flow.acquire().await` or `module_sink.send(...).await`. A CANCEL in this window is forwarded to the module before the module knows the corr, so shipped SDK modules drop it; the Request later runs anyway.
- **Evidence**: Current blocking points are exactly `flow.acquire().await` and `module_sink.send(frame).await` (`crates/subc-core/src/router.rs:465`, `crates/subc-core/src/router.rs:491`). Shipped module cancel handlers no-op for unknown corr: Rust SDK only cancels if in-flight entry exists (`crates/subc-client-rs/src/lib.rs:979-990`); TS provider only aborts an existing inflight controller (`clients/subc-client/src/provider.ts:694-696`).
- **Suggested Fix**: Add an explicit per-corr state machine: `Queued -> Dispatching(not_sent_yet) -> Delivered`. CANCEL must be able to atomically mark/cancel `Dispatching` before module send; drain then drops the Request, releases any acquired credit, and synthesizes `cancelled`.

## Finding 2: Channel-0 offload breaks route.bind response → immediate data ordering
- **Severity**: BLOCKER
- **Location**: Per-connection control FIFO task vs module data frames
- **Confidence**: high
- **Issue**: A module can send route.bind ACK on channel 0 and then immediately send a data frame on the new module route. Today, the read loop processes the ACK to completion before reading the data frame. The redesign enqueues channel-0 work and continues reading data, so the data frame can be looked up before bind commit and be dropped as Reserved/Absent.
- **Evidence**: Current read loop awaits routing before next read (`crates/subc-core/src/server.rs:357-375`), and channel-0 control runs inline (`crates/subc-core/src/router.rs:207-218`). Bind ACK completion commits the route via `complete_pending_relay` (`crates/subc-core/src/control.rs:2029-2032`) and `commit_route_locked` publishes maps before sending route.open (`crates/subc-core/src/forwarding.rs:1524-1536`). Data frames for Reserved/Absent module routes are dropped (`crates/subc-core/src/router.rs:227-245`). There is an explicit test for this old ordering: `accepted_route_publishes_route_open_before_immediate_reverse_request` (`crates/subc-core/src/router.rs:1078-1102`).
- **Suggested Fix**: Do not blindly offload all channel-0 frames. Module control responses that commit route.bind must either be processed synchronously in the read path or impose a per-connection barrier so later module data cannot overtake the commit.

## Finding 3: Snapshot stale Bound after release is a new observable state
- **Severity**: BLOCKER
- **Location**: ArcSwap snapshot forwarding / release windows
- **Confidence**: high
- **Issue**: The doc claims stale snapshots map to existing “channel gone” drops, but current `RouteBinding` Arcs are cloneable and not revocable. A reader can load an old snapshot after release and still hold a Bound route. In merge-1, that stale route can forward module→client frames; in merge-2, it can enqueue into a stale queue unless the queue has an independent closed flag.
- **Evidence**: Current lookup is serialized by one `RwLock` (`crates/subc-core/src/forwarding.rs:840-889`), so a lookup starting after release cannot see the old map. Release removes route maps and closes flow (`crates/subc-core/src/forwarding.rs:1409-1428`, `crates/subc-core/src/forwarding.rs:1440-1460`). Module→client forwarding does not check any route-closed flag; it rewrites, `try_send`s, then releases credit (`crates/subc-core/src/router.rs:281-309`).
- **Suggested Fix**: Add a per-binding atomic `closed/generation` guard checked by every data-plane admission/forward path, and close the queue receiver/state, not merely drop one sender. Snapshot publication alone is not enough.

## Finding 4: Merge-1 snapshot forwarding is not invariant-neutral
- **Severity**: BLOCKER
- **Location**: Rollout merge 1
- **Confidence**: high
- **Issue**: Landing ArcSwap before queues changes semantics by allowing post-release stale Bound reads. With the old `RwLock`, lookup visibility is synchronized with release; with ArcSwap, a reader can see a previously published table after the write side has removed the route.
- **Evidence**: Current data lookups take `read_inner()` (`crates/subc-core/src/forwarding.rs:846`), while releases take the write lock and remove the same maps (`crates/subc-core/src/forwarding.rs:614-657`, `crates/subc-core/src/forwarding.rs:1409-1470`). Current module→client path would forward any stale Bound without revalidation (`crates/subc-core/src/router.rs:281-309`).
- **Suggested Fix**: Do not land merge-1 alone unless it includes route tombstones/closed-bit validation that makes stale snapshots inert.

## Finding 5: Drain-task error paths are underspecified and would lose shipped Error-frame recovery
- **Severity**: BLOCKER
- **Location**: Drain task owning `flow.acquire` + `module_sink.send`
- **Confidence**: high
- **Issue**: Moving `acquire` and `send` off the read loop removes the caller that currently converts failures into canonical Error frames. The pseudocode has no handling for acquire-closed, module draining, writer closed, send failure, or post-acquire release.
- **Evidence**: Today `connection_loop` converts routable `RouterError`s into Error frames (`crates/subc-core/src/server.rs:377-390`). `handle_bound` maps closed acquire to `module_reloading`/backend errors (`crates/subc-core/src/router.rs:465-485`) and releases credit on send failure after acquire (`crates/subc-core/src/router.rs:491-496`).
- **Suggested Fix**: Drain task must synthesize the same Error frames itself for every failed acquire/send path and must remove outstanding/release credit exactly once on send failure.

## Finding 6: SDK NotSent/retry contract does not support `route_backpressure`
- **Severity**: BLOCKER
- **Location**: TS/Rust/Swift consumers and managed-call retry classifiers
- **Confidence**: high
- **Issue**: The design says queue overflow maps to existing NotSent/retryable behavior with zero SDK changes. Shipped clients do not do that. A daemon Error frame with `code:"route_backpressure"` is currently a terminal/module error, not NotSent.
- **Evidence**: TS defines `not_sent` narrowly as bytes never leaving the local process (`clients/subc-client/src/client.ts:184-194`); Error frames reject with `SubcError` (`clients/subc-client/src/client.ts:1057-1059`) and managed calls only retry `not_sent` or `unknown_channel` (`clients/subc-client/src/client.ts:421-450`). TS retryable set is only route.open codes (`clients/subc-client/src/client.ts:1240-1259`). Rust returns data-plane Error frames as `CallError::Module` (`crates/subc-client-rs/src/consumer.rs:570-579`), and its retryable set also only covers route.open (`crates/subc-client-rs/src/consumer.rs:3130-3134`). Swift route errors become `SubcError` with no retry classifier (`clients/subc-client-swift/Sources/SubcClient/Client.swift:475-482`, `671-673`).
- **Suggested Fix**: Update SDKs and docs before daemon change: classify daemon admission errors (`route_backpressure`, possibly `control_backpressure`) as a new “daemon_not_sent/admission_rejected” or explicitly broaden NotSent.

## Finding 7: Queue memory cap is frame-count based and permits catastrophic memory use
- **Severity**: BLOCKER
- **Location**: Bounded admission / DoS math
- **Confidence**: high
- **Issue**: A 4096-frame per-connection cap is not a safe memory bound because each frame body may be 64 MiB. Worst case is 256 GiB per connection; a single StatelessParallel route queue can hold 2048 × 64 MiB = 128 GiB.
- **Evidence**: Protocol max frame body is 64 MiB (`crates/subc-protocol/src/lib.rs:118-119`). The design proposes per-route depths up to 2048 and aggregate 4096 frames.
- **Suggested Fix**: Add byte-based queue budgets and much smaller per-route byte caps; admission must account `body.len()` before accepting.

## Finding 8: O(queue) CANCEL scans put attacker work on the read loop
- **Severity**: MAJOR
- **Location**: CANCEL queue inspection
- **Confidence**: high
- **Issue**: A client can fill a StatelessParallel route queue to 2048 entries, then spray pure-header CANCELs for missing corrs. Each 21-byte frame forces ~2048 comparisons on the latency-critical read loop.
- **Evidence**: CANCEL is a pure-header frame (`crates/subc-protocol/src/lib.rs:162-165`). The design explicitly puts O(queue) CANCEL scans on the read loop.
- **Suggested Fix**: Maintain an indexed `corr -> queue entry/state` map so CANCEL is O(1), or bound per-CANCEL scan work and fall back to state tombstones.

## Finding 9: Control queue overflow policy is wrong for module responses
- **Severity**: MAJOR
- **Location**: Channel-0 control queue overflow
- **Confidence**: medium-high
- **Issue**: Channel 0 carries not only client commands but module responses that settle daemon-originated route.bind/control RPCs. A generic `control_backpressure` Error cannot safely replace a module Response/Error; it can leave the client route.open pending until timeout or corrupt relay state.
- **Evidence**: Module channel-0 Response/Error is routed into `handle_module_relay_response` (`crates/subc-core/src/router.rs:405-412`, `crates/subc-core/src/control.rs:1879-2045`), which completes pending relays/control RPCs (`crates/subc-core/src/control.rs:2029-2032`).
- **Suggested Fix**: Reserve capacity or priority for module control responses, process relay completions inline, or close the offending connection on overflow rather than synthesize an unrelated error.

## Finding 10: I3/I7 invariant claims are false as written
- **Severity**: MAJOR
- **Location**: Design  invariants
- **Confidence**: high
- **Issue**: “Release paths untouched” and “module→client direction unchanged” are not true once queue flush/stop and per-corr `outstanding` gates are added. The change may be desirable, but the invariant claim is false and hides review surface.
- **Evidence**: Current module→client terminal path releases on every terminal after successful `try_send` (`crates/subc-core/src/router.rs:281-309`), with terminal types defined at `crates/subc-core/src/router.rs:501-506`. Current `ChannelFlow.release` is aggregate, not per corr (`crates/subc-core/src/forwarding.rs:1702-1731`), so the R11 rider necessarily changes behavior for duplicate/late terminals.
- **Suggested Fix**: Rewrite I3/I7 to state the actual changed semantics and add tests for duplicate terminal, late terminal after release, and terminal for unknown corr.

## Finding 11: Synthetic `cancelled` Error frame is mechanically supported, but only after race fixes
- **Severity**: OK with caveat
- **Location**: Daemon-synthesized terminal vocabulary / SDK duplicate handling
- **Confidence**: high
- **Issue**: The daemon can build `Error{code:"cancelled"}` without parsing request bodies, and SDKs generally tolerate late duplicate terminals. This does not save the queued→dispatching race in Finding 1.
- **Evidence**: `RouterError::RouteError` accepts arbitrary code/message and `to_error_frame` emits canonical JSON Error frames (`crates/subc-core/src/router.rs:602-608`, `617-633`); canonical `ErrorBody` is `{code,message}` (`crates/subc-protocol/src/lib.rs:121-126`). TS drops terminal frames with no waiter (`clients/subc-client/src/client.ts:1078-1091`); Rust no-ops when pending entry is absent (`crates/subc-client-rs/src/consumer.rs:1902-1906`); Swift ignores nonmatching in-flight keys in its read loops (`clients/subc-client-swift/Sources/SubcClient/Client.swift:383-388`, `474-485`).
- **Suggested Fix**: Keep synthetic cancelled, but only synthesize from an atomic queued/dispatching state that proves the module cannot also receive the Request.

## Open-question verdicts
- **Q1 route_backpressure fail-loud**: right direction, wrong as written until SDK classifiers and byte caps are changed.
- **Q2 daemon-synth cancelled**: right only with a dispatching-state fix; current queue-only test is unsafe.
- **Q3 whole channel-0 FIFO**: wrong; route.bind completions must not be overtaken by data.
- **Q4 R11 rider now**: right in principle, but it invalidates I7 and must be part of the per-corr state machine.
- **Q5 whole-table Arc swap**: wrong as a standalone merge unless stale snapshots are made inert with tombstones/closed checks.

## Summary
Severity counts: 7 BLOCKER, 3 MAJOR, 1 OK-with-caveat. Overall verdict: **NO-GO**. Required blockers to clear: fix CANCEL dispatching race, preserve route.bind/data ordering, make stale snapshots inert before merge-1, fully specify drain-task error/credit paths, update SDK backpressure contracts, and add byte-based queue caps.