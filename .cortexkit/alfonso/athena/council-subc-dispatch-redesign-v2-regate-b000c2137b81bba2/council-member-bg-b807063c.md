## V1 blocker dispositions

### B1 — **NOT-CLOSED**
Merge-0 is only proposed, external consumers are unavailable here, and its TS mechanism targets the wrong hook: daemon `ERROR` frames bypass `classifyFailure` and are rejected directly in `dispatch`. Rust still returns `CallError::Module`; Swift retains only a textual remote error and has no managed retry loop. (`docs/subc-dispatch-redesign-v2.md:223-243`; `clients/subc-client/src/client.ts:783-815,1036-1061,1114-1160`; `crates/subc-client-rs/src/consumer.rs:561-585`; `clients/subc-client-swift/Sources/SubcClient/Client.swift:671-674`)

### B2 — **NOT-CLOSED**
The dequeue and `Queued→Claimed` transition use two separate lock acquisitions, so CANCEL can remove a still-`Queued` slot after its corr was popped but before it is claimed. More seriously, `Delivered` is set before `module_sink.send().await`; a CANCEL in that interval can overtake the Request, and provider CANCEL is a no-op until the Request has installed its in-flight entry. (`docs/subc-dispatch-redesign-v2.md:70-101`; `crates/subc-core/src/router.rs:40-47`; `crates/subc-client-rs/src/lib.rs:855-912,988-999`)

- **Queued boundary:** pop/unlock → CANCEL removes slot/synthesizes → drain indexes a removed slot; this is panic/zero-terminal territory unless extra unspecified recovery exists.
- **Claimed/acquire:** CANCEL merely flags; the synthetic terminal waits for `acquire` to return, so a permanently saturated flow can leave a never-delivered cancelled request without a terminal.
- **Delivered/send boundary:** CANCEL is forwarded before the Request may reach the module; it can be ignored, then the Request runs uncancelled. Duplicate-corr overwrite can additionally produce synthetic `cancelled` plus an old module terminal.

### B3 — **CLOSED, narrowly**
The v2 `Mutex<RouteInbox>` explicitly removes v1’s unsynchronized/mpsc-scan data-race ambiguity. I found no inherent lost wake for a single drain consumer if it uses the normal predicate loop: `notify_one` coalescing is sufficient because one wake drains all queued work. (`docs/subc-dispatch-redesign-v2.md:27-54,83-87`)  
This does **not** establish the claimed O(1) cancellation behavior; that remains B6.

### B4 — **NOT-CLOSED**
`teardown: TeardownKind` is not in the declared `RouteInbox`, no set-before-`flow.close()` ordering is specified, and shipped `ChannelFlow::close()` carries no reason. The proposed guard only describes releasing flow credit, not atomically cleaning the corresponding slot/outstanding state on panic, abort, select-cancel, or a terminal racing successful enqueue. (`docs/subc-dispatch-redesign-v2.md:109-153`; `crates/subc-core/src/forwarding.rs:1023-1042,1692-1739`; `crates/subc-core/src/router.rs:465-496`)

### B5 — **CLOSED for the original fast-terminal race, conditionally**
For a unique corr and a correct slot implementation, recording `Delivered`/`outstanding` before `module_sink.send` establishes the required happens-before: the module cannot emit a terminal before receiving the frame. (`docs/subc-dispatch-redesign-v2.md:91-107`; `crates/subc-core/src/router.rs:281-309`)  
This closure is conditional on fixing B2/B4 and enforcing corr uniqueness; otherwise an old terminal can remove the wrong slot.

### B6 — **NOT-CLOSED**
`HashMap<corr, Slot>` provides lookup, not O(1) removal from `VecDeque<corr>`. The specified `remove from queue+slots` needs a corr→node/index structure; a `VecDeque` search/removal is linear/shifts elements, recreating the CANCEL read-loop DoS. (`docs/subc-dispatch-redesign-v2.md:38-54,70-76`)

### B7 — **CLOSED for the bind-ACK barrier**
Keeping module connections inline preserves the required module ACK → committed route → immediate reverse-request ordering. Shipped completion commits under the forwarding lock, and the existing regression test asserts channel-0 RouteOpen precedes the reverse request. (`docs/subc-dispatch-redesign-v2.md:176-194`; `crates/subc-core/src/control.rs:2029-2032`; `crates/subc-core/src/forwarding.rs:1510-1536`; `crates/subc-core/src/router.rs:1078-1102`)

### B8 — **CLOSED as a documentation correction**
v2 correctly admits that the terminal release call site changes and that shipped `release()` already has only a global best-effort CAS guard. The corr-local gate and existing CAS are compatible: the gate is the real duplicate-terminal linearization; the CAS remains only a fallback and cannot prove accounting correctness. (`docs/subc-dispatch-redesign-v2.md:113-133,245-264`; `crates/subc-core/src/router.rs:281-309`; `crates/subc-core/src/forwarding.rs:1702-1731`)

### B9 — **NOT-CLOSED; newly regresses graceful endpoint drain**
`admission=Closing` does block a stale snapshot reader that locks afterward, but the drain accepts `Closing` at its pre-send decision (`admission == Closed` is the only rollback condition). It can therefore send after teardown starts, and v2 releases immediately after joining the drain task without waiting for delivered `outstanding` requests—unlike shipped endpoint drain, which waits for flow quiescence before route removal. (`docs/subc-dispatch-redesign-v2.md:92-101,155-174`; `crates/subc-core/src/supervise.rs:2418-2435,2567-2595`)

### B10 — **NOT-CLOSED**
Merge-1 is not standalone-safe as specified: v2 names the future dispatcher push as the client-side closed check, but merge-1 precedes the dispatcher. Today client ingress calls `handle_bound` directly; without a closed recheck there, a stale snapshot still reaches `flow.acquire()` and yields `backend_error`. (`docs/subc-dispatch-redesign-v2.md:203-221,295-303`; `crates/subc-core/src/router.rs:335-343,452-485`)  
Also, a one-time `closed=false` read can precede release while forwarding occurs after release; no atomic ordering is specified. Literal “unknown_channel” is wrong for module ingress anyway: today absent module frames are silently dropped. (`crates/subc-core/src/router.rs:238-245,344-358`)

## New defects / regressions

### Finding 1: AcquiredCredit cannot safely transfer ownership across send/abort
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign-v2.md:91-111`
- **Confidence**: high
- **Issue**: Committing the guard before `send` leaks a Delivered slot/credit if the task aborts during send; committing after send permits a fast module terminal to release first, then an aborting guard can release a second credit.
- **Evidence**: `ChannelFlow::acquire` forgets its semaphore permit, so credit needs explicit ownership (`forwarding.rs:1692-1699`); module terminal release is concurrent (`router.rs:281-309`).
- **Suggested Fix**: Put a corr-owned credit token in a `Sending`/`Delivered` slot and make terminal, send-failure, cancel, panic, and abort consume that same token exactly once.

### Finding 2: Teardown abort leaves Delivered accounting and/or drops live terminals
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign-v2.md:157-174`
- **Confidence**: high
- **Issue**: Cancellation of a blocked `module_sink.send` bypasses the stated `Err(_)` cleanup arm. A Delivered slot may remain with no module request and no terminal; endpoint drain also removes routes before existing in-flight terminals quiesce.
- **Evidence**: v2 explicitly selects send against cancellation, but flushes only queued corrs; shipped supervisor waits on `endpoint_in_flight_count` before `release_module_endpoint_routes`. (`docs/subc-dispatch-redesign-v2.md:97-101,160-172`; `crates/subc-core/src/supervise.rs:2418-2435,2579-2595`)
- **Suggested Fix**: Differentiate graceful reload drain from forced teardown; wait for Delivered slots/outstanding to settle, and force-settle/clear every slot before abort/removal.

### Finding 3: Duplicate corr overwrite is an active credit-leak and double-terminal vector
- **Severity**: BLOCKER
- **Location**: enqueue at `docs/subc-dispatch-redesign-v2.md:61-67`; Q4 at `:316-319`
- **Confidence**: high
- **Issue**: `slots.insert(corr, ...)` overwrites a queued or Delivered entry. An old terminal can then remove a new slot, suppress the old release, or coexist with synthetic cancellation for the reused corr.
- **Evidence**: v2 itself acknowledges silent overwrite leaks; shipped release is corr-blind and only protects aggregate underflow, not ownership. (`docs/subc-dispatch-redesign-v2.md:316-319`; `crates/subc-core/src/forwarding.rs:1702-1731`)
- **Suggested Fix**: Reject duplicate in-flight/queued Request corr before mutation, close or protocol-error the connection, and test old-terminal/new-corr collision.

### Finding 4: Synthetic terminal delivery has no nonblocking failure policy
- **Severity**: BLOCKER
- **Location**: `docs/subc-dispatch-redesign-v2.md:64-76,94-101`
- **Confidence**: high
- **Issue**: Read-loop paths promise synthetic `cancelled`, `route_backpressure`, and `route_closing` without an await. The only shipped nonblocking API is fallible `try_send`; if client egress is full, the promised terminal can vanish.
- **Evidence**: `FrameSink::send` awaits, while `try_send` can fail; current routing recovery awaits egress rather than solving this condition. (`crates/subc-core/src/router.rs:40-81`; `crates/subc-core/src/server.rs:388-401`)
- **Suggested Fix**: Reserve a bounded response lane or define `try_send` failure as epoch-fenced connection close; test full client egress for every synthetic terminal.

### Finding 5: Client-side control actor can delay teardown and retain writer senders
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign-v2.md:184-194`
- **Confidence**: high
- **Issue**: A FIFO for all client channel-0 frames queues Ping and channel-0 Goodbye behind route.open and other blocking supervisor operations. No lifecycle/cancellation/join rule is supplied for this new task, although it necessarily owns a `FrameSink` clone.
- **Evidence**: route.open waits up to its relay deadline (`control.rs:1156-1192`); several other client controls await too (`control.rs:756-805`). On peer EOF, server drops its local sender then waits indefinitely for the writer unless all clones exit (`server.rs:252-277`; `router.rs:25-47`).
- **Suggested Fix**: Handle Ping and connection/route Goodbye inline; register control-task handles in a per-connection cancellation scope and abort/join them before writer wait.

### Finding 6: Resource bounds remain frame-count-only and route-task count is effectively unbounded
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign-v2.md:307-321`
- **Confidence**: high
- **Issue**: Byte caps and aggregate task/route caps remain an open question, not a merge prerequisite. Each queued slot owns a frame body; one body can be 64 MiB, and the current allocator permits all nonzero u16 route channels.
- **Evidence**: bodies are allocated before dispatch admission (`frame_io.rs:73-86`), maximum body is 64 MiB (`subc-protocol/src/lib.rs:114-119`), and route allocation only exhausts after cycling channel space (`forwarding.rs:1298-1333`).
- **Suggested Fix**: Enforce route, connection, and process byte budgets plus a live-dispatcher cap; charge/decharge on every queue/slot transition and forced teardown.

### Finding 7: Retry and invariant claims are internally inconsistent
- **Severity**: MAJOR
- **Location**: `docs/subc-dispatch-redesign-v2.md:232-237,247-264`
- **Confidence**: high
- **Issue**: Independent retry-with-backoff can reorder an earlier rejected request behind a later accepted request, contrary to unqualified I1 FIFO. `route_closing` is emitted and tested but omitted from I8’s new-code list and SDK plan.
- **Evidence**: TS managed calls have independent retry loops (`clients/subc-client/src/client.ts:405-455`); v2 emits `route_closing` at admission but lists only backpressure codes in I8. (`docs/subc-dispatch-redesign-v2.md:64-67,263-264`)
- **Suggested Fix**: Narrow I1 to admitted requests or add per-route retry sequencing; document `route_closing` and its consumer behavior explicitly.

### Finding 8: Lock hierarchy is unspecified
- **Severity**: MAJOR
- **Location**: teardown and terminal paths
- **Confidence**: medium-high
- **Issue**: The design needs a global forwarding-write-lock ↦ RouteInbox hierarchy, but states none. Existing release paths hold the forwarding write lock; future teardown must set inbox state, while terminal delivery failures can call forwarding escalation.
- **Evidence**: current release takes the forwarding write lock (`forwarding.rs:614-657`); terminal delivery failure escalates via forwarding (`router.rs:285-305`).
- **Suggested Fix**: Normatively require `ForwardingInner write lock → RouteInbox`, never the reverse; release both before egress, await, cancellation, or join. Set teardown reason under inbox before closing flow.

## Q1'–Q5' rulings

| Question | Ruling | Reason |
|---|---|---|
| Q1' hard-gate merge-0 | **RIGHT-BUT-UNSAFE** | Hard-gating is correct; “our consumers are fast to bump” is not evidence, and broca/aft/alfonso-core are unverified. The doc also conflicts by later saying daemon-first deployment. |
| Q2' bounded join/abort | **WRONG** | Acceptable only for an actually closing connection. v2 applies it to GOODBYE and endpoint drain, where it can abandon terminals and accounting. |
| Q3' byte caps | **RIGHT-BUT-UNSAFE** | Byte budgets are necessary, but leaving them open is unsafe; they need concrete pre-admission accounting and aggregate caps. |
| Q4' enforce corr uniqueness | **RIGHT-BUT-UNSAFE** | Correct direction, but it must be a mandatory enqueue check before `slots.insert`, not an unresolved lean. |
| Q5' whole-table snapshot | **RIGHT-BUT-UNSAFE** | Reasonable performance default, but only after correct publication, Acquire/Release semantics, client-path rechecks, and churn measurements. It does not make merge-1 standalone today. |

## Invariants and rollout

I3 is now honestly worded. I4/I7 are still incomplete: Closing produces a new `route_closing` observable, and module-side stale bindings must preserve today’s silent drop rather than a literal `unknown_channel`. I6 is false until CANCEL forwarding and synthetic egress have a non-awaiting, failure-defined lane.

The rollout is contradictory:  requires deployed SDK merge-0 before daemon dispatch, while  says “daemon-first order.” Require an explicit fleet-version gate before any daemon can emit either backpressure code. (`docs/subc-dispatch-redesign-v2.md:232-243,295-305`)

## Verdict: **NO-GO**

Required changes before another gate:

1. Make dequeue+Claim atomic; serialize Request and Delivered-CANCEL egress through one route sender, with a real `Sending` state. (`docs/subc-dispatch-redesign-v2.md:70-101`)
2. Replace vague `AcquiredCredit` with slot-owned, exactly-once credit transfer/cleanup covering send success, send failure, cancellation, panic, abort, and terminal. (`forwarding.rs:1692-1731`)
3. Use an O(1) cancelable FIFO/tombstone design and enforce corr uniqueness before insertion. (`docs/subc-dispatch-redesign-v2.md:38-54,316-319`)
4. Specify a reserved/nonblocking synthetic-error egress policy. (`router.rs:40-81`)
5. Preserve shipped endpoint drain-to-quiescence; make teardown lifecycle async-safe despite current synchronous cleanup/Drop paths. (`supervise.rs:2567-2595`; `router.rs:391-397`)
6. Complete and deploy merge-0 across TS/Rust/Swift, including direct Error-frame classification and route.open/control-backpressure retry handling; independently verify absent external consumers.
7. Do not land merge-1 standalone until both current client ingress paths recheck closed state, atomic orderings are specified, and module/client stale semantics are direction-correct. (`router.rs:335-343,452-485`)
8. Add control-task cancellation, inline Ping/Goodbye handling, byte/task caps, and adversarial tests for every interleaving above.

## Summary

**5 independent BLOCKER mechanisms and 4 MAJOR defects remain**, in addition to unresolved B2/B6. The bind-ACK barrier, queue synchronization primitive, fast-terminal insertion ordering, and I3/I4/I7 correction are genuine improvements, but they do not offset the remaining cancellation, credit-ownership, teardown, SDK, and merge-1 correctness failures. External consumer compatibility is unverified in this checkout.