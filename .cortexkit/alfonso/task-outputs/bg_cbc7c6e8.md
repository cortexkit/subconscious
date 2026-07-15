---
task_id: bg_cbc7c6e8
agent: council-member
session_id: ses_09b6049f9ffedPAxM2462rhfGi
parent_session_id: ses_09b61906bffeZMC8Uc32l89Pqb
status: completed
completed_at: 2026-07-15T07:21:15Z
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
**Planning codebase exploration strategy**
**Starting document and source outlining**

[assistant] 2026-07-15T07:13:47Z
**Planning deep router code review**

[assistant] 2026-07-15T07:13:57Z
**Planning control handler implementation**

[assistant] 2026-07-15T07:14:03Z
**Inspecting control flow via symbols**

[assistant] 2026-07-15T07:14:10Z
**Refining client call routing and cancellation**

[assistant] 2026-07-15T07:14:18Z
**Planning module and consumer discovery**

[assistant] 2026-07-15T07:14:29Z
**Investigating Rust send_request behavior**

[assistant] 2026-07-15T07:14:36Z
**Assessing SDK error classification differences**

[assistant] 2026-07-15T07:14:44Z
**Extracting client constants**

[assistant] 2026-07-15T07:14:50Z

[assistant] 2026-07-15T07:14:52Z
**Searching for required crates**

[assistant] 2026-07-15T07:14:57Z
**Researching subc-mcp consumer behavior**

[user] 2026-07-15T07:15:05Z
<system-reminder>
Routine context housekeeping is near: a large span of this session will be comparted soon, and ~84k tokens of tool output remain unreduced. Drop spent outputs with ctx_reduce first so the archived span is the part that matters.
oldest reclaimable: §2§ read · §3§ read · §4§ read · §5§ read.
</system-reminder>

[assistant] 2026-07-15T07:15:05Z
**Planning subc-mcp main line design**

[assistant] 2026-07-15T07:15:12Z

[assistant] 2026-07-15T07:15:16Z
**Planning per-issue severity analysis**

[assistant] 2026-07-15T07:15:31Z

[assistant] 2026-07-15T07:15:34Z
**Inspecting route_open behavior and tests**

[assistant] 2026-07-15T07:15:44Z
**Investigating route.open and reverse request ordering**

[assistant] 2026-07-15T07:15:48Z
**Planning connection cleanup and frame inspection**

[assistant] 2026-07-15T07:15:56Z

[assistant] 2026-07-15T07:15:59Z
**Reviewing frame build validation lines**

[assistant] 2026-07-15T07:16:03Z
**Identifying memory DoS risk with max frame size**
**Analyzing concurrency in route bind and snapshot publish**
**Verifying module-to-client frame sequencing**
**Identifying control and data frame ordering defect**
**Diagnosing route.bind commit race condition**
**Assessing atomicity of queue operations**
**Analyzing client-side concurrency constraints**
**Evaluating module drain and snapshot release risks**
**Identifying stale snapshot enqueue blocker**
**Identifying stale snapshot release issue**
**Analyzing CANCEL and delivery race condition**
**Designing robust cancellation state handling**
**Analyzing queue insertion timing and synchronization**
**Identifying subtle credit leak in send-insert order**
**Designing state machine for send cancellation**
**Analyzing queue handling and credit release**
**Assessing connection and module termination handling**
**Identifying discrepancies in terminal behavior and backpressure**
**Assessing error handling in call loop**

[assistant] 2026-07-15T07:17:17Z
**Identifying SDK backpressure mismatch issue**
**Analyzing request queueing and channel state handling**
**Examining route.open and channel assignment timing**
**Analyzing snapshot stale-read issues**
**Evaluating stale snapshot delivery risks**
**Planning snapshot publish with closed flag**
**Analyzing CANCEL attack complexity**
**Reviewing queue depth and concurrency limits**

[assistant] 2026-07-15T07:17:50Z

[assistant] 2026-07-15T07:17:52Z

[assistant] 2026-07-15T07:17:56Z

[assistant] 2026-07-15T07:18:01Z

[assistant] 2026-07-15T07:18:03Z
**Analyzing Rust local semaphore behavior and backpressure**

[assistant] 2026-07-15T07:18:08Z
**Planning concise per-issue findings with severity**
**Documenting cancel limbo with exact source citations**
**Analyzing cancellation timing causing lost cancels**
**Analyzing credit acquire/release race conditions**
**Evaluating task scheduling during send await**
**Assessing mutex impact on delivery insertion**
**Verifying over-release guard effectiveness**
**Confirming duplicate release credit stealing**
**Ensuring flushing before route release**
**Analyzing error handling on send failure**
**Identifying missing error frame synthesis in drain task**
**Confirming queue flush and backpressure behavior**
**Revising NotSent error contract**
**Verifying routeRequest cancellation handling**
**Evaluating stale snapshot reads**
**Assessing route_open publish order and snapshot timing**
**Analyzing stale snapshot and epoch fence issues**
**Designing control queue overflow handling for module responses**
**Assessing control queue overflow risks**
**Evaluating channel-0 control queue design flaws**
**Determining blockers for protocol design**
**Planning detailed issue findings**
**Identifying CANCEL dispatch and routing issues**
**Designing closed flag fix and snapshot handling**
**Identifying errors in drain task and credit release**
**Defining exactly-once delivery table and release rules**
**Analyzing error handling and retry logic**
**Planning control queue overflow fixes**
**Assessing RouterError and BufReader handling**
**Documenting route.open behavior and control offload issue**
**Confirming module_channel linkage in route.bind**
**Assessing synchronous processing for control FIFO**

[assistant] 2026-07-15T07:20:10Z
**Planning final issue summary structure**
**Verifying citation details for findings**
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: CANCEL can be lost after dequeue but before module delivery
- **Severity**: BLOCKER
- **Location**: Dispatch queue CANCEL semantics / queued→delivered boundary
- **Confidence**: high
- **Issue**: The design treats “not in queue” as “already delivered or unknown.” That misses the drain-task limbo where a Request has been popped from the queue but is still waiting on `flow.acquire().await` or `module_sink.send(...).await`. A CANCEL in this window is forwarded to the module before the module knows the corr, so shipped SDK modules drop it; the Request later runs anyway.
- **Evidence**: Current blocking points are exactly `flow.acquire().await` and `module_sink.send(frame).await` (`crates/subc-core/src/router.rs:465`, `crates/subc-core/src/router.rs:491`). Shipped module cancel handlers no-op for unknown corr: Rust SDK only cancels if in-flight entry exists (`crates/subc-client-rs/src/lib.rs:979-990`); TS provider only aborts an existing inflight controller (`clients/subc-client/src/provider.ts:694-696`).
- **Suggested Fix**: Add an explicit per-corr state machine: `Queued -> Dispatching(not_sent_yet) -> Delivered`. CANCEL must be able to atomically mark/cancel `Dispatching` before module send; drain then drops the Request, releases any acquired credit, and synthesizes `cancelled`.

## Finding 2: Channel-0 offload breaks route.bind response → immediate data ordering
- **Severity**: BLOCKER
- **Location**: Per-connection control FIFO task vs module data frames
- **Confidence**: high
- **Issue**: A module can send route.bind ACK on channel 0 and then immediately send a data frame on the new module route. Today, the read loop processes the ACK to completion before reading the data frame. The redesign enqueues channel-0 work and continues reading data, so the data frame can be looked up before bind commit and be dropped as Reserved/Absent.
- **Evidence**: Current read loop awaits routing before next read (`crates/subc-core/src/server.rs:357-375`), and channel-0 control runs inline (`crates/subc-core/src/router.rs:207-218`). Bind ACK completion commits the route via `complete_pending_relay` (`crates/subc-core/src/control.rs:2029-2032`) and `commit_route_locked` publishes maps before sending route.open (`crates/subc-core/src/forwarding.rs:1524-1536`). Data frames for Reserved/Absent module routes are dropped (`crates/subc-core/src/router.rs:227-245`). There is an explicit test for this old ordering: `accepted_route_publishes_route_open_before_immediate_reverse_request` (`crates/subc-core/src/router.rs:1078-1102`).
- **Suggested Fix**: Do not blindly offload all channel-0 frames. Module control responses that commit route.bind must either be processed synchronously in the read path or impose a per-connection barrier so later module data cannot overtake the commit.

## Finding 3: Snapshot stale Bound after release is a new observable state
- **Severity**: BLOCKER
- **Location**: ArcSwap snapshot forwarding / release windows
- **Confidence**: high
- **Issue**: The doc claims stale snapshots map to existing “channel gone” drops, but current `RouteBinding` Arcs are cloneable and not revocable. A reader can load an old snapshot after release and still hold a Bound route. In merge-1, that stale route can forward module→client frames; in merge-2, it can enqueue into a stale queue unless the queue has an independent closed flag.
- **Evidence**: Current lookup is serialized by one `RwLock` (`crates/subc-core/src/forwarding.rs:840-889`), so a lookup starting after release cannot see the old map. Release removes route maps and closes flow (`crates/subc-core/src/forwarding.rs:1409-1428`, `crates/subc-core/src/forwarding.rs:1440-1460`). Module→client forwarding does not check any route-closed flag; it rewrites, `try_send`s, then releases credit (`crates/subc-core/src/router.rs:281-309`).
- **Suggested Fix**: Add a per-binding atomic `closed/generation` guard checked by every data-plane admission/forward path, and close the queue receiver/state, not merely drop one sender. Snapshot publication alone is not enough.

## Finding 4: Merge-1 snapshot forwarding is not invariant-neutral
- **Severity**: BLOCKER
- **Location**: Rollout merge 1
- **Confidence**: high
- **Issue**: Landing ArcSwap before queues changes semantics by allowing post-release stale Bound reads. With the old `RwLock`, lookup visibility is synchronized with release; with ArcSwap, a reader can see a previously published table after the write side has removed the route.
- **Evidence**: Current data lookups take `read_inner()` (`crates/subc-core/src/forwarding.rs:846`), while releases take the write lock and remove the same maps (`crates/subc-core/src/forwarding.rs:614-657`, `crates/subc-core/src/forwarding.rs:1409-1470`). Current module→client path would forward any stale Bound without revalidation (`crates/subc-core/src/router.rs:281-309`).
- **Suggested Fix**: Do not land merge-1 alone unless it includes route tombstones/closed-bit validation that makes stale snapshots inert.

## Finding 5: Drain-task error paths are underspecified and would lose shipped Error-frame recovery
- **Severity**: BLOCKER
- **Location**: Drain task owning `flow.acquire` + `module_sink.send`
- **Confidence**: high
- **Issue**: Moving `acquire` and `send` off the read loop removes the caller that currently converts failures into canonical Error frames. The pseudocode has no handling for acquire-closed, module draining, writer closed, send failure, or post-acquire release.
- **Evidence**: Today `connection_loop` converts routable `RouterError`s into Error frames (`crates/subc-core/src/server.rs:377-390`). `handle_bound` maps closed acquire to `module_reloading`/backend errors (`crates/subc-core/src/router.rs:465-485`) and releases credit on send failure after acquire (`crates/subc-core/src/router.rs:491-496`).
- **Suggested Fix**: Drain task must synthesize the same Error frames itself for every failed acquire/send path and must remove outstanding/release credit exactly once on send failure.

## Finding 6: SDK NotSent/retry contract does not support `route_backpressure`
- **Severity**: BLOCKER
- **Location**: TS/Rust/Swift consumers and managed-call retry classifiers
- **Confidence**: high
- **Issue**: The design says queue overflow maps to existing NotSent/retryable behavior with zero SDK changes. Shipped clients do not do that. A daemon Error frame with `code:"route_backpressure"` is currently a terminal/module error, not NotSent.
- **Evidence**: TS defines `not_sent` narrowly as bytes never leaving the local process (`clients/subc-client/src/client.ts:184-194`); Error frames reject with `SubcError` (`clients/subc-client/src/client.ts:1057-1059`) and managed calls only retry `not_sent` or `unknown_channel` (`clients/subc-client/src/client.ts:421-450`). TS retryable set is only route.open codes (`clients/subc-client/src/client.ts:1240-1259`). Rust returns data-plane Error frames as `CallError::Module` (`crates/subc-client-rs/src/consumer.rs:570-579`), and its retryable set also only covers route.open (`crates/subc-client-rs/src/consumer.rs:3130-3134`). Swift route errors become `SubcError` with no retry classifier (`clients/subc-client-swift/Sources/SubcClient/Client.swift:475-482`, `671-673`).
- **Suggested Fix**: Update SDKs and docs before daemon change: classify daemon admission errors (`route_backpressure`, possibly `control_backpressure`) as a new “daemon_not_sent/admission_rejected” or explicitly broaden NotSent.

## Finding 7: Queue memory cap is frame-count based and permits catastrophic memory use
- **Severity**: BLOCKER
- **Location**: Bounded admission / DoS math
- **Confidence**: high
- **Issue**: A 4096-frame per-connection cap is not a safe memory bound because each frame body may be 64 MiB. Worst case is 256 GiB per connection; a single StatelessParallel route queue can hold 2048 × 64 MiB = 128 GiB.
- **Evidence**: Protocol max frame body is 64 MiB (`crates/subc-protocol/src/lib.rs:118-119`). The design proposes per-route depths up to 2048 and aggregate 4096 frames.
- **Suggested Fix**: Add byte-based queue budgets and much smaller per-route byte caps; admission must account `body.len()` before accepting.

## Finding 8: O(queue) CANCEL scans put attacker work on the read loop
- **Severity**: MAJOR
- **Location**: CANCEL queue inspection
- **Confidence**: high
- **Issue**: A client can fill a StatelessParallel route queue to 2048 entries, then spray pure-header CANCELs for missing corrs. Each 21-byte frame forces ~2048 comparisons on the latency-critical read loop.
- **Evidence**: CANCEL is a pure-header frame (`crates/subc-protocol/src/lib.rs:162-165`). The design explicitly puts O(queue) CANCEL scans on the read loop.
- **Suggested Fix**: Maintain an indexed `corr -> queue entry/state` map so CANCEL is O(1), or bound per-CANCEL scan work and fall back to state tombstones.

## Finding 9: Control queue overflow policy is wrong for module responses
- **Severity**: MAJOR
- **Location**: Channel-0 control queue overflow
- **Confidence**: medium-high
- **Issue**: Channel 0 carries not only client commands but module responses that settle daemon-originated route.bind/control RPCs. A generic `control_backpressure` Error cannot safely replace a module Response/Error; it can leave the client route.open pending until timeout or corrupt relay state.
- **Evidence**: Module channel-0 Response/Error is routed into `handle_module_relay_response` (`crates/subc-core/src/router.rs:405-412`, `crates/subc-core/src/control.rs:1879-2045`), which completes pending relays/control RPCs (`crates/subc-core/src/control.rs:2029-2032`).
- **Suggested Fix**: Reserve capacity or priority for module control responses, process relay completions inline, or close the offending connection on overflow rather than synthesize an unrelated error.

## Finding 10: I3/I7 invariant claims are false as written
- **Severity**: MAJOR
- **Location**: Design  invariants
- **Confidence**: high
- **Issue**: “Release paths untouched” and “module→client direction unchanged” are not true once queue flush/stop and per-corr `outstanding` gates are added. The change may be desirable, but the invariant claim is false and hides review surface.
- **Evidence**: Current module→client terminal path releases on every terminal after successful `try_send` (`crates/subc-core/src/router.rs:281-309`), with terminal types defined at `crates/subc-core/src/router.rs:501-506`. Current `ChannelFlow.release` is aggregate, not per corr (`crates/subc-core/src/forwarding.rs:1702-1731`), so the R11 rider necessarily changes behavior for duplicate/late terminals.
- **Suggested Fix**: Rewrite I3/I7 to state the actual changed semantics and add tests for duplicate terminal, late terminal after release, and terminal for unknown corr.

## Finding 11: Synthetic `cancelled` Error frame is mechanically supported, but only after race fixes
- **Severity**: OK with caveat
- **Location**: Daemon-synthesized terminal vocabulary / SDK duplicate handling
- **Confidence**: high
- **Issue**: The daemon can build `Error{code:"cancelled"}` without parsing request bodies, and SDKs generally tolerate late duplicate terminals. This does not save the queued→dispatching race in Finding 1.
- **Evidence**: `RouterError::RouteError` accepts arbitrary code/message and `to_error_frame` emits canonical JSON Error frames (`crates/subc-core/src/router.rs:602-608`, `617-633`); canonical `ErrorBody` is `{code,message}` (`crates/subc-protocol/src/lib.rs:121-126`). TS drops terminal frames with no waiter (`clients/subc-client/src/client.ts:1078-1091`); Rust no-ops when pending entry is absent (`crates/subc-client-rs/src/consumer.rs:1902-1906`); Swift ignores nonmatching in-flight keys in its read loops (`clients/subc-client-swift/Sources/SubcClient/Client.swift:383-388`, `474-485`).
- **Suggested Fix**: Keep synthetic cancelled, but only synthesize from an atomic queued/dispatching state that proves the module cannot also receive the Request.

## Open-question verdicts
- **Q1 route_backpressure fail-loud**: right direction, wrong as written until SDK classifiers and byte caps are changed.
- **Q2 daemon-synth cancelled**: right only with a dispatching-state fix; current queue-only test is unsafe.
- **Q3 whole channel-0 FIFO**: wrong; route.bind completions must not be overtaken by data.
- **Q4 R11 rider now**: right in principle, but it invalidates I7 and must be part of the per-corr state machine.
- **Q5 whole-table Arc swap**: wrong as a standalone merge unless stale snapshots are made inert with tombstones/closed checks.

## Summary
Severity counts: 7 BLOCKER, 3 MAJOR, 1 OK-with-caveat. Overall verdict: **NO-GO**. Required blockers to clear: fix CANCEL dispatching race, preserve route.bind/data ordering, make stale snapshots inert before merge-1, fully specify drain-task error/credit paths, update SDK backpressure contracts, and add byte-based queue caps.
</COUNCIL_MEMBER_RESPONSE>

