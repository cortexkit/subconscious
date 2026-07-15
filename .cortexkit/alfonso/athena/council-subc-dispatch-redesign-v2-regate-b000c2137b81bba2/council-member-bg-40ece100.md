# subc-core dispatch redesign v2 — adversarial re-gate verdict

I read the v2 doc, the v1 synthesis, and the shipped source (forwarding.rs, router.rs, control.rs, server.rs, supervise.rs, fake-aft-stub.rs, subc-client-rs/consumer.rs, subc-client/client.ts). The v2 mechanism is sound in spirit, but as specified it is **not implementable** and re-introduces at least two race windows the v1 gate explicitly closed. I find the v2 spec to be in the same "redesign the mechanism, re-gate" state as v1.

## B1–B10: closure verdict

### B1 (SDK prerequisite — `route_backpressure` → NotSent false claim) — **CLOSED**
The v2 doc correctly identifies that the v1 "zero SDK changes" claim is false. Verified at source: `clients/subc-client/src/client.ts:427-450` retries only `code === "unknown_channel"` or `kind === "not_sent"`; `errorFromFrame` (client.ts:1118-1127) produces a plain `SubcError` wrapped as `terminalCallError` ("terminal" kind) for an Error frame, and `call()` does NOT retry terminal. `crates/subc-client-rs/src/consumer.rs:570-579` mirrors this (returns `CallError::Module(body)` for non-unknown_channel errors). v2's `merge-0` prerequisite is the right fix; Q1' presents the hard-gate vs interim decision honestly.

### B2 (CANCEL queued→delivered limbo) — **CLOSED** (with one new race below)
The per-corr state machine `Queued→Claimed→Delivered` with CANCEL decided under the same lock removes the v1 limbo. Verified by tracing each CANCEL arrival point: Queued → daemon-synthesized cancelled (no credit held); Claimed{cancelled} → set flag, drain rolls back; Delivered | None → forward to module, its terminal releases. The state machine is sound.

### B3 (queue data structure unspecified / mpsc-scan race) — **CLOSED**
v2 specifies `VecDeque<u64>` + `HashMap<u64, Slot>` behind a per-route `Mutex<RouteInbox>`, with O(1) push/pop/get/remove-by-corr and no mpsc. No data race. Concrete enough to implement.

### B4 (drain-task error arms unspecified) — **PARTIALLY CLOSED (one wire-observable regression)**
v2 specifies the three error arms: `ChannelFlowClosed` disambiguation (Reloading/Goodbye/ConnClose/None), `module_sink.send` failure (backend_error + release), RAII panic guard. The shipped test `blocked_flow_control_acquire_wakes_when_module_tears_down` (forwarding.rs:3811-3877) accepts EITHER a route GOODBYE OR an `Error{backend_error}` for the blocked corr. The v2 mechanism maps `module.stop()` → connection-close path → teardown=ConnClose → silent drop. The test stays green because the client eventually sees a GOODBYE, which the test accepts. **But the wire-observable for "module dies with blocked acquire" changes from `backend_error` to silent drop + later GOODBYE**, contradicting v2's own I7 "module→client wire behavior unchanged" (the error is on the client→module terminal path, not strictly module→client, but the client observable is different). The T8 preservation claim is technically true for the assertion but materially wrong for the observable.

### B5 (outstanding insert-ordering) — **CLOSED**
v2 sets `state=Delivered` and increments `outstanding` UNDER the inbox lock BEFORE `module_sink.send().await`. The module cannot emit a terminal for a frame it hasn't seen, so a fast module terminal cannot race the insert. Sound.

### B6 (O(queue) CANCEL scan DoS) — **CLOSED**
CANCEL is O(1) via `slots.get_mut(corr)`. No linear scan on the read loop. ✓

### B7 (channel-0 FIFO breaks bind-ACK→data barrier) — **CLOSED**
v2 explicitly scopes offload to the CLIENT connection's channel-0 only. Module connections stay serial: HELLO, bind-ack, catalog.update, and data all run on the module connection's single inline read path, so the bind-commit-before-next-frame barrier is preserved by construction. The shipped test `accepted_route_publishes_route_open_before_immediate_reverse_request` (router.rs:1078-1102) is a unit test that doesn't go through the read-loop offload, so it stays green; v2's T14 plans the real integration test.

### B8 (false invariants I3/I7/I4) — **CLOSED**
v2 rewrites I3 (release call site gains exactly-once gate), I4 (GOODBYE flushes queued client→module frames, raw-wire semantic change), I7 (closed recheck added, duplicate/late-terminal credit fixed) as honest DELTAs. The "trusted vs enforced" R11 framing is corrected to the concurrent-duplicate case. ✓

### B9 (GOODBYE flush non-atomic + teardown hang) — **PARTIALLY CLOSED (one missing piece)**
The 3-phase `Open/Closing/Closed` admission gate is the right primitive: `admission=Closing` happens-before the snapshot removal that drops the binding, so a stale-snapshot reader hits `admission != Open` and synthesizes `route_closing` rather than delivering to a released module. The bounded-join + abort after 2s (Q2' guess, no justification but in line with the shipped `CLOSE_DRAIN_GRACE=2s` at server.rs:28) prevents connection-close hang. The JoinHandle is owned by the teardown path, not the binding, avoiding the binding→handle→task→binding strong cycle. **However, the v2 mechanism does not specify how the teardown path finds all live dispatchers for routes on a closing connection** — see New Defect 2 below.

### B10 (merge-1 not invariant-neutral) — **CLOSED**
v2 publishes the snapshot INSIDE the write-lock critical section for every mutation, and sets `RouteBinding.closed: AtomicBool` to true at release BEFORE the snapshot that removes it is published. Data-plane consumers check `closed` after loading the snapshot and treat closed-but-still-visible as `unknown_channel` (using the existing `RouterError::UnknownChannel` at router.rs:642) — restoring today's `unknown_channel` observable exactly. The TOCTOU between snapshot load and `closed.load` is safe because `closed` is an `AtomicBool` with a fresh Acquire load at the check, not a value captured at snapshot-load time. The v1 finding's specific claim (new `backend_error` state) is avoided.

---

## New defects introduced by v2

### New Defect 1 (BLOCKER): Module→client terminal path cannot reach the dispatcher's `slots` for the R11 gate
**File**: `crates/subc-core/src/router.rs:281-309`; v2 `docs/subc-dispatch-redesign-v2.md:122-128`.
**Issue**: v2 specifies that the module→client terminal path must do `slots.remove(corr)` on the route's `RouteInbox` to enforce the exactly-once release gate:
```
on terminal frame for corr, module->client path:
  lock inbox:
    if slots.remove(corr) was Delivered: outstanding-=1; release=true else release=false
```
But the shipped `RouteBinding` (`crates/subc-core/src/forwarding.rs:52-65`) has no reference to any `RouteDispatcher`. The v2 doc's `RouteBinding` change is *only* `closed: AtomicBool` (v2  lines 207-211). The module→client path at router.rs:285-310 holds an `Arc<RouteBinding>` cloned from `lookup_data_route` (forwarding.rs:840-890) and has no path to the route's per-dispatcher inbox. **The v2 mechanism as written is unworkable** — the module→client path literally cannot acquire the route's `RouteInbox` mutex. The implementation would need to add `dispatcher: Weak<RouteDispatcher>` (or equivalent) to `RouteBinding`, and the v2 doc is silent on this. Same gap applies to the rollback-on-admission=Closing path in the drain: who tells the binding's "the route is Closing"? It needs the dispatcher↔binding wiring both directions.

### New Defect 2 (BLOCKER): Connection-close teardown cannot enumerate live dispatchers
**Files**: `crates/subc-core/src/server.rs:241-267` (writer wait), `crates/subc-core/src/forwarding.rs:1168-1239` (`cleanup_connection`), v2  lines 156-174.
**Issue**: v2's teardown step 1 says "lock inbox: admission = Closing" for each live dispatcher on the closing connection. The teardown step 4 says "bounded-join the drain task". But the v2 doc specifies NO data structure for "the set of live dispatchers". The `ForwardingTable` (`forwarding.rs:283-287`) holds `inner: RwLock<ForwardingInner>` with `client_to_module: HashMap<ClientRouteKey, Arc<RouteBinding>>` and `module_to_client: HashMap<ModuleRouteKey, Arc<RouteBinding>>` — but no dispatcher map. `cleanup_connection` (forwarding.rs:1168-1239) iterates `client_to_module` to release routes, and never touches a dispatcher. Without a `dispatchers: HashMap<ClientRouteKey, Arc<RouteDispatcher>>` (or equivalent) in `ForwardingInner`, the teardown path has no way to find the dispatchers whose `admission` must be set to `Closing`, whose `flow` must be closed, whose `cancel_token` must be cancelled, and whose `JoinHandle` must be bounded-joined. **The v2 mechanism as written is unworkable** — the teardown path literally cannot find the dispatchers.

### New Defect 3 (MAJOR): Two-lock-dance in the drain task reintroduces a CANCEL window
**File**: v2  lines 86-87.
**Issue**: The drain task does:
```
corr = { lock inbox; pop_front queue or None }        // lock #1
{ lock inbox; slots[corr].state = Claimed{cancelled:false} }   // lock #2
```
Two separate `lock inbox` acquisitions. Between them, the state is "slot exists, not in queue, state=Queued". A CANCEL arriving in this window finds state=Queued, removes the slot from `slots`, and synthesizes a cancelled terminal. Then the drain re-locks and tries `slots[corr].state = Claimed{...}` — but `slots` no longer has the corr. In Rust this is a panic (`IndexMut` on missing key) or a silent no-op depending on the implementer's choice. The v2 doc's prose claims "the read loop and the drain task both make their cancel/deliver decision under the SAME lock" — this is true for the read loop's CANCEL vs the drain's claim, but the drain's own pop-then-claim is a window the prose doesn't cover. The fix is to combine pop+state-Claimed into one lock acquisition, or to handle the "slot missing because CANCEL raced" case explicitly. This is a v1-B2-style race the v2 claims to have closed.

### New Defect 4 (MAJOR): `AcquiredCredit` RAII guard's `commit()` is implicit in the pseudocode
**File**: v2  lines 88-101.
**Issue**: v2 says "scope guard (`AcquiredCredit` that calls `flow.release()` on drop unless `commit()`ed)" but the pseudocode never shows a `commit()` call. The Ok arm of `module_sink.send().await` is:
```
Ok(()) => {}                                          // terminal will release on module->client path
```
Without an explicit `guard.commit()` here, the guard's drop fires on function return, calling `flow.release()` — and the module→client terminal path also calls `flow.release()` after `slots.remove(corr)` returns true. The shipped `flow.release()` has a CAS `in_flight!=0` guard (forwarding.rs:1702-1731) that ignores over-release only when `in_flight==0`; if both releases race while `in_flight==1`, the second CAS decrements to 0 and adds a permit — a real double release. The v2 mechanism's "exactly-once" guarantee depends on an implementation detail (`commit()`) that the spec doesn't mandate. The same hazard exists on the rollback path (v2 line 94: explicit `flow.release()` plus guard's implicit drop release).

### New Defect 5 (MAJOR): Rollback path may orphan slots in `slots` (memory leak)
**File**: v2  lines 92-94.
**Issue**: The rollback on `Claimed{cancelled:true}` or `admission==Closed` is:
```
unlock; flow.release(); synthesize Error{cancelled} if route-alive; drop; continue
```
"drop" is ambiguous — it could mean "drop the frame" (slot remains) or "remove the slot and drop the frame". The pseudocode does not say `slots.remove(corr)`. If the implementation interprets "drop" as frame-only, the slot remains in `slots` with state=Claimed{cancelled:true} forever (no future terminal will arrive for a Request the module never saw). Over time, for a route that receives many cancellations, `slots` grows unboundedly. This is a slow memory leak that would only surface under long-running high-cancellation workloads. The doc should specify `slots.remove(corr)` explicitly.

### New Defect 6 (MAJOR): Panic guard releases credit but does not clean up slot (memory leak + inconsistent state)
**File**: v2  line 111.
**Issue**: v2 says the RAII guard "releases the credit" on panic between acquire and send. But the slot is in `slots` with state=Claimed, and the `outstanding` count is unaffected (the drain hadn't reached the Delivered increment yet). After the panic, the route is alive, the binding is live, but the slot is orphaned. The next CANCEL for that corr would find state=Claimed and set `cancelled=true` — but the drain task is dead, so the flag is never observed. The frame is held forever in the slot. Worse, the v2 doc's T10 test plan tests CANCEL in each state but does NOT test a drain-task panic. This is a v1-B4 gap the v2 claims to have closed.

### New Defect 7 (MEDIUM): `slots` HashMap insert silently overwrites on duplicate corr
**File**: v2  line 41 (`slots: HashMap<u64, Slot>`),  line 66 (`slots.insert(corr, Slot{frame, Queued})`).
**Issue**: v2 Q4' explicitly says corr-uniqueness is a protocol requirement but the mechanism section does NOT enforce it. In Rust, `HashMap::insert` is a silent overwrite: the old slot is dropped, the old frame is leaked (or freed), and the old credit is permanently leaked (no future terminal will find the old slot under `slots.remove(corr)` — the new slot is there, and when the new terminal fires it removes the new slot, never the old one). The v1 B11 finding (3/8 raised it as a BLOCKER for two) is acknowledged as Q4' but not closed by the mechanism. v2 should add `if slots.contains_key(&corr) { synthesize Error{protocol_violation}; return }` to the read-loop enqueue path.

### New Defect 8 (MEDIUM): Module-death-on-blocked-acquire observable changes
**File**: v2  lines 140-148.
**Issue**: v2's `handle_closed` maps:
- `ConnClose` (module connection close) → silent drop
- `None` (no teardown reason) → `backend_error`
For the shipped test `blocked_flow_control_acquire_wakes_when_module_tears_down` (forwarding.rs:3811-3877), the test calls `module.stop()` (line 3869), which is a module connection close. v2 sets teardown=ConnClose → silent drop. The test passes because it accepts either GOODBYE or `backend_error`, and the connection close path emits a route GOODBYE. But for a module that dies (process crash, supervisor kill) WITHOUT a route GOODBYE being emitted (the module is gone; no goodbye), the client sees neither a cancelled/closed terminal for the blocked corr nor an error. The pre-v2 observable was `backend_error`. v2 changes this to "no terminal, wait for connection close". This is a wire-observable change for the client and a regression in error-reporting latency.

### New Defect 9 (MEDIUM): `outstanding` "mirrors" `in_flight` but timing differs
**File**: v2  line 42 (comment "mirrors flow.in_flight; for drain-side assertions").
**Issue**: `outstanding` is incremented on `state=Delivered` and decremented on terminal removal (slot removed by module→client path or drain send-failure arm). `flow.in_flight` is incremented by `flow.acquire().await` and decremented by `flow.release()`. The two are NOT synchronized: `flow.in_flight` is incremented at acquire (before Delivered), `outstanding` is incremented at Delivered (after acquire); `outstanding` is decremented at slot removal, `flow.in_flight` is decremented at `flow.release()` (after slot removal). The drain's admission check `queue.len()+outstanding >= depth_cap` (v2 line 65) uses `outstanding`. If any code path uses `outstanding` to assert "matches flow.in_flight", the assertion can fail transiently. The v2 doc's "mirrors" comment invites such use without specifying the invariant.

### New Defect 10 (MEDIUM): No byte-based memory budget (v1 B12 not closed)
**File**: v2  Q3' line 314-315.
**Issue**: v1 finding #12 (3/8 raised it as a BLOCKER) flagged that frame-count caps are not memory bounds: max body is 64 MiB (`crates/subc-protocol/src/lib.rs:114-119`), so a 2048-deep queue with 64 MiB bodies is 128 GiB per route. v2's mechanism section (  uses frame-count admission only. Q3' asks about byte-based secondary cap but the doc itself says "Lean: keep max(4,2×window)" without committing to a byte cap. v2 does not close v1 B12; it merely flags it as an open question. The memory DoS surface remains.

### New Defect 11 (MEDIUM): Read loop's enqueue path uses `FrameSink` clone, but the dispatcher's `module_sink` is not specified to be the binding's
**File**: v2  line 33-35 (struct fields `flow: Arc<ChannelFlow>`, `module_sink: FrameSink`).
**Issue**: v2's `RouteDispatcher` has its own `flow` and `module_sink` fields. The doc says "the EXISTING per-route credit sem (unchanged: acquire/forget + CAS release)" — so the dispatcher's `flow` is the same `Arc<ChannelFlow>` as the binding's (cloned at bind time). But the doc doesn't say where the dispatcher's `module_sink` comes from. If it's a separate clone of the binding's `module_sink` (an mpsc sender), then closing one closes the other (good). But the v2 teardown step 2 says `flow.close() with teardown=<reason>` — it does NOT say `module_sink.close()`. If the module connection closes, the `module_sink` is dropped by the connection's teardown, and the drain's `module_sink.send().await` returns Err. That's fine. But the v2 doc doesn't address what happens if the `module_sink` is dropped while the dispatcher's `flow` is still open — the drain's send fails, the error arm fires (backend_error + release), but the route is still "bound" from the snapshot's perspective. The v2 mechanism assumes the binding→module_sink lifetime and the dispatcher→module_sink lifetime are synchronized, but doesn't specify how.

### New Defect 12 (LOW): AcquiredCredit RAII guard interacting with the outstanding count
**File**: v2  lines 91-101.
**Issue**: The rollback path increments `outstanding`? No — the rollback is on `state=Claimed{cancelled:true}`, not on `state=Delivered`, so `outstanding` is NOT incremented. The rollback decrements the credit but not outstanding. The `module_sink.send` Ok path doesn't touch outstanding. The module→client terminal path does `outstanding -= 1`. The `outstanding` count tracks "Delivered but not yet Settled" corrs. But the admission check `queue.len()+outstanding >= depth_cap` (v2 line 65) treats both queue and outstanding as "in flight". If a corr is in `outstanding` (Delivered, module processing) and a CANCEL arrives, the CANCEL is forwarded to the module (state=Delivered), the module emits a terminal, the terminal removes the slot, decrements outstanding, releases credit. The slot was in outstanding, and the admission check counted it. During the time the module is processing, the corr is in outstanding. If the depth cap is hit, new enqueues are rejected. This is the intended behavior. No defect here, but the interaction is subtle and the v2 doc doesn't explain it.

---

## Q1'–Q5' open-question rulings

| Question | Lean | Ruling | Rationale |
|---|---|---|---|
| Q1' merge-0 hard-gate vs blocking-admission interim | hard-gate | **RIGHT** | Hard-gating is cleaner; the doc honestly presents the interim fallback. The contradiction is process-level (deploy sequencing), not mechanism. |
| Q2' teardown bounded-join timeout (2s?) | 2s + abort | **RIGHT-BUT-UNSAFE** | The 2s matches the shipped `CLOSE_DRAIN_GRACE` (server.rs:28), so it's a defensible default. The "abort-after-timeout can drop an in-flight terminal (acceptable: connection is closing)" is correct for full connection close but not for partial teardowns (e.g., supervisor reload of a single module). The doc should narrow the scope. |
| Q3' depth caps: keep max(4,2×window)? Byte-based secondary cap? | keep max(4,2×window) | **RIGHT-BUT-UNSAFE** | The frame-count cap is specified; the byte-based cap is open. v1 B12 (256 GiB per connection memory DoS) is not closed. The mechanism admits on frame count only, which is the exact attack surface v1 flagged. |
| Q4' `slots` HashMap vs slab/index; corr-uniqueness enforcement | enforce at enqueue | **RIGHT-BUT-UNSAFE** | The lean is correct (enforce at enqueue), but the mechanism section (  does not implement the enforcement. `slots.insert(corr, ...)` is a silent overwrite. The doc acknowledges the gap but doesn't close it. |
| Q5' snapshot publish granularity: whole-table vs per-shard | whole-table | **RIGHT** | Whole-table publish is correct for read-mostly workloads (mutations rare vs lookups). The T9 perf evidence is the right gate. ✓ |

---

## Bottom-line verdict: **NO-GO**

The v2 mechanism is sound in spirit and correctly identifies the v1 gaps. The closure arguments for B1, B2, B3, B5, B6, B7, B8, B10 are convincing and source-cited. But the v2 doc is a **design spec, not an implementable spec** — it has at least two BLOCKER design gaps (New Defects 1 and 2: the module→client terminal path cannot find the route's `RouteInbox` because `RouteBinding` has no dispatcher reference, and the connection-close teardown cannot enumerate live dispatchers because `ForwardingInner` has no dispatcher map), plus four MAJOR race/leak/observable hazards (New Defects 3-6: two-lock-dance CANCEL window, implicit `commit()`, rollback slot leak, panic slot leak), plus wire-observable changes (New Defect 8) and a still-open memory DoS (New Defect 10).

**Required changes before re-gate** (all must be addressed; each is file:line-cited):

1. **BLOCKER**: Add `dispatcher: Weak<RouteDispatcher>` (or `Arc<RouteDispatcher>`) to `RouteBinding` (forwarding.rs:52-65). The module→client terminal path at `router.rs:281-309` uses this to lock the route's `RouteInbox` and do `slots.remove(corr)` for the R11 gate.
2. **BLOCKER**: Add `dispatchers: HashMap<ClientRouteKey, Arc<RouteDispatcher>>` (or equivalent) to `ForwardingInner` (forwarding.rs:236-256). The connection-close teardown (`cleanup_connection` at forwarding.rs:1168-1239 and `RouterConnection::drop` at router.rs:391-398) uses this to enumerate live dispatchers, set `admission=Closing`, close `flow` with `teardown`, cancel the token, and bounded-join each drain task before the epoch-fenced release.
3. **MAJOR**: Combine the drain task's `pop_front` and `state=Claimed` into a single lock acquisition (v2  lines 86-87). Or add an explicit "slot was removed by CANCEL between pop and claim — synthesize cancelled, continue" branch.
4. **MAJOR**: Make the `AcquiredCredit` guard's `commit()` explicit in the pseudocode (v2  line 98). Show `guard.commit()` after the Ok arm of `module_sink.send().await`, and `guard.disarm()` (or equivalent) on the rollback path before the explicit `flow.release()`.
5. **MAJOR**: Specify `slots.remove(corr)` explicitly on the rollback path (v2  line 94). The "drop" is ambiguous.
6. **MAJOR**: Specify panic guard behavior beyond "release the credit" (v2  line 111). Must also `slots.remove(corr)` and `outstanding -= 1` (or equivalent) to clean up the slot. Add a T-panic test to T10.
7. **MAJOR**: Revert the silent-drop-on-ConnClose mapping (v2  line 146). The shipped test expects a `backend_error` observable; v2 should preserve it. Map `ConnClose` → `backend_error` to match the shipped behavior, not silent drop.
8. **MEDIUM**: Add corr-uniqueness enforcement to the read-loop enqueue path (v2  line 66). Reject duplicate-corr enqueue as a protocol violation with a typed error frame, not a silent overwrite.
9. **MEDIUM**: Add a byte-based secondary cap (v2 Q3' line 314-315) to close the v1 B12 memory DoS. The frame-count cap is not a memory bound.
10. **MEDIUM**: Remove the "mirrors flow.in_flight" comment on `outstanding` (v2  line 42) or replace with the actual invariant: "delivered-not-settled count; briefly diverges from `flow.in_flight` by the slot-removal-to-release window".

**One single un-refuted concurrency or contract defect is a NO-GO.** I find at least six (New Defects 1-6), two of which are BLOCKERs that make the mechanism un-implementable. The v2 spec must be revised and re-gated.

**External consumers (broca/aft/alfonso-core) are not in this checkout**, as the v1 synthesis noted and v2 acknowledges (Q1'). The `route_backpressure` / `control_backpressure` codes' impact on those consumers cannot be verified here and must be checked in their own repos.