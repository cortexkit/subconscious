## Finding 1: `route_backpressure`/`cancelled` NotSent mapping is UNIMPLEMENTED in every SDK — "zero SDK changes" is a false consumer-contract claim
- **Severity**: BLOCKER
- **Location**: doc 3.5:154-157, 230-233, Goal 5 (lines 52-54), I8:226-228 vs. all three SDK terminal classifiers
- **Confidence**: high
- **Issue**: The design's load-bearing consumer claim — `route_backpressure` "rides the existing error-classification paths," "maps to the existing NotSent contract," "Nothing required" from SDKs — is FALSE against shipped source. A synthesized `Error{route_backpressure}` terminal is classified as a **hard terminal failure**, not a retryable NotSent, in every client. Under saturation the consumer gets a thrown application error where today it gets transparent TCP backpressure.
- **Evidence**:
  - **TS**: incoming Error settles as `pending.reject(this.errorFromFrame(frame))` (client.ts:1057-1058); `errorFromFrame` returns a plain `SubcError`, not `SubcCallError` (client.ts:1116-1126). `managedRequest` wraps a non-`SubcCallError` as `terminalCallError` → kind `"terminal"` (client.ts:739-742,1155-1158). `call()` retries only `err.code === "unknown_channel"` (client.ts:427) or `kind === "not_sent"` (client.ts:435); a `terminal` error is re-thrown at client.ts:450. No path maps `route_backpressure` to retry.
  - **Rust**: Error terminal → `PendingTerminal::Error` → `CallError::Module(body)` (consumer.rs:2877-2884, 394, 579). The managed loop retries only `body.code == "unknown_channel"` (consumer.rs:570-578); everything else `return Err(CallError::Module(body))` (consumer.rs:579). Not NotSent, not retried.
  - **Swift**: no retry/classifier logic exists at all (grep for retry/notSent/backpressure = 0 matches); an Error terminal becomes a bare `SubcError` (Client.swift:671-674).
  - The ONLY retryable-code classifier is `isRetryableRouteOpenCode` / `is_retryable_route_open_code`, a CLOSED set `{unknown_module, module_reloading, target_unavailable, module_timeout}` gated to channel-0 route.open only (client.ts:1252-1259; doc-cited byte-identical to consumer.rs:3130-3135). It never touches data-plane request terminals.
- **Suggested Fix**: This is not "additive config." Add a PREREQUISITE SDK merge (merge-0) that (a) parses data-plane Error `code`, (b) adds `route_backpressure` to a data-request retryable set mapping to not_sent/retry-in-place in TS+Rust+Swift, and (c) ships before merge-2. Until then merge-2 converts transient saturation into hard consumer errors. Correct the doc: Goal 5 /  / I8 are false as written.

## Finding 2: Removal of implicit backpressure lets consumers over-issue into hard failures
- **Severity**: MAJOR
- **Location**: doc 3.5:144-157; server.rs:172-178 (shipped serial-loop backpressure)
- **Confidence**: high
- **Issue**: Today the blocking read loop is the flow-control governor: a client pipelining faster than credits free simply TCP-backpressures (memory #7022, backpressure-by-serialization). The redesign replaces this with fail-loud overflow at depth `max(4, 2×window)` — Serial=4 (doc 3.5:149), trivially overrun. Combined with Finding 1, a consumer that today relied on implicit bounded in-flight now over-issues and receives hard terminal errors instead of natural throttling.
- **Evidence**: shipped implicit bound at server.rs:177-178 ("intentionally keeping inbound dispatch serial: each routed frame is awaited before reading the next"); redesign removes it (doc 34-37) with no consumer-side rate limiter added.
- **Suggested Fix**: Either land SDK retry-with-backoff for `route_backpressure` (Finding 1) or reconsider Q1's per-route pause-set, which preserves old semantics without SDK changes.

## Finding 3: The dispatch-queue primitive is underspecified — "CANCEL inspects the queue" is incompatible with an mpsc drained by the drain task
- **Severity**: MAJOR
- **Location**: doc 3.2:89-95, 3.3:117-123, 3.5:158-160
- **Confidence**: high
- **Issue**: 3.3 requires the READ LOOP to scan/remove-by-corr from the per-route queue while 3.2's `drain_task` concurrently `recv()`s FIFO from the same queue. A tokio mpsc receiver is single-consumer and NOT scannable from the sender side — so the queue cannot be an mpsc. It must be a shared `Mutex<VecDeque>` (or equivalent), meaning every CANCEL scan and every drain `recv` contends a per-route lock. The doc's "O(queue) scan, no await" (3.5:160) hides that this scan now takes a lock shared with the drain task — a new read-loop contention point the design never accounts for, and the exact HOL-class hazard the redesign set out to kill.
- **Evidence**: 3.2:91 `queue.recv()` (mpsc semantics) vs 3.3:117 "CANCEL inspects the route's dispatch queue" and 3.5:159 "executed by the read loop against the queue structure itself" — mutually exclusive for a tokio mpsc.
- **Suggested Fix**: Specify the concurrency primitive explicitly (shared `Mutex<VecDeque>` + a `corr→node` index for O(1) removal) and prove the read-loop lock hold is bounded and non-awaiting.

## Finding 4: O(queue) CANCEL scan on the read loop is a self-inflicted DoS / HOL vector
- **Severity**: MAJOR
- **Location**: doc 3.5:158-160, 3.3:117
- **Confidence**: high
- **Issue**: Adversary opens a StatelessParallel route (queue depth 2048, 3.5:150), fills it to 2048 Requests (within the 4096 per-connection cap, 3.5:162), then sprays cheap 21-byte CANCELs for non-existent corrs. Each CANCEL forces an O(2048) scan **on the latency-critical read loop**, finds nothing, forwards to module; the loop cannot read the next frame until the scan completes. The per-connection cap bounds queue SIZE but not (scan cost × CANCEL rate).
- **Evidence**: worst-case read-loop work = O(queue_depth) per CANCEL ≈ 2048 comparisons/frame; N CANCELs ⇒ O(N·2048) read-loop ops, unbounded in N — reintroducing the cross-channel HOL the design claims to eliminate (Goal 1, lines 42-44).
- **Suggested Fix**: Back the queue with a `corr→node` index so CANCEL removal/lookup is O(1); never linear-scan on the read loop.

## Finding 5: I3 "release paths untouched / byte-identical" is self-contradicting with the 3.7 R11 rider
- **Severity**: MAJOR
- **Location**: doc I3:219, 3.2:97-101 vs 3.7:178-188; router.rs:307-309
- **Confidence**: high
- **Issue**: I3 asserts release paths are "byte-identical (release paths untouched)," but 3.7 inserts an `outstanding.remove(corr)` gate *before* `route.flow.release()` — release now fires conditionally (3.7:183 "release fires only if outstanding.remove(corr) returned true"). The shipped release site is unconditional at router.rs:307-309 (`if releases_credit { route.flow.release(); }`). The rider is by definition a change to that path. The exactly-once proof for the CANCEL case (3.3:125) *depends on* this modification, so calling it "untouched" both misstates the invariant and hides the rider's real surface.
- **Evidence**: shipped release is unconditional (router.rs:307-309); 3.7 makes it conditional on a new per-route set. Additionally the set's insert (drain task, client connection) and remove (module→client path, router.rs:307 — a DIFFERENT connection's task) run on two tasks, so "no global lock" (3.7:184) still requires a per-route lock shared across connections — unspecified.
- **Suggested Fix**: Reword I3 to "release call site gains an exactly-once gate; epoch-fence and escalation semantics unchanged." Specify the `outstanding` set's lock and prove insert-before-any-possible-terminal ordering (delivery→insert must happen-before the module's terminal can reach the remove).

## Finding 6: Drain-task panic credit accounting is unproven exactly-once
- **Severity**: MAJOR
- **Location**: doc 3.6:171-174 (panic backstop, "abort-guard mirroring broca")
- **Confidence**: medium
- **Issue**: The drain task now owns `flow.acquire().await` then `module_sink.send().await` (3.2:93). A panic/abort can occur (a) before acquire — nothing to release; (b) after acquire, before the outstanding-insert — one credit acquired, in_flight=1, MUST release and there is no corr in the set; (c) after insert. The design's guard is hand-waved as "must release the route" without specifying that it must know whether the *current* frame acquired its credit and whether it was inserted. A blind release over-releases in case (a) (masked only by the `in_flight==0` guard logging "over-release ignored," forwarding.rs:1705-1714); a non-release in case (b) leaks a credit while the binding may be recreated — the exact silent wedge 3.6 claims to kill.
- **Evidence**: acquire returns `Err(ChannelFlowClosed)` when the sem is closed (forwarding.rs:1692-1693); release no-ops on a closed sem (forwarding.rs:1723) and on in_flight==0 (forwarding.rs:1705-1714). The interleave between acquire-success and set-insert is a genuine unguarded window the doc does not close.
- **Suggested Fix**: Make acquire+insert a single critical step (insert corr into `outstanding` *before* awaiting send, immediately after acquire returns Ok) and have the abort-guard release exactly the credits whose corrs are still in `outstanding` for that route — not a blanket "release the route."

## Finding 7: New task topology narrows the shipped "route.open happens-before subsequent same-connection frames" guarantee
- **Severity**: MAJOR
- **Location**: doc 3.4:132-140, 3.8:205-209
- **Confidence**: high
- **Issue**: Today the single serial loop processes channel-0 route.open INLINE and to completion before the next read (server.rs:357-375, router.rs:214), so even a client that pipelines a data frame immediately after route.open (without awaiting the response — legal on the wire) has the bind committed before its data frame is read. Under the redesign, route.open goes to a separate control FIFO task while the data frame hits the per-route drain path; the snapshot lookup sees Absent (3.8:208-209) and the Request is dropped/errored. The design leans entirely on "SDKs single-flight route.open and await responses" (3.4:136) — true for the shipped SDKs (Rust publishes the handle before resolving the waiter, consumer.rs:2854-2867), but it is a *narrowing* of shipped wire behavior for any non-SDK/future consumer that pipelines.
- **Evidence**: shipped inline control at router.rs:207-218; SDK await-reliance acknowledged at 3.4:136. Old guarantee = unconditional same-connection ordering; new guarantee = conditional on client round-trip.
- **Suggested Fix**: State the narrowed contract explicitly in I1/3.4 ("data frames on a channel are only guaranteed routable after the client observes that channel's route.open Response"); add a regression test for pipelined-open-then-data to document the intended drop.

## Finding 8: Double-terminal for one corr — daemon vocabulary and SDK tolerance both hold (refutes the client-side risk)
- **Severity**: OK
- **Location**: doc 3.3:117-126,  hunt; router.rs:582-633; client.ts:1103-1110; consumer.rs:1902-1906
- **Confidence**: high
- **Issue/verdict**: `RouterError::to_error_frame` genuinely supports an arbitrary `code` (`cancelled`, `route_backpressure`) with envelope-only channel/epoch/corr and a canonical JSON body, zero body parse — VERIFIED at router.rs:602-608 → error_frame at router.rs:617-632. If a race causes BOTH a synthetic `cancelled` and a module terminal for one corr, both SDKs tolerate the second: TS `settle` is object-identity-idempotent and the second terminal hits "dropped terminal frame with no waiter" (client.ts:1078-1091); Rust `settle_pending` removes-then-checks and a second terminal finds no entry and returns (consumer.rs:1903-1906). Credit side stays exactly-once via the `outstanding` gate (Finding 5). This portion of I5/I7 holds.
- **Suggested Fix**: none — keep as an explicit "second terminal is harmless" test assertion (T3/T7).

## Finding 9: Merge-1 snapshot forwarding is invariant-neutral ONLY if publish is synchronous inside the mutation's write-lock section
- **Severity**: MINOR
- **Location**: doc 267, 3.8:198-211
- **Confidence**: medium
- **Issue**: At merge-1 control still runs inline (B3 not yet moved), so a route.open commit and a subsequent same-task data lookup are ordered on one task. Read-your-writes is preserved because the shipped write path commits under the write lock (release/bind mutate `client_to_module`/`module_to_client` under `write_inner`, e.g. forwarding.rs:1420-1461). If the ArcSwap publish is done synchronously in that same critical section, a same-task lookup after the commit loads a snapshot ≥ commit and sees the bind — neutral. If publish is deferred/batched, a same-task lookup could load a pre-bind snapshot and wrongly Absent-drop a Request the old RwLock would have routed — a NEW observable state.
- **Evidence**: shipped RwLock read at forwarding.rs:846; 3.8:199-201 says "apply to canonical state, then publish" — the ordering is stated but the "publish before releasing the write lock" requirement is not made explicit.
- **Suggested Fix**: Make it a hard requirement: publish the new snapshot *while still holding the write lock*, before returning from the mutation. Add a merge-1 test that binds then immediately routes on the same task.

## Finding 10: Open-question leans
- **Severity**: MINOR
- **Location**: doc 
- **Confidence**: medium
- **Q1** (fail-loud vs pause-set): lean fail-loud is **WRONG as sequenced** — given Finding 1, fail-loud produces hard consumer errors until SDK support lands; the pause-set preserves old semantics with zero SDK changes. At minimum, gate fail-loud behind the SDK merge.
- **Q2** (daemon-synthesized cancelled): lean yes is **RIGHT** (Finding 8 confirms SDK idempotency), but only once the queue primitive supports O(1) scannable removal (Finding 3/4).
- **Q3** (whole channel-0 FIFO): **RIGHT** relative to the route.open-only alternative, but neither restores the pipelined-open ordering (Finding 7) — accept with the contract note.
- **Q4** (R11 rider now): defensible on cost, but **the doc framing is wrong** — it is not "release paths untouched" (Finding 5), and the panic path is under-specified (Finding 6). Land only with those closed.
- **Q5** (whole-table Arc swap): **RIGHT** — read-mostly justifies it; per-shard only if T9 shows publish-hot.

## Summary
- **BLOCKER (1)**: `route_backpressure` NotSent mapping is unimplemented in TS, Rust, and Swift — the design's "zero SDK changes / rides existing classification" contract is false against source; merge-2 without a prerequisite SDK merge turns transient saturation into hard application errors.
- **MAJOR (6)**: over-issue regression (F2); dispatch-queue primitive underspecified & incompatible with mpsc drain (F3); O(queue) CANCEL-scan DoS/HOL on the read loop (F4); I3 "release untouched" self-contradiction + cross-task outstanding-set locking (F5); unproven drain-task-panic credit accounting (F6); narrowed route.open happens-before contract (F7).
- **MINOR (2)** / **OK (1)**: merge-1 publish-ordering requirement (F9); open-question corrections (F10); double-terminal tolerance verified safe (F8).

Overall risk: HIGH, confidence high on the blocker and on F3/F4/F5 (all cited to shipped source). Per the gate rule (a single un-refuted concurrency/contract defect is NO-GO), multiple stand un-refuted.

**MEMBER VERDICT: NO-GO** — blockers: (1) SDK `route_backpressure`→NotSent mapping does not exist in any of the three shipped clients ("zero SDK changes" false; add a prerequisite SDK merge); (2) dispatch-queue primitive underspecified and its read-loop CANCEL scan is an O(queue) DoS/HOL vector (F3+F4; require O(1) corr-indexed removal); (3) I3 "release paths untouched" is false and the drain-task-panic credit accounting is unproven exactly-once (F5+F6; specify acquire+insert atomicity and the abort-guard's per-corr release). Resolve those three and F7's contract note and this becomes GO-WITH-CHANGES.