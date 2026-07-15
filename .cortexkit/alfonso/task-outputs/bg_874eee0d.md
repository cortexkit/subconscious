---
task_id: bg_874eee0d
agent: council-member
session_id: ses_09b6049f4ffeNEc6m31BLpHVVI
parent_session_id: ses_09b61906bffeZMC8Uc32l89Pqb
status: completed
completed_at: 2026-07-15T07:15:12Z
---

[user] 2026-07-15T07:13:36Z
## Solo Analysis Mode
You MUST do ALL exploration yourself using your available read/search tools.
- Do NOT use task or any delegation tool under any circumstances
- Do NOT delegate to explore, librarian, or any other subagent
- Do NOT spawn background tasks
- Search the codebase directly — you have full read-only access to every file
- This mode produces the most thorough analysis because you see every result firsthand


## Analysis Intent: AUDIT

You are conducting an **audit** — your goal is to find discrete issues, risks, or violations.

**Focus:**
- Search for problems, anti-patterns, security risks, correctness issues, or violations of stated requirements
- Each finding must be a distinct, actionable item with concrete evidence
- Severity determines priority: critical (blocks/breaks), high (significant risk), medium (should fix), low (nice to fix)
- For each finding, provide the specific location (reference, section, or component where it occurs)
- State your confidence: high (clear evidence), medium (likely but needs verification), low (suspicion, investigate further)
- **This is a broad sweep, not a targeted trace.**

**Analytical standards:** Support claims with concrete evidence. State confidence (high/medium/low) for key assertions. Note caveats and limitations.

**Structure your response as:**
```
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: [Title]
- **Severity**: critical/high/medium/low
- **Location**: [specific reference — e.g. component, section, endpoint, rule]
- **Confidence**: high/medium/low
- **Issue**: [what is wrong and why it matters]
- **Evidence**: [concrete reference, snippet, or observation that proves the issue]
- **Suggested Fix**: [actionable recommendation]

## Finding 2: [Title]
...

## Summary
[Total findings by severity. Overall risk assessment with confidence levels.]
</COUNCIL_MEMBER_RESPONSE>
```

## Analysis Question

You are a member of an ADVERSARIAL design-review council gating a concurrency-critical redesign of the `subc-core` daemon BEFORE any implementation. Your job is to HUNT for correctness defects, false invariant claims, and broken consumer contracts — not to praise the design. Assume the design is guilty until proven innocent. Every claim you make about SHIPPED behavior MUST cite file:line from the repo. The design is gated; a single un-refuted concurrency defect is a NO-GO.

## Repo & artifacts
Repo: `~/Work/Projects/CortexKit/subconscious` (subc-core Rust daemon). Design doc under review: `docs/subc-dispatch-redesign.md` (committed f3185c89). Read it in full first. Then verify its source claims against master.

## What subc is
A zero-deserialization loopback frame router. 21-byte envelope; the daemon never parses frame bodies. Per-route flow-control credits: Serial window=1, ModuleManaged=32, StatelessParallel=1024. Routes are epoch-fenced. Frames route client→module (Requests, credit-gated) and module→client (terminals release credit).

## Shipped behavior — VERIFIED anchors (re-verify, then extend)
- Per-connection read loop routes each frame TO COMPLETION before the next read: `crates/subc-core/src/server.rs:357-400` (`connection_loop`), routing awaited at `server.rs:370-374`.
- Routing a client Request awaits `route.flow.acquire().await` inline: `crates/subc-core/src/router.rs:465` (`ForwardBackend::handle_bound`), then awaits `route.module_sink.send(frame).await` inline: `router.rs:491`. On send failure after acquire, credit released: `router.rs:494-496`.
- Channel-0 control runs inline in the read path: `router.rs:207-218` → `handle_control_frame(ctx, frame).await` at `router.rs:214`.
- Module→client terminal path is non-blocking `try_send` + `route.flow.release()`: `router.rs:281-310` (release at `router.rs:307-309`, gated by `is_terminal_frame` at `router.rs:281`/`501-506`).
- Every data-frame lookup takes one process-wide `std::sync::RwLock` via `read_inner()`: `crates/subc-core/src/forwarding.rs:846` (`lookup_data_route`).
- `RouterError::to_error_frame` — the `RouteError{code,message}` variant emits a canonical JSON ErrorBody with an ARBITRARY code and needs only channel/epoch/corr from the envelope (no body parse): `router.rs:582-633`. This is the vocabulary the daemon-synthesized `cancelled` / `route_backpressure` / `control_backpressure` terminals would reuse.
- Consequence (verified): a saturated route blocks the whole connection INCLUDING the CANCEL that would relieve it (CANCEL sits unread behind a blocked `acquire`) — cancellation is structurally defeated on saturated serial routes.

## The redesign (summary — read the doc for the authoritative version)
1. Read loop never awaits route work: read, classify, hand off (sync, non-blocking).
2. Per-route bounded dispatch queue + one drain task each owning credit-acquire + module-send (moves B1 `flow.acquire` + B2 `module_sink.send` off the read loop).
3. CANCEL inspects the route's dispatch queue: still-queued Request → remove it + DAEMON synthesizes terminal `Error{code:"cancelled"}` for that corr (no credit was acquired, so no release; module never sees it). Already-delivered/unknown → forward CANCEL as today (module emits the cancelled terminal, which releases credit).
4. GOODBYE for a route: flush queue (drop queued frames), then today's epoch-fenced release + relay. Flush must precede binding release.
5. Per-connection FIFO control task (channel-0 offload) — a slow route.open stalls only later control commands, never data/CANCEL.
6. Queue overflow (Request) = fail-loud retryable `Error{route_backpressure}` for that corr (maps to SDK NotSent). Non-Request frames (CANCEL/GOODBYE) are never dropped for capacity — they inspect/flush the queue on the read loop (O(queue) scan, no await). Per-connection aggregate cap 4096 frames → protocol-error close.
7. Per-route `outstanding: HashSet<corr>` (insert on delivery to module, remove-once on terminal); release fires only if `outstanding.remove(corr)` returned true — makes credit release exactly-once, retiring the R11 duplicate-terminal double-release defect.
8. ArcSwap snapshot-published forwarding table: data-plane reads lock-free; mutations stay serialized under the existing write lock, then publish a fresh snapshot.
9. Two-merge rollout: merge-1 snapshot forwarding (claimed invariant-neutral standalone); merge-2 dispatch queues + control offload + R11 rider.

## YOUR HUNT (work each independently, cite source, then give a verdict)

1. MISSED INTERLEAVES. Enumerate concurrency windows and for each: prove safe against a specific existing semantic, or flag it. Cover at minimum: bind-commit vs snapshot-publish vs first-frame arrival; GOODBYE flush vs a concurrent enqueue into the same route queue; CANCEL racing the queued→delivered boundary in BOTH orders (CANCEL wins / Request-delivery wins); ANY window where BOTH the daemon-synthesized `cancelled` terminal AND a module terminal fire for the SAME corr (prove impossible or exhibit it); drain-task panic/leak; connection-close vs drain-task shutdown ordering; late enqueue after the queue sender is dropped/closed.

2. CREDIT ACCOUNTING — exactly-once acquire AND release across ALL exit paths: (a) delivered + module terminal; (b) queued + CANCEL-synthesized-cancelled; (c) queue flushed on GOODBYE; (d) connection death mid-flight (Request in queue, Request delivered-not-terminated); (e) module death mid-flight; (f) drain-task panic. For EACH, state whether a credit was acquired and whether it is released exactly once, and check the `outstanding` set insert/remove pairing. Find any leak or double-release.

3. SECTION 4 INVARIANTS — attack each claim against source. Especially: I3 (epoch-fenced release + escalation semantics "byte-identical, release paths untouched") — is that TRUE given the drain task now owns acquire and the outstanding-set now gates release? I6 (BufReader cancel-safety preserved) — does the new sync hand-off truly keep the "only cancel is connection close" property? I7 (module→client direction unchanged) — does the outstanding-set gate change this path's behavior on duplicate/late terminals?

4. BACKPRESSURE CONTRACT SHIFT. Previously-blocking sends (socket TCP backpressure) become fast retryable `route_backpressure` errors. Does any shipped SDK/consumer contract break? Look at the TS/Rust/Swift SDK error classifiers and retry logic, and the daemon-consuming patterns (broca, aft, alfonso-core if present in-repo). Is the NotSent mapping HONEST in every overflow case — i.e., is it guaranteed the Request never reached the module when `route_backpressure` is synthesized? Could a consumer that relied on implicit backpressure (bounded in-flight via blocking) now over-issue and get surprised?

5. ORDERING. Per-route Request FIFO across the queue — preserved? Per-connection control FIFO sufficiency: is route.open→route.close ordering actually safe when control is a single FIFO task but data frames are on separate per-route drain tasks (can a data frame for a route be processed before that route's route.open control command commits, or after its route.close)? Any CROSS-route or control-vs-data ordering the OLD single serial loop implicitly provided that a consumer could depend on, which the new multi-task design breaks?

6. DAEMON-SYNTHESIZED `cancelled` TERMINAL. Verify `RouterError::to_error_frame` (router.rs:582-633) genuinely supports emitting `code:"cancelled"` with zero body deserialization and correct channel/epoch/corr. Then: is a LATE `Error{cancelled}` harmless against the ACTUAL cancel-handling paths in all three SDK clients (find them)? Specifically, if the daemon synthesizes `cancelled` for a queued Request, but a race delivered it, could the module ALSO emit a terminal → does any SDK mishandle a second terminal for a settled corr?

7. SNAPSHOT STALE-READ WINDOWS. Enumerate every window where a data-frame reader loads a stale ArcSwap snapshot. For each, map it to an ALREADY-existing semantic (pre-commit → Absent drop; post-release → channel-gone drop). Flag ANY new observable state the old RwLock-serialized lookup could not produce (e.g., a reader seeing a route Bound that the write side already released, enqueueing into a queue whose drain task is mid-shutdown).

8. DoS MATH. Queue depth max(4, 2×window) (Serial=4, ModuleManaged=64, StatelessParallel=2048); per-connection aggregate cap 4096; O(queue) CANCEL scan on the READ LOOP per CANCEL. Can an adversarial client make read-loop work superlinear or expensive — e.g., fill queues to max then spray CANCELs to force repeated O(queue) scans, all on the latency-critical read loop? Quantify worst-case read-loop work per malicious frame.

9. ROLLOUT. Is merge-1 (snapshot-published forwarding) TRULY invariant-neutral as a standalone landing, given the read loop still awaits routing at that point? Does moving lookups from RwLock-read to ArcSwap-load, WITHOUT the dispatch queues, introduce any window (e.g. a lookup seeing a snapshot slightly staler than a same-thread control mutation would have)?

10. OPEN QUESTIONS Q1–Q5 (doc §8). For each lean, say whether it is RIGHT or WRONG and why. Q1 fail-loud route_backpressure vs per-route pause-set; Q2 daemon-synthesized cancelled vs forward-both+SDK tombstones; Q3 whole channel-0 FIFO vs route.open-only offload; Q4 R11 rider now vs defer; Q5 whole-table Arc swap vs per-shard.

## OUTPUT FORMAT
Produce PER-ISSUE findings with: a severity (BLOCKER / MAJOR / MINOR / OK), the specific interleave or contract, a source citation (file:line) for any shipped-behavior claim, and a concrete fix if it is a defect. Then a one-line member verdict: GO / GO-WITH-CHANGES (list the specific changes) / NO-GO (list the blockers). Be concrete and adversarial. Do NOT restate the design back; spend your tokens on finding what breaks. If you cannot verify a source claim, say so explicitly rather than assuming.

[assistant] 2026-07-15T07:13:37Z
The user wants me to conduct an adversarial design review of a concurrency-critical redesign for the subc-core daemon. I need to:

1. Read the design doc `docs/subc-dispatch-redesign.md` in full
2. Verify claims against the codebase
3. Hunt for defects across 10 specific areas
4. Output in COUNCIL_MEMBER_RESPONSE format with findings

Let me start by reading the design doc and the key source files mentioned.
Conducting an adversarial audit: reading the design doc and verifying shipped behavior in the codebase.

[assistant] 2026-07-15T07:13:40Z
Good progress. I need to continue exploring:
- Client-side routing (ForwardBackend, CANCEL handling)
- Flow control acquire/release
- SDK error handling, cancel, NotSent
- Control route.open ordering
- R11 double release
- Module handle_cancel

[assistant] 2026-07-15T07:13:42Z
I need more detail on SDK error classification, cancel handling, flow control, bind/commit paths, handle_route_goodbye, and consumer patterns for broca/aft.

[assistant] 2026-07-15T07:13:44Z
I need more details on:
- How TS/Rust consumer handles terminal Error frames and duplicate terminals
- control.rs handle_route_goodbye, bind commit
- Swift client error handling
- flow control acquire/release
- broca/alfonso patterns
- server.rs:172-178 serial comment

[assistant] 2026-07-15T07:13:46Z
Let me read more about:
1. flow control acquire/release in forwarding.rs
2. control handle_route_goodbye and bind commit
3. consumer pending terminal handling for duplicate terminals
4. Swift client error handling
5. broca/alfonso patterns

[assistant] 2026-07-15T07:13:49Z
Let me read consumer pending terminal handling, TS client resolve pending, wire spec on late terminals, route open control path, broca drop guard, and Swift client

[assistant] 2026-07-15T07:13:55Z
Need consumer pending terminal handling - settle, late responses, cancel handling in client

[assistant] 2026-07-15T07:13:58Z
Let me read settle_pending in consumer.rs and client dispatch for late terminals

[assistant] 2026-07-15T07:14:01Z
I need more on:
1. Client cancel handling on TS side for routed cancel
2. Swift client
3. What happens when duplicate error arrives
4. commit_route_locked spawn queue
5. reserved vs bound state for route.open ordering
6. broca/alfonso patterns for implicit backpressure
7. read_frame and BufReader

[assistant] 2026-07-15T07:14:03Z
Let me read client.ts settle and errorFromFrame, and check how managed call handles cancelled code from daemon.

Also critical: GOODBYE flush timing - doc says read loop does GOODBYE with queue flush, but client GOODBYE path is handle_route_goodbye in router.rs:336-340 which is sync release today - redesign must change this.

CANCEL on client path - today goes through handle_bound for Cancel? Let me check - FrameType::Cancel would go to handle_bound without acquire (acquired_credit only for Request at router.rs:463-464).

For CANCEL race with delivery - need queue structure with mutex on read loop for inspect+dequeue vs drain dequeue - classic race unless single-threaded ownership on read loop for cancel inspect OR atomic "in delivery" state per corr.

Design says CANCEL inspects queue on read loop O(n) - drain task also recv from queue - race:
- Drain pops Request A, about to acquire
- CANCEL scans queue, doesn't find A, forwards to module
- Module might still get A if delivery happened

Opposite:
- CANCEL finds A in queue, removes, synthesizes cancelled
- Drain already popped A - duplicate handling

The "already delivered" case forwards CANCEL - good.

The race at boundary: if drain popped but not yet inserted outstanding:
- CANCEL not in queue → forward CANCEL
- Request still in flight to module
That's correct.

If drain acquired credit and sent but outstanding not yet inserted (design says insert on delivery):
- Need to see design detail: "inserted on delivery to module" - is that after send succeeds?
- If after send: between send ok and insert, CANCEL not in queue, forwards - OK
- If CANCEL removes from queue while drain has frame: only one wins if queue is single consumer

Critical race: drain recv() removes from channel; CANCEL scans linked structure - must be same deque with coordination. If mpsc channel, CANCEL can't scan in-flight item in drain task's hands - goes to forward path - module may see request AND cancel - OK

If Request still in mpsc buffer, can CANCEL remove from mpsc? Standard mpsc can't remove arbitrary items - DESIGN ASSUMES inspectable queue structure (not plain mpsc without scan-remove). Doc says "remove it from the queue" - needs a custom queue (VecDeque + notify) with sync inspect on read loop while drain awaits - race: drain holding frame between recv and acquire - CANCEL must check "in_delivery" set too OR accept forward-both.

BLOCKER if queue is mpsc only.

I'll search design for queue implementation details - doc says "dispatch_queue" bounded FIFO - implementation unspecified. The O(queue) scan implies deque or similar shared between read loop and drain.

Interleave: read loop scans+removes vs drain recv - needs mutex on route queue state.

GOODBYE flush vs concurrent enqueue: read loop enqueues Request; same read loop processes GOODBYE? Single read loop per connection - sequential frames. But GOODBYE could be processed while another connection... per route: if same connection sends GOODBYE then can't interleave with enqueue from same task unless async handoff - handoff is sync try_push so GOODBYE frame read after try_push - if GOODBYE read, flush queue - but drain might have popped already.

Actually same read loop: order preserved per connection socket. B arrives, enqueued. GOODBYE arrives, flush - removes B from queue if still there. Good.

Concurrent enqueue from SAME connection can't happen - single read loop.

Late enqueue after sender dropped: try_push fails → channel-gone semantics - need to define error path.

Bind vs snapshot: commit_route_locked under write lock then publish snapshot - if data frame uses snapshot after publish but before client receives route.open response - could data arrive before route.open ack on client? Client shouldn't send until ack - but malicious client could - today same with RwLock after commit.

Control vs data ordering: route.open on control task; data on drain. Client awaits route.open before data (SDK). Malicious client: data before bind commit → Absent/Unknown - today can drop or unknown_channel.

After bind commit snapshot published, data frame can be processed on drain before route.open response sent on control egress - client might not have handle yet but daemon accepts - pre-existing?

route.open commits binding then sends response - if data processed first on module side before client knows channel - module could get request - client might not have corr registered - separate issue.

cross-route ordering never guaranteed - OK.

I3 epoch-fenced release - release still on module→client path with outstanding gate - escalation paths in forwarding.rs - need verify release paths untouched - acquire moved to drain changes WHEN credit consumed but still once per delivered request.

GOODBYE flush without releasing credits for queued requests - BLOCKER: queued Requests never acquired credit - OK. Delivered not terminated - leak on GOODBYE flush? Doc says drop queued frames - if Request was delivered, not in queue - credit still held until terminal or teardown.

Connection death mid-flight: teardown releases routes flow.close() at release_client_route_locked:1424 - acquire waiting in drain? ChannelFlowClosed - need release acquired credits for in-flight.

Let me check cleanup_connection and flow.close behavior for in-flight acquire.

[assistant] 2026-07-15T07:14:13Z
Key findings for late terminals:
- TS client.ts:1083-1090 drops terminals with no waiter (logs debug)
- TS settle is identity-guarded - second terminal dropped at dispatch
- Rust consumer settle_pending removes entry first - second terminal ignored (lines 1902-1906)

For route_backpressure:
- Design says maps to NotSent
- Shipped: daemon Error frames go through Module() in Rust (line 579) and errorFromFrame in TS - NOT automatically NotSent
- Only unknown_channel has special retry in managed call path
- **MAJOR**: route_backpressure will be classified as terminal Module error unless SDK updated - contradicts design §5 "nothing required" and §3.5 NotSent mapping

For client-side flow control:
- Rust consumer has per-route sem.acquire_owned before send_request (consumer.rs:525-536) - this limits client-side in-flight parallel to window size
- TS client doesn't seem to have same semaphore - can pipeline unlimited requests to socket if daemon doesn't block read loop
- After redesign, TS could flood queues until route_backpressure

Read managed call path in TS for retries on errors

[assistant] 2026-07-15T07:14:16Z
Let me read the cancel test and managed call error handling for cancelled

[assistant] 2026-07-15T07:14:19Z
Key findings for SDK:

1. **route_backpressure** NOT in isRetryableRouteOpenCode - only route.open codes. For managed `call()`, daemon Error frames become `SubcCallError` with kind "terminal" via managedRequest - need to verify.

2. **unknown_channel** gets special retry in call() - but route_backpressure would NOT get that unless added.

3. **NotSent** semantics: TS managed call only retries `not_sent` and `unknown_channel`. Daemon Error = terminal = NOT auto-retry for route_backpressure!

4. Design doc says "retryable-in-place, maps to NotSent" but shipped code classifies Error frames as Module/terminal NOT not_sent!

**MAJOR CONTRACT BUG**: Design claims route_backpressure maps to NotSent and SDK classifies retryable - but:
- Rust: `CallError::Module(body)` for all Error terminals except unknown_channel special case (consumer.rs:579)
- TS: Error frame → terminal SubcCallError (via managedRequest path)

Need read managedRequest

[assistant] 2026-07-15T07:14:23Z
managedRequest catches SubcCallError and rethrows - but sendManaged rejects with SubcCallError from classifyFailure, not from error frames directly. I need sendManaged's error path for Error frames.

sendManaged uses classifyFailure on write errors - but terminal errors come through resolve/reject on the promise. Let me read sendManaged completion path.

[assistant] 2026-07-15T07:14:25Z
So for managed TS client:
- Error frame rejects with SubcError via errorFromFrame
- rejectPending uses classifyFailure only when there's classifyFailure on pending - managed has classifyFailure
- But wait - reject with errorFromFrame - does it use classifyFailure? rejectPending uses pending.classifyFailure?.(err) ?? err
- So Error frame gives SubcError, not SubcCallError - managed call catch checks SubcCallError only for retry logic
- SubcError from Error frame - would NOT be SubcCallError - would throw terminalCallError wrapping?

Actually line 1058: pending.reject(this.errorFromFrame(frame)) - rejects with SubcError
managedRequest catch: if SubcCallError throw; else terminalCallError
So daemon Error frames become SubcCallError terminal via terminalCallError? errorCode(cause) would get code from SubcError

Actually terminalCallError wraps non-SubcCallError into SubcCallError terminal

So cancelled, route_backpressure from daemon → SubcError → wrapped as terminal SubcCallError - NO auto retry for not_sent kind

Design says route_backpressure maps to NotSent - BUT shipped SDK does NOT classify error frame codes to not_sent - only socket queue boundary!

This is a MAJOR finding: design section 5 says "nothing required" and route_backpressure joins retryable set - but I found isRetryableRouteOpenCode doesn't include route_backpressure, and Error frames become terminal/module not not_sent.

For Rust consumer: CallError::Module(body) for Error terminals in call() - not retried except unknown_channel special case.

So route_backpressure would be CallError::Module - application error, NOT NotSent - managed call won't retry in place.

BLOCKER for consumer contract unless SDK changes are actually required contrary to design claim.

Also Swift - need quick check

GOODBYE flush: design says CANCEL/GOODBYE on read loop - but data path GOODBYE for client goes to handle_route_goodbye which is sync release today - redesigned needs flush on route queue first. If GOODBYE only in read loop without going through queue, parallel request enqueue could happen - spec says flush before release under teardown path in drain task - contradiction if GOODBYE handled only on read loop?

Design 3.3: GOODBYE flush on route teardown - 3.6 release path flush queue stop drain. Client GOODBYE today router.rs:336-340 calls handle_route_goodbye without awaiting forward path for queued requests.

CANCEL on client bound path: goes through handle_bound today for Cancel frame type - no special case, acquired_credit false, send to module.

I3 epoch-fenced release: release paths call flow.close() at release_client_route_locked:1424 - drain task design adds outstanding set on module->client only - epoch release still removes binding and closes flow - acquire moves to drain - if connection dies with queued requests, credits never acquired - OK; delivered without terminal - credit leak unless flush releases? GOODBYE flush drops queued without acquire - delivered in flight - module must terminal or leak - same as today

Connection death: cleanup releases routes, flow.close() - drain tasks must exit; queued Requests never acquired - OK; in-flight at module - credit still held until terminal or... module keeps going - same as today

Drain panic: design says abort-guard release route - must also release credits for outstanding in outstanding set and queued? If panic mid-acquire after acquire but before send - credit held, outstanding not inserted if insert after send - design says insert on delivery to module - if acquire succeeds then panic before send - LEAK

Finding: outstanding insert timing - doc says "insert on delivery to module by drain task" - if after successful module_sink.send, acquire already happened - panic after acquire before send = leak. If insert before send, duplicate terminal logic wrong.

Read doc again: "inserted on delivery to module" - typically after send succeeds. Panic between acquire and send = credit leak BLOCKER unless acquire and send are atomic w.r.t panic guard.

CANCEL queue scan vs delivery race: needs mutex on queue or single-threaded drain + read loop coordination - design doesn't specify lock. Read loop removes from queue while drain might recv same frame - classic race unless removal is synchronized with drain's dequeue. MAJOR if not specified.

CANCEL wins: removed from queue, synthetic cancelled - OK
Delivery wins: not in queue, forward cancel - OK  
Both deliver + synthetic: if CANCEL checks queue, sees not queued, forwards to module; simultaneously drain delivers Request - module runs both cancel and request - module emits cancelled - but request also executed - BAD
Or CANCEL removes, drain already took frame - can't remove - forward cancel - OK

Race: drain pops Request from queue (not yet acquire), CANCEL scans queue doesn't find it, forwards CANCEL, drain acquires and sends Request - BOTH - BLOCKER unless CANCEL also checks outstanding/drain state

Design only says "inspect dispatch queue" not in-flight between queue and module.

Finding: CANCEL-vs-delivered boundary incomplete - in-flight after dequeue but before module accept needs handling (outstanding set empty until after delivery - CANCEL would forward).

Bind vs first frame: control FIFO means route.open completes before later control; data frames can arrive after snapshot shows Bound but before client got route.open ack? Client shouldn't send until ack - SDK single-flights route.open

Data before route.open commit: snapshot Absent or Reserved - Request on Reserved gets UnknownChannel error today router.rs:322-332

Control offload: route.open in control task; data in parallel - client sends data after receiving route.open response - if data arrives before bind commit due to snapshot lag on merge-1 only - stale Absent drop vs today RwLock read after write in same process - ArcSwap might publish before client's route.open response sent? Bind commit under write lock then publish - client's route.open still in control task - order: commit publishes snapshot, then response sent - data could arrive on another task before response on wire but after snapshot - IMPROVEMENT vs race. Data before commit: snapshot without route - Absent - OK

merge-1 without queues: read loop still awaits route_for_connection - snapshot change only - lookup uses ArcSwap - mutation still under write lock - reader sees published snapshot atomically - slightly staler than holding read lock concurrent with write? Write blocks readers today; ArcSwap readers never block - writer publishes after commit - frame arriving during write still awaits routing in merge-1 so still serial per connection - OK for merge-1

Per-connection cap 4096: enforcement unspecified on increment/decrement

DoS: N routes * cancel each O(depth) - serial route depth 4 cheap; stateless parallel 2048 * many cancels; aggregate 4096 cap limits total queued

HOL across routes: fixed by design - OK

Client-side semaphore: Rust consumer acquires client-side sem before send - daemon no longer blocks - client can pile more in daemon queue until route_backpressure - bounded by queue depth per route - honest NotSent for overflow if SDK classifies correctly - SDK issue above

Rust Module vs NotSent for route_backpressure: CallError::Module - callers may not retry

TS managed: terminal kind - no reconnect retry for not_sent path unless code matches - cancelled is terminal

Late duplicate terminal: outstanding set fixes credit; SDK drops late per wire spec 408, client.ts:1083-1090

Synthetic cancelled then module terminal: second dropped at SDK - OK for credit with outstanding

I6 BufReader: try_push on queues - if try_push fails for non-request... cancel doesn't enqueue. Good.

I7 module→client unchanged except outstanding gating release - duplicate forward still, credit inert - OK

Q1-Q5 leans: I'll note Q1 lean wrong for faithful old semantics but OK if SDK fixed; Q2 right; Q3 whole FIFO right; Q4 R11 now right; Q5 whole table right unless T9 says otherwise

Drain-task GOODBYE: who runs flush? 3.6 says release path - client GOODBYE on read loop must trigger flush synchronously before release_client_route - must not delegate only to drain task if read loop handles GOODBYE

Control queue vs data: route.close ordering - if route.close is control and must happen before data on new route - data uses committed snapshot - OK

Find R5 test references

I'll also verify Reserved route request behavior with error frame

Double release on send failure: today router.rs:494-496 releases if acquire then send fails - drain task needs same

backend_error from acquire failure when not draining - today returns error frame to client? handle_bound returns Err which propagates - connection_loop sends to_error_frame for routable errors

When acquire fails in drain, need synthetic error to client not block - design doesn't detail non-queue acquire failure path

ChannelFlowClosed on release route.flow.close() - acquire fails - should error client - OK

GOODBYE flush vs concurrent enqueue: read loop enqueues Requests; GOODBYY handled on read loop - if GOODBYE runs flush then release drops sender, concurrent enqueue on same connection serial on read loop - can't enqueue after GOODBYE frame processed if GOODBYE comes after prior frames read - same serial read order - BUT drain task still accepts until sender dropped - frames already enqueued before GOODBYE processed remain - flush must drop those - design says flush queue - OK. Frame read AFTER GOODBYE in same loop iteration order - if client sends Request then GOODBYE in one TCP chunk, order preserved. If GOODBYE first then Request on new epoch - different route

Snapshot stale: post-release enqueue - sender closed, try_enqueue fail - maps channel-gone - need spec for error vs drop

Write the response.
Checking how the TS client rejects Error frames in managed sends and whether `cancelled` is treated specially:

[assistant] 2026-07-15T07:14:40Z
The forwarding test at 3080 shows same-channel out-of-order completion is EXPECTED when B has 0ms delay and A has 300ms - B arrives first. So cross-corr ordering of responses was never FIFO by completion time.

Key TS managed send: when Error terminal arrives, reject with SubcError from errorFromFrame, NOT SubcCallError with terminal kind. So call() catches SubcCallError only - cancelled would be... SubcError not SubcCallError, so terminalCallError wraps it? Actually catch at 421 checks SubcCallError - SubcError would throw terminalCallError from 741? managedRequest catches non-SubcCallError and wraps terminal.

Actually sendManaged reject uses pending.reject with classifyFailure OR errorFromFrame - errorFromFrame returns SubcError, not SubcCallError. So managed call gets SubcError rejection... Promise reject with SubcError. managedRequest catches and if not SubcCallError throws terminalCallError.

So cancelled error from daemon/module → SubcError at reject → terminal SubcCallError wrapper. NOT not_sent retry.

For route_backpressure - same path - terminal unless we add special handling. Design says SDK should classify route_backpressure as retryable NotSent - **NOT IMPLEMENTED** in TS or RS today. **MAJOR contract gap** vs design claim "maps to NotSent".

Rust: Error terminals → CallError::Module(body), NOT NotSent. So route_backpressure would be Module error, not retryable not_sent unless they add special case like unknown_channel.

This is a clear BLOCKER or MAJOR: design §5 says nothing required but §3.5 says maps to NotSent contract - contradicts §5. Actual SDKs treat daemon Error frames as Module/terminal except unknown_channel retry once.

I'll also note: connection cleanup releases routes via flow.close() but does NOT synthesize terminals for in-flight requests - client sees connection drop → outcome_unknown. Queued requests at daemon - design should flush on connection close - doc says "Connection close: existing teardown already releases all routes; that now also tears down all drain tasks" - but credit for queued undelivered - no acquire, OK. For in-flight delivered - module may still complete - credit release on terminal - OK.

Drain panic without guard: credit leak + wedge.

GOODBYE flush on read loop vs enqueue race: if not under write lock together, concurrent route_frame can enqueue after flush started but before sender dropped - need mutex on route or serialize flush with enqueue.

CANCEL O(n) on read loop: with max per-route 2048 for StatelessParallel and many routes, aggregate 4096 - worst case many routes × CANCEL each scanning full queue = if attacker opens many routes with depth 4 each and sends cancel for each corr... Actually CANCEL scans ONE route's queue per frame, O(depth) max 2048. Per CANCEL frame O(2048). Spray 4096 cancels on same route after filling - 4096 * 2048 = 8M iterations on read loop without await - CPU DoS. **MAJOR**

Cross-route ordering: old loop was strictly serial per connection - all frames processed in socket order. New design: data frames for route X can be processed by drain while route Y drain runs concurrently - responses can interleave on egress. Was that guaranteed in order on wire before? Module→client try_send order follows read loop order on module connection, not client read order. Client→module was serial so request order to module was socket order globally per connection. **NEW**: two routes on same client connection - requests could be read in order A_X, B_Y but drain tasks send to modules concurrently - module sees interleaved delivery per connection... Actually each route has own module channel - order per route preserved in queue. Cross-route socket order: client sends req on ch5 then ch7 - read loop enqueues both quickly - two drains - order of module delivery across channels not defined before either (separate channels). **Within same route** FIFO preserved.

Control vs data: route.open completes in control task publishing bind+snapshot. Data frame read before route.open ack on client - client shouldn't send until open returns. Adversarial client could pipeline data before open response processed - snapshot might show Reserved/Absent - existing semantics.

Staged merge-1: read loop still awaits route_for_connection - only lookup changes to ArcSwap. Stale snapshot: between write lock commit and publish, readers see old snapshot without route - Absent. Between release in write lock and publish, readers might see old Bound - **could forward to route still in canonical state being torn down** - today RwLock read sees committed state; with ArcSwap if publish happens AFTER mutation atomically at end of write lock, readers either see pre or post. If publish is after full commit same as lock release, OK. Window if publish delayed: write lock released but snapshot not yet updated - another writer? Mutations serialized on write lock - only one writer. Reader loads snapshot without lock - between mutation complete and swap, old snapshot shows bound - **same as today** if we consider rwlock read after write released sees new state; arcswap if swap after write section ends, identical. If swap BEFORE maps updated internally but swap points to old arc - no. Lean: merge-1 OK if swap is last step under write lock.

I3 epoch-fenced release: release paths still call release_client_route and flow.close(); drain task owns acquire - release still on module path with outstanding set - escalation paths unchanged structurally but **in_flight at release** must include queued+acquired states - on release flow.close() blocks new acquires; drain must exit and flush - if queue has frames with acquired credit? only after delivery - flush drops queued without acquire - good. Delivered not terminated - credit leak until module terminal or connection death - **same as today**.

I6: route_frame try_push only - if try_push sync, OK. But aggregate cap check, snapshot load, queue inspect for overflow path - all sync.

Late enqueue after sender closed: route_backpressure or channel-gone - design says fail - need explicit handling.

Double terminal race: outstanding.remove gates second release - first terminal releases credit, second inert - client: TS settle once, second dropped at 1083 - **OK**

CANCEL wins vs delivery wins: doc says mutex on queue needed - not specified - **BLOCKER** without formal queue op (remove-if-present vs pop front atomically).

Module death: drain await send fails - need release acquired credit - today router.rs 494-496 on send fail - must be in drain task - design says same - OK if implemented.

Swift client - similar patterns likely - brief mention if no managed retry for route_backpressure.

Verdict: NO-GO or GO-WITH-CHANGES with blockers:
1. SDK route_backpressure/cancelled classification mismatch for managed retry
2. CANCEL/delivery queue race unspecified
3. GOODBYE flush vs enqueue serialization unspecified  
4. Drain panic backstop must be normative not handwave
5. O(queue) cancel DoS mitigation missing (cap scans, shard locks, or bound cancel rate)
6. Connection-death queued requests - credits OK; delivered - OK
7. TS Error frame not SubcCallError kind terminal for managed - actually cancelled is application-level cancel - should not retry - OK as terminal

For route_backpressure MUST be not_sent - SDK changes REQUIRED contradicting design §5

I'll write the final response.
<COUNCIL_MEMBER_RESPONSE>

## Finding 1: `route_backpressure` / `control_backpressure` are not mapped to `NotSent` in shipped SDKs (design overclaims “zero SDK changes”)
- **Severity**: BLOCKER
- **Location**: Design 3.5,  vs `clients/subc-client/src/client.ts`, `crates/subc-client-rs/src/consumer.rs`
- **Confidence**: high
- **Issue**: The design states queue overflow synthesizes retryable `route_backpressure` and “maps to the existing NotSent contract” (doc 3.5:154–157), and  claims “Nothing required.” Shipped behavior treats inbound `Error` terminals as **application/module errors** (Rust: `CallError::Module` at `consumer.rs:579`; TS managed path rejects via `errorFromFrame` → `SubcError`, not `not_sent`, at `client.ts:1058`, `781–791`). Only `unknown_channel` gets a dedicated in-place retry (`consumer.rs:570–577`, `client.ts:427–430`). `is_retryable_route_open_code` / `isRetryableRouteOpenCode` do **not** include `route_backpressure` (`consumer.rs:3130–3134`, `client.ts:1252–1258`). Consumers that today rely on **implicit TCP/socket backpressure** (read loop blocked on `acquire`/`send` at `router.rs:465`, `491`) can pipeline more requests and get **non-retryable `Module`/`terminal` errors** instead of `NotSent`, breaking managed `call()` retry semantics documented at `client.ts:187–194` and `consumer.rs:823–826`.
- **Evidence**: Blocking acquire path `crates/subc-core/src/router.rs:463–496`; Error terminal → `CallError::Module` `crates/subc-client-rs/src/consumer.rs:579`; no `route_backpressure` in retry classifiers `consumer.rs:3130–3134`, `client.ts:1252–1258`.
- **Suggested Fix**: Either (a) **mandate SDK changes**: classify `route_backpressure` / `control_backpressure` as `NotSent` (and document `cancelled` as terminal/non-retry), with cross-client parity tests; or (b) reject fail-loud overflow and keep blocking per-route admission until SDKs land. Do not gate implementation on  as written.

## Finding 2: CANCEL vs queued→delivered boundary is a real double-effect race without a specified atomic queue primitive
- **Severity**: BLOCKER
- **Location**: Design 3.3 / 3.2; interleave hunt #1
- **Confidence**: high
- **Issue**: Rule: CANCEL removes target Request from queue **or** forwards if already delivered (`docs/subc-dispatch-redesign.md:117–126`). Drain task concurrently `recv()`s the same Request and runs `acquire` + `module_sink.send` (`doc:90–94`). Without a single atomic “dequeue-by-corr OR mark in-flight” step, **CANCEL-wins-after-pop** delivers to module while read loop also synthesizes `cancelled`, or **delivery-wins** leaves CANCEL to module for unknown corr (module no-op per doc:114 / `crates/subc-client-rs/src/lib.rs:979–989`). That revives “cancel lost, request still runs” or duplicate terminals for one corr.
- **Evidence**: Module `handle_cancel` only acts on known in-flight keys `lib.rs:979–988`; today CANCEL bypasses credit `router.rs:461–463` but is still behind read-loop routing `server.rs:370–374`.
- **Suggested Fix**: Per-route mutex or `select`-style queue API: atomically `{ remove queued corr | if in_flight set, forward cancel only }`; drain must insert into `outstanding` **before** `send` awaits (or use a `Delivering(corr)` state) so CANCEL can distinguish queued vs in-flight vs delivered.

## Finding 3: GOODBYE flush vs concurrent `route_frame` enqueue is underspecified (late enqueue after “flush”)
- **Severity**: MAJOR
- **Location**: Design 3.3 / 3.6 / 3.8 (`flush-then-release`, stale snapshot argument)
- **Confidence**: medium
- **Issue**: GOODBYE must flush queue then release (`doc:127–130`, `forwarding.rs:1414–1424` `flow.close()` today). If flush and `try_enqueue` are not under the **same** route lifecycle lock, a data frame can snapshot-load a still-`Bound` route and enqueue **after** flush began but **before** sender drop—violating “client settled locally, no further module delivery” (GOODBYE semantics via `router.rs:336–340` → `control.rs:439–457`). Doc hand-waves “sender closed → channel-gone” (`doc:206–208`) but does not define client-visible error vs silent drop for that race.
- **Evidence**: Release removes binding under write lock `forwarding.rs:1409–1424`; data enqueue is intended lock-free off snapshot (`doc:192–203`)—flush/release must be one serialized episode with enqueue gate.
- **Suggested Fix**: Teardown sequence under route mutex: `reject_enqueue` → drain/flush queue → drop sender → join drain → then `release_client_route` + snapshot publish; late `try_enqueue` returns explicit `route_backpressure` or `Absent` without touching module.

## Finding 4: Credit accounting on connection/module death and drain failure needs explicit parity with `router.rs:494–496`
- **Severity**: MAJOR
- **Location**: Credit hunt #2; `forwarding.rs:1168–1190`, `router.rs:494–496`, `forwarding.rs:1702–1730`
- **Confidence**: high
- **Issue**: Paths (a) delivered + module terminal: OK with `outstanding` rider (`doc:178–184`, release today `router.rs:307–309`). (b) queued + synthetic `cancelled`: no acquire—OK (`doc:121–122`). (c) GOODBYE flush: queued drops without acquire—OK if flush is complete. (d) connection death: `cleanup_connection` releases routes and `flow.close()` `forwarding.rs:1168–1188`, `1424`—does **not** emit terminals; credits for **delivered** requests depend on module terminals or leak until… module still may send terminal; client gone → try_send drop `router.rs:285–305`—credit still released on terminal path if frame “accepted” by try_send—OK. (e) **Drain `module_sink.send().await` failure after `acquire`**: shipped releases credit `router.rs:494–496`; design must require identical logic in drain task—**not optional**. (f) **Drain panic without abort-guard**: live binding + closed queue = permanent wedge (doc acknowledges `doc:171–174` but broca pattern is not in subc-core)—credit leak + HOL returns.
- **Evidence**: Acquire/release pairing `router.rs:463–496`; over-release guard only ignores extra releases `forwarding.rs:1705–1714` (R11 duplicate terminal), not missing release.
- **Suggested Fix**: Normative drain `Drop`/panic handler: flush, release all `outstanding` credits or force `release_client_route`; property tests for each exit path in  T3–T4,T7.

## Finding 5: I3 “release paths untouched / byte-identical escalation” is false as stated once drain owns acquire and `outstanding` gates release
- **Severity**: MAJOR
- **Location**: Design  I3 vs 3.2 / 3.7
- **Confidence**: high
- **Issue**: I3 claims epoch-fenced release + escalation semantics are “byte-identical (release paths untouched)” (`doc:219`). Acquire moves off read loop into drain (`doc:90–94`); release gains `outstanding.remove(corr)` gate (`doc:182–184`). Reload drain / `endpoint_is_draining` rejection today happens at acquire in `router.rs:465–477`; with queuing, requests can sit **queued** through reload marking without acquiring—**different** client-visible behavior vs today’s immediate `module_reloading` error at acquire time. Escalation on `try_send` failure unchanged `router.rs:286–300` but **credit release only if terminal forwarded**—duplicate terminal behavior changes (R11 rider)—intentional, not “untouched.”
- **Evidence**: `module_reloading` at acquire `router.rs:471–477`; `flow.close()` on release `forwarding.rs:1424`.
- **Suggested Fix**: Reword I3 to “epoch-fenced release map unchanged; acquire timing and duplicate-terminal credit semantics **intentionally** change with documented client impact.” Add test: reload with non-empty dispatch queue.

## Finding 6: I6 BufReader cancel-safety — plausible but only if hand-off stays strictly non-awaiting
- **Severity**: MINOR (OK if implementation discipline holds)
- **Location**: Design 3.1 / I6; `server.rs:357–368`
- **Confidence**: medium
- **Issue**: Invariant: only connection close cancels read (`doc:224–225`). Preserved if `route_frame` is sync `try_push` + snapshot load only (`doc:78–82`). Risk: accidental `.await` on egress for synthetic errors, aggregate-cap close, or control enqueue backpressure would reintroduce B1/B2 on read path.
- **Evidence**: Today full await `server.rs:370–374`.
- **Suggested Fix**: Lint/review gate: read-loop call graph must not await except `read_frame` / close; synthetic errors via `try_send` to egress only.

## Finding 7: I7 module→client “unchanged” is misleading — duplicate-terminal **credit** behavior changes (R11 rider)
- **Severity**: MINOR (documented rider, not a client wire break)
- **Location**: Design  I7 vs 3.7; `router.rs:281–309`
- **Confidence**: high
- **Issue**: Wire path still lookup + `try_send` + release (`router.rs:281–309`). **Observable daemon state** changes: second duplicate terminal no longer inflates semaphore (`forwarding.rs:1705–1714` today logs over-release; rider makes second release inert). Clients still receive both Error frames if module misbehaves; TS drops late terminal `client.ts:1083–1090`, Rust `settle_pending` removes entry once `consumer.rs:1902–1905`.
- **Evidence**: `consumer.rs:1902–1905`; `client.ts:1103–1109` single settle.
- **Suggested Fix**: Amend I7 to “wire behavior unchanged; credit accounting on duplicate terminals **fixed** (R11).”

## Finding 8: DoS — O(queue) CANCEL scan on read loop is attacker-scalable
- **Severity**: MAJOR
- **Location**: Design 3.5 (`doc:158–161`); hunt #8
- **Confidence**: high
- **Issue**: Non-Request CANCEL not enqueued; read loop scans queue O(depth) per frame (`doc:158–160`). Per-route depth up to **2048** (StatelessParallel, `doc:149–150`), aggregate **4096** (`doc:162–164`). Adversary: fill queues, spray CANCELs with arbitrary corrs → up to **4096 × 2048 ≈ 8.4M** queue steps per connection burst, all on the latency-critical read loop, while legitimate cross-channel traffic shares the same task (`server.rs:348–400`).
- **Evidence**: Serial depth 4, StatelessParallel 2048 from `doc:149–150`; single connection loop `server.rs:357–374`.
- **Suggested Fix**: Index queue by `corr` (`HashMap` + FIFO list), O(1) cancel lookup; cap CANCEL processing per read iteration; or bound work per malicious frame with protocol-error close.

## Finding 9: Ordering — per-route Request FIFO preserved; cross-route never guaranteed; control-vs-data hazard for misbehaving clients
- **Severity**: MINOR
- **Location**: Hunt #5; I1 `doc:215–216`
- **Confidence**: high
- **Issue**: Per-route FIFO follows single drain (`doc:86–95`)—OK. Old connection loop processed frames in socket order (`server.rs:357–374`); cross-route **request** order to modules was socket-serial; new design allows concurrent drains—**only matters per module channel**, and channels were already independent—OK. **Control FIFO** (`doc:134–140`) vs data: well-behaved SDKs await `route.open` before data (`consumer.rs:520–558`). Raw client sending data on reserved/Absent gets today’s drops/errors (`router.rs:322–333`, `313–320`)—unchanged. No new guarantee broken for compliant clients.
- **Evidence**: I1 cross-route disclaimer `doc:216`; `Reserved` → `unknown_channel` for Request `router.rs:322–331`.
- **Suggested Fix**: OK; add regression test for “data on reserved slot during in-flight route.open” if snapshot staleness differs from RwLock read-your-writes on control task only.

## Finding 10: Daemon-synthesized `cancelled` — `to_error_frame` OK; late duplicate terminal harmless on clients; race is daemon-side (Finding 2)
- **Severity**: MAJOR (client OK, daemon race not)
- **Location**: Hunt #6; `router.rs:582–633`, `client.ts:1057–1059`, `consumer.rs:1902–1905`
- **Confidence**: high
- **Issue**: `RouterError::RouteError` → arbitrary code JSON `router.rs:602–608`, `617–632`—supports `cancelled` without body parse—OK. Late second terminal: TS first settle wins `client.ts:1103–1109`, drops orphan `1083–1090`; Rust `settle_pending` no-ops if pending gone `consumer.rs:1903–1905`. **Managed `call()`** does not auto-retry `cancelled` (Error → not `not_sent`)—correct for cancel. **Problem** is concurrent module terminal + synthetic `cancelled` from race (Finding 2), not SDK mishandling.
- **Evidence**: `to_error_frame` `router.rs:582–633`; wire spec late drop `docs/specs/subc-wire-v1-final.md:407–408` (verified in read at 407–408).
- **Suggested Fix**: Fix queue atomicity; optionally suppress module terminal forward if synthetic already sent for corr (daemon tombstone set).

## Finding 11: Snapshot stale-read windows — mostly maps to existing semantics; new risk is enqueue-to-dying-route if teardown ordering wrong
- **Severity**: MAJOR (contingent on Finding 3 fix)
- **Location**: 3.8 `doc:204–211`; hunt #7
- **Confidence**: medium
- **Issue**: Pre-bind Absent / stale epoch—same as `lookup_data_route` `forwarding.rs:869–880`. Post-release stale Bound in snapshot until publish—reader may enqueue to queue whose drain is shutting down—doc claims sender-closed maps to channel-gone (`doc:206–208`)—**equivalent only if** enqueue gate and publish are ordered (Finding 3). RwLock today: readers block on writers during mutation `forwarding.rs:846`—ArcSwap removes reader blocking but should not widen binding lifetime if publish is atomic at end of `write_inner` mutations (`commit_route_locked` `forwarding.rs:1472–1529`).
- **Evidence**: `read_inner()` on hot path `forwarding.rs:846`; commit under write lock `1472–1529`.
- **Suggested Fix**: Publish snapshot only as final step of each forwarding mutation; include generation/teardown epoch in `RouteBinding` visible to `try_enqueue`.

## Finding 12: Rollout merge-1 — largely invariant-neutral if snapshot swap is tied to write lock; no dispatch queues yet
- **Severity**: OK
- **Location**:  `doc:266–269`; hunt #9
- **Confidence**: medium
- **Issue**: With read loop still awaiting full route (`server.rs:370–374`), merge-1 only changes lookup from `read_inner()` to `ArcSwap::load` on data plane. No new interleave if every mutation clones-and-swaps before releasing write lock—staleness bounded to “one publish behind,” same as releasing RwLock read after write. **Risk** if control-plane code reads snapshot for bind verification while data reads snapshot—must keep control on lock for read-your-writes per doc `doc:203–204`.
- **Suggested Fix**: GO for merge-1 with test: bind commit visible to data plane only after swap; control handlers still use `write_inner`/`read_inner` for catalog.

## Finding 13: Open questions Q1–Q5
- **Severity**: OK (leads mostly right; Q1 lean conflicts with SDK reality)
- **Confidence**: high
- **Q1** fail-loud `route_backpressure` (**lean yes**): **RIGHT** for avoiding HOL, but **WRONG** to ship without SDK `NotSent` mapping (Finding 1).
- **Q2** daemon-synthesized `cancelled` (**lean yes**): **RIGHT** vs forward-to-module unknown corr (`lib.rs:979–988` no-op).
- **Q3** whole channel-0 FIFO (**lean yes**): **RIGHT** for `route.open` vs `route.close` ordering (`control.rs:767–780`); partial offload reintroduces reorder risk.
- **Q4** R11 rider now (**lean yes**): **RIGHT**; cheap with `outstanding` (`forwarding.rs:1702–1714` shows need).
- **Q5** whole-table Arc swap (**lean yes**): **RIGHT** at bind/release frequency (`commit_route_locked` `forwarding.rs:1472+`); optimize only if T9 proves hot.

## Summary
| Severity | Count |
|----------|-------|
| BLOCKER  | 2 (SDK/backpressure contract; CANCEL/delivery queue race) |
| MAJOR    | 6 (GOODBYE/enqueue teardown, credit/drain panic, I3 wording/semantics, DoS scan, snapshot/teardown, synthetic cancel race) |
| MINOR    | 3 (I6 discipline, I7 clarification, ordering OK) |
| OK       | 2 (merge-1 rollout, Q2–Q5 leans) |

**Member verdict: NO-GO** until (1) SDK/classification story for `route_backpressure`/`control_backpressure` is specified and implemented or design explicitly defers fail-loud overflow, (2) per-route queue concurrency (CANCEL vs drain vs GOODBYE) is specified with atomic ops, (3) GOODBYE flush/enqueue/snapshot teardown is totally ordered, (4) drain panic/backstop and O(1) or bounded CANCEL lookup are in the design normatively—not just test plan bullets.

</COUNCIL_MEMBER_RESPONSE>

