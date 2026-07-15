# Adversarial Council Verdict — subc-core dispatch redesign

**Design under review:** `docs/subc-dispatch-redesign.md` (committed f3185c89)
**Gated against:** shipped subc-core daemon source at master
**Council:** 8 independent models (Opus 4.8, GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh, XAI Composer 2.5, Ollama Minimax M3, Ollama GLM 5.2, Gemini Flash 3.5)
**Intent:** AUDIT (adversarial, concurrency-critical, source-cited)

---

## VERDICT: **NO-GO** (unanimous, 8/8)

Every member independently returned NO-GO. The convergence is not stylistic agreement — multiple **distinct** defects were found independently by 6–8 members each, all cited to shipped source. Per the gate rule (a single un-refuted concurrency/contract defect is NO-GO), the design fails on **at least five** un-refuted defect classes, three of which are BLOCKER by near-unanimous vote.

The design's *architecture* (per-route drain tasks, snapshot table, R11 rider, daemon-synthesized cancelled terminal) is judged **sound in spirit** by the sophisticated members. What fails the gate is that the load-bearing correctness details — queue data structure, drain-task error arms, teardown atomicity, the outstanding-set insertion ordering, and the SDK backpressure contract — are either unspecified or provably wrong against source. This is a **"redesign the mechanism, re-gate"** outcome, not a "scrap it" outcome.

---

## Confidence-Ranked Findings

### UNANIMOUS / NEAR-UNANIMOUS BLOCKERS

#### #1: `route_backpressure` → NotSent mapping is FALSE in every shipped SDK ("zero SDK changes" is a false contract claim)
- **Severity**: Critical (BLOCKER)
- **Confidence**: Unanimous (8/8)
- **Members**: All eight
- **Issue**: The design's load-bearing consumer claim — `route_backpressure` "maps to the existing NotSent contract," "rides existing error-classification paths," §5 "Nothing required" from SDKs — is false against source. A daemon-synthesized `Error{code:"route_backpressure"}` is classified as a **hard terminal / outcome-unknown** failure, NOT retryable-NotSent, in all three clients.
- **Evidence (multiply cited)**:
  - **TS**: incoming Error settles as `pending.reject(errorFromFrame(frame))` → plain `SubcError`, wrapped as `terminalCallError` kind `"terminal"` (`client.ts:1057-1058, 1116-1126, 1155-1158`). Managed `call()` retries ONLY `code === "unknown_channel"` or `kind === "not_sent"` (`client.ts:427, 435`); a `terminal` error re-throws (`client.ts:450`). **Worse (Minimax, Gemini):** because the daemon read the frame, `handedToSocket = write.queued = true` (`client.ts:807`), so `classifyFailure` yields `outcome_unknown` (`client.ts:781-792`) — the *opposite* of "never reached the module" — and managed `call()` reconnects/tears down the connection on that path.
  - **Rust**: Error terminal → `PendingTerminal::Error` → `CallError::Module(body)` (`consumer.rs:2877-2884, 570-579`). Retries only `unknown_channel`; everything else `return Err(CallError::Module(body))`. Not NotSent, not retried.
  - **Swift**: no retry/classifier logic exists; `SubcError` has no code field; `remoteError` preserves only textual JSON (`Client.swift:31-34, 671-674`).
  - The ONLY retryable-code classifier (`isRetryableRouteOpenCode` / `is_retryable_route_open_code`) is a CLOSED set `{unknown_module, module_reloading, target_unavailable, module_timeout}` gated to channel-0 route.open (`client.ts:1252-1259`, `consumer.rs:3130-3135`) — it never touches data-plane request terminals.
- **Impact**: merge-2 without a prerequisite SDK merge converts transient saturation into **hard application errors / silent request loss / connection teardown** in production. The "additive config" framing is wrong.
- **Fix Direction**: Add a PREREQUISITE SDK merge (merge-0) that parses data-plane Error `code`, adds a distinct `daemon_not_sent`/`route_backpressure` retryable class mapping to in-place retry with backoff (NOT reconnect / route eviction) across TS+Rust+Swift, with parity tests — landed **before** merge-2. Correct Goal 5 / §5 / I8 in the doc. (Alternative: keep per-route blocking admission until SDKs land — see #10/Q1.)

#### #2: CANCEL vs the queued→delivered boundary — a limbo window that re-introduces cancel-loss AND can double-fire a terminal
- **Severity**: Critical (BLOCKER)
- **Confidence**: Unanimous (8/8)
- **Members**: All eight
- **Issue**: The design treats "not in queue" as "delivered or unknown." That is false during the drain-task limbo: a Request popped by `queue.recv()` but still awaiting `flow.acquire().await` or `module_sink.send().await` is in neither the queue nor (yet) the `outstanding` set. Two symmetric races:
  - **CANCEL loses**: CANCEL arrives in the limbo window, doesn't find the corr queued, forwards to the module *before* the Request arrives; module `handle_cancel` no-ops on the unknown corr; Request then runs uncancelled — **the exact R5 defect this design exists to kill, transposed.**
  - **Double terminal**: CANCEL finds the frame still in the queue struct and removes it + synthesizes `cancelled`, while the drain task has concurrently claimed and delivered it → module ALSO emits a terminal. Both fire for one corr, breaking the design's 3.3 claim ("queued case has exactly the synthetic terminal; delivered has exactly the module's").
- **Evidence**: blocking points are exactly `router.rs:465` (`flow.acquire().await`) and `router.rs:491` (`module_sink.send().await`); module cancel is a no-op for unknown corr (`subc-client-rs/src/lib.rs:979-990`, `provider.ts:694-696`); module inserts its own in-flight entry only AFTER receiving the Request (`fake-aft-stub.rs:339-344`). Today the serial loop structurally prevents this by not reading CANCEL until routing completes (`server.rs:357-375`).
- **Impact**: Cancellation defeated under exactly the saturation conditions the redesign targets; and/or duplicate terminals + credit-accounting corruption.
- **Fix Direction**: Replace "inspect the queue from the read loop" with a **single atomic per-corr decision point**: a route-local state machine `Queued → Claimed/Acquiring → Delivered → Settled (+Cancelled)`. Insert corr into `outstanding` (or a `Delivering` marker) **before** the send await; have CANCEL check queue *and* the delivering/outstanding state under one route lock; a cancel-winning claim must prevent the send or roll back the acquired credit. (See #4 fix — the tombstone-set approach also resolves this by making the drain task the serial decision-maker.)

#### #3: Dispatch-queue data structure is unspecified and incompatible with the "read-loop scans the queue" requirement (data race)
- **Severity**: Critical (BLOCKER)
- **Confidence**: Majority→Unanimous (7/8 explicit; 8/8 via the DoS corollary #6)
- **Members**: Opus 4.8, GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh, XAI Composer, Minimax, Gemini (GLM via #6)
- **Issue**: 3.2 drains via `queue.recv()` (tokio mpsc semantics — single-consumer, non-scannable from the sender side). 3.3 requires the **read loop** to scan/remove-by-corr from the *same* queue concurrently. A tokio mpsc receiver does not expose its buffer; a bare `VecDeque`/`Vec` scanned by the read loop while the drain task mutates it is a **data race** (torn reads / UB). The two requirements are mutually exclusive without an explicit shared synchronized structure — which the doc never names.
- **Evidence**: 3.2:91 `queue.recv()` vs 3.3:117 "CANCEL inspects the route's dispatch queue" / 3.5:159 "executed by the read loop against the queue structure itself."
- **Impact**: Undefined behavior, or (if naïvely mutexed) a new per-route lock shared between read loop and drain task on the latency-critical path — the HOL-class hazard the redesign set out to kill.
- **Fix Direction**: Specify the primitive explicitly. Recommended (converges across members): `VecDeque<(corr,Frame)>` behind a route-local mutex **plus a `corr → node/state` index** for O(1) removal, **plus a concurrent `cancelled_corrs`/tombstone set** the drain task checks before acquire. Prove the read-loop lock hold is bounded and non-awaiting; quantify contention in T9.

#### #4: Drain-task error arms (`flow.acquire` closed, `module_sink.send` failure, panic) are unspecified — credit leak / double-release / lost Error-frame recovery
- **Severity**: Critical (BLOCKER) — scored MAJOR by a few, BLOCKER by most
- **Confidence**: Unanimous (8/8)
- **Members**: All eight (framed as credit-accounting and/or drain-error findings)
- **Issue**: 3.2's `drain_task` pseudocode `{ flow.acquire().await; module_sink.send().await; }` has **no error arm**. Moving acquire+send off the read loop removes the caller that today converts failures into canonical Error frames and releases credit on send-failure. Multiple concrete gaps:
  - Shipped code releases the just-acquired credit on `module_sink.send` failure (`router.rs:491-496`); the drain task must replicate this exactly or leak a permit.
  - `flow.acquire()` returning `ChannelFlowClosed` (`forwarding.rs:1692-1700`) must be disambiguated into (a) module-reloading → synthesize `module_reloading` error; (b) GOODBYE teardown → drop silently (client settled); (c) connection close → exit silently. The drain task cannot tell these apart without extra per-route state. The existing test `blocked_flow_control_acquire_wakes_when_module_tears_down` (`forwarding.rs:3811-3877`) demands a `backend_error` terminal here.
  - `ChannelFlow::acquire` does `permit.forget()` (`forwarding.rs:1692-1699`) — credit is NOT held RAII; panic/abort after acquire but before insert/send leaks the permit.
  - Panic backstop ("abort-guard mirroring broca") only releases the *binding*, not the in-flight credit or the outstanding entry; the broca pattern is not present in subc-core.
- **Evidence**: `router.rs:465-496`, `forwarding.rs:1692-1731` (forget + close semantics), `server.rs:377-390` (today's Error-frame recovery in the read loop), `forwarding.rs:3811-3877` (the test).
- **Impact**: Credit leak (silent wedge — the exact failure the design kills) or double-release; loss of shipped Error-frame recovery; an existing test breaks (contradicting T8).
- **Fix Direction**: Normative drain-task error handling: insert corr into `outstanding` immediately after `acquire` returns Ok (before send); on send failure `outstanding.remove` + `flow.release` + drop (no retry, preserving I1 at-most-once) + synthesize `backend_error` if route alive; distinguish reload/GOODBYE/close via a per-route `tearing_down`/state flag; make the panic guard an RAII credit guard that releases exactly the corrs still in `outstanding`, and disarm on intentional abort.

#### #5: `outstanding.insert(corr)` ordering — insert-after-send races a fast terminal → permanent credit leak
- **Severity**: Critical (BLOCKER)
- **Confidence**: Majority (6/8)
- **Members**: Opus 4.8, GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh, XAI Composer, Gemini
- **Issue**: 3.7 says "insert on delivery to module" without fixing the ordering relative to `module_sink.send().await`. A module on another runtime thread can process the Request and its terminal can reach the daemon's module→client path (`router.rs:281-309`) **before** the drain task records the corr. `outstanding.remove(corr)` then returns false, release never fires, and the later insert leaks the corr and its credit permanently.
- **Evidence**: `router.rs:491-496` (send), `router.rs:281-309` (terminal release gated by the new set), drain pseudocode ordering ambiguity.
- **Impact**: Permanent per-corr credit leak under a fast module — window silently shrinks over time.
- **Fix Direction**: Insert into `outstanding` **before** `module_sink.send().await` (i.e., immediately after acquire returns Ok), using an RAII guard that rolls back on send-failure/cancel/panic. (Same fix collapses #2's limbo window.)

#### #6: O(queue) CANCEL scan on the read loop is an attacker-amplified DoS / HOL vector
- **Severity**: High→Critical (BLOCKER-adjacent) — scored MAJOR/HIGH by most
- **Confidence**: Unanimous (8/8)
- **Members**: All eight
- **Issue**: 3.3/3.5 put an O(queue) CANCEL scan on the latency-critical read loop and never drop CANCEL for capacity. Adversary fills a StatelessParallel queue to depth 2048 (within the 4096 per-connection cap), then sprays cheap 21-byte CANCELs for non-existent corrs; each forces up to 2048 comparisons on the read loop before the next frame can be read.
- **Evidence / math (independently computed)**: worst-case ≈ 4096 × 2048 ≈ **8.4M comparisons per burst**; draining an adversarially-ordered full 2048 queue ≈ 2,098,176 comparisons. Depths from 3.5:149; single read loop `server.rs:357-374`; CANCEL is a pure 21-byte header (`subc-protocol/src/lib.rs:162-165`).
- **Impact**: Read-loop starvation — reintroduces the cross-channel HOL the design claims to eliminate (Goal 1).
- **Fix Direction**: `corr → node/state` index or `cancelled_corrs` tombstone set for O(1) CANCEL; never linear-scan on the read loop. (Same structure as #3.)

#### #7: Whole channel-0 FIFO offload breaks the route.bind-ACK → immediate-data ordering (module first-frame drop)
- **Severity**: Critical (BLOCKER) — MAJOR for a couple who note SDK single-flight mitigates for compliant clients
- **Confidence**: Majority (6/8)
- **Members**: GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh, Gemini, Opus (as "narrowed contract"), XAI (partial)
- **Issue**: Today the serial loop commits a module's `route.bind` ACK inline before reading the next frame. Under offload, the ACK goes to the control FIFO task while the module's immediately-following data frame hits the per-route data path; the snapshot lookup sees `Reserved`/`Absent` and the frame is **dropped**. A control-only FIFO orders control-vs-control, not control-before-subsequent-data.
- **Evidence**: shipped inline commit `server.rs:357-375` + `control.rs:2029-2032` + `commit_route_locked` publishing maps before route.open (`forwarding.rs:1524-1536`); Reserved/Absent module frames dropped (`router.rs:227-245`); TS/Rust providers send ACK then immediately `onBound`/`on_bound` (`provider.ts:824-849`, `lib.rs:1096-1104`); **explicit shipped test** `accepted_route_publishes_route_open_before_immediate_reverse_request` (`router.rs:1078-1102`); ordering is normative in `docs/specs/subc-wire-v1-final.md:123-165`.
- **Impact**: Silent loss of a module's first reverse frame; breaks a spec-normative barrier and an existing test.
- **Fix Direction**: Keep module-originated bind completions (and other route-publication barriers) **inline** on the read path, or impose a per-connection ingress sequence fence so subsequent same-connection data cannot overtake the commit. Do NOT subject critical ACK/Response frames to control-queue overflow. (This makes Q3's "whole channel-0 FIFO" lean **wrong** — see #10.)

---

### MAJORITY / STRONG FINDINGS

#### #8: I3 "release paths untouched" and I7 "module→client unchanged" are FALSE against source
- **Severity**: Medium (doc-correctness, but hides review surface)
- **Confidence**: Unanimous (8/8)
- **Members**: All eight
- **Issue**: I3 claims "epoch-fenced release + escalation semantics byte-identical (release paths untouched)"; I7 claims module→client is unchanged. But 3.7 inserts an `outstanding.remove(corr)` gate before `route.flow.release()` — the shipped release is unconditional at `router.rs:307-309`. The rider *by definition* changes that path; the CANCEL exactly-once proof *depends on* the change. (GPT 5.5 notes today's release already has a CAS `in_flight` over-release guard at `forwarding.rs:1702-1731`, so the rider catches a *different* defect — duplicate-release-of-same-credit — and the doc's "trusted vs enforced" framing is imprecise.)
- **Impact**: The invariant checklist misdirects the gate away from the real behavioral delta.
- **Fix Direction**: Reword I3 → "epoch-fence + escalation predicates preserved; release call site gains an exactly-once `outstanding` gate." Reword I7 → "wire behavior unchanged; duplicate/late-terminal credit accounting intentionally fixed (R11)." Add tests for duplicate terminal, late terminal after release, terminal for unknown corr. Also flag I4: under shipped serial ingress a Request preceding GOODBYE is delivered before GOODBYE is read; flush-on-GOODBYE is a real semantic change for raw-wire clients (SDKs settle locally first, so acceptable for them).

#### #9: GOODBYE flush vs concurrent enqueue is not atomic (late enqueue delivers to a released module); teardown can hang connection close
- **Severity**: High (BLOCKER for several)
- **Confidence**: Majority (6/8)
- **Members**: GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh, Minimax, XAI, Gemini
- **Issue**: "Flush must precede binding release so no frame can enqueue after flush" conflates flushing existing frames with preventing future enqueues. A lock-free snapshot reader can hold a still-`Bound` `Arc<RouteBinding>` with a live queue sender and `try_push` *after* the flush scan but *before* the sender drop → frame delivered to a released module. Also: **dropping one sender does not close a bounded channel** while stale snapshots/tasks retain sender clones; a drain task blocked on `module_sink.send`, or a control task blocked in route.open, retains `FrameSink` clones and can **hang the writer/connection shutdown** (`server.rs:241-267` waits on the writer). `JoinHandle` drop does not abort a tokio task.
- **Evidence**: cloned `Arc<RouteBinding>` from lookup (`forwarding.rs:840-889`) owning both sinks; epoch-fenced release under write lock (`forwarding.rs:1409-1470`); writer wait (`server.rs:241-267`).
- **Fix Direction**: Shared `Open/Closing/Closed` route admission state. Atomically mark CLOSING and **close the receiver/admission gate before flushing**; then cancel/join the drain task before epoch-fenced removal. Use a connection-scoped cancellation token with explicit `close → cancel → bounded-join → abort` ordering before awaiting the writer; avoid a `binding → JoinHandle → task → binding` strong cycle. Specify a lock hierarchy (queue-lock vs global-write-lock) to avoid inversion.

#### #10: Merge-1 (snapshot forwarding) is NOT invariant-neutral as a standalone landing
- **Severity**: High (BLOCKER for several) / conditionally-OK for others
- **Confidence**: Majority (6/8 not-neutral-as-written; 2 rate merge-1 acceptable only under strict publication rules)
- **Members**: Not-neutral: GPT 5.6 Terra PRO, GPT 5.5 xhigh, GLM, Minimax; conditional-OK-with-constraints: GPT 5.6 PRO, Opus, XAI
- **Issue**: Old `RwLock` read provides read-after-write serialization: a reader acquiring the read lock after a release-writer cannot see the old binding. ArcSwap `load()` lets a reader observe a snapshot published **before** a release, then use a stale `Bound` route — routing into a closed flow / dead module sink. Today's post-release lookup would yield `unknown_channel`; the stale path yields the `backend_error` route (`router.rs:465-485`) — a **new observable state**. Bind has the inverse stale-Absent window.
- **Evidence**: `read_inner()` at `forwarding.rs:846`; release removes maps + closes flow under write lock (`forwarding.rs:1409-1428`); module→client forwards any stale `Bound` without revalidation (`router.rs:281-309`).
- **Nuance**: A reader retaining an old `Arc<RouteBinding>` across release *already exists today* (lookup clones the Arc and drops the read lock before routing — `forwarding.rs:840-890`), so the delta is narrower than "brand new," but the *window widens*.
- **Fix Direction**: Make merge-1 conditionally neutral: **publish the fresh snapshot while still holding the write lock**, as the final step of each mutation, before any externally observable route.open/GOODBYE effect. Add a per-binding atomic `closed/generation` guard checked by every data-plane forward path so stale `Bound` snapshots are inert. Add a merge-1 test: bind-then-immediately-route on the same task (read-your-writes), and a control-plane read-your-writes regression (hello → catalog_update → catalog_list). Do NOT land merge-1 alone without the closed-bit validation.

#### #11: `HashSet<corr>` cannot enforce exact accounting without corr-uniqueness enforcement
- **Severity**: High (BLOCKER for two)
- **Confidence**: Minority→Majority (3/8 raised explicitly; high-signal)
- **Members**: GPT 5.6 PRO, GPT 5.6 Terra PRO (BLOCKER), noted implicitly by others
- **Issue**: The wire requires corr non-reuse but the daemon does not enforce it (`subc-wire-v1-final.md:392-408`; `router.rs:452-497` admits on route/flow only). Two delivered Requests with the same corr → one set entry → first terminal releases, second leaks a credit. Send R1(corr=x) delivered, queue R2(corr=x), CANCEL(x) removes R2 + synthesizes cancelled while R1 later emits its terminal → both terminals for x. A late duplicate terminal for an old x can remove a newly-reused x.
- **Fix Direction**: Reject duplicate queued/outstanding corrs before admission (protocol-violation close), or enforce monotonicity. A plain set/counter cannot distinguish an old duplicate terminal from a reused correlation. Add adversarial duplicate/reuse tests. Restate I2 as "release once per *uniquely delivered* Request."

#### #12: Frame-count caps are not memory bounds — catastrophic memory DoS
- **Severity**: High (BLOCKER for the three who raised it)
- **Confidence**: Minority (3/8) — high confidence where raised, cited to source
- **Members**: GPT 5.6 PRO, GPT 5.6 Terra PRO, GPT 5.5 xhigh
- **Issue**: A 4096-frame cap is not a byte bound. Max body is 64 MiB (`subc-protocol/src/lib.rs:114-119`), bodies are owned `Vec<u8>` allocated before admission (`frame_io.rs:74-84`). StatelessParallel depth 2048 → up to **128 GiB** on one route; aggregate 4096 → **256 GiB** per connection. Route allocation permits all nonzero u16 channels → up to ~65,535 drain tasks per connection.
- **Fix Direction**: Enforce per-route / per-connection / process-global **byte** budgets charged before body admission and released via RAII on every dequeue/remove/flush/panic path; add practical per-connection route/task caps. Frame-count may remain a secondary bound.

#### #13: Synthetic error frames have no reliable non-blocking egress policy
- **Severity**: Medium→High
- **Confidence**: Minority (3/8)
- **Members**: GPT 5.6 PRO, GPT 5.5 xhigh, Gemini
- **Issue**: The read loop cannot await `egress.send`, but `try_send` can fail when client egress is full — the promised `cancelled`/`route_backpressure`/`control_backpressure` terminal may vanish, or (if awaited) reintroduce B1/B2 on the read path. Today route-generated errors use an **awaited** egress (`server.rs:377-390`).
- **Fix Direction**: Reserved bounded egress (or a connection response actor) for synthetic errors; on exhaustion, epoch-fenced connection close, and the client must treat close as outcome-unknown. Distinguish queue `Closed` from `Full` (a stale route must not emit `route_backpressure`). Lint/review-gate the read-loop call graph to forbid `.await` except `read_frame`/close.

#### #14: Control-queue overflow policy is wrong for module control RESPONSES
- **Severity**: Medium→High
- **Confidence**: Minority (2/8) — high-signal, source-cited
- **Members**: GPT 5.5 xhigh, GPT 5.6 Terra PRO (via bind-ACK)
- **Issue**: Channel 0 carries not only client commands but module Responses/Errors that settle daemon-originated relay/control RPCs (`router.rs:405-412` → `handle_module_relay_response` → `control.rs:1879-2045, 2029-2032`). A generic `control_backpressure` error cannot replace a module Response/Error — it can leave a client route.open pending until timeout or corrupt relay state. Also: a single control FIFO puts Ping behind a 12s route.open (there is no channel-0 `route.close`; route close is data-channel GOODBYE, so the claimed route.open→route.close protection is partly illusory — `subc-control/src/lib.rs:43-90`).
- **Fix Direction**: Reserve capacity/priority for module control responses (process relay completions inline); handle Ping/Pong inline; on genuine control overflow close the connection rather than synthesize an unrelated error.

---

### VERIFIED-SAFE (refuted risks — do not re-litigate)

#### #15: Daemon `to_error_frame` supports `code:"cancelled"` with zero body deserialization — VERIFIED
- **Confidence**: Unanimous (8/8), OK
- `RouterError::RouteError{code,message}` → `error_frame(*channel,*epoch,*corr,code,message)` builds a canonical `ErrorBody{code,message}` JSON body from the envelope alone (`router.rs:602-608, 617-633`). Supports `cancelled` / `route_backpressure` / `control_backpressure` with no body parse. **Claim holds.**

#### #16: A late/duplicate `cancelled` terminal is harmless to SDK settlement — VERIFIED
- **Confidence**: Unanimous (8/8), OK
- TS `settle` is identity-idempotent and drops orphan terminals (`client.ts:1078-1091, 1103-1110`); Rust `settle_pending` removes-then-checks, second terminal no-ops (`consumer.rs:1902-1906`); Swift ignores non-matching corrs (`Client.swift:383-409, 444-485`). **SDK state is not corrupted by a double terminal** — but this does NOT save the daemon-side credit accounting (#2/#5) or make the double-fire acceptable at the daemon.

#### #17: I6 BufReader cancel-safety — plausible, contingent on discipline
- **Confidence**: Majority, MINOR
- Preserved IF `route_frame` stays strictly sync `try_push` + snapshot load. Risk: any accidental `.await` (synthetic-error egress #13, control enqueue backpressure, aggregate-cap close) reintroduces B1/B2. Note: non-forwarding backends (Echo awaits egress `router.rs:406-417`; fallthrough `router.rs:344-349`) are omitted from the hand-off design and must be offloaded or bounded.

---

## Open-Question Rulings (consolidated)

| Q | Lean | Council ruling | Rationale |
|---|------|----------------|-----------|
| **Q1** fail-loud `route_backpressure` vs pause-set | fail-loud | **WRONG as sequenced** (6/8) | Fail-loud is architecturally cleaner (avoids HOL) but produces hard consumer errors until the SDK NotSent mapping lands (#1). Gate fail-loud behind merge-0, or use a per-route pause-set to preserve semantics with zero SDK changes. |
| **Q2** daemon-synthesized cancelled vs forward-both+tombstones | yes | **RIGHT in principle, unsafe as specified** (8/8) | Correct vs adding unknown-corr DoS surface to modules, but requires the atomic per-corr state machine (#2) and corr-uniqueness (#11) first. |
| **Q3** whole channel-0 FIFO vs route.open-only | yes | **WRONG** (5/8) / contested | Whole-FIFO breaks the normative module bind-ACK → immediate-data barrier (#7) and puts Ping behind route.open (#14). Keep route-publication commits inline / add ingress fences. (Gemini/Minimax/XAI leaned RIGHT on ordering grounds but did not weigh the bind-ACK barrier.) |
| **Q4** R11 rider now vs defer | yes | **RIGHT direction, wrong mechanism** (8/8) | Fix R11 now (cheap with per-route state), but via RAII credit ownership + synchronized map + corr-uniqueness; and acknowledge it changes I7 (#8). |
| **Q5** whole-table Arc swap vs per-shard | yes | **RIGHT, conditional** (8/8) | Read-mostly justifies whole-table; require under-write-lock publication (#10) and route-churn benchmarks in T9 before rejecting sharding. |

---

## Summary Table

| # | Finding | Severity | Agreement | Members |
|---|---------|----------|-----------|---------|
| 1 | `route_backpressure`→NotSent false in all SDKs | Critical | Unanimous 8/8 | all |
| 2 | CANCEL queued→delivered limbo (loss + double-terminal) | Critical | Unanimous 8/8 | all |
| 3 | Queue data structure unspecified / mpsc-scan race | Critical | 7/8 | Opus, 5.6PRO, TerraPRO, 5.5, XAI, Minimax, Gemini |
| 4 | Drain-task error arms unspecified (leak/double-release) | Critical | Unanimous 8/8 | all |
| 5 | outstanding.insert-after-send race → credit leak | Critical | 6/8 | Opus, 5.6PRO, TerraPRO, 5.5, XAI, Gemini |
| 6 | O(queue) CANCEL scan DoS on read loop | High | Unanimous 8/8 | all |
| 7 | Whole channel-0 FIFO breaks bind-ACK→data ordering | Critical | 6/8 | 5.6PRO, TerraPRO, 5.5, Gemini, Opus, XAI |
| 8 | I3/I7 (and I4) invariant claims false | Medium | Unanimous 8/8 | all |
| 9 | GOODBYE flush vs enqueue non-atomic; teardown hang | High | 6/8 | 5.6PRO, TerraPRO, 5.5, Minimax, XAI, Gemini |
| 10 | Merge-1 snapshot NOT invariant-neutral standalone | High | 6/8 | TerraPRO, 5.5, GLM, Minimax (+2 conditional) |
| 11 | HashSet<corr> without corr-uniqueness leaks credit | High | 3/8 | 5.6PRO, TerraPRO (+implicit) |
| 12 | Frame-count caps → 256 GiB/conn memory DoS | High | 3/8 | 5.6PRO, TerraPRO, 5.5 |
| 13 | Synthetic-error egress has no reliable non-blocking lane | Med-High | 3/8 | 5.6PRO, 5.5, Gemini |
| 14 | Control-overflow policy wrong for module responses | Med-High | 2/8 | 5.5, TerraPRO |
| 15 | to_error_frame supports `cancelled` zero-parse | OK | Unanimous 8/8 | all |
| 16 | Late duplicate terminal harmless to SDKs | OK | Unanimous 8/8 | all |
| 17 | I6 BufReader cancel-safety (discipline-contingent) | Minor | Majority | several |

---

## Priority Recommendations

**BLOCKERS — must close before ANY merge (re-gate after):**
1. **SDK contract (merge-0 prerequisite):** implement code-aware `route_backpressure`/`control_backpressure` → retryable-NotSent classification in TS+Rust+Swift with in-place backoff (not reconnect), landed before merge-2. Correct Goal 5 / §5 / I8. (#1)
2. **Atomic CANCEL/delivery linearization:** replace read-loop queue-inspection with a route-local per-corr state machine; insert into `outstanding` *before* send; CANCEL decides under one lock (or via the drain task). (#2, #5)
3. **Specify the queue primitive:** synchronized `VecDeque` + `corr→node` index + `cancelled_corrs` tombstone set; O(1) CANCEL, no read-loop linear scan. (#3, #6)
4. **Normative drain-task error handling:** send-failure release, `ChannelFlowClosed` disambiguation (reload/GOODBYE/close), RAII panic guard releasing exactly-outstanding corrs, preserve shipped Error-frame recovery. Reconcile with the existing tear-down test. (#4)
5. **Preserve bind-ACK→data ordering:** keep module route-publication commits inline or add a per-connection ingress fence; do not whole-FIFO channel 0. (#7, #14)
6. **Atomic GOODBYE teardown + task lifecycle:** Open/Closing/Closed admission gate, close-receiver-before-flush, cancellation-token close→cancel→join→abort before writer wait; no strong binding↔task cycle. (#9)
7. **Byte-based memory budgets** charged pre-admission; corr-uniqueness enforcement before the `outstanding` set can be exact. (#11, #12)

**MERGE-1 (snapshot) — conditionally landable, but NOT as "invariant-neutral":**
- Publish snapshot under the write lock as the final mutation step; add per-binding `closed/generation` guard so stale `Bound` reads are inert; add read-your-writes tests (data-plane bind-then-route, control-plane hello→catalog). Only then is merge-1 safe standalone. (#10)

**DOC CORRECTIONS (before re-gate):**
- Rewrite I3/I4/I7 as intentional deltas; state the narrowed route.open happens-before contract; reserve egress for synthetic errors; fix the "trusted vs enforced" R11 framing. (#8, #13, #17)

**VERIFIED-SAFE — keep as explicit test assertions, do not re-litigate:** `to_error_frame` cancelled vocabulary (#15); late-duplicate-terminal SDK tolerance (#16).

**Note on external consumers:** broca / aft / alfonso-core are **not present in this checkout** (only `fake-aft-stub`), so their contracts could not be audited. The prompt's item #4 concern about those consumers is **unverified** and must be checked in their own repos before relying on the backpressure shift.

---

## Dismissed / Down-graded
- **"Double-terminal corrupts SDK client state"** — dismissed. All three SDKs are idempotent on second terminal (#16). The real risk is daemon-side credit accounting (#2/#5), not client state.
- **I6 BufReader cancel-safety broken** — down-graded to MINOR/contingent. Safe if the read-loop hand-off stays strictly non-awaiting; enforce via review-gate (#17).
