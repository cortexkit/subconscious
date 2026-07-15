---
task_id: bg_ddf133ed
agent: council-member
session_id: ses_09b6049fdffeTJYbXQorpkBWIV
parent_session_id: ses_09b61906bffeZMC8Uc32l89Pqb
status: completed
completed_at: 2026-07-15T07:18:35Z
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
Let me start by reading the design doc and the key source files.
I'll start by reading the design doc and the key source files in parallel.

[assistant] 2026-07-15T07:13:39Z
Let me read the key source files: server.rs (connection_loop), forwarding.rs (lookup_data_route, release paths), and the SDK cancel-handling paths.
Now let me read the server connection loop, forwarding table, and find the SDK cancel-handling paths in parallel.

[assistant] 2026-07-15T07:13:57Z
Let me examine the SDK terminal-handling paths (how they handle duplicate/late terminals for a settled corr) and the Rust client's cancel handling, plus the module-side handle_cancel.

[assistant] 2026-07-15T07:14:26Z
Let me look at how the Rust consumer handles incoming terminals and whether a second terminal for a settled corr causes issues, plus the TS client's terminal handling and error classification.

[assistant] 2026-07-15T07:14:33Z
Good — `settle_pending` at consumer.rs:1902-1906 does `pending.remove(&key)` and returns early if `None`. So a second terminal for a settled corr is a no-op in the Rust SDK. Let me verify the TS and Swift clients similarly, and check the error classification / NotSent mapping.

[assistant] 2026-07-15T07:14:48Z
Let me look at the TS client's `classifyFailure` and how it handles error frames received from the daemon (the terminal path), plus the Swift client's terminal/error handling.

[assistant] 2026-07-15T07:14:57Z
Let me look at the TS client's `dispatch` method to see how it handles incoming error/response/streamend frames and whether a second terminal for a settled corr is a no-op.

[assistant] 2026-07-15T07:15:05Z
Now let me examine the forwarding table's release path and the GOODBYE handling to verify the "release paths untouched" claim, plus the bind-commit path. Let me also check the control handler's route_goodbye and the release_module_route.

[assistant] 2026-07-15T07:15:08Z
Let me read the release paths and the flow control (Semaphore) to verify the credit accounting and the "release paths untouched" claim.

[assistant] 2026-07-15T07:15:11Z
Now let me look at the `ChannelFlow` (Semaphore-based credit) to understand the close/release semantics, and the control handler's route_goodbye path.

[assistant] 2026-07-15T07:15:15Z
Let me read the full ChannelFlow implementation (acquire/release/close/in_flight) and the control handler's emit_route_goodbyes to understand the release path fully.

[assistant] 2026-07-15T07:15:17Z
Now I have a strong understanding of the shipped behavior. Let me verify a few more critical details: the bind-commit path (where the drain task would be spawned), the Swift client's terminal handling, and check whether any in-repo consumer relies on implicit backpressure (broca/aft/alfonso-core).

[assistant] 2026-07-15T07:15:23Z
Let me check the Swift client's terminal/error handling and the Rust consumer's `managed_call_deadline_bounds_flow_control_wait` (which may rely on implicit backpressure), plus the `commit_route_locked` to understand the bind-commit path.

[assistant] 2026-07-15T07:15:28Z
Let me look at the Swift client's frame dispatch/terminal handling and the `commit_route_locked` function, plus check the Rust consumer's call path for how it handles the send (does it block on a bounded channel or rely on daemon backpressure).

[assistant] 2026-07-15T07:15:33Z
The Swift client is a module-side provider (it reads frames and handles requests), not a client-side consumer with pending corr tracking. Let me verify the Swift client's terminal handling for duplicate terminals and check the `commit_route_locked` function. Let me also look at how the Rust consumer sends requests (does it use a bounded channel that could block, relying on daemon backpressure).

[assistant] 2026-07-15T07:15:37Z
Let me find the `commit_route_locked` function and look at how the Rust consumer's writer path works (does it block/await on a bounded channel, relying on daemon-side backpressure to bound in-flight).

[assistant] 2026-07-15T07:15:39Z
Let me read the `commit_route_locked` and the Rust consumer's writer path (how it sends frames — does it block on a bounded channel).

[assistant] 2026-07-15T07:15:43Z
Let me look at the Rust consumer's writer task to understand whether it blocks (bounded channel) and relies on daemon backpressure, and the `send_outbound` / writer batching path.

[assistant] 2026-07-15T07:15:47Z
Let me find the Rust consumer's writer channel size and how `call()` enqueues to the writer (does it block/await, relying on daemon backpressure to bound in-flight).

[assistant] 2026-07-15T07:15:53Z
Let me check the `EGRESS_BUFFER` constant and the `call()` send path (line 1624) to see if it blocks (await) on the writer channel, which would mean the consumer relies on bounded writer capacity + daemon backpressure.

[assistant] 2026-07-15T07:15:56Z
This is critical. The Rust consumer's `call()` does `writer.send(WriteCommand{...})` at consumer.rs:1624 which is an `mpsc::Sender::send` — this **awaits** (blocks) when the writer channel (capacity `EGRESS_BUFFER=128`, consumer.rs:46) is full. The writer task drains this channel and writes to the socket. When the daemon's read loop blocks (today's behavior), the socket fills, the writer task blocks on `write_frame`/`flush`, the writer channel fills, and `call()` blocks — bounding in-flight requests implicitly via TCP backpressure.

Under the redesign, the daemon read loop never blocks, so the socket never backpressures, the writer task never blocks, the writer channel (128) drains fast, and `call()` returns quickly. The consumer can now issue up to 128 requests into the writer channel + whatever the socket buffer holds, all of which the daemon will eagerly enqueue into per-route dispatch queues (up to queue depth). The consumer's implicit in-flight bound (blocking on writer channel) is weakened. Let me verify the TS client's send path similarly.

[assistant] 2026-07-15T07:16:25Z
The TS client uses `this.sock.write()` which awaits `writeTracked().completed` — this awaits the socket write. Under today's daemon (blocking read loop), TCP backpressure propagates: when the daemon stops reading, the socket buffer fills, `sock.write` blocks, and `send()` blocks, bounding in-flight. Under the redesign, the daemon reads eagerly, so no TCP backpressure, `sock.write` completes fast, and the TS client can over-issue. The TS client has no per-route in-flight semaphore on the client side (it relies on the daemon's credit window + blocking). Let me verify there's no client-side semaphore. Let me also check the `isConsumerReconnectTransient` and error classification for `route_backpressure`.

[assistant] 2026-07-15T07:16:31Z
The TS client has NO client-side per-route semaphore — it relies entirely on the daemon's credit window + TCP backpressure to bound in-flight. The Rust consumer also relies on `writer.send().await` (bounded channel of 128) + daemon backpressure. Let me now verify the `isConsumerReconnectTransient` — it treats `not_sent` as transient (retryable). So `route_backpressure` → if classified as `not_sent` → retryable. But the TS client's `errorFromFrame` (client.ts:1116-1126) creates a `SubcError` with the code, and `dispatch` (client.ts:1057-1058) settles the pending with `this.errorFromFrame(frame)` via `rejectPending` → `classifyFailure`. But `classifyFailure` (client.ts:781-792) checks `handedToSocket` — if the frame WAS handed to socket (which it was, since the daemon read it), it returns `outcomeUnknownCallError`, NOT `notSentCallError`. This is a critical mismatch. Let me verify this carefully.

[assistant] 2026-07-15T07:16:35Z
This is a critical finding. The TS client's `sendManaged` sets `handedToSocket = write.queued` (client.ts:807). When the daemon reads the frame and synthesizes `route_backpressure`, the frame WAS handed to the socket. The `dispatch` method (client.ts:1057-1058) calls `this.settle(key, pending, () => pending.reject(this.errorFromFrame(frame)))`. The `rejectPending` uses `classifyFailure`, which at line 782 checks `if (!handedToSocket)` — but `handedToSocket` is `true`, so it falls through to line 791: `outcomeUnknownCallError`. 

So a `route_backpressure` error frame from the daemon is classified as `outcome_unknown`, NOT `not_sent`. But the design doc (3.5, line 157) claims: "SDK classification: retryable-in-place, maps to the existing NotSent contract (the request never reached the module)." This is WRONG for the TS client — it maps to `outcome_unknown`, which is a different contract (the request may have reached the module, outcome unknown). Let me verify the Rust consumer's classification too.

[assistant] 2026-07-15T07:16:40Z
Let me find how the Rust consumer handles incoming Error frames (terminal path) and whether it classifies them as NotSent or Module error.

[assistant] 2026-07-15T07:16:49Z
Let me read the `dispatch_frame` function to see how the Rust consumer handles incoming Error frames and classifies them.

[assistant] 2026-07-15T07:16:52Z
Critical finding for the Rust consumer. At consumer.rs:2877-2883, an incoming `Error` frame is parsed as `ErrorBody` and settled as `PendingTerminal::Error { body }`. Then `settle_pending` (consumer.rs:1902) calls `entry.settle_terminal(terminal)`, and `settle_terminal` (consumer.rs:2628-2641) for a `PendingTerminal::Error` produces `Err(CallError::Module(body))` — NOT `CallError::NotSent`. 

So in the Rust consumer, a daemon-synthesized `route_backpressure` Error frame is classified as `CallError::Module(ErrorBody{code:"route_backpressure", ...})` — an application-level module rejection, NOT `NotSent`. The design doc claims it "maps to the existing NotSent contract" — this is FALSE for both the TS and Rust SDKs. Let me verify the `isConsumerReconnectTransient` for the TS side and whether `outcome_unknown` is retryable, and check the Rust `CallError::Module` retryability.

[assistant] 2026-07-15T07:16:56Z
Now I have enough to confirm the backpressure contract shift. Let me verify one more thing — the `is_terminal_frame` check and whether `route_backpressure` / `cancelled` Error frames release credit on the module→client path. The daemon-synthesized `cancelled` terminal is sent to the CLIENT, not from the module. So it doesn't go through the module→client terminal path (router.rs:281-310). Let me verify the exact path for daemon-synthesized terminals and whether they correctly avoid the credit release path. Let me also check the `server.rs:172-178` "intentionally keeping inbound dispatch serial" comment.

[assistant] 2026-07-15T07:17:00Z
I now have enough evidence to write a thorough audit. Let me do a couple final checks: the `escalate_client_delivery_failure` path (to understand connection-close vs drain-task shutdown ordering) and verify whether the `outstanding` set insert/remove pairing has a gap for the GOODBYE-flush case.

[assistant] 2026-07-15T07:17:18Z
I now have comprehensive evidence. Let me compile the audit findings. Let me do one final check on the `read_inner` / `write_inner` lock structure to confirm the RwLock is process-wide (single lock for all connections/routes).

[assistant] 2026-07-15T07:17:21Z
Confirmed: single `RwLock<ForwardingInner>` at forwarding.rs:284, process-wide. Now I have all the evidence needed. Let me compile the full audit.

<COUNCIL_MEMBER_RESPONSE>

## Finding 1: `route_backpressure` is classified as `outcome_unknown` / `Module` error, NOT `NotSent` — the design's core backpressure contract claim is FALSE

- **Severity**: BLOCKER
- **Location**: Design doc 3.5 line 157 ("SDK classification: retryable-in-place, maps to the existing NotSent contract"); TS client `clients/subc-client/src/client.ts:781-792` (`classifyFailure`), `client.ts:807` (`handedToSocket = write.queued`), `client.ts:1057-1058` (dispatch Error → `rejectPending` → `classifyFailure`); Rust consumer `crates/subc-client-rs/src/consumer.rs:2877-2883` (Error frame → `PendingTerminal::Error`), `consumer.rs:2628-2641` (`settle_terminal` → `CallError::Module`)
- **Confidence**: high
- **Issue**: The design claims a daemon-synthesized `Error{code:"route_backpressure"}` "maps to the existing NotSent contract (the request never reached the module)." This is false in BOTH shipped SDKs:
  - **TS client**: `sendManaged` sets `handedToSocket = write.queued` (client.ts:807). The daemon DID read the frame (it classified and synthesized the error), so `write.queued` is `true`. When the `Error` frame arrives, `dispatch` (client.ts:1057-1058) calls `rejectPending` → `classifyFailure`. At client.ts:782, `if (!handedToSocket)` is FALSE, so it falls through to client.ts:791: `outcomeUnknownCallError("connection dropped before the managed call returned a response")`. The consumer gets `SubcCallError{kind:"outcome_unknown"}`, NOT `not_sent`. `outcome_unknown` means "the request may have reached the module, outcome unknown" — the OPPOSITE of the design's claim that it "never reached the module."
  - **Rust consumer**: `dispatch_frame` (consumer.rs:2877-2883) parses the Error body and calls `settle_pending(key, PendingTerminal::Error{body})`. `settle_terminal` (consumer.rs:2636) produces `Err(CallError::Module(body))` — an application-level module rejection, NOT `CallError::NotSent`. A consumer that retries `CallError::Module` would retry an application error; one that doesn't would treat `route_backpressure` as a permanent module rejection.
- **Evidence**: The `classifyFailure` guard at client.ts:782 keys on `handedToSocket`, which is `true` for any frame the daemon read. The Rust path at consumer.rs:2877-2883 unconditionally maps Error frames to `PendingTerminal::Error` → `CallError::Module`. Neither path inspects the error `code` to distinguish daemon-synthesized backpressure from module-originated errors.
- **Suggested Fix**: The design MUST add code-aware classification in both SDKs: `route_backpressure` (and `cancelled` when daemon-synthesized) must be recognized by `code` and classified as `NotSent`/`not_sent` regardless of `handedToSocket`. This is an SDK change, contradicting design  Goal 5 ("Zero SDK changes required") and  ("Nothing required"). Either (a) add the SDK classification logic and acknowledge it as a required SDK change, or (b) use a different frame type / flag that the SDKs already classify as not-sent. Without this, consumers get `outcome_unknown` for a provably-not-sent request — a correctness lie that breaks idempotent-retry safety (a consumer that retries `outcome_unknown` may double-execute if the request DID somehow reach the module, but here it provably didn't — yet the consumer is told it might have).

## Finding 2: CANCEL racing the queued→delivered boundary can produce BOTH a daemon-synthesized `cancelled` AND a module terminal for the same corr

- **Severity**: BLOCKER
- **Location**: Design 3.3 (CANCEL inspect-then-remove); shipped CANCEL path `crates/subc-core/src/router.rs:461-463` (CANCEL bypasses credit); module cancel handling `crates/subc-client-rs/src/lib.rs:979-990` (`handle_cancel` no-ops on unknown corr)
- **Confidence**: high
- **Issue**: The design says CANCEL "inspects the route's dispatch queue first." If the target Request is still queued → remove + synthesize `cancelled`. If "already delivered, or unknown" → forward CANCEL. The race window: CANCEL inspects the queue and finds the Request absent (it was JUST delivered to the module by the drain task between the inspect and the remove). CANCEL forwards to the module. The module emits a `cancelled` terminal. This is the "delivered case" and is safe. BUT the reverse race: CANCEL inspects the queue, finds the Request present, begins removal — but the drain task has ALREADY popped it from the queue and is mid-`flow.acquire()` or mid-`module_sink.send()`. The CANCEL's removal succeeds (the frame was still in the queue struct), the daemon synthesizes `cancelled`, but the drain task is concurrently delivering the Request to the module. The module receives the Request, processes it, and emits a terminal. Now BOTH the daemon-synthesized `cancelled` AND the module's terminal fire for the same corr. The design claims (3.3 line 126): "the queued case has exactly the daemon's synthetic terminal; the delivered case has exactly the module's." This race breaks that claim.
- **Evidence**: The design's inspect-then-remove is not atomic with the drain task's pop-and-deliver. `mpsc::Receiver::recv()` removes from the queue; a concurrent `Vec::remove` or linear scan-and-remove on the same queue is a data race unless the queue is a synchronized structure that the drain task also checks. The design does not specify the queue as a structure supporting concurrent inspection by the read loop while the drain task owns `recv()`. A `tokio::mpsc` receiver does not expose its internal buffer for inspection.
- **Suggested Fix**: The CANCEL-vs-delivery race requires a single atomic decision point. Options: (a) a per-corr `AtomicState` (Queued/Delivered/Cancelled) checked by both the drain task (before acquire) and CANCEL (before synthesize), with CAS to resolve the race; (b) route CANCEL through the drain task's queue as a control message so the drain task makes the decision serially. The current "inspect the queue from the read loop" approach is fundamentally racy because the drain task concurrently mutates the queue.

## Finding 3: GOODBYE flush vs concurrent enqueue — late enqueue after flush can deliver to a released module

- **Severity**: BLOCKER
- **Location**: Design 3.3 line 129-130 ("Queue flush must precede binding release so no frame can enqueue after flush"); design 3.6 line 170-171 ("flush queue, stop drain task (drop queue sender; task exits when drained), then existing release path"); shipped release `crates/subc-core/src/forwarding.rs:1409-1438` (`release_client_route_locked` removes binding + `route.flow.close()`)
- **Confidence**: high
- **Issue**: The design says GOODBYE flushes the queue, then releases the binding. It claims "no frame can enqueue after flush" because "the queue-sender drop makes late enqueues fail (sender closed)." But the read loop does a snapshot lookup (3.3, lock-free) THEN enqueues. A data frame can load a snapshot that still shows the route as Bound (the snapshot was published before the GOODBYE's release), pass the lookup, and call `dispatch_queue.push()` — all BEFORE the GOODBYE flush runs. The GOODBYE then flushes the queue (dropping this frame) and releases. This is the intended "late frame drops" semantic. BUT: the flush and the enqueue race. If the enqueue wins (frame is pushed after the flush scan but before the sender is dropped), the frame sits in the queue, the drain task may deliver it to the module AFTER the binding is released. The design says "flush must precede binding release" but does not establish that the flush is atomic with respect to concurrent enqueues. A `try_push` after flush but before sender-drop succeeds.
- **Evidence**: The design's ordering claim ("flush must precede binding release so no frame can enqueue after flush") conflates two operations: (1) flushing existing queued frames, and (2) preventing future enqueues. These are separate unless the sender is dropped BEFORE the flush scan, but the design says "drop queue sender; task exits when drained" — the sender drop and the flush are not specified as atomic. The snapshot lookup is lock-free (3.8), so a reader can hold a stale `Arc<RouteBinding>` with a live queue sender even after the write side has begun release.
- **Suggested Fix**: The release path must drop the queue sender (closing the queue) BEFORE flushing, and the flush must drain remaining frames. Any `try_push` after sender-close returns `SendError`, which the read loop maps to the existing channel-gone drop. The design must specify this ordering explicitly: close sender → drain remaining → release binding. The current "flush then release" wording is insufficient.

## Finding 4: Credit leak on connection death with Request queued but not delivered

- **Severity**: MAJOR
- **Location**: Design 3.6 line 175 ("Connection close: existing teardown already releases all routes; that now also tears down all drain tasks"); shipped credit `crates/subc-core/src/forwarding.rs:1692-1700` (`acquire` does `permit.forget()`), `forwarding.rs:1737-1739` (`close` closes semaphore), `forwarding.rs:1424` (`release_client_route_locked` calls `route.flow.close()`)
- **Confidence**: high
- **Issue**: For a Request that is QUEUED but not yet delivered (drain task hasn't acquired credit): no credit was acquired (acquire happens in the drain task, 3.2). On connection death, the route is released, `flow.close()` is called. The queued frame is dropped. No credit was acquired, so no release is needed — this is correct. BUT for a Request that is DELIVERED to the module (drain task acquired credit via `permit.forget()` at forwarding.rs:1698) but NOT YET terminated (module hasn't sent a terminal): credit IS acquired. On connection death, the route is released and `flow.close()` closes the semaphore. The `outstanding` set still has the corr. The module may later emit a terminal, which hits the module→client path (router.rs:281-310). But the route is released — `lookup_data_route` returns Absent/EpochMismatch, so the terminal is dropped (router.rs:227-236 or 238-244). The `route.flow.release()` at router.rs:307-309 is NEVER called because the route lookup failed before reaching it. The credit is leaked — but the semaphore is closed, so the leak is bounded (the route is gone). This is the SAME as shipped behavior (connection death mid-flight always leaked credit until the route was garbage-collected). The `outstanding` set doesn't change this. So this is NOT a new defect, but the design's I2 claim ("release exactly-once per terminal") is misleading for the connection-death case — the terminal is dropped, release never fires, and the `outstanding` set leaks the corr entry until the route structure is dropped.
- **Evidence**: `acquire` at forwarding.rs:1698 does `permit.forget()` — the permit is not held RAII, so only an explicit `release()` returns credit. On connection death, the route binding is removed (forwarding.rs:1420-1423), `flow.close()` closes the semaphore (forwarding.rs:1424). A late module terminal for the released route hits `DataRoute::Module(DataRouteState::EpochMismatch|Absent)` at router.rs:227-244 and returns early — `route.flow.release()` at router.rs:308 is never reached.
- **Suggested Fix**: Acknowledge that connection/module death mid-flight does not release credit for in-flight requests (same as today). The `outstanding` set should be dropped with the route structure (no leak beyond route lifetime). The design's I2 should be qualified: "release exactly-once per terminal THAT REACHES THE RELEASE PATH; connection/module death drops the terminal and the credit, bounded by route lifetime."

## Finding 5: I3 claim "release paths untouched" is FALSE — the `outstanding` set gates release, changing the module→client path

- **Severity**: MAJOR
- **Location**: Design  I3 line 219 ("Epoch-fenced release + escalation semantics byte-identical (release paths untouched)"); design 3.7 line 182-184 ("release fires only if `outstanding.remove(corr)` returned true"); shipped release path `crates/subc-core/src/router.rs:281-310` (module→client terminal → `route.flow.release()` at 307-309)
- **Confidence**: high
- **Issue**: The design claims I3: "release paths untouched." But 3.7 adds an `outstanding: HashSet<corr>` gate: "release fires only if `outstanding.remove(corr)` returned true." This DIRECTLY modifies the release path at router.rs:307-309. Today, `is_terminal_frame(frame.header.ty)` at router.rs:281 gates release, and `route.flow.release()` at router.rs:308 fires unconditionally for any terminal. Under the redesign, release fires only if `outstanding.remove(corr)` returns true. This changes behavior for: (a) duplicate terminals (R11 — intended, good); (b) a terminal for a corr that was CANCEL-removed from `outstanding` without delivery (if the CANCEL race in Finding 2 removes a corr that was never inserted because the Request was queued, not delivered — `outstanding` never had the corr, so a later module terminal for that corr would be a no-op release, which is correct, but only if the corr was never inserted). The claim "byte-identical, release paths untouched" is false — the release path gains a conditional gate.
- **Evidence**: The shipped release path is `if is_terminal_frame(ty) { route.flow.release(); }` (router.rs:307-309) — unconditional on the terminal type. The redesign adds `if outstanding.remove(corr) { route.flow.release(); }` — conditional on the corr being in the set. These are different code paths with different semantics.
- **Suggested Fix**: Correct I3 to: "Epoch-fenced release semantics preserved; the release path gains an `outstanding`-set gate (R11 rider) that makes release exactly-once. Escalation semantics (close-on-delivery-failure) unchanged." Do not claim "untouched."

## Finding 6: DoS — O(queue) CANCEL scan on the read loop is exploitable for superlinear read-loop work

- **Severity**: MAJOR
- **Location**: Design 3.5 line 159-160 ("O(queue) scan, no await"); design 3.5 line 149 (queue depths: Serial=4, ModuleManaged=64, StatelessParallel=2048); design 3.5 line 162 (per-connection aggregate cap 4096)
- **Confidence**: high
- **Issue**: CANCEL inspects the dispatch queue with an O(queue) scan on the READ LOOP. An adversarial client can: (1) fill a StatelessParallel route's queue to depth 2048 (legitimately — within the per-route cap), (2) spray CANCEL frames, each forcing an O(2048) scan of the queue on the latency-critical read loop. Worst case per malicious CANCEL frame: O(2048) comparisons. With the per-connection aggregate cap of 4096 frames across all routes, a client with 2 StatelessParallel routes at depth 2048 each can force O(2048) work per CANCEL. If the client sends N CANCELs, total read-loop work is O(N × 2048). At 4096 aggregate frames, the client can also fill the aggregate and trigger a protocol-error close, but BEFORE the close, each CANCEL does O(2048) work. This is superlinear in queue depth and runs on the read loop, blocking all other frames on that connection.
- **Evidence**: The design explicitly states "O(queue) scan, no await" on the read loop (3.5 line 159). Queue depth for StatelessParallel is 2048 (3.5 line 149). The scan is per-CANCEL, and CANCELs are never dropped for capacity (3.5 line 158).
- **Suggested Fix**: Use an O(1) lookup structure for CANCEL target resolution: a per-route `HashMap<corr, QueuePosition>` or an `AtomicState` per corr (Finding 2's fix also needs this). Alternatively, route CANCEL through the drain task (which already owns the queue) as a control message, removing the read-loop scan entirely. The O(queue) scan on the read loop is a DoS vector.

## Finding 7: Per-route Request FIFO may break across the queued→delivered boundary under CANCEL removal

- **Severity**: MAJOR
- **Location**: Design 3.3 (CANCEL removes a queued Request from the middle of the FIFO queue); design  I1 line 215 ("queue is FIFO per route; no reordering within a route's Requests")
- **Confidence**: medium
- **Issue**: The design claims I1: "queue is FIFO per route; no reordering within a route's Requests." CANCEL removes a queued Request from the queue. If the queue is a FIFO (mpsc channel), removal from the middle is not a standard mpsc operation — it requires either (a) a different data structure (VecDeque with linear scan + remove, which is O(queue) and breaks the mpsc abstraction), or (b) a tombstone marker that the drain task skips. If (a), the removal itself is O(queue) (same DoS as Finding 6). If (b), the drain task must check each popped frame against a tombstone set, adding per-frame overhead. The design does not specify the queue data structure or how mid-FIFO removal preserves ordering for the remaining frames. If a Request B is queued behind Request A, and CANCEL(A) removes A, B must still be delivered in order — but the removal mechanism is unspecified.
- **Evidence**: The design says "remove it from the queue" (3.3 line 118) but does not specify the queue type. `tokio::mpsc` does not support mid-queue removal. A `VecDeque` with linear remove is O(queue). The FIFO preservation claim depends on the unspecified data structure.
- **Suggested Fix**: Specify the queue data structure. A `VecDeque<Frame>` with a concurrent `HashSet<corr>` tombstone set (drain task skips tombstoned corrs) preserves FIFO for non-cancelled frames and makes CANCEL O(1) (insert into tombstone set). The drain task checks each frame's corr against the tombstone set before acquire. This also fixes Finding 2's race (the drain task makes the cancel decision serially).

## Finding 8: Merge-1 (snapshot forwarding) is NOT invariant-neutral — ArcSwap stale-read can observe a route Bound after release, enqueuing into a shutting-down drain task

- **Severity**: MAJOR
- **Location**: Design  line 267-269 ("merge-1 snapshot-published forwarding (R1) — read path mechanical, invariant-neutral"); design 3.8 line 205-211 ("a data frame that loads a snapshot published before its route's release can still enqueue into a queue that is being flushed")
- **Confidence**: medium
- **Issue**: The design claims merge-1 (snapshot forwarding alone, WITHOUT dispatch queues) is "invariant-neutral." But merge-1 changes the read-side from `RwLock::read()` (forwarding.rs:846 `read_inner()`) to `ArcSwap::load()`. Under the old RwLock, a reader that acquired the read lock AFTER a write-side release would see the released state (the write lock serialized mutations). Under ArcSwap, a reader can load a snapshot published BEFORE the release, then the release happens, then the reader uses the stale snapshot. In merge-1 (no dispatch queues), the read loop still awaits routing ( line 268: "read path mechanical"). The stale snapshot means a data frame can see a route as Bound after it was released, and the routing path (router.rs:335-342 `handle_bound`) will acquire credit and send to the module — but the module sink may be closed (module disconnected) or the route is gone. Today, the RwLock read lock serializes: if the release acquired the write lock first, the reader sees Absent. Under ArcSwap, the reader sees the pre-release snapshot. This is a NEW observable state: a frame routed to a route that was already released, where today the RwLock ordering would have shown Absent. The design acknowledges this for merge-2 (3.8 line 206) but claims merge-1 is "invariant-neutral" — it is NOT, because the stale-read window exists in merge-1 too, and the routing path still acquires credit + sends to a possibly-dead module sink.
- **Evidence**: `lookup_data_route` at forwarding.rs:846 takes `read_inner()` (RwLock read). Under ArcSwap, this becomes `snapshot.load()`. The RwLock provides read-after-write serialization; ArcSwap provides eventual consistency with a stale window. A reader loading a pre-release snapshot sees `Bound` where the RwLock would have shown `Absent` if the release committed first.
- **Suggested Fix**: Merge-1 is NOT invariant-neutral. The stale-read window maps to today's pre-commit (Absent drop) and post-release (channel-gone drop) windows ONLY IF the routing path handles a stale `Bound` route gracefully (credit acquire on a closed semaphore returns `ChannelFlowClosed`, module sink send fails). Verify these paths are safe under stale snapshots BEFORE claiming invariant-neutrality. The design should state: "merge-1 introduces a stale-read window where a reader sees Bound for a released route; this is safe because acquire on a closed flow returns ChannelFlowClosed (router.rs:465) and module_sink.send fails (router.rs:491), both mapped to existing error paths." Prove this, don't assert it.

## Finding 9: Q1 lean (fail-loud `route_backpressure`) is RIGHT but the NotSent mapping is unimplemented — Q2 lean (daemon-synthesized `cancelled`) is RIGHT but the race is unresolved

- **Severity**: MAJOR
- **Location**: Design  Q1 (line 274-277), Q2 (line 278-280)
- **Confidence**: high
- **Issue**: Q1 lean (fail-loud) is the correct choice over per-route pause-set (which reintroduces HOL). BUT the claim that it "maps to NotSent" is unimplemented (Finding 1). Q2 lean (daemon-synthesized `cancelled`) is correct over forward-both+SDK-tombstones (which adds unknown-corr state to modules). BUT the CANCEL-vs-delivery race (Finding 2) means the daemon-synthesized `cancelled` can fire alongside a module terminal. The design must resolve the race before Q2's lean is sound.
- **Evidence**: Finding 1 (NotSent mapping false), Finding 2 (race produces double terminal).
- **Suggested Fix**: Q1: RIGHT, but add SDK classification for `route_backpressure` code → `NotSent`. Q2: RIGHT, but fix the race (Finding 2) so exactly-one terminal is guaranteed.

## Finding 10: Q3 lean (whole channel-0 FIFO) is RIGHT; Q4 lean (R11 rider now) is RIGHT; Q5 lean (whole-table Arc swap) is acceptable

- **Severity**: OK
- **Location**: Design  Q3 (line 281), Q4 (line 282), Q5 (line 283-284)
- **Confidence**: high
- **Issue**: 
  - Q3: Whole channel-0 FIFO is correct. route.open→route.close ordering on the same connection must not reorder, and a single FIFO task preserves this. Offloading only route.open would still leave route.close inline, which could reorder relative to a delayed route.open. RIGHT.
  - Q4: R11 rider now is correct — the `outstanding` set is cheap and the per-route structure is already being built. Deferring would leave the double-release defect open. RIGHT.
  - Q5: Whole-table Arc swap is acceptable IF mutations are rare (the design asserts binds/releases are low-frequency). Per-shard adds complexity for a hot-path that is unlikely to be hot. RIGHT, contingent on T9 perf evidence.
- **Evidence**: Control FIFO ordering requirement (design 3.4 line 140); R11 defect (design 3.7 line 180-181); mutation frequency (design 3.8 line 201).
- **Suggested Fix**: None — these leans are correct.

## Finding 11: Late daemon-synthesized `cancelled` terminal is harmless in all three SDK clients (verified)

- **Severity**: OK
- **Location**: Rust consumer `crates/subc-client-rs/src/consumer.rs:1902-1906` (`settle_pending` removes pending, returns early if None); TS client `clients/subc-client/src/client.ts:1103-1110` (`settle` checks object identity, no-ops if pending already removed); Swift client `clients/subc-client-swift/Sources/SubcClient/Client.swift` (module-side provider, no client-side pending corr tracking — it's a provider, not a consumer)
- **Confidence**: high
- **Issue**: If the daemon synthesizes `cancelled` for a corr that was already settled (race in Finding 2), does any SDK mishandle a second terminal? 
  - Rust: `settle_pending` does `pending.remove(&key)` and returns early if `None` (consumer.rs:1903-1906). Second terminal is a no-op. SAFE.
  - TS: `settle` checks `this.pending.get(key) !== pending` (client.ts:1104) — if the pending was already removed, `get` returns `undefined`, which `!== pending`, so `settle` returns false (no-op). Second terminal is a no-op. SAFE.
  - Swift: The Swift client is a module-side provider (Client.swift reads ingress frames for a module, not a consumer with pending corr tracking). It has no client-side pending map to double-settle. N/A.
- **Evidence**: consumer.rs:1902-1906, client.ts:1103-1110.
- **Suggested Fix**: None — the SDKs are robust to duplicate terminals. BUT this means Finding 2's race does not corrupt SDK state — it only produces a confusing double-terminal (daemon `cancelled` + module terminal) where the second is silently dropped. The credit accounting is the real risk (Finding 2), not SDK state corruption.

## Finding 12: `RouterError::to_error_frame` supports `code:"cancelled"` with zero body deserialization — verified

- **Severity**: OK
- **Location**: `crates/subc-core/src/router.rs:582-633` (`to_error_frame` → `error_frame`); `router.rs:602-608` (`RouteError{code,message}` variant → `error_frame(*channel, *epoch, *corr, code, message)`)
- **Confidence**: high
- **Issue**: The design claims (3.3 line 120,  I5 line 222-223) that `RouterError::to_error_frame` can emit `code:"cancelled"` with zero body deserialization, using only channel/epoch/corr from the envelope. Verified: `RouteError{channel, epoch, corr, code, message}` at router.rs:602-608 calls `error_frame(*channel, *epoch, *corr, code, message)` at router.rs:617-633, which builds an `ErrorBody{code, message}` JSON body and a `Frame::build(Error, ...)` — no body deserialization, envelope-only. The daemon can synthesize `RouterError::route_error_with_epoch(channel, epoch, corr, "cancelled", "...")` and call `to_error_frame()`. CORRECT.
- **Evidence**: router.rs:602-608, router.rs:617-633.
- **Suggested Fix**: None — the claim is verified.

## Summary

**Findings by severity:**
- BLOCKER: 3 (Finding 1: route_backpressure NotSent mapping false; Finding 2: CANCEL-vs-delivery double-terminal race; Finding 3: GOODBYE flush vs concurrent enqueue)
- MAJOR: 5 (Finding 4: credit leak on connection death — same as today, but I2 claim misleading; Finding 5: I3 "release paths untouched" is false; Finding 6: DoS O(queue) CANCEL scan; Finding 7: FIFO breakage under CANCEL removal; Finding 8: merge-1 not invariant-neutral)
- MAJOR (Q leans): 1 (Finding 9: Q1/Q2 leans right but unimplemented/unresolved)
- OK: 3 (Finding 10: Q3/Q4/Q5 leans correct; Finding 11: late cancelled terminal harmless in SDKs; Finding 12: to_error_frame verified)

**Overall risk assessment (high confidence):** The design has 3 BLOCKER-level concurrency defects that must be resolved before implementation. The most critical is Finding 1: the design's central claim that `route_backpressure` "maps to the existing NotSent contract" is FALSE in both shipped SDKs — it maps to `outcome_unknown` (TS) / `CallError::Module` (Rust), which are different contracts with different retry semantics. This is not a subtle race; it's a direct contradiction of the design's stated invariant, verifiable from source. Finding 2 (CANCEL-vs-delivery race) and Finding 3 (GOODBYE flush vs enqueue) are classic inspect-then-act races that the design hand-waves with "no new interleaves" assertions that do not hold under concurrent drain tasks. The design's rollout claim that merge-1 is "invariant-neutral" (Finding 8) is also false — ArcSwap introduces a stale-read window that the old RwLock did not, and the safety of routing into a stale-Bound route must be proven, not asserted.

**Verdict: NO-GO**

Blockers:
1. Finding 1: `route_backpressure` does NOT map to NotSent in either SDK. Add code-aware SDK classification (required SDK change, contradicting "zero SDK changes") or use a different mechanism. Without this, consumers get `outcome_unknown` for a provably-not-sent request — a correctness lie.
2. Finding 2: CANCEL-vs-delivery race can produce double terminals (daemon `cancelled` + module terminal) for the same corr. The inspect-then-remove on the read loop is not atomic with the drain task's pop-and-deliver. Use a per-corr atomic state or route CANCEL through the drain task.
3. Finding 3: GOODBYE flush vs concurrent enqueue is not atomic. Specify close-sender-before-flush ordering and prove late `try_push` fails cleanly.

</COUNCIL_MEMBER_RESPONSE>

