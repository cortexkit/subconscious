# Adversarial Re-Gate Verdict — subc-core dispatch redesign v2

**Design under review:** `docs/subc-dispatch-redesign-v2.md` (committed 72891b31)
**Gated against:** shipped subc-core daemon source at master
**Council:** 8 independent models (Opus 4.8, GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh, XAI Composer 2.5, Ollama Minimax M3, Ollama GLM 5.2, Gemini Flash 3.5)
**Intent:** AUDIT (adversarial re-gate; verify each v1 blocker closure + hunt new defects)
**Prior gate:** v1 was unanimous 8/8 NO-GO with 10 blockers B1–B10.

---

## VERDICT: **NO-GO** (6 NO-GO / 2 GO-WITH-CHANGES)

Six members returned NO-GO; two (GLM 5.2, Gemini Flash) returned GO-WITH-CHANGES — and **both of those two explicitly state that N1 alone (the pop→claim two-lock CANCEL race) is severe enough to justify NO-GO under strict gate enforcement**, calling it a "route-level hang if implemented verbatim." Under the stated gate rule — *a single un-refuted concurrency or contract defect is NO-GO* — v2 does not pass.

**This is materially different from v1.** The council is unanimous that v2's **architecture is sound and its mechanisms are correct in spirit**: the single serialization point (`Mutex<RouteInbox>`), the per-corr state machine, insert-before-send ordering, the R11 slot-gate, client-side-only control offload, and publish-under-lock + `closed`-flag are all *the right mechanisms*, traced to shipped source. v1 was "redesign the mechanism." v2 is "the load-bearing critical sections are under-specified and re-open two of the exact races the design exists to kill, plus the dispatcher↔binding wiring the mechanism requires does not exist in the shipped types." That is a **re-spec-the-critical-sections-and-re-gate** outcome, not scrap-it.

**Genuinely and provably CLOSED (near-unanimous):** B3, B5, B6, B7, B8 — real, traced closures. B10 closed by 6/8. The doc is now honest about its I3/I4/I7 deltas.

**The load-bearing unresolved tension:** B5 (record Delivered *before* send) and B2 (CANCEL ordering) pull in opposite directions on where the Delivered marker sits. Marking Delivered-before-send (correct for B5) opens a window where a CANCEL matching `Some(Delivered)` is forwarded to the module on a *different actor* and can overtake the not-yet-sent Request — re-introducing LOOP-R5 cancel-loss. This must be designed explicitly.

---

## Per-Blocker Rulings (B1–B10)

Vote tallies aggregate the 8 members. "Conditional" closures are counted toward CLOSED only where the member traced the mechanism.

| Blocker | Council ruling | Vote breakdown |
|---|---|---|
| **B1** SDK backpressure contract | **CLOSED (design) — but merge-0 unimplemented + external consumers unverifiable; NEWLY-BROKEN on provenance** | CLOSED-design: 5 (Opus, GLM, Gemini, XAI-caveat, GPT5.5-caveat); NOT-CLOSED/NEWLY-BROKEN: 3 (GPT5.6PRO, TerraPRO, Minimax) |
| **B2** CANCEL limbo | **NOT-CLOSED** — pop→claim non-atomic + Delivered-before-send cancel reorder | NOT-CLOSED: 6; CLOSED-with-new-race: 2 (GLM, Gemini — both then file N1 as the residual) |
| **B3** queue primitive / mpsc-scan race | **CLOSED** | Unanimous 8/8 CLOSED (narrowly) |
| **B4** drain error arms / RAII / TeardownKind | **NOT-CLOSED** — TeardownKind not wired to `flow.close()` sites; RAII double-release; abort leaves Delivered credit | NOT-CLOSED/PARTIAL: 6; CLOSED: 2 (GLM, Gemini) |
| **B5** outstanding/Delivered before send | **CLOSED (conditional on B2 + corr-uniqueness)** | Unanimous 8/8 CLOSED (conditional) |
| **B6** O(queue) CANCEL DoS | **NOT-CLOSED** — `VecDeque` arbitrary remove is O(n); mark-and-skip not specified | NOT-CLOSED: 5; CLOSED-with-residual: 3 (Opus counts partial, GLM/Gemini file N2) |
| **B7** bind-ACK barrier | **CLOSED** | Unanimous 8/8 CLOSED |
| **B8** false invariants I3/I4/I7 + R11 framing | **CLOSED** | Unanimous 8/8 CLOSED |
| **B9** GOODBYE teardown atomicity / hang | **NOT-CLOSED** — async teardown not reconciled with shipped sync Drop; `FrameSink`-clone hang class; endpoint-drain quiescence lost | NOT-CLOSED/PARTIAL: 6; CLOSED: 2 (GLM, Gemini) |
| **B10** merge-1 snapshot standalone-neutral | **SPLIT — NOT-CLOSED as written** (merge-1 lands before the dispatcher; current `handle_bound` has no `closed` recheck) | NOT-CLOSED: 4 (GPT5.6PRO, TerraPRO, GPT5.5, XAI-partial via Minimax); CLOSED: 4 (Opus, Minimax-design, GLM, Gemini) |

**Note on B6:** several members who marked it CLOSED (O(1) *lookup* via HashMap) simultaneously filed a NEW finding that the `Some(Queued) => remove from queue` arm is O(n) on `VecDeque`. The council consensus is that the DoS is *narrowed* (lookup is O(1)) but *not eliminated* (queue removal is O(n)) until mark-and-skip tombstoning is specified.

---

## NEW Defects Introduced by v2 (grouped by confidence)

### UNANIMOUS / NEAR-UNANIMOUS BLOCKERS

#### #N1: Drain `pop_front` → `Claimed` is a two-lock dance — CANCEL deletes the slot in the gap → panic / route-level hang
- **Severity**: Critical (BLOCKER)
- **Confidence**: Unanimous (8/8)
- **Members Reported**: all eight
- **Issue**: The drain pops a corr under lock #1 (`corr = { lock; pop_front }`), then marks `Claimed` under a *separate* lock #2 (`{ lock; slots[corr].state = Claimed }`). Between them the slot is `{exists, off-queue, state=Queued}`. A CANCEL arriving here matches `Some(Queued)`, removes the slot + synthesizes `cancelled`; the drain's lock #2 then indexes a missing key → panic (HashMap miss) or silent no-op. A panicked drain stops popping → **route-level permanent stall**. The post-acquire block also has no `None`/missing-slot arm.
- **Evidence**: v2:86-87 (two `{ lock inbox }` scopes); v2:74 (Queued CANCEL arm removes slot); module cancel is a no-op for unknown corr (`subc-client-rs/src/lib.rs:988-998`, `fake-aft-stub.rs:384-388`).
- **Impact**: Reopens B2 at the pop boundary — a v1-B2-class race the v2 claims to have closed. Route hangs or double-terminals.
- **Fix Direction**: Combine `pop_front` + `state=Claimed` into ONE critical section; add an explicit missing-slot arm in the post-acquire block. (One-line spec fix, but a real hole as written.)

#### #N2: Delivered-before-send lets CANCEL overtake the Request on `module_sink` (LOOP-R5 transposed)
- **Severity**: Critical (BLOCKER)
- **Confidence**: Majority-strong (6/8 explicit; the tension is acknowledged by all)
- **Members Reported**: Opus, GPT5.6PRO, TerraPRO, GPT5.5, XAI, Minimax (GLM/Gemini treat B2 closed but this is the residual they under-weight)
- **Issue**: v2 sets `state=Delivered` under lock *before* `module_sink.send().await` (correct for B5). But a CANCEL in that window matches `Some(Delivered)` and is forwarded to `module_sink` from the **read-loop actor**, racing the **drain actor**'s not-yet-issued `send` on the *same* mpsc with no ordering. If the CANCEL enqueues first, the module runs `handle_cancel` on an unknown corr (no-op) then processes the Request **uncancelled** — the exact LOOP-R5 cancel-loss under the saturation the redesign targets. This is the load-bearing B5↔B2 tension.
- **Evidence**: v2:92-97 (Delivered set before send), v2:76 (Delivered CANCEL forwards to module_sink); `router.rs:491` send; module no-op on unknown corr (`lib.rs:988-998`).
- **Impact**: Cancellation defeated on a saturated route — the primary defect the redesign exists to kill.
- **Fix Direction**: Serialize ALL module-bound frames (Request + Delivered-corr CANCEL) through the single drain actor; or introduce a `Sending{cancelled}` sub-state that buffers the CANCEL until the Request is on the sink, then the drain issues the CANCEL after its own send.

#### #N3: `AcquiredCredit` RAII guard + explicit `flow.release()` → double-release; abort/panic leaks Delivered credit + orphans slot
- **Severity**: Critical (BLOCKER)
- **Confidence**: Unanimous (8/8)
- **Members Reported**: all eight
- **Issue**: The send-error and rollback arms call `flow.release()` explicitly (v2:94,100) while the guard "releases on drop unless `commit()`ed" (v2:109) — but the pseudocode never shows `commit()`/`disarm()`. On the Ok arm, the guard drops still-armed → the module→client terminal path *also* releases → **double release** masked only by the shipped CAS `in_flight!=0` guard (`forwarding.rs:1702-1731`), which decrements *another live request's* credit when `in_flight>1`. Symmetrically, teardown cancellation of a blocked `module_sink.send` after Delivered bypasses the `Err(_)` cleanup arm → the Delivered slot/credit is never rolled back; the panic guard releases credit but does not `slots.remove` → orphaned slot (slow memory leak) with a dead drain that will never observe a future `cancelled` flag.
- **Evidence**: v2:88-111; `forwarding.rs:1692-1699` (`acquire` forgets permit — non-RAII), `1702-1731` (CAS masks over-release); `router.rs:281-309` (concurrent terminal release).
- **Impact**: Credit corruption (window silently shrinks or another request loses its permit); slot/memory leaks; the exact wedge the design kills.
- **Fix Direction**: Make credit a single consuming ownership token in the Slot: `rollback(self)` / `transfer_to_delivered(self)` — never raw `release()` + armed Drop. Cancel, send-error, panic, abort, and terminal must all consume that ONE token exactly once and remove the slot. Make `commit()`/`disarm()` explicit in the pseudocode.

#### #N4: Duplicate-corr `slots.insert` silently overwrites → permanent credit leak + double/zero terminal (Q4' not enforced in the mechanism)
- **Severity**: Critical (BLOCKER) — MAJOR/HIGH for a couple
- **Confidence**: Unanimous (8/8)
- **Members Reported**: all eight
- **Issue**: The read-loop push does bare `slots.insert(corr, ...)` (v2:66). A reused in-flight corr silently overwrites the prior slot; the old corr's `outstanding`/credit is orphaned (its terminal removes the *new* slot, or a late terminal for an old x removes a newly-reused x). Q4' *leans* enforce-at-enqueue but the normative mechanism does not implement it. The shipped daemon admits on route/flow only, with no corr-uniqueness check (`router.rs:452-497`); the wire requires non-reuse but does not enforce it.
- **Evidence**: v2:66 vs Q4' (v2:316-319); `router.rs:452-498`; `subc-wire-v1-final.md:405-407`.
- **Impact**: Credit leak + broken exactly-once accounting; breaks the `outstanding`/`slots` gate that B5/B8 depend on.
- **Fix Direction**: Hoist Q4' into the normative push: `if slots.contains_key(corr) { synthesize Error{protocol_violation}; close connection; return }`. Note (GPT5.6PRO/TerraPRO): a plain `contains_key` does not stop *sequential* reuse aliasing a late terminal — add a generation/epoch to distinguish.

#### #N5: Synthetic-error egress on the read loop has no reliable non-blocking lane → the promised terminal can vanish
- **Severity**: High→Critical (BLOCKER for several)
- **Confidence**: Majority (6/8)
- **Members Reported**: Opus (N8-adjacent), GPT5.6PRO, TerraPRO, GPT5.5, XAI, GLM, Gemini
- **Issue**: The read loop cannot `await` egress (I6 cancel-safety), yet it must deliver synthetic `cancelled` / `route_backpressure` / `route_closing`. The only non-blocking shipped API is fallible `try_send`, which fails when the client egress buffer (capacity 64, `server.rs:243`) is full → the promised terminal **vanishes**, leaving the SDK with a timeout/outcome-unknown for a request that provably never reached the module. Shipped recoverable errors *await* egress (`server.rs:388-401`) — precisely what v2 forbids.
- **Evidence**: v2:64-76 (synthesize "to client"), I6; `server.rs:243,388-401`; `FrameSink::try_send` fails on Full (`router.rs:69-81`).
- **Impact**: Silent request loss / renewed HOL if awaited. Residual v1 #13, unaddressed by v2.
- **Fix Direction**: Reserved bounded egress lane or a connection response actor; on exhaustion, epoch-fenced connection close (client treats close as outcome-unknown). Distinguish `Closed` from `Full`. Lint-gate the read-loop call graph to forbid `.await` except `read_frame`/close.

### MAJORITY / STRONG FINDINGS

#### #N6: Teardown lifecycle not reconciled with shipped SYNC Drop; endpoint-drain quiescence lost; no single close-owner
- **Severity**: High (BLOCKER for several)
- **Confidence**: Majority (6/8)
- **Members Reported**: Opus, GPT5.6PRO, TerraPRO, GPT5.5, Minimax, XAI
- **Issue**: v2's async 3-phase teardown (cancel_token select + 2s bounded-join + `abort()`) cannot be invoked from the shipped synchronous teardown triggers: `RouterConnection::drop` (`router.rs:391-397`) is a sync `Drop` → `cleanup_connection` (sync, `control.rs:422-458`) — a sync Drop cannot `.await` a bounded-join. Two concurrent closers can both set `Closing`, overwrite the reason, and race the join/abort. Shipped module reload waits for in-flight quiescence before releasing routes (`supervise.rs:2567-2595`); v2's generic six-step teardown omits this ordering, so a reload/endpoint-drain aborts Delivered corrs instead of draining them.
- **Evidence**: `router.rs:391-397`; `control.rs:422-458`; `supervise.rs:2418-2435,2567-2595`; v2:157-174.
- **Fix Direction**: Put reason + `Option<JoinHandle>` in one lifecycle object with `Open→Closing` owner election; refactor route/connection shutdown into explicit async APIs; preserve reload drain-to-quiescence; cancel/join all route + control tasks before the writer wait; keep Drop only as an abort backstop.

#### #N7: Dispatcher↔binding wiring the mechanism requires DOES NOT EXIST in shipped types — spec is un-implementable as written
- **Severity**: High (BLOCKER) — SOLO-deep but structurally decisive
- **Confidence**: Solo (Minimax) — but un-refuted and source-decisive; adjacent to N6/lock-hierarchy findings from others
- **Members Reported**: Minimax (unique deep find)
- **Issue**: (a) The R11 gate requires the **module→client terminal path** (`router.rs:281-309`) to `slots.remove(corr)` on the route's `RouteInbox` — but the shipped `RouteBinding` (`forwarding.rs:52-65`) has **no reference to any `RouteDispatcher`**; v2's only `RouteBinding` change is `closed: AtomicBool`. The terminal path holds an `Arc<RouteBinding>` and literally cannot reach the inbox. (b) Connection-close teardown must enumerate live dispatchers to set `admission=Closing` — but `ForwardingInner` has **no dispatcher map**; `cleanup_connection` iterates `client_to_module` and never touches a dispatcher.
- **Evidence**: `forwarding.rs:52-65` (RouteBinding), `forwarding.rs:236-256`/`1168-1239` (ForwardingInner / cleanup), `router.rs:281-309`; v2:122-128,156-174,207-211.
- **Impact**: The two central v2 mechanisms (R11 gate + teardown) cannot be built against the shipped types without adding `dispatcher: Weak<RouteDispatcher>` to `RouteBinding` and a `dispatchers` map to `ForwardingInner` — both unspecified.
- **Fix Direction**: Add `dispatcher: Weak<RouteDispatcher>` to `RouteBinding` and `dispatchers: HashMap<ClientRouteKey, Arc<RouteDispatcher>>` to `ForwardingInner`; specify both directions of the wiring.

#### #N8: Merge-1 not standalone-landable — current `handle_bound` has no `closed` recheck; publish-vs-route.open-visibility ordering unspecified
- **Severity**: High
- **Confidence**: Majority (5/8 not-standalone-safe as written)
- **Members Reported**: GPT5.6PRO, TerraPRO, GPT5.5, XAI, (Minimax partial)
- **Issue**: v2 names the *future dispatcher push* as the client-side `closed` checker, but merge-1 lands **before** the dispatcher exists. Today client ingress calls `handle_bound` directly (`router.rs:335-343,452-485`); without a `closed` recheck there, a stale `Bound` snapshot still reaches `flow.acquire()` → `backend_error` (the exact new observable B10 claims to avoid). Also, "publish under lock" is insufficient unless publication precedes the already-externally-observable `client_permit.send(route_open_frame)` (`forwarding.rs:1524-1536`); that order is not normative or tested. Module-side stale should mimic today's silent drop (`router.rs:227-245`), not a literal `unknown_channel`.
- **Evidence**: `router.rs:335-343,432-485,227-245`; `forwarding.rs:1524-1536`; v2:203-221,295-303.
- **Fix Direction**: Merge-1 must add `closed` rechecks to BOTH current data-plane Bound branches; publish the bound snapshot immediately before `route_open` becomes visible; add read-your-writes tests (bind-then-immediately-route, control-plane hello→catalog).

#### #N9: `route_closing` is a NEW SDK-unclassified error code, omitted from merge-0 and I8
- **Severity**: High
- **Confidence**: Majority (4/8)
- **Members Reported**: GPT5.6PRO (via #11), TerraPRO, GPT5.5, Minimax
- **Issue**: v2 emits `Error{route_closing}` at admission (v2:64) but merge-0 only adds `{route_backpressure, control_backpressure}` and I8 lists only those. Current SDKs treat unknown data-plane Error codes as terminal / `CallError::Module` (TS `client.ts:1059-1060,423-452`; Rust `consumer.rs:570-583`). A reader that loads old Bound, sees `closed=false`, pauses, and resumes after teardown gets `route_closing` (admission=Closed) — contradicting B10's "restore `unknown_channel`" claim.
- **Evidence**: v2:63-65,167-170,263-264; `client.ts:734-744,1036-1061`; `consumer.rs:570-583`.
- **Fix Direction**: Emit canonical `unknown_channel` for Closing/Closed stale dispatch, OR add `route_closing` to merge-0 retryable set + I8 + parity tests.

#### #N10: Lock hierarchy (RouteInbox mutex vs global forwarding write lock) unspecified — latent inversion/deadlock
- **Severity**: Medium→High
- **Confidence**: Majority (5/8)
- **Members Reported**: Opus, GPT5.6PRO, TerraPRO, XAI, GLM (Gemini via fix-list)
- **Issue**: Teardown nests `forwarding-write ⊐ inbox`; the data path takes `forwarding-read`, releases it, then `inbox`; terminal-delivery-failure escalates back through forwarding. No path is *proven* free of `inbox`-then-`forwarding-write` inversion, and the doc never states the hierarchy.
- **Evidence**: `forwarding.rs:614-657,840-890,1409-1470`; `router.rs:285-305`; v2 teardown §5.
- **Fix Direction**: Make normative: `ForwardingInner write lock → RouteInbox`, never the reverse; release both before any egress/await/cancel/join. Audit every site.

#### #N11: Byte/task memory bounds still non-normative (v1 B12 not closed)
- **Severity**: High (BLOCKER for the three who scored it so)
- **Confidence**: Majority (6/8 raise it; leans-only in Q3')
- **Members Reported**: Opus, GPT5.6PRO, TerraPRO, GPT5.5, Minimax, GLM/Gemini (via Q3')
- **Issue**: `depth_cap` bounds frame *count*, not bytes. Bodies are owned `Vec<u8>` up to 64 MiB (`subc-protocol/src/lib.rs:114-119`) allocated before admission (`frame_io.rs:73-86`); a 2048-deep route ≈ 128 GiB, aggregate ≈ 256 GiB/conn. Route allocation permits all nonzero u16 channels → up to ~65,535 drain tasks/conn. v2 leaves byte budgets in Q3' as an open question.
- **Fix Direction**: Make per-route/per-connection/process-global BYTE budgets normative, charged pre-admission, RAII-released on every dequeue/remove/flush/panic path; add practical route/task caps.

### MINORITY / SOLO FINDINGS

#### #N12: v2 omits non-Request client→module data frames (Response/Error/StreamEnd) — reverse-request deadlock
- **Severity**: High (BLOCKER for the member who raised it)
- **Confidence**: Solo (GPT5.6PRO) — un-refuted, source-cited
- **Members Reported**: GPT5.6PRO
- **Issue**: The dispatcher specifies only Request + CANCEL, but the shipped client→module path forwards *every* data frame; Responses/Errors/stream frames (credit-free, even on Serial routes) are required for reverse requests. Putting them behind a drain already blocked acquiring credit for another Request can **deadlock the module's reverse RPC**.
- **Evidence**: `router.rs:452-498`; `tests/reverse_request.rs:96-137,140-231`.
- **Fix Direction**: Specify a preemptible, credit-free pass-through lane through the same ordered sink arbiter; an urgent Response/CANCEL must interrupt a pending acquire without overtaking its target Request.

#### #N13: `route_backpressure` has no daemon provenance — module-forgeable → unsafe auto-retry
- **Severity**: High (BLOCKER for the member who raised it)
- **Confidence**: Solo (GPT5.6PRO)
- **Members Reported**: GPT5.6PRO
- **Issue**: Error codes are open strings; modules can emit arbitrary `Error{code}` (`subc-client-rs/src/lib.rs:515-525`, `provider.ts:995-1018`) and the daemon forwards them unchanged (`router.rs:281-309`). A module that performed a side effect then emitted `route_backpressure` would be **auto-retried by the SDK** → duplicate execution.
- **Evidence**: `docs/subc-control-protocol.md:62`; `router.rs:281-309`.
- **Fix Direction**: Unforgeable daemon provenance (daemon-only header flag stripped/rejected on module ingress, or reserve/escape daemon codes). Add a test: module emits `route_backpressure` → never auto-retried.

#### #N14: `Slot{frame}` cannot retain its Frame after `module_sink.send(frame)` consumes it
- **Severity**: Medium
- **Confidence**: Solo (GPT5.6PRO)
- **Members Reported**: GPT5.6PRO
- **Issue**: The Delivered marker must remain in `slots`, but `module_sink.send(frame)` moves the Frame; `Frame::clone` deep-copies up to 64 MiB (`subc-protocol/src/frame.rs:12-17`).
- **Fix Direction**: Encode ownership in the state: `Queued(Frame) | Claimed{frame,cancelled} | Delivered` (no frame), or `Option<Frame>` moved out exactly once.

#### #N15: Whole-table publish is O(routes) locked work per mutation — attacker-controlled churn
- **Severity**: Medium
- **Confidence**: Solo (GPT5.6PRO)
- **Members Reported**: GPT5.6PRO
- **Issue**: Every mutation rebuilds the whole snapshot under the global write lock; route space reaches 65,535/endpoint. Authenticated route.open/GOODBYE churn → attacker-controlled O(N) locked work, O(N²) aggregate.
- **Fix Direction**: Gate whole-table publish on route/rate caps + an adversarial churn benchmark in T9; shard past a measured threshold.

#### #N16: Teardown step 5 drains only the queue, leaking Claimed/Delivered slots
- **Severity**: Medium
- **Confidence**: Solo (GLM)
- **Members Reported**: GLM
- **Issue**: Step 5 synthesizes cancelled for "queued corrs" only; Claimed/Delivered slots remain in `slots` with no terminal to the client.
- **Fix Direction**: Drain the entire `slots` map on Closed: synthesize cancelled for Queued/Claimed, account Delivered.

#### #N17: `outstanding` "mirrors flow.in_flight" comment invites a false invariant
- **Severity**: Low
- **Confidence**: Solo (Minimax)
- **Members Reported**: Minimax
- **Issue**: `outstanding` (incr at Delivered, decr at slot-removal) and `flow.in_flight` (incr at acquire, decr at release) diverge transiently by the acquire→Delivered and slot-removal→release windows. Any assertion of equality can transiently fail.
- **Fix Direction**: Remove the "mirrors" comment or state the real invariant (delivered-not-settled; briefly diverges).

---

## Q1'–Q5' Rulings (consolidated)

| Q | v2 Lean | Council ruling | Rationale |
|---|---|---|---|
| **Q1'** hard-gate merge-0 vs blocking-admission interim | hard-gate | **RIGHT-BUT-UNSAFE** (majority) | Hard-gating is architecturally cleaner, but merge-0 is unimplemented AND "live fleet-wide in broca/aft/alfonso-core" is **UNVERIFIABLE in this checkout**. Hard-gating on an unconfirmed external deploy is unsafe; the blocking-admission interim (drain-side wait, off the read loop) is HOL-free and a sound fallback. |
| **Q2'** 2s bounded-join + abort drops in-flight terminal | acceptable | **WRONG / RIGHT-BUT-UNSAFE** (split, majority against as-scoped) | Acceptable ONLY for a truly closing connection. v2 applies it to GOODBYE and endpoint/reload drain, where abort abandons Delivered terminals + credit accounting and violates shipped reload quiescence (`supervise.rs:2567-2595`). Must narrow scope + drain credit before abort. |
| **Q3'** byte-based secondary cap | keep frame cap, byte secondary | **RIGHT-BUT-UNSAFE / WRONG as gate lean** | Byte budgets are correct but MUST be normative + pre-admission + RAII, not an open question; the byte cap should arguably be PRIMARY (charged before body allocation). Frame-count-only is the exact v1 B12 DoS. |
| **Q4'** enforce corr-uniqueness at enqueue | enforce | **RIGHT-BUT-NOT-INTEGRATED** (unanimous) | The lean is correct but the normative push pseudocode does not implement it (N4); several note plain `contains_key` misses sequential reuse — add a generation. |
| **Q5'** whole-table Arc rebuild | whole-table | **RIGHT, conditional** (unanimous) | Read-mostly justifies whole-table; conditional on publish-under-lock + `closed` recheck + churn benchmarks in T9 (see N15). |

---

## Summary Table

| # | Finding | Severity | Agreement | Members |
|---|---|---|---|---|
| N1 | pop→claim two-lock CANCEL race → route hang | Critical | Unanimous 8/8 | all |
| N2 | Delivered-before-send CANCEL reorder (LOOP-R5) | Critical | Majority 6/8 | Opus,5.6PRO,Terra,5.5,XAI,Minimax |
| N3 | AcquiredCredit double-release / abort leak / orphan slot | Critical | Unanimous 8/8 | all |
| N4 | duplicate-corr silent overwrite → credit leak | Critical | Unanimous 8/8 | all |
| N5 | synthetic-error egress vanishes on full egress | High-Crit | Majority 6/8 | Opus,5.6PRO,Terra,5.5,XAI,GLM,Gemini |
| N6 | async teardown vs shipped sync Drop; quiescence lost | High | Majority 6/8 | Opus,5.6PRO,Terra,5.5,Minimax,XAI |
| N7 | dispatcher↔binding wiring absent → un-implementable | High | Solo (decisive) | Minimax |
| N8 | merge-1 not standalone (handle_bound no closed recheck) | High | Majority 5/8 | 5.6PRO,Terra,5.5,XAI,Minimax |
| N9 | route_closing new unclassified SDK code | High | Majority 4/8 | 5.6PRO,Terra,5.5,Minimax |
| N10 | lock hierarchy inbox vs forwarding-write unspecified | Med-High | Majority 5/8 | Opus,5.6PRO,Terra,XAI,GLM |
| N11 | byte/task memory bounds non-normative (B12) | High | Majority 6/8 | Opus,5.6PRO,Terra,5.5,Minimax,GLM |
| N12 | non-Request client→module frames omitted → reverse deadlock | High | Solo | 5.6PRO |
| N13 | route_backpressure module-forgeable → unsafe retry | High | Solo | 5.6PRO |
| N14 | Slot cannot retain Frame after send | Medium | Solo | 5.6PRO |
| N15 | whole-table publish O(routes) under write lock | Medium | Solo | 5.6PRO |
| N16 | teardown drains queue only, leaks Claimed/Delivered | Medium | Solo | GLM |
| N17 | outstanding "mirrors in_flight" false-invariant bait | Low | Solo | Minimax |

---

## Priority Recommendations

**BLOCKERS — must close before merge-2, then re-gate:**
1. **Atomic pop+claim** in the drain (one inbox lock) + explicit missing-slot arm. (N1)
2. **Serialize module-bound frames through the drain** — resolve the B5↔B2 tension: a `Sending{cancelled}` sub-state so a Delivered-window CANCEL cannot overtake the Request on `module_sink`. (N2)
3. **Single consuming credit token** owned by the Slot; no raw `release()` + armed Drop; cancel/send-error/panic/abort/terminal each consume it once + remove the slot; explicit `commit()`/`disarm()`. (N3)
4. **Enforce corr-uniqueness in the normative push** (with a generation for sequential reuse). (N4)
5. **Reserved non-blocking synthetic-error egress** (lane or response actor; close-on-full). (N5)
6. **Reconcile async teardown with shipped sync Drop**; preserve endpoint-drain quiescence; single close-owner; cancel/join all route + control tasks before the writer wait. (N6)
7. **Add the dispatcher↔binding wiring** (`Weak<RouteDispatcher>` on `RouteBinding`; `dispatchers` map on `ForwardingInner`) — the mechanism is un-implementable without it. (N7)
8. **Byte-based memory budgets** normative + pre-admission + RAII; route/task caps. (N11)

**MERGE-1 — do NOT land standalone as written:**
- Add `closed` rechecks to BOTH current data-plane Bound branches (`handle_bound` client→module + module→client); publish snapshot before `route_open` visibility; direction-correct stale semantics (client→module `unknown_channel`, module→client silent drop); read-your-writes tests. (N8)

**CONTRACT / SDK:**
- Resolve `route_closing` (reuse `unknown_channel` or add to merge-0 + I8 + parity). (N9)
- Add unforgeable daemon provenance for retryable codes. (N13)
- Complete + deploy-verify merge-0 across TS/Rust/Swift; **broca/aft/alfonso-core are NOT in this checkout — their contract impact is UNVERIFIABLE here and must be checked in their own repos.** (B1 caveat, unanimous)

**SPEC COMPLETENESS / re-gate flags:**
- Lock hierarchy normative (N10); non-Request pass-through lane (N12); Slot frame ownership (N14); whole-table churn caps + benchmark (N15); drain all slots on teardown (N16); fix the `outstanding` "mirrors" comment (N17); narrow Q2' abort scope.

---

## Genuinely CLOSED (do not re-litigate — traced, near-unanimous)

- **B3** queue primitive / mpsc-scan race — CLOSED 8/8.
- **B5** outstanding/Delivered before send — CLOSED 8/8 (conditional on B2 + corr-uniqueness).
- **B6** O(1) CANCEL *lookup* — CLOSED (residual O(n) *queue removal* tracked as N1/N2).
- **B7** bind-ACK barrier via client-side-only offload — CLOSED 8/8; shipped test `accepted_route_publishes_route_open_before_immediate_reverse_request` (`router.rs:1078-1102`) stays green.
- **B8** I3/I4/I7 honest deltas + R11 concurrent-duplicate framing; slot-gate composes cleanly with the shipped CAS guard (`forwarding.rs:1702-1731`) — no conflict — CLOSED 8/8.

**On R11 double-guard (your item 5):** the council is unanimous — v2's slot-gate and the shipped CAS `in_flight!=0` guard **compose cleanly, not conflict**: the slot-gate is the real linearization (duplicate finds slot gone → `release=false`), the CAS is the defensive last layer. But GPT5.5/Minimax warn T15 can **false-pass**: with a single in-flight request, the first release takes `in_flight` 1→0 and the duplicate is masked by CAS regardless of the gate — T15 must keep a *second* request outstanding to actually exercise the gate.

---

## Note on External Consumers

broca / aft / alfonso-core are **not present in this checkout** (only `fake-aft-stub`). The merge-0 "live fleet-wide" gate and the backpressure/`route_closing` contract impact on those consumers are **UNVERIFIABLE here** and must be audited in their own repos before merge-2. All eight members flagged this independently.
