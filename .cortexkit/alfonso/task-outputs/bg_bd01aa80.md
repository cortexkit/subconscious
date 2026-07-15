---
task_id: bg_bd01aa80
agent: council-member
session_id: ses_09b6049f1ffe0X0cpzS54w2OS9
parent_session_id: ses_09b61906bffeZMC8Uc32l89Pqb
status: completed
completed_at: 2026-07-15T07:42:49Z
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
**Planning comprehensive audit steps**
**Preparing to read full design document**

[assistant] 2026-07-15T07:13:48Z
**Inspecting source tree and outline**

[assistant] 2026-07-15T07:13:58Z
**Planning targeted source analysis**
**Identifying critical forwarding functions**

[assistant] 2026-07-15T07:14:14Z
**Outlining source structure and clients**

[assistant] 2026-07-15T07:14:35Z
**Assessing design ambiguity and defect candidates**
**Tracing client and module data flow handling**

[assistant] 2026-07-15T07:14:53Z
**Planning SDK error handling audit**

[assistant] 2026-07-15T07:15:15Z
**Identifying synchronization insufficiencies**
**Analyzing bind commit and response ordering**
**Highlighting bind publication ordering gap**
**Identifying synchronization issue in queue flushing**
**Analyzing stale snapshot sender retention problem**
**Examining sender clone retention in stale snapshots**
**Analyzing route lookup race conditions**
**Evaluating message queue cancellation race**
**Identifying cancellation delivery race flaw**
**Identifying cancellation coordination blocker**
**Analyzing delivery and cancellation ordering**
**Assessing send ordering and credit release**
**Analyzing request queue concurrency risks**
**Identifying design hole in cancellation logic**
**Investigating drain panic credit leak risks**
**Analyzing guard route release risks**
**Evaluating panic semantics in route release**
**Investigating asynchronous cleanup limitations**
**Analyzing queue drain and flush behavior**
**Identifying send await cancellation issue**
**Planning abort on send and acquire operations**
**Planning targeted route release guard**
**Analyzing credit accounting edge cases**
**Evaluating terminal release consistency**
**Analyzing potential stale route delivery race**
**Identifying undocumented credit release behavior**
**Designing synchronized route release gating**
**Designing synchronization for terminal concurrency**
**Evaluating synchronization impact on cancellation guarantees**
**Identifying risks of lost overflow errors**
**Planning egress capacity fix**
**Analyzing egress send failure handling**
**Verifying try_send success ordering**
**Analyzing SDK backpressure retry inconsistency**
**Tracing error mapping for backpressure retry**
**Identifying missing Swift SDK retry logic**
**Investigating correlation ID reuse risks**
**Assessing correlation ID uniqueness flaw**

[user] 2026-07-15T07:18:01Z
<system-reminder>
Routine context housekeeping is near: a large span of this session will be comparted soon, and ~119k tokens of tool output remain unreduced. Drop spent outputs with ctx_reduce first so the archived span is the part that matters.
oldest reclaimable: §2§ read · §3§ read · §4§ read · §5§ read.
</system-reminder>

[assistant] 2026-07-15T07:18:02Z
**Planning context management strategy**
**Cataloging key code sections**
**Verifying endpoint origin uniqueness**
**Evaluating correlation collision risks**
**Verifying wire spec on correlation reuse**
**Exploring semantic cancellation tests**

[assistant] 2026-07-15T07:18:43Z
**Reducing context size by dropping redundant outputs**
**Planning careful incremental context reduction**

[assistant] 2026-07-15T07:18:55Z
**Planning small batch deletions**

[assistant] 2026-07-15T07:19:03Z
**Analyzing per-route queue ordering**
**Clarifying control FIFO and route close handling**
**Assessing control and data queue concurrency**
**Analyzing control and data frame ordering**
**Evaluating control-data ordering dependencies**
**Identifying unresolved cross-route ordering ambiguities**
**Evaluating GoodBye handling across routes**
**Clarifying request delivery and snapshot ordering**
**Verifying ArcSwap load atomicity and staleness**
**Assessing snapshot mutation consistency in routing**

[assistant] 2026-07-15T07:19:54Z
**Analyzing queue flush and release contradictions**
**Identifying potential race conditions in cap tracking**
**Quantifying worst-case scan amplification**
**Assessing algorithmic DoS risk and optimization**
**Analyzing concurrency and cancellation bottlenecks**
**Planning O(1) cancellation with HashMap queue**
**Verifying snapshot consistency and control mutation safety**
**Confirming snapshot atomicity and performance tradeoffs**
**Evaluating fail-loud versus pause-set approaches**
**Assessing synchronization strategy limitations**
**Evaluating control ordering and flow gating**
**Deciding on whole table atomic swap**
**Verifying SDK error handling safety**
**Confirming SDK terminal error handling**
**Analyzing cancellation error handling**
**Verifying SDK handling of duplicate terminals**
**Assessing SDK cancellation and late response handling**
**Identifying missing cancellation terminal in Rust SDK**
**Verifying SDK cancellation terminal claim**
**Identifying cancellation causing stalled flow issue**
**Planning source citation and test review**

[assistant] 2026-07-15T07:21:16Z
**Identifying cancellation handling inconsistency in TS provider**
**Investigating Swift module provider existence**
**Planning SDK client classification**

[assistant] 2026-07-15T07:21:39Z
**Identifying cancellation handling discrepancy**
**Diagnosing TS cancellation and retry issues**
**Gathering exact error handling code lines**
**Clarifying SDK change requirements and retry behavior**
**Analyzing retry scope and queue concurrency risks**
**Identifying potential cancellation loss race condition**
**Clarifying cancel frame forwarding behavior**
**Identifying cancellation forwarding design blocker**
**Analyzing cancel consistency with request queue state**
**Assessing request ordering and outstanding tracking**
**Identifying concurrency race causing credit leak**
**Tracing terminal handling and credit release flaw**
**Designing send coordination and error handling**
**Planning credit accounting on connection closure**
**Inspecting cleanup and release semantics**

[assistant] 2026-07-15T07:22:51Z
**Clarifying task drain and cancellation semantics**
**Identifying stale snapshot channel leakage**
**Confirming stale snapshot dispatch conflict**
**Designing per-route closure state machine**
**Assessing connection close and task teardown issues**
**Identifying writer hang due to retained sinks**
**Assessing task blocking and cancellation handling**
**Identifying connection close and task lifecycle issues**
**Defining panic cleanup requirements and guards**
**Refining task lifecycle and release logic**
**Analyzing egress send synchronization conflicts**
**Evaluating capacity reservation for request handling**
**Redesigning error egress handling**
**Assessing nonblocking egress limitations**
**Verifying zero body deserialization impact**
**Analyzing queue full race conditions**
**Verifying correlation uniqueness and queue safety**
**Analyzing client terminal release flow**
**Assessing release conditions with outstanding gate**
**Identifying doc exception and test needs**
**Assessing request queue handling and GoodBye flush**
**Clarifying concurrency in GoodBye and enqueue handling**
**Evaluating GoodBye flush race conditions with drain task**
**Sequencing snapshot publish before response**
**Verifying task spawning and snapshot ordering**
**Confirming snapshot locking and spawn safety**
**Assessing connection close and cancellation logic**
**Evaluating nonblocking control handoff and locking implications**
**Highlighting lock discipline for concurrent tasks**
**Assessing concurrency and release guard issues**
**Analyzing terminal collision handling**
**Identifying Denial-of-Service risk from duplicate correlations**
**Planning duplicate correlation rejection**
**Identifying HashSet route association flaw**
**Analyzing connection cleanup and panic handling gaps**
**Planning comprehensive findings coverage**
**Planning source consumer exploration**
**Searching daemon consumption patterns**

[assistant] 2026-07-15T07:25:24Z
**Searching for PendingTerminal::Error usage**
**Planning context size reduction**
**Deciding to drop outputs 13-24**

[assistant] 2026-07-15T07:25:35Z
**Investigating Control offload operations**

[assistant] 2026-07-15T07:25:53Z
**Inspecting supervisor reload phases**
**Analyzing open route order**

[assistant] 2026-07-15T07:26:02Z
**Examining complete_pending_relay implementation**

[assistant] 2026-07-15T07:26:09Z
**Planning final response structure**
**Detailing findings and summary elements**
**Assessing control versus data ordering issues**
**Analyzing response ordering constraints**
**Designing state machine for request-cancel sequencing**
**Evaluating stale snapshot post-release risks**
**Analyzing TOCTOU risk in queue handling**
**Identifying stale sender causing task liveness issue**
**Assessing flush guarantee and release ordering flaws**
**Identifying major unresolved linearization issue**
**Analyzing design ambiguities in snapshot and concurrency**
**Confirming no blockers if publication ordered**
**Verifying error frame version handling**
**Analyzing route capacity and task scaling**
**Identifying DOS risk from excessive routes**
**Identifying major DoS risk in connection queues**
**Planning route and task limiting fixes**

[assistant] 2026-07-15T07:27:24Z
**Evaluating multi_provider_route_limit test**
**Investigating max frame body length**

[assistant] 2026-07-15T07:27:40Z
**Calculating memory consumption vulnerability**
**Assessing frame memory allocation risk**

[assistant] 2026-07-15T07:28:00Z
**Investigating constant re-export in envelope module**

[assistant] 2026-07-15T07:28:08Z
**Clarifying memory cap and frame handling**
**Planning quota enforcement before frame allocation**
**Identifying potential cancellation lock contention**
**Identifying delayed connection teardown due to control queue blocking**
**Assessing control queue backpressure and Hello ordering**
**Identifying module HELLO and data ordering issue**
**Analyzing route.bind response sequencing**
**Identifying consumer contract data loss bug**
**Documenting source locations for routing flow**
**Planning causal barrier per connection**
**Designing control-data synchronization strategy**
**Verifying data ordering with sendFrame calls**
**Clarifying control and data ordering separation**
**Identifying blocker due to lost data frames**

[assistant] 2026-07-15T07:29:10Z
**Analyzing response egress blocking**
**Evaluating cancellation race conditions**
**Identifying cancellation delivery race blocker**
**Designing atomic cancel handling**
**Planning atomic request delivery and cancel sequencing**
**Revising release path assumptions**
**Assessing invariant claim inconsistencies**
**Planning credit accounting matrix and citation inclusion**
**Identifying multiple blocker and major issues**
**Consolidating race condition issues**
**Assessing egress backpressure contract coverage**
**Searching for PendingTerminal Errors**

[assistant] 2026-07-15T07:30:02Z
**Planning terminal settlement reading**

[assistant] 2026-07-15T07:30:17Z

[assistant] 2026-07-15T07:30:25Z
**Tracing error propagation in TerminalFrame**

[assistant] 2026-07-15T07:30:33Z
**Tracing unary symbol usage**

[assistant] 2026-07-15T07:30:39Z
**Searching for TerminalFrame::Error usage**

[assistant] 2026-07-15T07:30:49Z
**Tracing TerminalFrame error handling**

[assistant] 2026-07-15T07:30:58Z
**Searching for private method call usage**

[assistant] 2026-07-15T07:31:06Z
**Planning to read section 1600-1670**

[assistant] 2026-07-15T07:31:12Z
**Tracing send_unary usage**

[assistant] 2026-07-15T07:31:18Z
**Analyzing TS request concurrency and Rust semaphore limits**
**Verifying Swift client serial behavior and error handling**
**Checking TypeScript and Swift behavior**

[assistant] 2026-07-15T07:31:35Z
**Planning to review full question 8**
**Planning content addition from line 279**

[assistant] 2026-07-15T07:31:44Z
**Analyzing ChannelFlow semaphore behavior**

[assistant] 2026-07-15T07:31:52Z
**Analyzing flow.acquire permit leakage and panic risks**
**Identifying credit leak on send failure in flow handling**
**Planning async error handling for module sink send**
**Refining error handling in drain task**
**Addressing race conditions in credit transfer**
**Planning tests for module sink edge cases**
**Tracing handle_goodbye source**

[assistant] 2026-07-15T07:32:25Z
**Identifying missing commit_route_locked segment**

[assistant] 2026-07-15T07:32:35Z
**Identifying writer hang due to stale FrameSink clones**
**Analyzing route binding self-cycle causing liveness failure**
**Clarifying ownership to prevent sink retention**
**Clarifying task exit conditions and ownership**
**Analyzing ArcSwap snapshot staleness and cloning**
**Requesting lookup_data_route details**

[assistant] 2026-07-15T07:32:57Z
**Analyzing snapshot update timing**
**Identifying stale-read window effects**
**Tracing post-release error behavior**
**Assessing snapshot reader timing**
**Analyzing snapshot publication order impact**
**Evaluating timing of snapshot store and response**
**Planning atomic RouteLiveness check**
**Defining canonical snapshot order and flow close**
**Implementing linearizable snapshot mutation**
**Clarifying queue flush and linearization**
**Identifying snapshot merge invariants issue**
**Assessing atomic reservation in try_push**
**Verifying cross-route ordering guarantees**
**Assessing route independence and protocol behavior**
**Reviewing fail-loud and cancel policies**
**Refining control transition serialization**
**Verifying error frame and terminal handling**
**Confirming SDK terminal handling correctness**
**Mapping route_backpressure to NotSent error**
**Verifying absence of retry in consumer patterns**
**Assessing error handling in FrameSink sending**
**Clarifying nonblocking send behavior**
**Analyzing backpressure handling and semantic shifts**
**Identifying intentional design change in backpressure**
**Analyzing matrix credit handling**
**Identifying credit handling blockers and race conditions**
**Reviewing route module connection sequencing and state locking**
**Proposing unified phase map under mutex**
**Clarifying header rewriting timing in routing**
**Ensuring correct header rewriting on cancellation**
**Analyzing concurrency handling and cancellation impacts**
**Investigating Cancel handling in Rust module**
**Searching for dispatch_module_frame usage**

[assistant] 2026-07-15T07:35:19Z
**Planning Rust dispatch analysis**

[assistant] 2026-07-15T07:35:30Z
**Assessing cancellation handling logic**

[assistant] 2026-07-15T07:35:41Z
**Analyzing cancellation handling inconsistencies**
**Planning credit release and cancellation enforcement**
**Verifying cancellation and goodbye semantics**
**Investigating drain worker acquire error**
**Designing atomic route state management**
**Planning per-route outbound serialization**
**Identifying race condition in cancel handling**
**Identifying cancellation boundary issue**
**Planning priority lane and cancellation fixes**
**Designing pending credit cancellation map**
**Refining cancellation and FIFO handling**
**Assessing control FIFO ordering and DoS risks**
**Analyzing cancel frame DoS complexity**
**Assessing queue cancellation efficiency**
**Designing scalable queue data structure**
**Evaluating queue cap with in-flight frames**
**Analyzing send buffer and memory limits**
**Identifying expensive route map cloning risk**
**Planning sharded route map optimization**
**Assessing mutation impact on table snapshot performance**
**Evaluating queue capacity and resource attack risks**
**Planning context reduction by dropping old outputs**
**Finalizing call range reviews**
**Planning RouteBinding struct review**
**Reviewing RouteBinding fields**

[assistant] 2026-07-15T07:37:26Z
**Inspecting server error handling after sink failure**

[assistant] 2026-07-15T07:37:37Z
**Planning full document review**

[assistant] 2026-07-15T07:37:44Z
**Searching CallError::NotSent usage**
**Inspecting `subc-mcp` as consumer**

[assistant] 2026-07-15T07:37:58Z
**Emphasizing NotSent contract limitations**
**Clarifying NotSent semantics and retry logic**

[assistant] 2026-07-15T07:38:13Z
**Assessing reconnectAfterDrop behavior**

[assistant] 2026-07-15T07:38:21Z
**Identifying improper reconnection on NotSent**

[assistant] 2026-07-15T07:38:32Z
**Distinguishing backpressure from NotSent errors**
**Verifying NotSent semantics and error terminality**
**Assessing snapshot loading post-release timing**
**Clarifying snapshot states and queue behavior**
**Designing atomic snapshot publish and queue handling**
**Reviewing channel closure and error handling semantics**
**Inspecting document mapping semantics**

[assistant] 2026-07-15T07:38:59Z
**Planning route enqueue method update**
**Auditing concurrency invariants**
**Designing control queue per endpoint**
**Analyzing task lifecycle and route binding behavior**
**Identifying connection death and correlation handling issues**
**Recognizing DoS risk from duplicate correlations**
**Implementing duplicate correlation rejection**
**Assessing memory and clone performance limits**
**Planning per-issue findings format**
**Designing detailed per-issue report structure**
**Enumerating detailed blocker findings**
**Fixing cancellation linearization issue**
**Refining cancellation terminal guarantees**
**Identifying credit transfer blocker**
**Diagnosing sender-drop teardown blocker**
**Planning dispatch controller fix**
**Planning snapshot store linearization**
**Redesigning route backpressure handling**
**Identifying correlation ID reuse issue**
**Identifying resource bound bottlenecks**
**Revising false invariant claims**
**Assessing Swift client pending map**
**Verifying daemon codec and audit matrix**
**Labeling severity counts and confirming concurrency defect**
**Citing exact source and doc references**
**Mapping multi-language code regions**
**Reviewing router validation and constants**
**Clarifying error handling and router behavior**
**Documenting queue capacity and cancel code details**
**Evaluating control backpressure error handling**
**Revising error handling strategy**
**Clarifying request ordering and cancellation invariants**
**Refining I6 plausibility assessment**
**Verifying frame error handling and protocol version**
**Drafting credit acquire-release status table**
**Confirming Channel0 queue classification**
**Outlining detailed findings and severity**
**Finalizing severity labels and count**
**Assessing critical memory failure impact**
**Verifying error handling and invariants**
**Clarifying terminal event handling**
<COUNCIL_MEMBER_RESPONSE>
## Finding 1: Whole channel-0 offload loses route.bind → first-data ordering
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:132-140`
- **Confidence**: high
- **Issue**: FIFO among control frames does not order a preceding control frame before subsequent data frames. A module can emit its first route frame immediately after its `route.bind` ACK; the new reader may process that data before the control worker commits the binding, silently dropping it as `Reserved`.
- **Evidence**: The TS provider sends the `route.bind` response, then installs the route and invokes `onBound` (`clients/subc-client/src/provider.ts:825-849`). Shipped routing awaits each frame before reading the next (`crates/subc-core/src/server.rs:381-410`), and route-bind response handling commits the binding (`crates/subc-core/src/control.rs:2029-2032`; `crates/subc-core/src/forwarding.rs:752-757`). Before commit, module route lookup returns `Reserved`, which is dropped (`crates/subc-core/src/forwarding.rs:851-860`; `crates/subc-core/src/router.rs:227-245`).
- **Suggested Fix**: Add a per-connection sequencing barrier for HELLO/route.bind lifecycle transitions and following data, or commit those transitions inline. A control-only FIFO is insufficient. Test ACK immediately followed by Push/Request.

## Finding 2: CANCEL has no atomic queued→delivered boundary
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:110-130`
- **Confidence**: high
- **Issue**: “Not in queue means delivered” is false while the drain owns a dequeued Request but is awaiting credit or module egress. In that interval, a bypassed CANCEL can reach the module before the Request and be ignored; alternatively, a racing queue removal can synthesize `cancelled` after the worker has already claimed the Request, allowing a later module terminal too.
- **Evidence**: Shipped serialization prevents this by not reading CANCEL until the prior route send completes (`crates/subc-core/src/server.rs:381-410`; `crates/subc-core/src/router.rs:461-497`). Provider CANCEL handlers simply no-op for an unknown corr (`crates/subc-client-rs/src/lib.rs:988-999`; `clients/subc-client/src/provider.ts:695-697`).
- **Suggested Fix**: Use one route-local state machine, not a bare queue: `Queued → Claimed/Acquiring → Sent → Settled`, with an atomic corr claim. Serialize module sends so a delivered-winning CANCEL follows its Request. A cancel-winning claim must prevent send or roll back an acquired credit.

## Finding 3: “Module emits a terminal on delivered CANCEL” is not true on master
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:52-54`, `:123-126`
- **Confidence**: high
- **Issue**: The redesign depends on a delivered CANCEL causing one module terminal and thus credit release. The shipped provider SDKs do not enforce that.
- **Evidence**: Rust only cancels a token; if the handler has started, its eventual normal outcome is sent (`crates/subc-client-rs/src/lib.rs:892-910`, `:988-999`). TS only aborts an `AbortController` (`clients/subc-client/src/provider.ts:695-697`); it emits no mandatory terminal. The fake AFT stub does synthesize a terminal (`crates/subc-core/src/bin/fake-aft-stub.rs:377-415`), but that is not a general SDK guarantee.
- **Suggested Fix**: Make “exactly one terminal after every delivered Request, including cancellation” an enforced provider-SDK/protocol contract, including handlers that ignore cancellation. Otherwise use daemon-owned cancellation/tombstone semantics with explicit credit handling.

## Finding 4: Credit transfer is not transactional in the proposed drain
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:90-101`, `:180-188`
- **Confidence**: high
- **Issue**: The drain pseudocode omits the shipped send-failure release and does not define whether `outstanding.insert(corr)` occurs before module visibility. Either mistake leaks a forgotten permit.
- **Evidence**: Shipped code explicitly releases on `module_sink.send` failure (`crates/subc-core/src/router.rs:491-496`). `ChannelFlow::acquire` increments `in_flight` and intentionally forgets the semaphore permit (`crates/subc-core/src/forwarding.rs:1692-1699`), so panic/send failure after acquire leaks credit unless explicitly repaired. A very fast module terminal before a post-send insert would see no set entry and fail to release.
- **Suggested Fix**: Before making the Request visible to the module, atomically mark it outstanding. On send failure, remove that mark, release credit, and emit a defined client outcome. Add injected tests for send failure, immediate terminal, and panic at every await boundary.

## Finding 5: Sender-drop teardown is unsafe with Arc snapshots and can hang connection shutdown
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:166-176`, `:205-211`
- **Confidence**: high
- **Issue**: Dropping “the queue sender” does not close a bounded channel while stale snapshot/binding references retain a sender. A stale reader can enqueue after flush/release; the worker either sends a supposedly flushed Request or remains alive forever. Persistent workers can also retain the client egress sender and prevent writer shutdown.
- **Evidence**: Data lookup returns cloned `Arc<RouteBinding>` values (`crates/subc-core/src/forwarding.rs:840-889`), and bindings own both `FrameSink`s (`:51-65`). The proposed snapshot necessarily preserves old binding references. On peer-close, the server drops normal handles then waits indefinitely for the writer in the non-close-request path (`crates/subc-core/src/server.rs:252-277`); a drain task retaining `client_sink` keeps that writer channel open.
- **Suggested Fix**: Decouple queue admission from sender lifetime. Add an explicit `Open/Closing/Closed` liveness gate checked atomically by enqueue, CANCEL, and drain; cancel and join workers before allowing egress shutdown. Do not rely on `JoinHandle` drop or sender refcount as lifecycle control.

## Finding 6: Merge-1 ArcSwap is not invariant-neutral as specified
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:198-211`, `:264-270`
- **Confidence**: high
- **Issue**: “Mutate canonical state, then publish” creates new post-mutation/pre-publish stale reads. Under the current RwLock, a reader beginning after a release writer acquired the lock cannot see the old binding; under ArcSwap it can.
- **Evidence**: Current lookup takes the shared lock (`crates/subc-core/src/forwarding.rs:840-846`); release removes the route and closes its flow under the writer lock (`:1420-1428`). A stale snapshot reader can instead find Bound, hit the closed flow, and produce the shipped `backend_error` path (`crates/subc-core/src/router.rs:465-485`) where a current post-release lookup would produce `unknown_channel` (`:350-360`). Bind has the inverse stale-Absent window.
- **Suggested Fix**: Define the snapshot store as the data-plane linearization point, initialize worker/liveness state before publication, and publish before route.open response admission. Explicitly specify post-release behavior for readers that loaded an old snapshot. Same-thread reads after a completed synchronous store are safe; cross-task readers are the defect.

## Finding 7: `route_backpressure → NotSent` breaks all shipped client contracts
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:154-157`, `:230-233`
- **Confidence**: high
- **Issue**: No shipped SDK maps a daemon Error terminal to `NotSent`; reusing that classification is semantically wrong and would trigger reconnect/reopen behavior on a healthy overloaded route.
- **Evidence**: TS documents `not_sent` as bytes never leaving the local process (`clients/subc-client/src/client.ts:186-210`), receives Error as a terminal (`:1059-1060`), and reconnects on `not_sent` (`:437-443`). Rust treats Error as a module terminal and reconnects/reopens on `NotSent` (`crates/subc-client-rs/src/consumer.rs:570-583`). Swift throws a generic remote error (`clients/subc-client-swift/Sources/SubcClient/Client.swift:475-482`). Also, nonblocking error admission is fallible: `FrameSink::send` awaits while `try_send` can fail (`crates/subc-core/src/router.rs:40-80`).
- **Suggested Fix**: Add a distinct proven-not-delivered/backpressure classification with bounded in-place retry, not reconnect semantics. Define an egress-error lane/reservation policy. A received per-route-full error can honestly prove non-delivery only if queue admission is atomic; aggregate-cap close or failed error admission must remain outcome-unknown.

## Finding 8: `HashSet<corr>` leaks credit for duplicate Request correlations
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign.md:180-188`
- **Confidence**: high
- **Issue**: Two delivered Requests with the same corr produce one HashSet entry. The first terminal removes/releases; the second cannot remove and leaks its credit.
- **Evidence**: Correlation non-reuse is a wire requirement, not daemon enforcement (`docs/specs/subc-wire-v1-final.md:392-408`). Shipped forwarding admits Requests based on route/flow only, with no corr uniqueness check (`crates/subc-core/src/router.rs:452-497`).
- **Suggested Fix**: Reject duplicate corr before queue admission across queued/claimed/outstanding states, preferably as a protocol violation. A counter map cannot safely associate duplicate terminals; uniqueness must be enforced.

## Finding 9: The stated bounds permit severe memory, task, and CPU DoS
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign.md:107-108`, `:149-164`
- **Confidence**: high
- **Issue**: A 4096-frame cap is not a viable memory bound when frames may carry 64 MiB bodies: it permits roughly 256 GiB retained per connection. It also does not cap zero-depth live routes/tasks, and O(queue) CANCEL scans remain on the latency-critical reader.
- **Evidence**: Bodies are owned `Vec<u8>`s (`crates/subc-protocol/src/frame.rs:12-17`), max at 64 MiB (`crates/subc-protocol/src/lib.rs:114-119`), and are allocated before admission (`crates/subc-transport/src/frame_io.rs:74-84`). A single StatelessParallel queue reaches 2048 × 64 MiB = 128 GiB; 4096 frames = 256 GiB. Route allocation permits all nonzero `u16` channels before exhaustion (`crates/subc-core/src/forwarding.rs:1293-1363`), hence up to 65,535 drain tasks per connection. A full 2048-frame route makes every miss-CANCEL cost 2048 scans; 100k 21-byte CANCELs induce about 205M comparisons.
- **Suggested Fix**: Enforce byte quotas before body allocation, practical per-connection route/task caps, and a global cap. Use corr-indexed/tombstoned queue removal or rate-limit CANCELs. Reconsider whole-table clone-on-write under route churn; “mutations are rare” is not an admission-control policy.

## Finding 10: I2/I3/I7 are false claims, not preserved invariants
- **Severity**: MINOR
- **Location**: `docs/subc-dispatch-redesign.md:213-228`
- **Confidence**: high
- **Issue**: I2 says release is once per terminal, while R11 deliberately makes duplicate terminals credit-inert. I3/I7 claim byte-identical/unchanged release behavior, but the new `outstanding.remove(corr)` changes whether release occurs.
- **Evidence**: Shipped module→client forwarding releases after every successfully enqueued terminal (`crates/subc-core/src/router.rs:281-309`). The proposed HashSet gate changes that behavior by design.
- **Suggested Fix**: Restate I2 as “once per uniquely delivered Request,” and explicitly mark R11 as an intentional module→client behavior change. I6 is plausible only if all admission/error paths remain non-awaiting; I5 is verified: `RouterError::to_error_frame` can emit arbitrary codes with channel/epoch/corr and no input-body parse (`crates/subc-core/src/router.rs:582-633`).

## Credit-path audit
- **Delivered + module terminal**: one acquire/one release is possible only with `outstanding` inserted before module visibility; otherwise terminal-first leaks.
- **Queued + daemon-cancelled**: intended acquire=0/release=0, but only after an atomic cancel claim prevents worker acquisition/send.
- **GOODBYE-flushed queued Request**: intended acquire=0/release=0; stale enqueue and claimed-worker races leave this unproven.
- **Client/module death, delivered-not-terminal**: shipped teardown closes flows rather than returning permits (`crates/subc-core/src/forwarding.rs:1420-1428`, `:1737-1739`). That is acceptable only after the binding/workers are truly destroyed.
- **Module send failure / drain panic**: acquire=1 and release is currently missing from the design pseudocode; this is a leak.
- **Outstanding pairing**: needs a lifecycle/corr state machine, not only a terminal HashSet.

## Verified non-blockers / limitations
- `cancelled` can be built correctly with the existing canonical error constructor; it requires no request-body parse (`crates/subc-core/src/router.rs:582-633`).
- A late second terminal is generally harmless to SDK settlement: TS single-winner settlement (`clients/subc-client/src/client.ts:1096-1111`), Rust pending removal, and Swift corr matching (`clients/subc-client-swift/Sources/SubcClient/Client.swift:452-486`). This does not make daemon double-terminal races acceptable.
- No Broca or Alfonso-core source tree is present in this checkout; the only AFT-named implementation found is `fake-aft-stub`, so those external consumer contracts could not be verified.

## Open-question rulings
- **Q1**: WRONG as leaned. Fail-loud may be viable, but not via existing `NotSent`; require separate backpressure semantics, byte caps, and backoff.
- **Q2**: WRONG as stated. Daemon synthesis is viable only after atomic cancel state and a real delivered-CANCEL terminal contract.
- **Q3**: WRONG. Whole channel-0 FIFO lacks control→data ordering; lifecycle controls need barriers.
- **Q4**: RIGHT direction, wrong mechanism. Fix R11 now, but reject duplicate Request corrs and define insertion/send ordering.
- **Q5**: WRONG as unconditional lean. Whole-table swap may be acceptable after hard route/mutation limits and correct linearization; sharding is not merely a perf-afterthought under adversarial churn.

## Summary
**8 BLOCKER, 1 MAJOR, 1 MINOR.** The design has unrefuted first-frame loss, CANCEL races, terminal-contract failure, credit leaks, teardown liveness failure, snapshot semantic changes, SDK contract breakage, and infeasible resource bounds.

**Verdict: NO-GO — require fixes for Findings 1-7 and 9 before implementation or merge-1/merge-2 rollout.**
</COUNCIL_MEMBER_RESPONSE>

