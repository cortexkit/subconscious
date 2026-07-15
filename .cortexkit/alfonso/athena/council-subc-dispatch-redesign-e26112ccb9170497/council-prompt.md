
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