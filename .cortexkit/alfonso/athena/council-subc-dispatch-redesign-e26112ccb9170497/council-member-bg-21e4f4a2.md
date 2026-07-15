## Finding 1: CANCEL loses against a dequeued request waiting for credit
- **Severity**: BLOCKER
- **Location**: Design 3.2–3.3
- **Confidence**: high
- **Interleave/contract**: On a serial route, A holds the only credit. The drain dequeues B and blocks in `flow.acquire()`. `CANCEL(B)` no longer finds B “still queued,” so the design forwards CANCEL to the module. The module has not seen B and ignores the unknown corr; after A terminates, B is delivered and runs uncancelled.
- **Evidence**: The proposed drain removes a frame before awaiting credit. Shipped modules treat unknown CANCEL as a no-op: `crates/subc-client-rs/src/lib.rs:979-989`; TS does likewise at `clients/subc-client/src/provider.ts:694-696`. Today forwarding CANCEL requires the same awaited module send as other non-Requests: `crates/subc-core/src/router.rs:461-491`.
- **Suggested Fix**: Define a linearizable per-corr state machine (`Queued → Acquiring → Committing → Delivered`, with `Cancelled`). Keep acquiring requests cancellable. Reserve module-sink capacity, then under the route lock either commit the Request synchronously or honor cancellation; only a committed Request may cause CANCEL forwarding. Add deterministic tests for both winner orders and the acquire-wait middle state.

## Finding 2: “Forward CANCEL as today” cannot be both reliable and non-blocking
- **Severity**: BLOCKER
- **Location**: Design 3.1, 3.3, 3.5
- **Confidence**: high
- **Interleave/contract**: A delivered Request is cancelled while the module egress queue is full. The read loop cannot await module capacity, but `try_send` may drop the CANCEL—recreating cancellation failure exactly under saturation.
- **Evidence**: Shipped forwarding awaits `route.module_sink.send(frame).await` for CANCEL and all other non-Requests at `crates/subc-core/src/router.rs:461-491`. The design provides no reliable non-awaiting lane after a queue miss.
- **Suggested Fix**: Send delivered-request cancellation through a bounded priority/control lane owned by the route actor, represented by one cancel bit/entry per tracked corr. The actor’s credit and module-send waits must select on cancellation. Do not use naked `try_send` for CANCEL.

## Finding 3: The assumed module “cancelled terminal” repair is not shipped
- **Severity**: BLOCKER
- **Location**: Design Goals 5 and 3.3
- **Confidence**: high
- **Issue**: Delivered-request cancellation can permanently leak route credit because both shipped provider SDKs contain paths that emit no terminal after CANCEL.
- **Evidence**:
  - TS aborts the controller on CANCEL (`clients/subc-client/src/provider.ts:694-696`) and then returns without a terminal both before/after handler execution (`provider.ts:891-893`, `926-935`).
  - Rust cancels the token (`crates/subc-client-rs/src/lib.rs:979-989`); a capacity-queued request exits without emitting a frame (`lib.rs:884-900`). Its test explicitly expects no terminal (`lib.rs:1638-1730`).
  - Only the fake-AFT stub explicitly emits `Error{cancelled}`; therefore existing daemon tests mask the SDK contract failure.
- **Suggested Fix**: Land and test TS and Rust provider changes that guarantee exactly one terminal for every delivered Request, including pre-handler and in-handler cancellation, before relying on this redesign. Audit external modules separately; broca/AFT/alfonso-core are not present in this repository.

## Finding 4: Whole channel-0 offload violates the bind-ACK ordering contract
- **Severity**: BLOCKER
- **Location**: Design 3.4; Q3
- **Confidence**: high
- **Interleave/contract**: The daemon reads a module’s RouteBind ACK, merely queues it to the control task, then reads an immediate reverse Request and performs a snapshot lookup before the ACK task commits the route. The legal reverse Request sees `Reserved`/`Absent` and is dropped.
- **Evidence**:
  - Shipped connection processing completes the ACK before reading the next frame: `crates/subc-core/src/server.rs:357-375`.
  - TS queues the ACK before invoking `onBound`, from which route traffic may start immediately: `clients/subc-client/src/provider.ts:824-848`.
  - Rust similarly queues ACK, installs, then invokes `on_bound`: `crates/subc-client-rs/src/lib.rs:1096-1104`; its ordering test observes ACK then route traffic at `lib.rs:1826-1872`.
  - This ordering is explicitly load-bearing in `docs/specs/subc-wire-v1-final.md:123-165`.
- **Suggested Fix**: Keep module-originated bind completions and other route-publication barriers inline, or make subsequent same-connection data wait on an ingress sequence fence. Do not subject critical ACK/Response frames to ordinary control-queue overflow. Q3’s whole-channel FIFO lean is wrong.

## Finding 5: The outstanding-set linearization is too late and exit accounting is incomplete
- **Severity**: BLOCKER
- **Location**: Design 3.2, 3.7; I2
- **Confidence**: high
- **Issue**: “Insert on delivery” does not specify an ordering that prevents a fast terminal from racing before insertion. A module writer/peer on another runtime thread can process a Request after it enters the mpsc sink but before the drain records the corr; the terminal then sees no outstanding entry, does not release, and the later insertion leaks permanently.
- **Evidence**: Shipped send-failure rollback is explicit at `crates/subc-core/src/router.rs:491-496`; terminal release occurs only after successful client enqueue at `router.rs:281-309`. The redesign pseudocode and accounting proof omit these phase boundaries.
- **Credit matrix**:

  | Exit | Credit acquired? | Required outstanding/release behavior |
  |---|---:|---|
  | Delivered + terminal | Yes | Insert before module visibility; terminal removes once and returns credit once |
  | Queued + daemon CANCEL | No | No outstanding entry and no release |
  | GOODBYE-flushed queue entry | No | Decrement aggregate queue accounting; no release |
  | Connection death, queued | No | Drop frame/accounting |
  | Connection death, delivered | Yes | Retire all guards when route closes; there may be no terminal |
  | Module send failure | Possibly | Remove provisional entry and return credit, matching `router.rs:494-496` |
  | Module death after delivery | Yes | Retire route/outstanding state and settle client by GOODBYE/close |
  | Drain panic | Phase-dependent | RAII must roll back uncommitted credit or retire committed state |

- **Suggested Fix**: Store an RAII credit guard in an outstanding map. Insert it before a synchronously committed `OwnedPermit::send`; rollback automatically on send failure, cancellation, panic, or teardown. Define whether removal occurs before or after client `try_send` to preserve the shipped delivery-failure behavior.

## Finding 6: The existing acquire-versus-reload race is inherited
- **Severity**: BLOCKER
- **Location**: `ChannelFlow`; design I3
- **Confidence**: high
- **Interleave/contract**: `acquire()` obtains a semaphore permit and pauses before incrementing `in_flight`. Reload closes admission, observes zero, concludes quiescence, releases routes, and sends module GOODBYE. The paused requester then increments and can send via its retained binding after phase two.
- **Evidence**:
  - The gap is at `crates/subc-core/src/forwarding.rs:1692-1699`.
  - Drain closes the semaphore at `forwarding.rs:1023-1042`.
  - Supervisor explicitly relies on “outstanding count can only fall,” checks for zero, then releases routes at `crates/subc-core/src/supervise.rs:2559-2595` and `2418-2436`.
  - The successful-acquire path does not recheck draining before send: `crates/subc-core/src/router.rs:465-491`.
- **Suggested Fix**: Linearize admission and `in_flight` increment against close under one route-local state lock. After obtaining a semaphore permit, check a locked `closed` flag before committing it; `close` must set that flag under the same lock before quiescence can be observed.

## Finding 7: GOODBYE teardown is not atomic with queue admission or an in-progress drain
- **Severity**: BLOCKER
- **Location**: Design 3.3, 3.6, 3.8; I3/I4
- **Confidence**: high
- **Interleave/contract**: Release can originate concurrently from module GOODBYE, endpoint drain, or connection cleanup while a client read loop enqueues. Flush-before-remove alone allows an enqueue after flush. A drain may also have popped/acquired a Request and send it after route release or after module GOODBYE.
- **Evidence**: Shipped epoch-fenced removal is one write-locked compare-and-remove (`crates/subc-core/src/forwarding.rs:1409-1470`), while cleanup has multiple release origins (`forwarding.rs:1168-1239`). The redesign moves queue operations outside this existing linearization point.
- **Issue**: The claim that “dropping the sender” makes stale-snapshot enqueues fail is false if old snapshots/readers or the task retain sender clones. Dropping one sender does not close the channel.
- **Suggested Fix**: Introduce shared `OPEN/CLOSING/CLOSED` route admission state. Atomically mark CLOSING and close the receiver/admission gate before flushing. Then cancel/join the drain or run teardown in the actor before epoch-fenced removal and relay. Specify a lock hierarchy to avoid queue-lock/global-write-lock inversion.

## Finding 8: Drain/control task ownership does not establish shutdown and can hang peer close
- **Severity**: BLOCKER
- **Location**: Design 3.6
- **Confidence**: high
- **Interleave/contract**: A drain blocked on module `send`, or a control task blocked in route.open, retains `FrameSink` clones after the socket read loop exits. Dropping a `JoinHandle` alone does not prove task termination. The connection writer consequently waits for senders that never disappear.
- **Evidence**: The shipped server drops the route context/connection and then awaits the writer (`crates/subc-core/src/server.rs:241-267`). Today the close-select cancels inline routing (`server.rs:370-375`). Detached tasks would escape that cancellation. Route-open’s module send itself has no encompassing timeout before the ACK wait: `crates/subc-core/src/control.rs:1064-1156`.
- **Suggested Fix**: Use a connection-scoped cancellation token and explicit close → cancel → bounded join → abort ordering for control and route tasks before awaiting the writer. Avoid a `binding → JoinHandle → task → binding` strong cycle. Panic guards should close the affected client connection or send client GOODBYE—not merely release toward the module—and intentional aborts must disarm the panic guard.

## Finding 9: Frame-count limits permit catastrophic memory exhaustion
- **Severity**: BLOCKER
- **Location**: Design 3.5
- **Confidence**: high
- **Issue**: A frame-count bound is not a meaningful byte bound. A queued `Frame` retains its body even if stored behind an `Arc`.
- **Evidence**: The shipped maximum body is 64 MiB at `crates/subc-protocol/src/lib.rs:115-119`. Therefore:
  - Stateless route depth 2048 permits up to **128 GiB** retained on one route.
  - Aggregate 4096 permits up to **256 GiB** on one connection.
  - This excludes control queues, outstanding frames, other connections, and allocator overhead.
- **Suggested Fix**: Enforce per-route, per-connection, and process-global byte budgets, charged before body admission and released through RAII on every dequeue/remove/flush/panic path. Frame-count limits may remain as a secondary bound.

## Finding 10: `route_backpressure → NotSent` is not implemented by any consumer SDK
- **Severity**: BLOCKER
- **Location**: Design 3.5,  Q1
- **Confidence**: high
- **Issue**: “Zero SDK changes” and “additive classifier config” are false.
- **Evidence**:
  - TS turns any data Error into `SubcError` (`clients/subc-client/src/client.ts:1034-1059`) and wraps it as terminal (`client.ts:732-742`). Its only retryable-code classifier is for route.open and contains four unrelated codes (`client.ts:1240-1258`).
  - Rust maps an ordinary data Error directly to `CallError::Module` (`crates/subc-client-rs/src/consumer.rs:561-585`); its retryable classifier is likewise route.open-only (`consumer.rs:3130-3135`).
  - Swift’s `SubcError` has no code field (`clients/subc-client-swift/Sources/SubcClient/Client.swift:31-34`) and `remoteError` preserves only textual JSON (`Client.swift:671-674`).
  - Naively classifying TS backpressure as `not_sent` is unsafe operationally: managed `call()` invokes reconnect for every NotSent (`client.ts:435-441`), and reconnect replaces the healthy socket and all routes (`client.ts:929-977`).
- **Suggested Fix**: Add a distinct daemon-rejected/NotSent classification and bounded in-place retry with backoff, without reconnect or route eviction. Update all three SDKs and their public contract. Under the stated zero-SDK requirement, Q1’s fail-loud lean is wrong.

## Finding 11: The hidden 4096 aggregate cap can close conforming clients
- **Severity**: BLOCKER
- **Location**: Design 3.5
- **Confidence**: high
- **Issue**: The cap is not negotiated or enforced by SDKs, yet overflow closes the entire connection and converts many known admissions into outcome-unknown failures.
- **Evidence**: Rust creates a 1024-per-route client semaphore (`crates/subc-client-rs/src/consumer.rs:37-48`, `1438-1441`), including for serial routes whose proposed daemon queue is only four. TS has no analogous route admission cap and writes requests directly (`clients/subc-client/src/client.ts:645-691`). Multiple otherwise valid routes can exceed 4096.
- **Suggested Fix**: Do not call this a protocol violation without a specified/advertised contract. Prefer per-request admission errors while preserving the connection, or add an SDK-visible connection budget and update all clients first.

## Finding 12: Overflow behavior omits the reverse-request terminal lane
- **Severity**: BLOCKER
- **Location**: Design 3.2, 3.5
- **Confidence**: high
- **Issue**: “Non-Request” is incorrectly treated as synonymous with CANCEL/GOODBYE. Client→module traffic also legally includes Response, Error, StreamEnd, StreamData, and Push. The design neither guarantees their admission nor defines a full-queue failure policy.
- **Evidence**: Shipped `handle_bound` forwards every frame type, gating only Request credit (`crates/subc-core/src/router.rs:452-498`). Reverse responses are proven client→module traffic at `crates/subc-core/tests/reverse_request.rs:580-602`; frame kinds are defined at `crates/subc-protocol/src/lib.rs:162-181`.
- **Suggested Fix**: Specify capacity, priority, ordering, and failure semantics for every frame type. Reverse terminals cannot be silently dropped; use reserved capacity/priority state or track reverse outstanding requests and close explicitly when reliable delivery is impossible.

## Finding 13: `HashSet<corr>` cannot enforce exact accounting without corr uniqueness
- **Severity**: BLOCKER
- **Location**: Design 3.7
- **Confidence**: high
- **Issue**: The wire/router does not currently enforce unique in-flight client correlations. A set cannot represent two acquired Requests with the same corr.
- **Evidence/interleave**: Send R1(corr=x) to the module, queue R2(corr=x), then CANCEL(x). The daemon removes R2 and synthesizes cancelled while R1 later emits its module terminal—both terminals fire for the same corr. Two delivered duplicates also acquire twice but occupy one set entry, leaking one credit. A late duplicate terminal from an old x can remove a newly reused x.
- **Suggested Fix**: Enforce non-reuse/monotonicity or at least reject duplicate queued/outstanding corrs before admission and close malformed peers. A multiset alone cannot distinguish old duplicate terminals from a reused correlation. Add adversarial duplicate/reuse tests.

## Finding 14: O(queue) CANCEL work is attacker-amplified on the read loop
- **Severity**: MAJOR
- **Location**: Design 3.3, 3.5
- **Confidence**: high
- **Issue**: A stateless route allows 2048 queued entries. An unknown or tail corr costs 2048 comparisons while holding the queue lock. Repeated sprays keep that cost indefinitely and may starve the drain.
- **Evidence/math**: Draining 2048 entries in adversarial search order costs `2048×2049/2 = 2,098,176` comparisons; 4096 unknown CANCELs against a full queue cost about `8,388,608` comparisons. Each attacking frame is only the 21-byte envelope.
- **Suggested Fix**: Maintain `corr → node/state` indexing with O(1) cancellation, using tombstones or an intrusive queue. Ensure aggregate accounting decrements on remove without scanning.

## Finding 15: Synthetic fail-loud terminals have no non-blocking reliable egress policy
- **Severity**: MAJOR
- **Location**: Design 3.3–3.5
- **Confidence**: high
- **Issue**: The read loop cannot await `client_sink.send`, but `try_send` can fail when client egress is full. The promised `cancelled`, `route_backpressure`, or `control_backpressure` terminal may therefore vanish.
- **Evidence**: Shipped route-generated errors are sent with an awaited egress operation at `crates/subc-core/src/server.rs:377-390`; module→client `try_send` failure triggers epoch-fenced connection-close escalation at `crates/subc-core/src/router.rs:281-305` and `forwarding.rs:1241-1265`.
- **Suggested Fix**: Give synthetic errors reserved bounded egress or a connection response actor. When that reserve is exhausted, epoch-fenced connection close is required. Document that the client must conservatively observe outcome-unknown on close even though the rejected frame itself was not delivered to the module. Distinguish queue `Closed` from `Full`; a stale route must not produce `route_backpressure`.

## Finding 16: I3, I4, and I7 are materially false as written
- **Severity**: MAJOR
- **Location**: Design 
- **Confidence**: high
- **Issue**:
  - **I3**: Release paths are necessarily changed by queue closure, drain shutdown, outstanding retirement, and new synthetic-error escalation.
  - **I4**: Under shipped serial ingress, a Request preceding GOODBYE is routed to completion before GOODBYE is read (`crates/subc-core/src/server.rs:357-375`; client GOODBYE release is `router.rs:335-340`). The redesign may read GOODBYE and flush that earlier Request without module delivery—a real semantic change.
  - **I7**: Gating release on `outstanding.remove(corr)` intentionally changes module→client terminal behavior from unconditional aggregate release after successful enqueue (`router.rs:281-309`).
- **Caveat**: TS and Rust close-route APIs settle local pending work before sending GOODBYE (`clients/subc-client/src/client.ts:543-556`; `crates/subc-client-rs/src/consumer.rs:2129-2138`), so queue flushing may be acceptable for SDK users. It is still not byte/semantic identity for raw wire clients.
- **Suggested Fix**: Rewrite the invariants as intentional deltas, specify raw-wire GOODBYE semantics, and add tests for every release origin and in-progress drain phase. Preserve only the actual epoch compare-and-remove and escalation predicates.

## Finding 17: Ordering claims are only partly valid
- **Severity**: MAJOR
- **Location**: Design 3.1, 3.4; I1; Q3
- **Confidence**: high
- **Issue**:
  - One drain can preserve FIFO among retained Requests; this part is sound.
  - Cross-route order was observable on one shipped read loop, although not promised as a contract; the redesign will reorder it.
  - A well-behaved client’s first post-route.open frame is safe only if queue creation and snapshot publication precede the RouteOpen response. All three clients install the handle before subsequent use; e.g. TS at `clients/subc-client/src/client.ts:354-390`, Rust at `crates/subc-client-rs/src/consumer.rs:2854-2866`, Swift at `clients/subc-client-swift/Sources/SubcClient/Client.swift:161-205`.
  - The claimed route.open→route.close protection is misleading: there is no channel-0 route.close command (`crates/subc-control/src/lib.rs:43-90`); route close is data-channel GOODBYE and therefore bypasses the control FIFO.
  - Sending all channel-0 frames to one FIFO also puts Ping behind a 12-second route.open, contradicting the prompt-progress goal.
- **Suggested Fix**: Publish a fully initialized route before exposing its response; handle Ping/Pong inline; preserve explicit barriers for route-mutating control completions and GOODBYE. Document the intentional loss of cross-route/control-data ordering.

## Finding 18: Snapshot merge-1 can be neutral, but only with stricter publication rules
- **Severity**: OK
- **Location**: Design 3.8, 
- **Confidence**: medium-high
- **Assessment**:
  - Pre-publish bind reads map to existing Reserved/Absent behavior.
  - A reader retaining an old Bound route across release already exists because shipped lookup clones an `Arc<RouteBinding>` and drops the read lock before routing (`crates/subc-core/src/forwarding.rs:840-890`; `router.rs:221-342`).
  - With merge-1’s still-serial ingress, a same-connection control mutation completes before the next frame (`server.rs:357-375`).
- **Required constraints**: Every mutation touching lookup fields must publish exactly once before releasing the write lock and before externally observable RouteOpen/GOODBYE effects. Bind commit currently couples map insertion and RouteOpen enqueue under the lock (`forwarding.rs:1510-1536`); preserve that ordering. Whole-table snapshots must be immutable and internally consistent.
- **Caveat**: Once queues land, stale Bound snapshots are safe only if the shared queue is actively closed as described in Finding 7; sender-drop alone is insufficient.
- **Suggested Fix**: Gate merge-1 with mutation-by-mutation publication tests and keep it separate. Under those constraints, merge-1 itself is not the blocker.

## Finding 19: Canonical synthetic errors and late-terminal SDK behavior are valid
- **Severity**: OK
- **Location**: Design 3.3; I5
- **Confidence**: high
- **Assessment**: `RouterError::RouteError` passes arbitrary code/message to canonical `ErrorBody` serialization and builds an Error using only channel/epoch/corr (`crates/subc-core/src/router.rs:582-633`); no frame-body deserialization is needed.
- **Late terminal evidence**:
  - TS removes the pending entry on first settlement and drops/logs later terminals (`clients/subc-client/src/client.ts:1034-1110`).
  - Rust removes the pending entry before settlement, making later frames no-ops (`crates/subc-client-rs/src/consumer.rs:1902-1922`).
  - Swift returns on the matching first terminal and ignores mismatched/no-longer-in-flight keys on later reads (`clients/subc-client-swift/Sources/SubcClient/Client.swift:444-485`, `383-409`).
- **Caveat**: SDK harmlessness does not repair daemon credit corruption or the duplicate-corr dual-terminal case.

## Finding 20: I6 is plausible, but static backends are omitted from the handoff design
- **Severity**: MINOR
- **Location**: Design 3.1; I6
- **Confidence**: high
- **Issue**: A synchronous `try_push` after a completed read preserves the BufReader rule: the read future remains cancellable only by connection close. However, “never awaits route work” is incomplete for non-forwarding backends.
- **Evidence**: Unknown dynamic routes may fall through to a registered backend (`crates/subc-core/src/router.rs:344-349`), and Echo awaits client egress (`router.rs:406-417`).
- **Suggested Fix**: Explicitly offload/remove static backends or route them through a bounded worker. Keep `read_frame` owned directly by the connection loop rather than spawning per-read tasks.

## Open-question verdicts
- **Q1 — WRONG as currently stated**: fail-loud admission can be a good policy, but not under “zero SDK changes,” hidden aggregate limits, or frame-count-only memory bounds. After SDK/backoff/byte-budget changes it is preferable to a pause-set that reintroduces TCP HOL.
- **Q2 — RIGHT in principle, unsafe as specified**: daemon synthesis is preferable for provably never-delivered work, but requires the linearizable state machine in Finding 1 and corr uniqueness in Finding 13.
- **Q3 — WRONG**: whole channel-0 FIFO breaks the normative module ACK→immediate-data barrier and Ping progress. Offload slow client commands selectively or add explicit ingress fences.
- **Q4 — RIGHT after redesigning the mechanism**: fix R11 now, but use RAII credit ownership and a properly synchronized map; acknowledge that I7 changes.
- **Q5 — RIGHT conditionally**: whole-table publication is correctness-safer than independently published shards. Require under-lock publication and worst-case route-churn benchmarks before rejecting it for performance.

## Summary
**11 BLOCKER, 6 MAJOR, 1 MINOR, 2 OK.** The redesign lacks a valid CANCEL/delivery linearization, relies on a cancellation-terminal guarantee contradicted by shipped SDKs, breaks bind-ACK ordering, leaves multiple credit/lifecycle exits unproved, and permits enormous memory retention. External broca/AFT/alfonso-core consumers could not be audited because they are not present in this repository.

**Member verdict: NO-GO — blockers Findings 1–13 must be resolved before implementation; merge-1 alone is acceptable only under Finding 18’s publication constraints.**