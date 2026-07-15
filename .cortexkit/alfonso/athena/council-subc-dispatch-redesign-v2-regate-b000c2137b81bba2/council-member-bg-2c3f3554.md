## V1 blocker verdicts (B1–B10)

| Blocker | Verdict | Justification |
|---------|---------|---------------|
| **B1** (SDK `route_backpressure` / zero SDK changes) | **NOT-CLOSED** | v2 correctly retracts v1 and mandates merge-0 (`docs/subc-dispatch-redesign-v2.md` , but **no SDK change exists in this repo**: TS `classifyFailure` only distinguishes `not_sent` vs `outcome_unknown` when `handedToSocket` (`clients/subc-client/src/client.ts:781-793`); daemon `Error` terminals settle via `errorFromFrame` → `SubcError` (`1059-1060`), and `call()` retries only `unknown_channel` / `not_sent`, treating other errors as terminal or `outcome_unknown` + reconnect (`423-452`). Rust mirrors: `CallError::Module` with no `route_backpressure` retry (`crates/subc-client-rs/src/consumer.rs:570-579`, `3130-3134`). Until merge-0 is deployed fleet-wide (broca/aft/alfonso-core **unverifiable here**), merge-2 fail-loud admission is still a production contract break. |
| **B2** (CANCEL limbo / double terminal) | **NOT-CLOSED** | The single-lock state machine fixes v1’s acquire/send limbo **only if** queue pop and slot mutation are one atomic step. v2 splits them: `pop_front` then a **second** lock to set `Claimed` (`docs/subc-dispatch-redesign-v2.md`  lines 86-87). A CANCEL on `Queued` can `remove` slot+queue while the drain already holds `corr` from `pop_front` → next step does `slots[corr].state = Claimed` on a **missing** entry (unspecified → panic or skip → credit/request orphan). That revives limbo/double-terminal at the pop boundary, not only at acquire. |
| **B3** (queue primitive / mpsc scan race) | **CLOSED** | `Mutex<RouteInbox>` with `VecDeque` + `HashMap` corr→`Slot`, O(1) CANCEL and drain (`, `). No concurrent mpsc recv + read-loop scan. |
| **B4** (drain error arms / RAII / `ChannelFlowClosed`) | **NOT-CLOSED** | Send-failure `release` + `AcquiredCredit` + `outstanding` rollback are specified (` lines 97-101, 109-111). **`TeardownKind` is not wired in shipped code**: `release_client_route_locked` only calls `route.flow.close()` (`crates/subc-core/src/forwarding.rs:1424`) with no reason field; `handle_closed` reads `self.teardown` (`) but the spec does not require atomic set-on-close at every `close()` site (release, reload drain, conn close, module death). Shipped disambiguation uses `endpoint_is_draining` on acquire failure in the read path (`router.rs:465-477`), not a per-route teardown flag—mapping module death to `None`→`backend_error` (`) is plausible for `blocked_flow_control_acquire_wakes_when_module_tears_down` (`crates/subc-core/tests/forwarding.rs:3811-3877`) **only if** teardown is set correctly on every close; that ordering is not specified. |
| **B5** (`outstanding` / Delivered before send) | **CLOSED** (conditional on B2) | `state=Delivered; outstanding+=1` under inbox lock **before** `module_sink.send().await` (` lines 92-97). Module→client terminal `slots.remove` + `release` (`) can then observe Delivered. Ordering matches the fast-terminal race v1 flagged (`router.rs:281-309` today releases unconditionally). **Invalid if B2 pop/slot race fires.** |
| **B6** (O(queue) CANCEL DoS) | **CLOSED** | `slots.get_mut(corr)` is O(1) (` lines 72-76); no read-loop scan. |
| **B7** (whole channel-0 FIFO / bind-ACK barrier) | **CLOSED** | Module connections stay inline (`); only **client** channel-0 is offloaded. Shipped barrier test `accepted_route_publishes_route_open_before_immediate_reverse_request` (`router.rs:1078-1102`) depends on module inline commit—v2 explicitly preserves that. Reserved/Absent drops remain on module path (`router.rs:227-245`). |
| **B8** (I3 “release untouched” / R11 framing) | **CLOSED** | v2 corrects framing: concurrent duplicate while `in_flight>0` (` lines 115-118). Call-site gate `slots.remove` was Delivered → `release` (` lines 123-128) **complements** shipped CAS `in_flight!=0` guard (`forwarding.rs:1702-1731`)—first terminal decrements; duplicate finds slot gone → `release=false` → second `release()` hits `observed==0` and is ignored. No conflict; redundant safety. |
| **B9** (GOODBYE flush vs enqueue; teardown hang) | **NOT-CLOSED** | `admission=Closing` before snapshot removal (` steps 1, 5-6) closes the stale-snapshot **push** hole in principle. Gaps: (1) drain pseudocode shows bare `module_sink.send().await` (` line 97)—`select!` against `cancel_token` is prose-only (` line 171), not normative in the loop; (2) `RouteBinding` clones `module_sink` on release (`forwarding.rs:1432`); a drain task blocked on `send` with a retained `FrameSink` can still hold the bounded channel open—same class as v1 #9 (`server.rs:262-272` writer grace/abort does not unblock module sink); (3) read-loop synthetic errors still **await** egress today (`server.rs:388-400`); v2 read path says “synthesize” without mandating non-blocking egress—v1 #13 survives. |
| **B10** (merge-1 snapshot not invariant-neutral) | **CLOSED** (design-level) | Publish-under-write-lock + `closed: AtomicBool` set before publish-remove + recheck → `unknown_channel` (`, `router.rs:350-358` pattern). Restores stale-`Bound` observable vs `backend_error` from blocked acquire on dead flow (`router.rs:465-484`). **Implementation not present**; mechanism is coherent if every consumer checks `closed` after snapshot load. |

---

## New defects introduced by v2

### Finding N1: Pop-then-Claimed is not atomic with CANCEL (reopens B2)
- **Severity**: BLOCKER  
- **Location**: `docs/subc-dispatch-redesign-v2.md`  drain loop (lines 86-87)  
- **Confidence**: high  
- **Issue**: After `pop_front`, the corr is out of the FIFO but may still be `Queued` in `slots`. CANCEL can remove the slot and synthesize `cancelled` while the drain still proceeds with that `corr`, leading to missing-slot access, duplicate terminals (synthetic + module), or an acquired credit with no slot.  
- **Evidence**: Two separate critical sections; CANCEL only matches `Some(Queued)` etc. (` 72-76), not “popped but not yet Claimed.”  
- **Suggested fix**: Single lock: `pop_front` + transition `Queued→Claimed{cancelled:false}` atomically; or keep corr in queue until post-acquire claim; drain must handle absent slot after pop as rollback without acquiring.

### Finding N2: Duplicate-corr `slots.insert` can leak credit (Q4′ not enforced)
- **Severity**: BLOCKER  
- **Location**: ` read loop line 66; ` Q4′ lean only  
- **Confidence**: high  
- **Issue**: `slots.insert(corr, …)` silently overwrites; v1 #11 remains. First in-flight corr loses its slot; terminal/release accounting attaches to one corr; the other leaks window capacity (`router.rs:452-497` admits without corr uniqueness).  
- **Evidence**: No reject-on-duplicate in pseudocode; wire uniqueness not enforced in daemon.  
- **Suggested fix**: Under lock, reject duplicate corr in `queue`/`slots`/non-terminal `outstanding` with protocol error close; normative in  not only Q4′.

### Finding N3: `TeardownKind` lifecycle unspecified vs all `flow.close()` sites
- **Severity**: MAJOR  
- **Location**: `, ` step 2; `forwarding.rs:1424`, `1455`  
- **Confidence**: high  
- **Issue**: `handle_closed` branches on `teardown` (` 141-148) but shipped `close()` has no kind. Without a happens-before rule (set kind under inbox or forwarding write lock, then `close()`), drain can read stale `None` and emit wrong terminal (module reload vs `backend_error` vs silent drop).  
- **Suggested fix**: Extend close path: `set_teardown(kind); flow.close()` under defined lock order; table every caller (release, reload, conn drop).

### Finding N4: `AcquiredCredit` + explicit `release` on rollback/send-error may double-release
- **Severity**: MAJOR  
- **Location**: ` lines 94-101, 109-111  
- **Confidence**: medium  
- **Issue**: Rollback and send-error paths call `flow.release()` explicitly while `AcquiredCredit` may still be in scope until `commit()`. Double `release()` is masked by CAS (`forwarding.rs:1705-1714`) but can hide a logic bug (outstanding decremented once, credit released twice attempt).  
- **Suggested fix**: Normative: `commit()` disarms guard before terminal-path release; rollback uses guard drop only, or explicit release + `mem::forget(guard)`.

### Finding N5: Notify lost-wakeup / spurious-wakeup pattern unspecified
- **Severity**: MAJOR  
- **Location**: ` `notify`; ` line 86 (“if None -> wait notify”)  
- **Confidence**: medium  
- **Issue**: Classic pattern requires `notify_one` after push (` 67) and a loop: recheck queue under lock after `notified().await` to avoid lost wakeup if notify fires between pop-empty and wait. Spec does not show the wait loop or `Closing` exit predicate.  
- **Suggested fix**: Document `while admission==Open { lock; pop or break; unlock; work; }` / `notified()` with lock-held recheck.

### Finding N6: Global forwarding write lock vs `RouteInbox` mutex hierarchy absent
- **Severity**: MAJOR  
- **Location**: `, `; `forwarding.rs` `RwLock` + future per-route `Mutex`  
- **Confidence**: medium  
- **Issue**: Teardown step 6 uses epoch-fenced release under forwarding write lock (`forwarding.rs:1409-1470`) while dispatcher holds `inbox`. If any path takes `write_inner` then awaits or blocks on `inbox` (or reverse), deadlock. v2 does not state lock order (e.g. **never** hold `inbox` across forwarding write).  
- **Suggested fix**: Explicit hierarchy: forwarding write > route inbox; teardown sets `admission`/`teardown` without nesting; snapshot publish only under write lock with inbox released.

### Finding N7: Synthetic terminal egress still blocking / lossy
- **Severity**: MAJOR  
- **Location**: ` synthesize paths; shipped `server.rs:388-400`  
- **Confidence**: high  
- **Issue**: v2 promises `cancelled` / `route_backpressure` / `route_closing` from read loop “non-blocking O(1)” (`, I6) but does not require reserved try_send lane or connection close on full egress. Full client egress (`CONNECTION_EGRESS_BUFFER` 64, `server.rs:243`) can drop or force await—zero-terminal / renewed HOL.  
- **Suggested fix**: Reserved synthetic egress or actor; on `Full`, escalate close; forbid `.await` on read hand-off except documented exceptions.

### Finding N8: Client control-queue offload can reorder module relay responses (v1 #14 partial carryover)
- **Severity**: MAJOR  
- **Location**: `  
- **Confidence**: medium  
- **Issue**: Client channel-0 FIFO can queue client `route.open` behind other control while module **Responses** on channel 0 still go through control handler inline on whichever connection (`router.rs:207-217`). If client control task reorders client-originated control vs data, OK; but `control_backpressure` on overflow (`) applied to responses that settle relay RPCs is still hazardous—v2 does not exempt module-originated channel-0 responses.  
- **Suggested fix**: Inline module control responses / relay completions; cap only client-issued control commands.

### Finding N9: Byte/memory DoS (v1 #12) still open
- **Severity**: MAJOR  
- **Location**: ` Q3′ only; depth_cap frame-count (` line 65)  
- **Confidence**: high  
- **Issue**: `depth_cap` bounds frame count, not bytes; bodies allocated on read before admission (`frame_io` path per v1 synthesis). v2 does not charge byte budget at enqueue.  
- **Suggested fix**: Per-route/per-connection byte cap at push, RAII on dequeue/teardown.

---

## Q1′–Q5′ lean rulings

| Q | Lean in v2 | Ruling |
|---|------------|--------|
| **Q1′** | Hard-gate merge-0 | **RIGHT** — fail-loud without SDK is proven fatal (`client.ts:781-793`, `consumer.rs:570-579`). Interim blocking admission is **RIGHT-BUT-UNSAFE** if used to skip merge-0 indefinitely (reintroduces wait semantics, must cap wait). |
| **Q2′** | 2s bounded-join; abort OK on close | **RIGHT** — matches `server.rs:262-272` writer pattern; acceptable to drop in-flight terminal when connection is closing **if** credit/outstanding drained in step 5 and abort does not leak `in_flight` (`forwarding.rs:1697-1698`). |
| **Q3′** | Frame cap; byte cap “lean measure” | **WRONG** as gate lean — frame-only cap does not close DoS; must be **RIGHT-BUT-UNSAFE** to ship merge-2 without byte budget. |
| **Q4′** | Enforce corr uniqueness at enqueue | **RIGHT** — but **WRONG** until  mandates it (currently **NOT-CLOSED**, N2). |
| **Q5′** | Whole-table snapshot | **RIGHT, conditional** on publish-under-lock + `closed` recheck (`); shard later per T9. |

---

## Summary

| Severity | New findings |
|----------|----------------|
| BLOCKER | N1 (pop/Claimed vs CANCEL), N2 (duplicate corr) |
| MAJOR | N3–N9 |

**V1 blockers fully CLOSED:** B3, B6, B7, B8.  
**Conditionally CLOSED:** B5 (needs B2), B10 (design only).  
**NOT-CLOSED:** B1 (merge-0 not in tree / fleet), B2 (pop atomicity), B4 (TeardownKind wiring), B9 (select!/sink clones/synthetic egress).

---

## Bottom-line verdict: **NO-GO**

Un-refuted concurrency/contract defects remain:

1. **B2 / N1** — split `pop_front` and `Claimed` leaves a CANCEL vs drain race (`docs/subc-dispatch-redesign-v2.md:86-87`).  
2. **B1** — merge-0 prerequisite not implemented; current SDKs treat data-plane Errors as terminal/outcome_unknown (`client.ts:1059-1060`, `423-452`; `consumer.rs:570-579`).  
3. **N2 / Q4′** — duplicate corr not enforced at enqueue (`66`).  
4. **B4 / N3** — `TeardownKind` not tied to all `flow.close()` call sites (`forwarding.rs:1424`).  
5. **B9 / N7** — teardown `select!` and non-blocking synthetic egress not normative; `FrameSink` clone hang class persists (`forwarding.rs:1432`, `server.rs:388-400`).

### Minimum changes for **GO-WITH-CHANGES** (re-gate after spec edit + tests T10–T13)

1. Atomic **pop+Claim** (or equivalent) under one inbox lock (`86-87`).  
2. **Reject duplicate corr** at enqueue (`66`).  
3. **TeardownKind** set-before-close table for every `flow.close()` (`forwarding.rs:1409-1470`).  
4. Normative drain **`select!`** (acquire, send, cancel_token) + **lock hierarchy** doc (`, `).  
5. **Synthetic error egress** policy (try_send / reserved buffer / close on full) (`server.rs:243`, `388-400`).  
6. **merge-0 landed and verified** in consumer repos before merge-2 emit (`).  
7. **AcquiredCredit** commit/disarm rules (`94-101`).  
8. Notify **wait loop** with lock-held recheck (`86`).  
9. Add **byte budget** to admission or accept as explicit re-gate blocker (` Q3′`).

Until those are in the spec and covered by T10–T13 (and merge-0 is deploy-proven), the gate should stay **NO-GO**.