# Adversarial Re-Gate — subc-core dispatch redesign v2

Verdict up front: **NO-GO**, but materially closer than v1. Six of ten v1 blockers are genuinely closed; the drain/cancel/teardown mechanism still carries **two un-refuted concurrency defects** (both in the cancel path — the exact class the redesign exists to kill) plus several MAJOR spec gaps. Per the gate rule (a single un-refuted concurrency defect = NO-GO), v2 does not pass as written.

---

## Per-blocker rulings (B1–B10)

**B1 — route_backpressure→NotSent false in SDKs → CLOSED (design/contract).**
v2  drops the false "zero SDK changes" claim and adds a hard-gated prerequisite merge-0. A distinct retryable class is genuinely required: a data-plane terminal `Error` settles as a plain `SubcError`→`terminalCallError` (client.ts:1060, 423) and `CallError::Module(body)` (consumer.rs:579) — neither retries; the only retryable classifier is the channel-0 route.open closed set (consumer.rs:3130-3135, client.ts:1242-1261). In-place retry fits both managed loops (TS `continue` at client.ts:432/443; Rust `continue` at consumer.rs:577/583). **Caveats (preserve):** (a) merge-0 is unimplemented; TS needs the route_backpressure `SubcError` threaded across the `SubcError`→`SubcCallError` boundary (the `!(err instanceof SubcCallError)` throw at client.ts:423 currently makes it terminal); (b) broca/aft/alfonso-core are **not in this checkout** — the "merge-0 live fleet-wide" gate is **UNVERIFIABLE** here and must be checked in their repos.

**B2 — CANCEL queued→delivered limbo → NOT-CLOSED.** Two residual holes:
- *pop→mark-Claimed is non-atomic.*  drain uses **two** separate critical sections: `corr = { lock; pop_front }` then `{ lock; slots[corr].state=Claimed }`. In the gap the slot is still `Queued` but off the deque. A CANCEL arriving here hits the `Some(Queued)` arm, removes the slot + synthesizes `cancelled`; the drain then `slots[corr]` is **gone**, and the post-acquire block (`Claimed{cancelled:true}`/`else→Delivered`) has **no `None` arm** — if `None` falls through to `else`, the frame is sent to the module → double terminal + uncancelled execution.
- *Delivered-set-before-send lets CANCEL overtake the Request.*  sets `state=Delivered` **under lock before** `module_sink.send().await` (correct for B5). But a CANCEL in that window hits `Some(Delivered) => forward CANCEL to module_sink`. Drain and read-loop then **race on the same `module_sink` mpsc with no ordering**; if the cancel is enqueued first, the module runs `handle_cancel` on an unknown corr (no-op — subc-client-rs/lib.rs:995) then processes the Request uncancelled. This is the **exact LOOP-R5 cancel-loss** transposed into the Delivered-before-send window (router.rs:491 send; module inserts its in-flight entry only after receiving the Request).

**B3 — queue data structure / mpsc-scan race → CLOSED.**  names `VecDeque<u64>` + `HashMap<u64,Slot>` behind one `Mutex<RouteInbox>`; both actors mutate under the same mutex. No mpsc receiver-side scan, no torn reads. The data-race is eliminated.

**B4 — drain error arms → PARTIALLY-CLOSED (NOT-CLOSED).** The arms now exist (send-fail release mirrors router.rs:491-496; `ChannelFlowClosed` disambiguation; RAII panic guard) — a real improvement. Two new holes:
- *teardown field crosses lock domains.*  reads `kind = self.teardown` under the **inbox** lock, but the mutator that closes the flow (`release_client_route_locked`, forwarding.rs:1409-1438; `release_module_route_locked`, 1440-1470) runs under the **forwarding write lock** and calls `flow.close()` (1424/1455). The drain wakes from `acquire()==Err(closed)` the instant `sem.close()` runs (forwarding.rs:1737-1739) and can read a **stale `teardown` (None)** before the mutator sets it → spurious `backend_error` on a graceful reload/Goodbye (wrong: a reload should be `module_reloading` so the client retries).
- *module-death shoehorned into the `None` "defensive" arm.*  routes the primary failure (module endpoint gone) through the `None→backend_error` catch-all, while a module connection-close would naturally be `ConnClose→silent`. The field cannot distinguish "module tore down my client-route" (→backend_error) from "client tore down" (→silent) unless the *writer* encodes who — never specified. The cited test (forwarding.rs:3811-3878) is **lenient** (accepts Goodbye, backend_error, *or nothing*), so it does not actually pin this.

**B5 — outstanding.insert-after-send race → CLOSED.**  records `Delivered` + `outstanding+=1` under the inbox lock **before** `module_sink.send`. Happens-before: Delivered-set → send → module processes → terminal →  terminal path locks inbox, finds `Delivered`, releases. A fast terminal always observes `Delivered`; no leak. (This is precisely what *creates* the B2 Delivered-before-send hole above — B5 and B2 pull in opposite directions on where the marker sits.)

**B6 — O(queue) CANCEL scan DoS → NOT-CLOSED (partial).** `slots` gives O(1) *state lookup*, but the `Some(Queued) => remove from queue+slots` arm must remove a specific corr from a `VecDeque` — `VecDeque::remove` is **O(n)** (find index + shift), executed on the **read loop under the inbox lock**. An attacker fills to `depth_cap` (~2×window; window=1024 for StatelessParallel, forwarding.rs:25) and back-cancels, reproducing ~O(depth²) work on the latency-critical path — the same HOL vector v1 flagged. The v1-recommended tombstone/index (mark slot cancelled, leave in deque, drain skips on pop) is **not** in the doc;  literally says "remove from queue."

**B7 — whole channel-0 FIFO breaks bind-ACK barrier → CLOSED.**  correctly narrows offload to **client-side** channel-0; module connections stay inline, preserving the bind-commit-before-next-frame barrier by construction. The shipped test `accepted_route_publishes_route_open_before_immediate_reverse_request` (router.rs:1078-1102) stays green. *(Minor residual: a raw-wire client that pipelines bind+data without awaiting the ack could see its data race ahead of the offloaded route.open — SDKs single-flight bind so are unaffected.)*

**B8 — I3/I4/I7 false invariants → CLOSED.**  reword all three into honest deltas and correctly re-scope R11 to the **concurrent-duplicate** case, matching the shipped CAS `in_flight!=0` guard (forwarding.rs:1702-1731) — two concurrent terminals with `in_flight>0` both pass `!=0` and double-decrement. s slot-gone gate closes that cleanly (duplicate finds slot removed → `release=false`) and stacks *safely* with the CAS (the CAS is the last-ditch net; the gate is the real fix). No conflict.

**B9 — GOODBYE flush vs enqueue / teardown hang → PARTIALLY-CLOSED.** The try_push-after-flush hole **is** closed: `admission=Closing` set under the inbox lock (the only enqueue path,  checks it) is a genuine happens-before barrier vs binding removal. **But** the async 3-phase teardown (cancel_token select + 2s bounded-join + `abort()`) is **not reconciled with the shipped synchronous teardown trigger**: `RouterConnection::drop` (router.rs:391-397) is a sync `Drop` calling `cleanup_connection` (sync, control.rs:422-437) — a sync Drop cannot `.await` a bounded-join. The drain-task join/abort must be relocated into the async connection scope, unspecified. Also the `cancel_token`-vs-`flow.close()`-wake race (which branch fires handle_closed) is underspecified.

**B10 — merge-1 snapshot not neutral → CLOSED.**  publishes the snapshot **inside** the write-lock critical section (no publish-after-unlock window) and adds `closed: AtomicBool` set under the write lock before the removing snapshot, checked on both data paths (router.rs:281-309 and dispatcher push) → stale-Bound reads observe `unknown_channel`, restoring today's post-release observable (not `backend_error`). Residual read-`closed=false`-then-release TOCTOU is **no wider than today's** Arc-clone-then-drop-read-lock window (forwarding.rs:840-890), which v2 acknowledges.

---

## NEW defects introduced by v2 mechanisms

**N1 (BLOCKER) — Delivered-before-send cancel/request reorder on `module_sink`.** See B2. Concrete interleaving: drain sets `Delivered`+unlocks (95) → preempted → CANCEL read loop matches `Some(Delivered)`, forwards CANCEL to `module_sink` → module `handle_cancel` no-ops unknown corr (lib.rs:995) → drain resumes, sends Request → module runs uncancelled. Reintroduces LOOP-R5 under the saturation the design targets.

**N2 (BLOCKER/MAJOR) — pop→mark-Claimed non-atomic gap.** See B2. Two separate `{lock}` blocks; CANCEL in the gap deletes the slot; post-acquire block has no `None`/missing-slot arm → double terminal or credit mishandling. Fix is trivial (pop AND mark Claimed in one critical section; add explicit `None` arm) but as-written it is a real hole.

**N3 (MAJOR) — `teardown` field lock-domain crossing → stale read.** Written under the forwarding write lock (release_*_locked, forwarding.rs:1409-1470), read under the inbox lock (. No stated ordering; drain can wake on `sem.close()` and read stale `None` → wrong synthetic terminal (backend_error instead of module_reloading, defeating client retry).

**N4 (MAJOR) — `AcquiredCredit` RAII + manual release double-release.** s send-failure arm does an explicit `flow.release()` (line 100) *and* the guard "calls `flow.release()` on drop unless commit()'ed." The doc specifies `commit()` only at the Delivered transition, not on the manual-release path → the guard drops still-armed → **double release**. The shipped CAS guard (forwarding.rs:1716) would partially mask it while corrupting the window count. Internally inconsistent about who owns the release.

**N5 (MAJOR) — lock hierarchy inbox-mutex vs forwarding-write-lock unspecified.** Teardown nests `forwarding-write ⊐ inbox` (to set admission/teardown); data path takes `forwarding-read` then releases then `inbox`. No inversion is *proven* absent, and the doc never states the hierarchy — a latent deadlock surface that must be made normative and checked at every site.

**N6 (MAJOR) — duplicate-corr overwrite leak not in normative .**  push does bare `slots.insert(corr, …)`; a reused in-flight corr overwrites the prior slot, orphaning its `outstanding`/credit. The fix is only *leaned* in Q4', not integrated into the normative read-loop pseudocode. As-specified, merge-2 leaks on duplicate corr (wire allows the daemon does not enforce non-reuse).

**N7 (MAJOR) — frame-count depth cap is not a byte bound.** The 256 GiB/conn memory-DoS (v1 #12) is only *leaned* in Q3', not normative. Bodies are owned `Vec<u8>` admitted before the cap; a byte budget must be normative, not an open question.

**N8 (MINOR) — `None`-arm forwards unknown CANCELs to the module.**  `Some(Delivered)|None => forward CANCEL to module_sink` sprays CANCELs for never-seen corrs to the module (no-op at lib.rs:995 but unbounded traffic) — the unknown-corr surface the daemon-synthesized-cancelled design meant to avoid.

---

## Q1'–Q5' rulings

- **Q1' (hard-gate merge-0 vs blocking-admission interim)** — **RIGHT**, with the caveat that "merge-0 live fleet-wide in broca/aft/alfonso-core" is **UNVERIFIABLE in this checkout**. The blocking-admission interim (backpressure-by-wait on the drain side, off the read loop) is genuinely HOL-free and a sound fallback.
- **Q2' (2s bounded-join; abort may drop a terminal)** — **RIGHT** for client-close; slightly unsafe for module-teardown where the client is still waiting (must ensure the synthetic backend_error wins the abort race).
- **Q3' (byte-based secondary cap)** — **RIGHT but must be normative**, not an open question (see N7).
- **Q4' (enforce corr-uniqueness at enqueue)** — **RIGHT but not integrated** into s normative push (see N6).
- **Q5' (whole-table Arc vs shard, measure in T9)** — **RIGHT, conditional** on churn benchmarks; read-mostly justifies whole-table.

---

## Bottom line: **NO-GO**

Closed and proven: **B1 (design), B3, B5, B7, B8, B10** — real, traced closures, not paper. The architecture is sound and the doc is now honest about its deltas.

Blockers remaining (each cited):
1. **N1** — Delivered-before-send cancel reorder on `module_sink` (95-98; router.rs:491; lib.rs:995). Move cancel-forwarding of a `Delivered`/pre-send corr onto the drain *after* its send completes (flag + re-check), or introduce a `Delivering` sub-state that buffers the cancel until the request is on the sink.
2. **N2** — merge pop_front + `state=Claimed` into one critical section and add an explicit missing-slot (`None`) arm in the post-acquire block (86-95).
3. **B6/* — replace `remove from queue` with an O(1) tombstone (mark slot Cancelled, drain skips on pop); prohibit O(n) `VecDeque::remove` under the read-loop lock ( 74).
4. **N3+N5+B9** — specify the inbox↔forwarding-write lock hierarchy; make `teardown` set-before-`flow.close()` under a consistent lock (atomic or inbox-guarded write); reconcile the async 3-phase teardown with the shipped sync `RouterConnection::drop` (router.rs:391-397) by owning drain join/abort in the async connection scope ( .
5. **N4** — make `AcquiredCredit` the *single* release owner: remove the manual `flow.release()` in the send-fail arm or disarm the guard there (100, 109).
6. **N6** — hoist Q4' corr-uniqueness enforcement into normative  push.
7. **N7** — make byte-based memory budgets normative (Q3'→ spec).

This is a "re-spec the drain/cancel/teardown critical sections and re-gate" outcome, not "scrap it." The tension between B5 (Delivered before send) and B2 (cancel ordering) is the load-bearing unresolved item and must be designed explicitly before merge-2.

*Caveats/limitations: merge-0 SDK code and external consumers (broca/aft/alfonso-core) are not in this checkout — their contract impact is asserted, not verified. Confidence: high on N1/N2/N3/N4/B6 (traced to shipped source and pseudocode); medium on N5/B9 lifecycle (depends on unstated plumbing that could be made correct).*