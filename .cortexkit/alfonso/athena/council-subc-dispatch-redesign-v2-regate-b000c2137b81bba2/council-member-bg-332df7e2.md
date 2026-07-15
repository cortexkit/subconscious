# Verdict: **NO-GO**

Only **4/10** v1 blockers are closed. Six remain open or are newly broken, including unsafe automatic retries, two remaining CANCEL races, incorrect credit RAII, incomplete teardown integration, and a non-landable merge-1.

## A. B1–B10 Re-gate

| Blocker | Ruling | Justification |
|---|---|---|
| **B1 — SDK backpressure contract** | **NEWLY-BROKEN** | V2 correctly recognizes that a distinct in-place retry path is needed: existing TS `not_sent` reconnects (`client.ts:417-452`), Rust invalidates/reconnects (`consumer.rs:570-583`), and Swift has neither code parsing nor a retry loop (`Client.swift:214-227,444-487,671-674`). However, `route_backpressure` has no trustworthy provenance: modules may emit arbitrary `Error { code: String }` (`subc-client-rs/src/lib.rs:515-525`; `provider.ts:995-1018`), and subc forwards those terminals unchanged (`router.rs:281-309`), so a module-generated `route_backpressure` is indistinguishable from daemon rejection and can cause duplicate execution. |
| **B2 — CANCEL limbo** | **NOT-CLOSED** | The drain pops under one lock and marks `Claimed` under a second (`v2:86-87`), allowing CANCEL to remove the still-`Queued` slot between them. Separately, `Delivered` is recorded before the Request is enqueued (`v2:92-97`), so CANCEL can be forwarded first; modules no-op unknown-corr CANCEL (`subc-client-rs/src/lib.rs:988-998`). |
| **B3 — queue synchronization primitive** | **CLOSED, narrowly** | A route-local mutex now serializes both actors and removes the v1 data race (`v2:29-47`). This does not validate the claimed O(1) removal or the two-step pop/claim transition; those remain under B2/B6. |
| **B4 — drain error arms / RAII** | **NOT-CLOSED** | Send failure is specified, but teardown reason synchronization and cancellation/abort cleanup are not. Worse, rollback explicitly calls `flow.release()` while the described armed `AcquiredCredit` also releases on drop (`v2:93-110`), which can steal another request’s credit because shipped `release()` suppresses only when the aggregate count is already zero (`forwarding.rs:1702-1731`). |
| **B5 — outstanding-before-send** | **CLOSED, in isolation** | `Delivered` and `outstanding` are committed under the inbox lock before `module_sink.send` (`v2:92-107`); a causal module terminal cannot exist before the send enqueues the Request. Thus the fast-terminal race against current `router.rs:281-309,491` is closed, subject to corr uniqueness and valid Slot ownership. |
| **B6 — O(queue) CANCEL DoS** | **NOT-CLOSED** | `slots` makes lookup O(1), but removing an arbitrary corr from `VecDeque<u64>` remains O(queue); no index or tombstone-safe drain algorithm is specified (`v2:40-41,49-54,74`). |
| **B7 — module bind-ACK barrier** | **CLOSED, narrowly** | Module connections remain inline, preserving commit-before-next-module-frame and the shipped barrier test (`router.rs:199-218,1078-1102`). Client-wide control offload introduces separate control/data ordering defects described below. |
| **B8 — dishonest I3/I4/I7 claims** | **CLOSED** | V2 now accurately labels the release call-site gate, GOODBYE queue flush, and stale-Bound check as deltas (`v2:215-219,245-264`) instead of claiming the current unconditional release site is untouched (`router.rs:307-309`). |
| **B9 — teardown atomicity/lifecycle** | **NOT-CLOSED** | Marking `Closing` under the inbox lock does close the stale-snapshot push-after-flush hole. But bounded async join is not integrated with the shipped synchronous release/Drop paths (`router.rs:391-397`; `control.rs:422-458`), concurrent close ownership is undefined, endpoint-drain quiescence is omitted, and connection/control tasks can still retain `FrameSink` clones while `server.rs:238-278` waits for the writer. |
| **B10 — standalone snapshot merge** | **NOT-CLOSED** | Merge-1 names only the future dispatcher as the client-side closed checker; current `ForwardBackend::handle` has no such check (`router.rs:432-450`). Also, “publish under lock” is insufficient unless publication precedes the already externally observable `client_permit.send(route_open_frame)` at `forwarding.rs:1524-1536`; that order is not normative or tested. |

## B. New Defects Introduced by V2

## Finding 1: Retryable error codes have no daemon provenance
- **Severity**: BLOCKER
- **Location**: `v2 ; module error emission and `router.rs:281-309`
- **Confidence**: high
- **Issue**: SDKs cannot safely infer “never reached the module” from the string `route_backpressure`.
- **Evidence**: Error codes are open strings (`docs/subc-control-protocol.md:62`); Rust and TS modules can emit arbitrary codes (`subc-client-rs/src/lib.rs:515-525`; `provider.ts:995-1018`), and subc forwards module Error bodies without parsing or tagging them (`router.rs:281-309`). A module that performed a side effect and then emitted that code would be retried automatically.
- **Suggested Fix**: Add unforgeable daemon provenance, e.g. a daemon-only header flag stripped/rejected on module ingress, or parse and reserve/escape daemon codes. Code-only retry is unsafe. Add a test where a module emits `route_backpressure` and verify it is never automatically retried.

## Finding 2: CANCEL still has two request-ordering races
- **Severity**: BLOCKER
- **Location**: `v2:70-107`
- **Confidence**: high
- **Issue**: Neither dequeue→claim nor Delivered→actual sink enqueue is atomic with CANCEL.
- **Evidence**:
  1. Drain pops `x`, unlocks, then CANCEL sees `Queued`, removes/synthesizes cancellation; drain subsequently indexes a missing slot (`v2:86-87`).
  2. Drain marks `Delivered`, unlocks, then CANCEL forwards before `send(frame)` is polled (`v2:92-97`). Since unknown-corr CANCEL is a no-op (`subc-client-rs/src/lib.rs:988-998`), the later Request runs uncancelled.
  3. The read loop cannot reliably forward CANCEL directly: shipped `FrameSink` offers awaited `send` or fallible `try_send` (`router.rs:40-54,69-80`).
- **Suggested Fix**: Combine pop+`Claimed` in one critical section. Reserve the module-sink permit outside the lock, then under the lock commit `Delivered` and synchronously enqueue through the owned permit before CANCEL can observe Delivered; alternatively add a `Sending { cancelled }` state with one ordered sink arbiter.

## Finding 3: `VecDeque<u64>` cannot provide O(1) queued cancellation
- **Severity**: BLOCKER
- **Location**: `v2:38-54,70-75`
- **Confidence**: high
- **Issue**: `slots` identifies the corr but supplies no O(1) link into the FIFO.
- **Evidence**: Removing an arbitrary element from a `VecDeque` requires scanning/shifting. Leaving a tombstone instead makes `slots[corr]` at `v2:87` invalid and strands queue capacity; counting only live slots would instead permit tombstone growth.
- **Suggested Fix**: Use an indexed intrusive deque/slab with O(1) unlink, or specify bounded tombstones plus safe skip/compaction. Pre-size storage and move dropped `Frame`s outside the lock.

## Finding 4: `AcquiredCredit` can double-release; cancellation/abort can leak
- **Severity**: BLOCKER
- **Location**: `v2:88-111,160-164`; `forwarding.rs:1692-1731`
- **Confidence**: high
- **Issue**: Credit ownership is not represented by one consuming object on every exit.
- **Evidence**: The cancellation rollback calls `flow.release()` (`v2:94`) while the armed guard is said to release on drop (`v2:109-111`). With another request outstanding, the second release passes the aggregate nonzero CAS and decrements that other request. Conversely, after `Delivered` commits the guard, cancellation or abort during blocked `module_sink.send` leaves the slot/outstanding credit behind; phase 5 only says to drain queued entries.
- **Suggested Fix**: Make `ChannelFlow::acquire_guard()` return the guard directly. Use consuming `rollback(self)` and `transfer_to_slot(self)` operations—never a raw release plus armed Drop. Cancellation, send error, panic, and abort must all remove the same slot and release only when that removal wins.

## Finding 5: Teardown lacks a synchronized reason, single winner, and usable async owner
- **Severity**: BLOCKER
- **Location**: `v2:135-174`; current cleanup/release paths
- **Confidence**: high
- **Issue**: The described teardown cannot be safely invoked from current source.
- **Evidence**:
  - `teardown` is absent from `RouteInbox`, yet `handle_closed` reads it while holding that lock (`v2:38-43,139-148`); setting it before `flow.close()` under the same lock is not normative.
  - Two concurrent closers can both assign `Closing`, overwrite the reason, and attempt to join/abort one handle.
  - Current route GOODBYE and connection cleanup are synchronous (`control.rs:422-458`; `router.rs:391-397`), while v2 requires an awaited bounded join.
  - Current module reload waits for in-flight quiescence before releasing routes (`supervise.rs:2567-2594`); the generic v2 six-step teardown omits this ordering.
  - Peer-close/error paths wait on the writer without the close-request timeout (`server.rs:258-278`).
- **Suggested Fix**: Put reason and `Option<JoinHandle>` inside one lifecycle object; use an `Open→Closing` compare/owner election. Refactor connection and route shutdown into explicit async APIs, preserve reload quiescence, cancel/join all route and control tasks before writer wait, and retain Drop only as an abort backstop.

## Finding 6: V2 omits non-Request client→module data frames
- **Severity**: BLOCKER
- **Location**: `v2 ; `router.rs:452-498`
- **Confidence**: high
- **Issue**: The dispatcher specifies only Request and CANCEL, but the shipped path forwards every data frame; Responses/Errors/stream frames are required for reverse requests.
- **Evidence**: A consumer Response must route back to a module (`tests/reverse_request.rs:96-137`). These frames intentionally take no forward credit, including on a Serial route (`tests/reverse_request.rs:140-231`). Putting them behind a drain already blocked acquiring credit for another Request can deadlock the module’s reverse RPC.
- **Suggested Fix**: Specify a preemptible, credit-free pass-through lane handled by the same ordered sink arbiter. An urgent Response/CANCEL must interrupt a pending credit acquire without overtaking the Request it semantically targets.

## Finding 7: Client control offload breaks control/data teardown barriers
- **Severity**: BLOCKER
- **Location**: `v2:176-194`
- **Confidence**: high
- **Issue**: “Only route.open blocks” is false, and offloading every client channel-0 frame lets later data overtake connection teardown and supervisor operations.
- **Evidence**: Restart, reload, rescan, set-enabled, and health-probe all await (`control.rs:756-805`). Channel-0 GOODBYE synchronously cleans the whole connection today (`control.rs:2047-2062`), while route GOODBYE is a nonzero-channel frame (`router.rs:335-340`). Under v2, a queued channel-0 GOODBYE or reload can be overtaken by data; a full control queue would incorrectly answer GOODBYE with `control_backpressure`.
- **Suggested Fix**: Keep HELLO/PING/GOODBYE and order-sensitive supervisor controls inline or behind an ingress sequence fence. Offload only parsed `route.open` operations whose cross-data relaxation is explicitly safe. Give the control task connection-scoped cancellation/join ownership.

## Finding 8: Synthetic terminals have no reliable non-awaiting egress
- **Severity**: BLOCKER
- **Location**: `v2:64-76,94-101,193`
- **Confidence**: high
- **Issue**: `cancelled`, `route_backpressure`, `route_closing`, and `control_backpressure` can vanish exactly when egress is saturated.
- **Evidence**: The connection egress channel has capacity 64 (`server.rs:21,243`). Current recoverable router errors await `egress.send` (`server.rs:388-401`); v2 forbids that await, while `FrameSink::try_send` fails on Full (`router.rs:69-80`). A vanished backpressure terminal leaves the SDK with a timeout/outcome-unknown despite the Request provably not reaching the module.
- **Suggested Fix**: Define a reserved/high-priority response lane or response actor with explicit overflow escalation. Add full-egress tests for every synthetic terminal and specify the transport classification if even the reserved lane cannot deliver.

## Finding 9: Corr uniqueness is still not enforced
- **Severity**: BLOCKER
- **Location**: `v2:61-67,316-319`
- **Confidence**: high
- **Issue**: `slots.insert(corr, ...)` silently replaces existing credit ownership; Q4′ is only an open-question lean.
- **Evidence**: If R1(x) is Delivered and R2(x) overwrites it as Queued, R1’s terminal removes a non-Delivered slot and does not release R1; the queued node for R2 then refers to no slot. Sequential reuse is also unsafe: a late duplicate terminal for old x can remove a newly admitted x. Current daemon admission performs no corr check (`router.rs:452-498`).
- **Suggested Fix**: Make uniqueness enforcement normative before enqueue. Reject/close on reuse, including sequential connection-lifetime reuse—not merely `slots.contains_key` for currently live entries—or add a generation that late terminals cannot alias.

## Finding 10: Merge-1 is not standalone-landable
- **Severity**: BLOCKER
- **Location**: `v2:196-221,295-303`
- **Confidence**: high
- **Issue**: Both client-side closed validation and bind publication ordering are incomplete.
- **Evidence**: The current merge-1 client path is `ForwardBackend::handle`, not the merge-2 dispatcher, and it has no `closed` check (`router.rs:432-450`). At bind, route maps are inserted and the route.open frame is made externally visible at `forwarding.rs:1524-1536`; publishing a snapshot later in the same lock is too late on a multithreaded runtime.
- **Suggested Fix**: Merge-1 must add closed checks to both current forwarding directions. Publish the bound snapshot immediately before `client_permit.send`, and set closed/publish removal before every release effect. Add a paused-publication test where the client sends data immediately after receiving route.open.

## Finding 11: `route_closing` contradicts the stale-route and SDK contracts
- **Severity**: BLOCKER
- **Location**: `v2:63-65,167-170,207-220,263-264`
- **Confidence**: high
- **Issue**: The new code is omitted from merge-0 and I8 and prevents the claimed restoration of `unknown_channel`.
- **Evidence**: A reader can load old Bound, observe `closed=false`, pause, then resume after teardown; dispatcher admission is now Closed and emits `route_closing`, not `unknown_channel`. TS wraps unrecognized Error frames as terminal (`client.ts:734-744,1036-1061`), Rust returns `CallError::Module` (`consumer.rs:570-583`), and Swift preserves only text.
- **Suggested Fix**: Emit canonical `unknown_channel` for Closing/Closed stale dispatch, or add a separate stale-route class that evicts/reopens rather than retries the same handle. Update I8 and parity tests if the new code is retained.

## Finding 12: `Slot` cannot retain its Frame after sending it
- **Severity**: MAJOR
- **Location**: `v2:45-46,92-98`
- **Confidence**: high
- **Issue**: A Delivered marker must remain in `slots`, but `module_sink.send(frame)` consumes the Frame.
- **Evidence**: `Slot` always contains `frame: Frame`; the pseudocode says “take Slot,” retain it as Delivered, and also send its frame. `Frame::clone` deep-clones its `Vec<u8>` (`subc-protocol/src/frame.rs:12-17`), potentially copying 64 MiB.
- **Suggested Fix**: Encode ownership in the state, e.g. `Queued(Frame) | Claimed { frame, cancelled } | Delivered`, or use `Option<Frame>` and move it out exactly once.

## Finding 13: Byte and task bounds remain non-normative
- **Severity**: BLOCKER
- **Location**: `v2 Q3′`; route allocation and frame reader
- **Confidence**: high
- **Issue**: Frame-count bounds are not memory bounds, and every live route adds a task.
- **Evidence**: The reader allocates the full body before admission (`frame_io.rs:73-86`), with a 64 MiB maximum (`subc-protocol/src/lib.rs:114-119`). A 2048-frame route can retain roughly 128 GiB, and current allocation permits every nonzero `u16` channel (`forwarding.rs:1293-1333`)—up to 65,535 route tasks per connection.
- **Suggested Fix**: Make per-route, per-connection, and process-global byte budgets mandatory and RAII-charged across enqueue/remove/flush/panic paths. Add practical route/task caps before spawning.

## Finding 14: Whole-table publish creates O(routes) work under the global writer lock
- **Severity**: MAJOR
- **Location**: `v2:203-206,320-321`
- **Confidence**: medium
- **Issue**: Mutations are assumed rare, but authenticated clients can churn route.open/GOODBYE.
- **Evidence**: Every mutation rebuilds the whole snapshot while holding the existing global write lock; current route space reaches 65,535 entries per endpoint (`forwarding.rs:1293-1333`). Repeated churn therefore creates attacker-controlled O(N) locked work and potentially O(N²) aggregate copying.
- **Suggested Fix**: Gate whole-table publication on hard route/rate caps and an adversarial churn benchmark; shard once a measured threshold is exceeded.

## Finding 15: T15 can false-pass against the shipped CAS guard
- **Severity**: MAJOR
- **Location**: `v2:284-285`; `forwarding.rs:1702-1731`
- **Confidence**: high
- **Issue**: Duplicate terminals for the sole in-flight request do not expose R11 today.
- **Evidence**: The first release changes `in_flight` 1→0; the duplicate sees zero and is ignored. The real defect occurs when another request keeps the aggregate count nonzero.
- **Suggested Fix**: T15 must deliver A and B, emit two terminals for A, prove B remains counted, then terminal B and verify exact restoration of the window.

## C. Verified-Safe Points

- **Notify itself is not the problem**: with exactly one drain waiter, Tokio `notify_one()` stores a permit across the `None→wait` gap; notifications may coalesce safely because the drain loops until empty. Do not replace it with non-storing `notify_waiters`.
- **R11 gate and shipped CAS are compatible**: the inbox gate prevents the second release call; the aggregate CAS remains a defensive last layer. It does not conflict, but it also cannot repair other double-release paths.
- **B5’s causal ordering is sound** once Slot ownership is fixed.
- **The module bind-ACK barrier is preserved** by keeping module connections inline.
- **Admission=Closing is a valid stale-push barrier** if one close owner sets it before any global removal.

## D. Q1′–Q5′ Rulings

| Question | Ruling | Reason |
|---|---|---|
| **Q1′ hard-gate merge-0** | **RIGHT-BUT-UNSAFE** | Hard-gating is preferable to the undefined blocking interim, but merge-0 first needs trustworthy daemon provenance and verified broca/aft/alfonso-core deployment. Those repos are absent, so their contract impact is unverifiable here. |
| **Q2′ 2s join / abort acceptable** | **WRONG** | “Connection is closing” is not generally true: the sequence also covers route GOODBYE and endpoint reload. Abort is acceptable only after state/credit cleanup and reason-specific settlement; reload must preserve current quiescence ordering (`supervise.rs:2567-2594`). |
| **Q3′ byte caps** | **RIGHT-BUT-UNSAFE** | Correct direction, but it must be normative, pre-enqueue, RAII-released, and include a process-global budget plus route/task caps. |
| **Q4′ enforce corr uniqueness** | **RIGHT-BUT-UNSAFE** | Enforcement is mandatory, but rejecting only currently in-flight duplicates does not prevent sequential reuse from aliasing a late terminal. |
| **Q5′ whole-table snapshot first** | **RIGHT-BUT-UNSAFE** | Plausible at small scale, but only after fixing publish ordering and imposing churn/route caps; T9 must include adversarial O(N) mutation load. |

## Required Changes Before Re-gate

1. Add unforgeable daemon provenance for retryable admission failures (`router.rs:281-309`).
2. Atomically dequeue+claim and atomically order Request enqueue before Delivered-visible CANCEL handling (`v2:86-97`).
3. Replace `VecDeque` removal with a genuinely O(1), bounded indexed FIFO.
4. Make credit a single consuming RAII ownership object across rollback, send error, cancellation, panic, and abort (`forwarding.rs:1692-1731`).
5. Add a preemptible credit-free lane for Responses/Errors/stream frames and CANCEL (`router.rs:452-498`; `tests/reverse_request.rs:140-231`).
6. Define teardown reason synchronization, one close owner, reload quiescence, explicit async cleanup, and a no-coholding lock hierarchy. Recommended hierarchy: never hold `RouteInbox` while acquiring the forwarding write lock, or vice versa.
7. Cancel/join route and control tasks before every writer wait (`server.rs:238-278`).
8. Provide reliable reserved egress for synthetic terminals.
9. Keep connection GOODBYE and order-sensitive controls inline/fenced (`control.rs:756-805,2047-2062`).
10. Make corr non-reuse, byte budgets, aggregate queue limits, and route/task caps normative.
11. Make merge-1 add current-path closed checks and publish before route.open becomes visible (`forwarding.rs:1524-1536`).
12. Remove or correctly classify `route_closing`.
13. Repair Slot frame ownership and strengthen T15.

## Summary

- **B1–B10**: 4 CLOSED, 5 NOT-CLOSED, 1 NEWLY-BROKEN.
- **Findings**: **12 BLOCKER**, **3 MAJOR**.
- **Overall risk**: unacceptable; confidence high on the concurrency and source contradictions, medium only on the performance threshold for whole-table publication.
- **External limitation**: broca, aft, and alfonso-core are unavailable in this checkout, so merge-0 fleet compatibility cannot be validated.

A single blocker would require NO-GO; multiple independent blockers remain.