## Finding 1: Missing Correlation ID Uniqueness Check at Enqueue
- **Severity**: high
- **Location**: `docs/subc-dispatch-redesign-v2.md`  (Read loop data Request frame pseudocode)
- **Confidence**: high
- **Issue**: The read loop enqueues incoming requests and inserts them into `slots` without checking if the correlation ID (`corr`) is already present. If a client sends a duplicate or reused `corr` (which is a protocol violation but possible from buggy or malicious clients), `slots.insert(corr, ...)` will overwrite the existing slot. This leads to permanent credit leaks (the overwritten slot's credit is never released) and state corruption (terminal frames for the old request will remove the new slot or vice versa).
- **Evidence**: The pseudocode in  for the read loop data Request frame does:
  ```
  slots.insert(corr, Slot{frame, Queued}); queue.push_back(corr)
  ```
  without any check for `slots.contains_key(corr)`.
- **Suggested Fix**: Add a check `if slots.contains_key(corr)` at the beginning of the read loop's data Request frame handler. If true, treat it as a protocol violation and close the connection immediately.

## Finding 2: O(N) Queue Removal in CANCEL Path
- **Severity**: medium
- **Location**: `docs/subc-dispatch-redesign-v2.md`  (Read loop CANCEL frame pseudocode)
- **Confidence**: high
- **Issue**: The CANCEL path attempts to remove the correlation ID from the queue structure: `Some(Queued) => remove from queue+slots`. Since `queue` is a `VecDeque<u64>`, removing an arbitrary element from the middle of a `VecDeque` is an O(N) operation where N is the queue depth (up to 4096). Under saturation, an attacker sending bursts of CANCEL frames for queued requests can cause significant CPU overhead on the latency-critical read loop, violating the O(1) non-blocking claim.
- **Evidence**: The pseudocode in  states:
  ```
  Some(Queued) => remove from queue+slots; unlock; synthesize Error{cancelled} to client
  ```
  and  defines `queue: VecDeque<u64>`.
- **Suggested Fix**: Use lazy deletion (tombstoning). In the CANCEL path, remove the corr only from `slots` (which is a `HashMap` and thus O(1)) and do NOT touch `queue`. When the drain task pops a corr from `queue`, it locks `inbox` and checks if the corr is in `slots`. If not found, it simply ignores it and continues.

## Finding 3: Memory Leak of Claimed/Delivered Slots during Teardown
- **Severity**: medium
- **Location**: `docs/subc-dispatch-redesign-v2.md`  (Teardown step 5)
- **Confidence**: high
- **Issue**: Step 5 of the teardown path states: "lock inbox: admission = Closed; drain queued corrs -> synthesize cancelled/drop per reason". However, draining only the `queue` leaves any slots that are in `Claimed` or `Delivered` states inside the `slots` map. While the dispatcher is eventually dropped when the route is released, any delay or failure in releasing the route will leak these slots in memory. Furthermore, the client will not receive terminal frames for these outstanding requests.
- **Evidence**: Step 5 in  only specifies draining "queued corrs" (which refers to the `queue` VecDeque), leaving the `slots` HashMap partially populated.
- **Suggested Fix**: Modify step 5 to drain the entire `slots` map:
  ```rust
  lock inbox:
    admission = Closed;
    for (corr, slot) in slots.drain() {
        if slot.state is Queued or Claimed:
            synthesize Error{cancelled} to client;
    }
  ```

## Finding 4: Unverifiable Contract Impact on External Consumers
- **Severity**: low
- **Location**: `docs/subc-dispatch-redesign-v2.md`  (SDK merge-0)
- **Confidence**: high
- **Issue**: The design states that the daemon dispatch merge is gated on merge-0 being live in external consumers `broca`, `aft`, and `alfonso-core`. However, these repositories are not present in the current workspace, making it impossible to verify the contract impact or the readiness of these consumers.
- **Evidence**: The codebase contains only `fake-aft-stub.rs` and no actual source code for `broca`, `aft`, or `alfonso-core`.
- **Suggested Fix**: Explicitly flag this as an external dependency that must be audited and verified in their respective repositories before landing merge-2.

## Summary

### Total Findings by Severity
- **Critical**: 0
- **High**: 1 (Finding 1)
- **Medium**: 2 (Finding 2, Finding 3)
- **Low**: 1 (Finding 4)

### Overall Risk Assessment
The overall risk of the v2 design is **Low-to-Medium**, assuming the suggested changes are implemented. The design successfully addresses all 10 blockers from v1 with robust concurrency mechanisms. Confidence in this assessment is **High** based on direct source code verification.

### Verification of V1 Blockers (B1-B10)
- **B1**: **CLOSED**. The design introduces a prerequisite SDK merge-0 that updates TS (`client.ts:420-455`), Rust (`consumer.rs:560-588`), and Swift (`Client.swift:31-34`) clients to classify `route_backpressure` and `control_backpressure` as retryable errors and retry them in-place.
- **B2**: **CLOSED**. The route-local state machine (`Queued -> Claimed -> Delivered`) under the single `inbox` lock eliminates the limbo window. CANCEL and delivery transitions are serialized under the same lock, preventing double-firing or lost cancels.
- **B3**: **CLOSED**. The design explicitly specifies `RouteInbox` with `queue: VecDeque<u64>` and `slots: HashMap<u64, Slot>` protected by a single `Mutex` (, avoiding data races between the read loop and the drain task.
- **B4**: **CLOSED**. The drain task error arms are fully specified ( , including `ChannelFlowClosed` handling (mapping to `module_reloading` or `backend_error` to preserve the test `blocked_flow_control_acquire_wakes_when_module_tears_down` in `tests/forwarding.rs:3811`) and an RAII `AcquiredCredit` guard for panics.
- **B5**: **CLOSED**. The state is updated to `Delivered` and `outstanding` is incremented under the lock BEFORE `module_sink.send().await` (, ensuring a fast module terminal cannot arrive before the slot is marked `Delivered`.
- **B6**: **CLOSED**. The `slots` HashMap provides O(1) lookup for CANCEL frames ( , eliminating the O(queue) linear scan on the read loop.
- **B7**: **CLOSED**. Scoping the control offload to the client-side connection only ( ensures that module-side bind ACKs and subsequent data frames stay on the single inline read path, preserving the bind barrier test `accepted_route_publishes_route_open_before_immediate_reverse_request` (`router.rs:1078-1102`).
- **B8**: **CLOSED**. The invariants I3 and I7 are reworded to honestly reflect the changes to the release call site (`router.rs:307-309`) and the addition of the `closed` recheck.
- **B9**: **CLOSED**. The 3-phase teardown gate ( uses `admission = Closing` as a barrier, a connection-scoped `cancel_token` to prevent hangs on blocked sends, and a bounded-join + abort to ensure the drain task exits.
- **B10**: **CLOSED**. The snapshot is published under the write lock, and the `closed: AtomicBool` flag on `RouteBinding` is checked by data-plane consumers to restore the `unknown_channel` observable on stale-Bound paths (.

### Rulings on Open Questions (Q1'-Q5')
- **Q1'**: **RIGHT**. Hard-gating on merge-0 is cleaner and avoids the complexity of shipping an interim blocking-admission mechanism.
- **Q2'**: **RIGHT**. Dropping an in-flight terminal during connection close is acceptable since the connection is terminating.
- **Q3'**: **RIGHT**. A byte-based secondary cap is necessary to prevent memory DoS from large frames.
- **Q4'**: **RIGHT**. Enforcing correlation ID uniqueness at enqueue is correct and necessary to prevent credit leaks and state corruption.
- **Q5'**: **RIGHT**. Whole-table Arc swap is appropriate since mutations are rare compared to lookups.

### Bottom-Line Verdict
**GO-WITH-CHANGES**

**Required Changes:**
1. **Enforce Correlation ID Uniqueness**: Check `slots.contains_key(corr)` in the read loop data Request path ( and close the connection on violation.
2. **Lazy Deletion for CANCEL**: Implement lazy deletion (tombstoning) in the CANCEL path ( to avoid O(N) queue removal on `VecDeque`.
3. **Drain All Slots on Teardown**: Drain the entire `slots` map (not just the `queue`) in step 5 of the teardown path ( to prevent memory leaks and ensure all pending requests receive terminal frames.
4. **Define Lock Hierarchy**: Explicitly document and enforce the lock hierarchy: **Global Forwarding Write Lock -> Route Inbox Lock**.