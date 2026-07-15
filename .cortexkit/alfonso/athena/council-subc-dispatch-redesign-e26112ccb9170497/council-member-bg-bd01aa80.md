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