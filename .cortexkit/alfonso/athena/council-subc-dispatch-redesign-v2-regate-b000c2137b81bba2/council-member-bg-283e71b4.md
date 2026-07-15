## B1-B10 Closure Rulings

- **B1 — CLOSED, with caveat**: v2 correctly stops claiming “zero SDK changes” and makes SDK merge-0 prerequisite. Current TS/Rust/Swift sources do classify data-plane Error frames as terminal/module errors, so the prerequisite is real (`client.ts:421-452,1059-1060`; `consumer.rs:570-579`; `Client.swift:478-482,671-674`). External broca/aft/alfonso-core impact remains unverifiable in this checkout.
- **B2 — NOT-CLOSED**: v2 still has CANCEL limbo. The drain pseudocode pops a corr and only later marks it `Claimed` (`docs/subc-dispatch-redesign-v2.md:85-88`), allowing CANCEL to see stale `Queued`. Also, `Delivered` is set before `module_sink.send`; a CANCEL can be forwarded before the Request is actually enqueued, and module SDKs no-op unknown CANCELs (`provider.ts:695-697`; `lib.rs:988-998`).
- **B3 — NOT-CLOSED**: naming `VecDeque<u64> + HashMap<u64, Slot>` does not provide O(1) remove-by-corr. v2 claims every op is O(1) (`docs/...v2.md:49-54`) but `Some(Queued) => remove from queue+slots` (`docs/...v2.md:72-75`) is not O(1) with the stated structures.
- **B4 — NOT-CLOSED**: v2 adds error arms, but cancellation of a blocked `module_sink.send` after `Delivered` has no specified credit/slot rollback. This matters because `ChannelFlow::acquire` forgets the permit and increments `in_flight` (`forwarding.rs:1692-1699`), and release is only best-effort (`forwarding.rs:1702-1731`).
- **B5 — CLOSED narrowly**: recording `Delivered/outstanding` before `module_sink.send` does close the fast-terminal-before-insert leak for unique corrs, because the module cannot emit a terminal until after the Request is sent (`router.rs:281-309`, `router.rs:491-496`). This closure depends on fixing the duplicate-corr and Delivered-before-send CANCEL issues.
- **B6 — NOT-CLOSED**: the O(queue) scan is removed only on paper. With `VecDeque`, queued CANCEL either scans/removes O(n) or leaves tombstones not described by the drain/capacity logic.
- **B7 — CLOSED**: v2 correctly limits control offload to client-side and keeps module bind ACK processing inline, preserving the shipped bind barrier (`control.rs:1879-2032`; `forwarding.rs:1524-1536`; test `router.rs:1078-1102`).
- **B8 — CLOSED narrowly**: v2 honestly rewords I3/I7 and the per-corr release gate composes with the existing CAS guard (`forwarding.rs:1702-1731`) rather than conflicting. Duplicate corr reuse still breaks the gate; see new blocker.
- **B9 — NOT-CLOSED**: `Open/Closing/Closed` is the right shape, but v2 omits cleanup for cancel-aborted Delivered sends, does not define a lock hierarchy against the forwarding write lock, and omits lifecycle cancellation for the new client control task. Current server waits on writer shutdown after dropping only local sinks (`server.rs:252-277`).
- **B10 — NOT-CLOSED as written**: publish-under-lock + `closed` is the right mechanism, but merge-1 standalone omits the current client→module `handle_bound` consumer. Before merge-2 there is no dispatcher push; stale `Bound` would still call `handle_bound` and turn closed flow into `backend_error` (`router.rs:335-342`, `router.rs:465-484`).

## New v2 Defects

## Finding 1: Delivered-before-send lets CANCEL overtake or block
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign-v2.md:91-98`, `:70-77`; `router.rs:491`; `FrameSink::send` `router.rs:40-47`
- **Confidence**: high
- **Issue**: v2 marks a corr `Delivered` before the Request is actually sent. A CANCEL arriving in this window is forwarded to the module from a different actor; it can enqueue before the Request or block the read loop on bounded `send().await`.
- **Evidence**: module CANCEL handlers no-op unknown corrs (`provider.ts:695-697`; `lib.rs:988-998`).
- **Suggested Fix**: serialize all module-bound route frames through one drain actor, or reserve/enqueue the Request synchronously before exposing `Delivered`.

## Finding 2: `route_closing` is a new unclassified SDK error
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign-v2.md:63-65`, `:232-237`, `:263-264`
- **Confidence**: high
- **Issue**: v2 emits `Error{route_closing}` but merge-0 only adds `{route_backpressure, control_backpressure}`. Current SDKs treat unknown data-plane Error codes as terminal/module errors.
- **Evidence**: TS rejects Error frames as `SubcError` (`client.ts:1059-1060`) and managed call only retries `unknown_channel`/`not_sent` (`client.ts:429-452`); Rust returns `CallError::Module` (`consumer.rs:570-579`).
- **Suggested Fix**: use existing `unknown_channel` for closing/stale admission or include `route_closing` in merge-0 retryable in-place semantics.

## Finding 3: Duplicate corr overwrite remains a credit-leak vector
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign-v2.md:63-67`, Q4 at `:316-319`
- **Confidence**: high
- **Issue**: main enqueue pseudocode uses `slots.insert(corr, ...)` without rejecting duplicates. A reused corr can overwrite `Delivered` or `Queued` state, causing lost terminals, skipped release, or drain panic.
- **Evidence**: shipped `handle_bound` admits Requests without corr uniqueness checks (`router.rs:452-498`); wire hygiene requires no reuse (`subc-wire-v1-final.md:405-407`).
- **Suggested Fix**: enforce `slots.contains_key(corr)` before enqueue and close as protocol violation or return a non-overwriting error.

## Finding 4: Synthetic terminal egress is unspecified
- **Severity**: BLOCKER
- **Location**: queued CANCEL/backpressure paths in `docs/...v2.md:63-77`; current egress APIs `router.rs:40-47,69-81`
- **Confidence**: high
- **Issue**: read-loop synthetic Errors cannot safely use current APIs: awaited `send` reintroduces HOL, while `try_send` can drop the only terminal for a request the daemon removed from the queue.
- **Evidence**: current router recovery awaits egress send (`server.rs:388-401`); `try_send` is best-effort and errors when full (`router.rs:69-81`).
- **Suggested Fix**: add a reserved response/terminal lane or a connection response actor with defined close-on-overflow semantics.

## Finding 5: Teardown cancellation can leak Delivered credit
- **Severity**: BLOCKER
- **Location**: `docs/...v2.md:97-101`, `:160-165`, `:171-172`
- **Confidence**: high
- **Issue**: if a drain has acquired credit, marked `Delivered`, and is blocked in `module_sink.send`, teardown cancellation interrupts the await but v2 specifies no `slots.remove/outstanding--/flow.release` branch.
- **Evidence**: `acquire()` increments and forgets credit (`forwarding.rs:1692-1699`); supervisor quiescence reads `in_flight` (`forwarding.rs:1097-1107`; `supervise.rs:2424-2434`).
- **Suggested Fix**: every select cancel/abort path must roll back any Claimed/Delivered-but-unsent credit under the inbox lock, then call release exactly once.

## Finding 6: Merge-1 closed check omits current client→module path
- **Severity**: BLOCKER
- **Location**: `docs/...v2.md:203-221`; current `router.rs:335-342`, `router.rs:452-498`
- **Confidence**: high
- **Issue**: v2 says closed checks happen in module→client and dispatcher push, but merge-1 lands before the dispatcher exists. The current `handle_bound` path would still route stale `Bound` to a closed flow and emit `backend_error`.
- **Suggested Fix**: merge-1 must add `closed` rechecks to both existing data-plane Bound branches; client→module Request should become `unknown_channel`, module→client stale frames should mimic today’s drop path (`router.rs:227-245`).

## Finding 7: Control-task lifecycle is missing
- **Severity**: MAJOR
- **Location**: `docs/...v2.md:184-193`; server writer lifecycle `server.rs:252-277`; route.open wait `control.rs:1156-1193`
- **Confidence**: medium-high
- **Issue**: the new per-client control task can hold `egress` while blocked in route.open. On peer close, server awaits the writer with no close-request timeout; retained sender clones can delay or hang shutdown.
- **Suggested Fix**: connection owns a control-task cancel token and JoinHandle; cancel/join/abort it before awaiting writer drain.

## Q1'-Q5' Rulings

- **Q1'**: RIGHT — hard-gate merge-0. The blocking interim is underspecified and should not ship without a separate design.
- **Q2'**: RIGHT-BUT-UNSAFE — bounded abort is acceptable only after cancel branches release/clear Delivered credit.
- **Q3'**: RIGHT-BUT-UNSAFE — byte caps are mandatory. Current frame bodies may be 64MiB and are allocated before admission (`subc-protocol/src/lib.rs:114-119`; `frame_io.rs:73-86`).
- **Q4'**: RIGHT — enforce duplicate corr at enqueue; but unsafe because  pseudocode does not.
- **Q5'**: RIGHT-BUT-UNSAFE — whole-table snapshot is plausible, but merge-1 is not standalone until all current Bound consumers recheck `closed`.

## Bottom Line

**Verdict: NO-GO.** v2 improves the architecture but still has unclosed concurrency blockers: CANCEL can still be lost or block, queue removal is not actually O(1), teardown can leak forgotten credits, synthetic terminal egress is undefined, duplicate corr enforcement is absent, and merge-1 is not standalone as specified. Confidence: high.