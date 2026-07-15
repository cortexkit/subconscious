# Adversarial Re-Gate: subc-core dispatch redesign v2

Verified against: v1 archive (synthesis.md), v2 doc (docs/subc-dispatch-redesign-v2.md), shipped source at master (forwarding.rs, router.rs, server.rs, control.rs, consumer.rs, client.ts, fake-aft-stub.rs). B1–B10 map to v1 synthesis findings #1–#10 in order (confirmed via docs/subc-dispatch-redesign.md:14-62).

## Part A — v1 Blocker Closure Verification

### B1 (SDK contract false) — **CLOSED** (mechanism correct; external-consumer gap flagged)
v2  acknowledges the v1 claim was false and specifies merge-0: a distinct data-plane retryable class `{route_backpressure, control_backpressure}` → retry-in-place (not reconnect, not evict). I verified the v1 defect at source: TS `classifyFailure` (client.ts:781-794) returns `outcome_unknown` once `handedToSocket=true` (line 793), and managed `call()` (client.ts:445-451) reconnects on `outcome_unknown`; Rust `consumer.rs:570-579` returns `CallError::Module(body)` for any non-`unknown_channel` Error terminal; the only retryable-code classifier is the closed route.open set (consumer.rs:3130-3135, client.ts:1252-1259). The proposed in-place-retry arm FITS the existing managed-call loop structure (TS client.ts:420-454 has a `continue`-shaped loop; Rust consumer.rs:561-586 likewise) — a new arm that `continue`s without `reconnectAfterDrop`/`invalidate_route` is structurally clean. **Caveat (carried forward):** broca/aft/alfonso-core are NOT in this checkout (v2  admits this); their contract impact is UNVERIFIABLE. The merge-0 mechanism is sound but its fleet-wide deployment to those consumers cannot be confirmed here.

### B2 (CANCEL limbo) — **CLOSED** (with a NEW residual race — see N1)
v2 s per-corr state machine (Queued→Claimed→Delivered) decided under the single `inbox` lock is the correct mechanism. Tracing the Claimed/acquire boundary: CANCEL arriving while the drain is blocked on `flow.acquire().await` locks inbox, sees `Claimed{cancelled:false}`, sets `cancelled=true`, unlocks; drain post-acquire re-locks, sees `cancelled:true` → rollback (release + synthesize cancelled). The Delivered/send boundary: CANCEL sees Delivered → forwards to module (module owns the terminal). No limbo: a corr is exactly one of Queued/Claimed/Delivered under the lock. The rollback-vs-module-terminal race is closed because the drain sets Delivered under lock BEFORE `module_sink.send`, so the module→client terminal path ( and the rollback are mutually exclusive on the `slots.remove` gate. **However**, the drain pseudocode pops and claims in TWO separate lock acquisitions ( lines 86-87), creating a window where CANCEL can delete the slot between them — see N1.

### B3 (queue primitive) — **CLOSED** (with a residual O(1) gap — see N2)
v2  names the primitive explicitly: `Mutex<RouteInbox>` with `VecDeque<u64>` + `HashMap<u64,Slot>`. This resolves the mpsc/scan incompatibility (v1 #3). Every critical section is await-free. **Residual:** the Queued-CANCEL "remove from queue" is not O(1) on a `VecDeque` — see N2.

### B4 (drain error arms) — **CLOSED**
v2  +  specify all three arms: (a) send-failure → `outstanding-=1; remove slot; flow.release(); synthesize backend_error` (mirrors router.rs:491-496); (b) `ChannelFlowClosed` → disambiguated via `teardown:TeardownKind` (Reloading→module_reloading, Goodbye→silent, ConnClose→silent, None→backend_error); (c) RAII `AcquiredCredit` guard releasing on drop unless `commit()`ed (handles panic between acquire and send). I verified the shipped test `blocked_flow_control_acquire_wakes_when_module_tears_down` (forwarding.rs:3811-3878) accepts EITHER a Goodbye OR `backend_error` (lines 3872-3876); v2's None→backend_error arm preserves it. The `permit.forget()` non-RAII problem (forwarding.rs:1692-1699) is addressed by the AcquiredCredit scope guard. CLOSED.

### B5 (insert ordering) — **CLOSED**
v2  explicitly records `state=Delivered; outstanding+=1` under the inbox lock BEFORE `module_sink.send().await`. Happens-before: the module cannot see the frame until after `send` returns, and `send` happens after the Delivered-commit unlock. The module→client terminal path ( does `slots.remove(corr)` under lock and releases only if was-Delivered. A fast module terminal therefore always finds the corr Delivered. CLOSED.

### B6 (O(queue) CANCEL scan DoS) — **CLOSED** (with N2 residual)
v2 s `HashMap<u64,Slot>` gives O(1) CANCEL lookup ( read-loop CANCEL does `slots.get_mut(corr)`). No read-loop linear scan. The DoS vector from v1 #6 is eliminated. **Residual:** the Queued-case queue removal is still potentially O(n) — see N2.

### B7 (bind-ACK barrier) — **CLOSED** (genuine correction)
v2  explicitly reverses v1's wrong Q3 lean: "v1's whole channel-0 FIFO was WRONG." Offload is scoped to CLIENT-side channel-0 only; MODULE connections stay inline. I verified the shipped barrier: `commit_route_locked` (forwarding.rs:1472-1536) publishes maps (1524-1529) AND sends `route_open_frame` via `OwnedPermit` (1536) inline under the write lock, before the module's next frame is read. The shipped test `accepted_route_publishes_route_open_before_immediate_reverse_request` (router.rs:1078-1102) asserts route.open (corr 700, ch 0) precedes the reverse request (corr 800, client ch) — preserved because module frames aren't reordered. CLOSED.

### B8 (false invariants) — **CLOSED**
v2  rewrites I3/I4/I7 as intentional deltas: I3 → "release call site gains the exactly-once gate" (not "untouched"); I7 → "duplicate/late-terminal credit accounting intentionally fixed; closed recheck added"; I4 → "GOODBYE flushes queued client→module frames (raw-wire semantic change; SDKs settle locally first)." I verified the R11 framing correction: shipped `release()` (forwarding.rs:1702-1731) has a CAS `in_flight!=0` guard that ignores over-release at 0, so R11 is specifically the concurrent-duplicate case (two terminals while `in_flight>0`). v2 s `slots.remove` gate correctly targets this. The invariants are now honest. CLOSED.

### B9 (GOODBYE flush non-atomic + teardown hang) — **CLOSED** (lock hierarchy undocumented — see N3)
v2 s 3-phase Open/Closing/Closed gate: admission=Closing (step 1) happens-before snapshot removal (step 6), so a stale-snapshot reader's push hits `admission!=Open` → `route_closing` (the admission check, not the flush, is the barrier — correct). The `cancel_token` select on `module_sink.send`/`flow.acquire` + bounded-join + abort (step 4) prevents a blocked drain from hanging connection close. I verified the hang vector: shipped `drain_writer` (server.rs:425-442) awaits `rx.recv()`; a drain task holding a `FrameSink` clone (module egress tx) blocks `module_sink.send` only while the module connection is backpressured (not closing) — v2's cancel-token interrupts this. JoinHandle ownership by the teardown path (not the binding) avoids the strong cycle. CLOSED.

### B10 (merge-1 standalone-neutral) — **CLOSED**
v2  publish-under-write-lock + `closed:AtomicBool` on the `RouteBinding` set true BEFORE the removing snapshot is published. The key insight I verified: `closed` lives on the `Arc<RouteBinding>` object (shared), NOT on the snapshot — so even a reader holding a pre-release snapshot sees `closed=true` after the release sets it (same atomic). A reader loading a post-release snapshot sees no binding → `unknown_channel`. The residual pre-release-snapshot-load + post-release-forward race (reader sees `closed=false`, then release completes, forward hits `flow.close()`d sem → `ChannelFlowClosed`) exists TODAY (shipped `lookup_data_route` clones the Arc and drops the read lock before routing — forwarding.rs:840-890), so v2 introduces no NEW observable. CLOSED.

---

## Part B — NEW Defects Introduced by v2 Mechanisms

### N1: Drain pop→claim two-lock race (route-level hang)
- **Severity**: MAJOR (borderline BLOCKER)
- **Location**: v2  drain task pseudocode, lines 86-87
- **Confidence**: high
- **Issue**: The drain pops `corr` from the queue in lock #1, then sets `slots[corr].state = Claimed` in a SEPARATE lock #2. Between these two locks, a CANCEL arriving on the read loop locks inbox, sees the slot still `Queued` (drain hasn't claimed yet), removes it from `slots` and synthesizes `cancelled`. The drain's lock #2 then indexes `slots[corr]` — which no longer exists → panic (HashMap miss) or logic fault. If the drain task panics, no one pops the remaining queued corrs on that route → route-level permanent stall.
- **Evidence**: v2 86 `corr = { lock inbox; pop_front queue or None }` then 87 `{ lock inbox; slots[corr].state = Claimed{cancelled:false} }` — two distinct lock scopes. CANCEL's Queued arm (74) removes from `slots`. Shipped module cancel is a no-op for unknown corr (fake-aft-stub.rs:384-388), so the race is between the daemon's own read loop and drain, not the module.
- **Suggested Fix**: Combine pop+claim in ONE lock: `{ lock inbox; if let Some(corr)=queue.pop_front() { slots[corr].state=Claimed{cancelled:false}; Some(corr) } else { None } }`. Or re-check `slots.contains_key(corr)` after pop and skip if gone.

### N2: VecDeque Queued-CANCEL removal is not O(1)
- **Severity**: MINOR/MAJOR (spec completeness)
- **Location**: v2 74 ("remove from queue+slots"), 41 (`queue: VecDeque<u64>`)
- **Confidence**: high
- **Issue**: Removing an arbitrary corr from a `VecDeque<u64>` by value is O(n) (linear scan) or breaks FIFO (swap-remove). The design claims every op is O(1) (49-50). The Queued-CANCEL path says "remove from queue+slots" — the `slots` removal is O(1) (HashMap), but the `queue` removal is not, unless mark-and-skip is used.
- **Evidence**: `VecDeque` has no O(1) arbitrary-position delete. 49 "every critical section is O(1) (push, pop-front, get/remove-by-corr, flag set)."
- **Suggested Fix**: For Queued CANCEL, mark the slot `Cancelled` and LEAVE the corr in the queue; the drain skips Cancelled slots on pop (O(1)). Specify this explicitly. (This also closes N1's race partially, since the slot persists.)

### N3: Lock hierarchy (inbox mutex vs global forwarding write lock) undocumented
- **Severity**: MINOR
- **Location**: v2   (teardown), 
- **Confidence**: medium
- **Issue**: v2 never states the ordering between the route-local `inbox` Mutex and the global forwarding `RwLock`. The teardown path acquires inbox ( step 1) then the global write lock ( step 6) → inbox-before-global. I traced the read-loop push path (lookup_data_route read lock released at forwarding.rs:846-890, then inbox lock) and the module→client terminal path (router.rs:221-224 lookup released, then  inbox lock) — both release the global read lock before the inbox, so no inversion exists today. But the design must document the hierarchy and prove no path holds the global WRITE lock while locking an inbox (e.g., if `commit_route_locked` initializes a dispatcher inbox).
- **Evidence**: forwarding.rs:620 (release under write lock), 846 (lookup under read lock), v2  step 1+6.
- **Suggested Fix**: Add an explicit "lock hierarchy: inbox → (release) → global write lock; never hold global write lock while locking inbox" statement and audit all call sites.

### N4: corr-uniqueness check missing from  push pseudocode
- **Severity**: MINOR
- **Location**: v2 66 (`slots.insert(corr, Slot{frame, Queued})`),  Q4'
- **Confidence**: high
- **Issue**: Q4' leans "enforce at enqueue (reject duplicate-corr as protocol violation)" — correct, since `HashMap::insert` overwrites and would leak the overwritten corr's credit. But the  read-loop push pseudocode does `slots.insert(corr, ...)` with no duplicate check. If a client reuses an in-flight corr, the slot is silently overwritten and the original corr's credit leaks.
- **Evidence**: 66 vs  Q4' lean. Shipped daemon admits on route/flow only (router.rs:452-497), no corr-uniqueness enforcement.
- **Suggested Fix**: Add `if slots.contains_key(corr): unlock; synthesize Error{protocol_violation}; close connection; return` before the insert in .

### N5: Synthetic-error egress reliability on the read loop unspecified
- **Severity**: MAJOR
- **Location**: v2 65,68,74 (synthesize Error{route_backpressure/route_closing/cancelled} "to client"),  I6
- **Confidence**: high
- **Issue**: The read loop cannot await `egress.send` (I6 cancel-safety), but v2's synthesized errors (route_backpressure, route_closing, cancelled) must reach the client. v2 says "synthesize Error{...} to client; return" without specifying the egress mechanism. The shipped read loop uses `ctx.egress.send(error_frame).await` (server.rs:399) — an await. A `try_send` can fail when the client egress buffer (CONNECTION_EGRESS_BUFFER) is full, causing the promised terminal to VANISH. This is the residual v1 #13 defect, unaddressed by v2.
- **Evidence**: server.rs:243 (CONNECTION_EGRESS_BUFFER), 399 (awaited egress); v2  synthesized errors; v2  I6 only addresses the dispatcher push, not the error egress.
- **Suggested Fix**: Specify a reserved bounded egress lane (or a connection response actor) for synthetic errors; on exhaustion, epoch-fenced connection close (client treats close as outcome-unknown). Distinguish queue `Closed` from `Full`.

### N6: teardown:TeardownKind must be set at every flow.close() call site
- **Severity**: MINOR
- **Location**: v2   shipped forwarding.rs:1424, 1455, 1543
- **Confidence**: medium
- **Issue**: v2 s disambiguation reads `self.teardown` under the inbox lock in `handle_closed`. Every existing `flow.close()` call site (release_client_route_locked:1424, release_module_route_locked:1455, commit_route_locked abandoned path:1543) must set `teardown` first. If any site forgets, the drain reads `None` → `backend_error` (defensive default, safe but may mislabel a GOODBYE as backend_error). This is implementation discipline, not a design hole, but the design should enumerate the call sites.
- **Suggested Fix**: Audit and list all `flow.close()` call sites with their required TeardownKind mapping.

---

## Part C — Q1'–Q5' Lean Rulings

| Q | Lean | Ruling | Rationale |
|---|------|--------|-----------|
| Q1' hard-gate vs interim | hard-gate | **RIGHT-BUT-UNSAFE** | Hard-gate is cleaner, but broca/aft/alfonso-core are NOT in this checkout (v2  admits it) — you cannot verify merge-0 is deployed to them. Hard-gating on an unverifiable external deploy is unsafe. The blocking-admission interim (drain-side wait, off read loop) preserves semantics with zero SDK changes and no HOL. Recommend interim until external verification completes. |
| Q2' 2s bounded-join + abort drops in-flight terminal | acceptable | **RIGHT** | On abort, an in-flight Delivered corr's credit leaks, but the flow is being closed (route lifetime bounds the leak). A late terminal arriving after teardown hits `closed=true` → `unknown_channel` and does not release (slots already removed in step 5). Acceptable for connection close. |
| Q3' depth caps + byte secondary | byte secondary | **RIGHT** (nuance) | Correctly addresses v1 #12 (256 GiB memory DoS). But the byte cap should be PRIMARY, charged pre-admission (before body allocation at frame_io.rs:74-84), not secondary. Frame-count may remain as a secondary bound. |
| Q4' enforce corr-uniqueness at enqueue | enforce | **RIGHT-BUT-UNSAFE** | The lean is correct (prevents slot-overwrite credit leak), but the  mechanism pseudocode does NOT implement it (N4). The lean must be reflected in the push pseudocode. |
| Q5' whole-table Arc rebuild | whole-table | **RIGHT** | Mutations are rare vs hot lookups; whole-table is simpler and correct. Measure in T9 before optimizing. |

---

## Bottom-Line Verdict: **GO-WITH-CHANGES**

v2 is a substantial, architecturally-sound closure of all 10 v1 blockers. The single serialization point (route-local `Mutex<RouteInbox>`), the per-corr state machine, the insert-before-send ordering, the RAII credit guard, the teardown:TeardownKind disambiguation, the client-side-only control offload, and the publish-under-lock + `closed`-flag snapshot are all correct mechanisms that I traced to shipped source. B1–B10 are CLOSED in principle. This is a "specify the remaining details" outcome, not a "redesign the mechanism" outcome (unlike v1).

**Required spec changes before implementation (must fix):**
1. **N1** (86-87): Combine drain pop+claim into ONE inbox lock acquisition, or re-check slot existence after pop. As written, the two-lock window lets CANCEL delete the slot and the drain faults → route-level hang. One-line spec fix.
2. **N5** (65,68,74 +  I6): Specify the synthetic-error egress mechanism (reserved bounded lane or response actor; on exhaustion, epoch-fenced close). The read loop cannot `await` egress and `try_send` can silently drop the promised terminal under client backpressure.

**Required spec completeness (should fix):**
3. **N2** (74): Specify mark-and-skip (leave Cancelled Queued slots in the VecDeque, drain skips on pop) instead of O(n) queue removal, to honor the O(1) claim.
4. **N4** (66): Add the corr-uniqueness check to the push pseudocode (reject duplicate-corr as protocol violation), reflecting the Q4' lean.
5. **N3** (: Document the lock hierarchy (inbox → release → global write lock; never hold global write lock while locking inbox) and audit call sites.
6. **N6** (: Enumerate all `flow.close()` call sites (forwarding.rs:1424, 1455, 1543) with their required TeardownKind mapping.

**Carried-forward caveat (not a blocker):** broca/aft/alfonso-core contract impact for B1/merge-0 is UNVERIFIABLE in this checkout. Recommend Q1' interim (blocking-admission) until external verification, rather than hard-gating on an unconfirmed external deploy.

No single un-refuted concurrency defect at v1's severity remains: N1 is a one-line spec fix (combine two locks), not an architectural hole; N5 is an egress-policy specification gap, not a structural flaw. The architecture is endorsed; the remaining work is spec precision. If strict gate enforcement is preferred, N1 alone is severe enough (route-level hang if implemented verbatim) to warrant NO-GO until the spec is corrected — but the fix is mechanical, so GO-WITH-CHANGES is the proportionate call.