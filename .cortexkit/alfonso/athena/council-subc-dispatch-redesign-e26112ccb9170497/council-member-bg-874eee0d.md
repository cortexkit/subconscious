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