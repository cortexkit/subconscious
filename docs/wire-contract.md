# Wire contract

Every connection to the daemon, whether from a client or a module, carries the
same framing: a fixed 21-byte envelope header followed by an opaque body. The
daemon routes data-plane frames on the header and never parses their bodies;
it parses only the JSON bodies of channel-0 control frames
([`control-plane.md`](control-plane.md)). This document
states the header, the frame kinds, the flags, the error body and the error
codes, and which refusals a caller may retry.

The source of truth is `crates/subc-protocol/src/lib.rs`. The golden fixtures
under `crates/subc-protocol/tests/golden/` and
`crates/subc-control/tests/golden/` pin the JSON shapes; every SDK is tested
against them.

The handshake that precedes the first envelope is described in
[`architecture.md`](architecture.md) section 2 and [`../SECURITY.md`](../SECURITY.md).
Channel-0 operations are in [`control-plane.md`](control-plane.md).

## 1. Envelope

All integers are little-endian.

| Offset | Size | Field | Type | Meaning |
| --- | --- | --- | --- | --- |
| 0 | 4 | `len` | u32 | Number of body bytes after the header. |
| 4 | 1 | `ver` | u8 | Envelope version. |
| 5 | 1 | `type` | u8 | Frame kind (section 2). |
| 6 | 1 | `flags` | u8 | Flag bits (section 3). |
| 7 | 2 | `channel` | u16 | Route slot in the sender's local space; `0` is the control channel. |
| 9 | 4 | `epoch` | u32 | Binding epoch of that slot; always `0` on channel 0. |
| 13 | 8 | `corr` | u64 | Correlation id. A `CANCEL` carries the corr of the call it cancels. |

Rules:

1. **Frozen prefix.** `len` (offset 0) and `ver` (offset 4) keep their meaning
   and position in every envelope version. A reader reads the first 5 bytes,
   learns the version, looks up that version's header length, then reads the
   rest. `decode_header` enforces this order. Test:
   `little_endian_and_frozen_prefix_layout` (`crates/subc-protocol/src/lib.rs`).
2. **One supported version.** `PROTOCOL_VERSION` and `MIN_SUPPORTED_VERSION`
   are both `2`. Any other `ver` is refused as `UnsupportedVersion`, so a stale
   binary fails on its first frame with a named error instead of desynchronising
   the stream. Test: `reject_unsupported_version` (`crates/subc-protocol/src/lib.rs`).
3. **Body cap.** A header declaring a body longer than `MAX_FRAME_BODY_LEN`
   (64 MiB) is refused before the body is allocated, and a writer refuses to
   send one (`crates/subc-transport/src/frame_io.rs`). Tests:
   `body_len_over_cap_is_rejected_before_allocation` (`crates/subc-transport/src/frame_io.rs`) and
   `writer_refuses_oversized_public_frames_before_emitting_bytes` (`crates/subc-transport/src/frame_io.rs`).
4. **Control channel epoch.** A frame on channel 0 with a non-zero epoch is
   refused (`NonzeroEpochOnControlChannel`). Test:
   `nonzero_epoch_on_control_channel_is_rejected` (`crates/subc-protocol/src/lib.rs`).

## 2. Frame kinds

The `type` byte is a closed set. An unknown value is refused
(`UnknownFrameType`; test `reject_unknown_frame_type` (`crates/subc-protocol/src/lib.rs`)).

| Value | Kind | Body |
| --- | --- | --- |
| 0 | `REQUEST` | Opaque (JSON-RPC by convention, or bytes when `BINARY` is set). |
| 1 | `RESPONSE` | Opaque. |
| 2 | `PUSH` | Opaque, unsolicited. |
| 3 | `STREAM_DATA` | Opaque, one chunk of a streamed reply. |
| 4 | `STREAM_END` | Opaque, the final chunk. |
| 5 | `ERROR` | `ErrorBody` (section 5). |
| 6 | `CANCEL` | None. |
| 7 | `PING` | None. |
| 8 | `PONG` | None. |
| 9 | `HELLO` | `ModuleHelloBody` (module registration). |
| 10 | `HELLO_ACK` | `ModuleHelloAckBody`. |
| 11 | `GOODBYE` | None. |

`CANCEL`, `PING`, `PONG` and `GOODBYE` are pure-header frames: `len` must be
`0`, and a pure-header frame that declares body bytes is refused
(`PureHeaderFrameWithBody`; test `reject_pure_header_frame_with_body_len` (`crates/subc-protocol/src/lib.rs`)).

`GOODBYE` on a route channel ends that route. A client that receives a
`GOODBYE` while a request is in flight cannot know whether the module ran the
request; the outcome is unknown, never "not sent". See
[`client-affordances.md`](client-affordances.md) section 3.

## 3. Flags

| Bits | Field | Values |
| --- | --- | --- |
| 0 | `BINARY` | Body is raw bytes rather than JSON. |
| 1-2 | `PRIORITY` | `0` passive, `1` interactive, `2` background. `3` is refused (`ReservedPriorityBits`). |
| 3 | `LAST` | Final frame of a streamed message. |
| 4-5 | `ADMISSION` | `0` normal, `1` expedite, `2` sheddable. `3` is refused (`ReservedAdmissionClass`). |
| 6 | `DAEMON_ORIGIN` | The frame was authored by the daemon. |
| 7 | `SUBSCRIPTION` | A request the client declares as a held-open subscription. |

Rules:

1. **Sheddable only on lossy kinds.** The sheddable class is legal only on
   `PUSH` and `STREAM_DATA`; on any other kind the header is refused
   (`SheddableIllegalFrameType`). Terminal frames can therefore never be shed.
   Test: `sheddable_rejected_on_every_illegal_frame_type` (`crates/subc-protocol/src/lib.rs`).
2. **Admission is a hint today.** The daemon decodes and forwards the expedite
   and sheddable classes but does not reorder or drop frames because of them.
   Senders may set them; they change no delivery guarantee.
3. **Daemon origin identifies daemon-authored frames.** Every decoder accepts
   bit 6 (test `daemon_origin_flags_decode_and_round_trip` (`crates/subc-protocol/src/lib.rs`)).
   The daemon sets it on its own errors
   through `daemon_error_flags()`; test
   `daemon_authored_data_plane_errors_carry_daemon_origin` (`crates/subc-daemon/src/router.rs`)
   asserts this. The daemon clears it on
   forwarded module frames. A client may trust a `route_busy` refusal only
   when this flag is set and its route's open reply carried `request_window`.
4. **Subscription credits.** A `REQUEST` with bit 7 set takes a request credit
   tagged as a subscription. A drain does not wait for subscription credits to
   be returned; it excludes them and reports how many it excluded in
   `route.closed.excluded_subscriptions`. Tests:
   `subscription_flag_decodes_and_tags_the_request` (`crates/subc-protocol/src/lib.rs`), and
   `bit_set_subscription_is_excluded_and_reported_by_route_closed` (`crates/subc-core/tests/forwarding.rs`).

## 4. Routes, channels and epochs

A route joins one client channel to one module channel. Each side names the
route by `(channel, epoch)` in its own local space, and the daemon rewrites
both fields when it forwards a frame. Neither side ever sees the other side's
numbers.

- The daemon allocates channels and mints epochs. Epoch `0` belongs to channel
  0; the first binding of any route slot has epoch `1`, and every later binding
  of the same slot has a larger epoch.
- A client learns its pair from the `route.open` reply (`route_channel`,
  `route_epoch`); a module learns its pair from `route.bind` (`route_channel`,
  `epoch`).
- Both sides stamp their pair into every frame on the route.
- A `REQUEST` naming a channel the sender has no route on is answered with an
  `ERROR` carrying `unknown_channel`, and the connection stays open. Any other
  frame kind on such a channel is dropped without a reply
  (`crates/subc-daemon/src/router.rs`). Tests:
  `unknown_channel_emits_canonical_error_frame` (`crates/subc-daemon/src/router.rs`), and
  `unknown_channel_is_returned_as_error_frame_and_connection_continues` (`crates/subc-daemon/src/server.rs`).
- A `REQUEST` naming an installed channel with an older epoch is answered with
  `stale_route_epoch`; other frame kinds are dropped. Test: `module_request_on_stale_epoch_receives_stale_route_epoch` (`crates/subc-daemon/src/router.rs`).

### Module-originated channel-0 requests

Registered modules send `catalog.update`, `supervisor.live_roots`, `scope.sync`,
`scope.apply` and `scope.describe` as channel-0 `REQUEST` frames. Their bodies
are defined by `subc_protocol::session::ModuleControlRequestFromModule`.

`supervisor.agent_commands` is restricted further: only the current supervised
launch's own connection may declare its commands. Consumer, unsupervised and
stale-launch connections receive an `ERROR` with code `not_supervised`; the
connection stays open. The request body is:

```json
{"op":"supervisor.agent_commands","started":[{"pid":42,"start_time_ms":1700000000123}],"finished":[{"pid":41,"start_time_ms":1700000000000,"cpu_ms":250}],"known":[{"pid":42,"start_time_ms":1700000000123}]}
```

`started` and `finished` are required arrays; `known` is optional and replaces
the module's complete live root set. Start times come from
`subc_os::process_start_time_ms`: milliseconds since the Unix epoch, compared
within that platform's kernel timestamp resolution. A started root must still
be live with that identity and must be a direct child of the module at the
check. Invalid roots are ignored independently and logged once per identity;
they do not refuse the batch. `known` only re-confirms identities already
accepted in this launch, or in a previous launch that retained the same Linux
cgroup or Windows job object across the module's restart. It never accepts a new identity, the module itself or an ancestor.
A live identity keeps its accounting baseline even after being finished, so
re-declaring it cannot replay CPU. A module has at most 1,024 live roots. The successful `RESPONSE` body is `{}`. An
older daemon returns `unsupported_control_frame`; the Rust module SDK maps
that refusal to `AgentCommandsError::Unsupported` without closing the connection.
Declarations are applied serially per supervised connection. At most four may
be in flight: an excess request receives an immediate retryable `busy`, not
reader backpressure, so tool routes and Cancel/Goodbye remain readable. Each
reply has a 200 ms send deadline. Contended commits revalidate up to four times
before asking the caller to retry. `supervisor.list` also admits at most four
detached requests per handler, returning `busy` above that limit; synchronous
status reads run on blocking workers rather than the async reader's runtime.
Busy refusals first use nonblocking egress. When egress is full, a separate
per-connection sender retains at most four refusals, including its active send,
each with a 200 ms deadline. Further overflow or expiry drops the refusal rather
than parking or closing the shared supervised connection. The per-module
`server.describe.counters.agent_command_refusals_dropped_by_module` diagnostic
counts those drops. A dropped refusal gives the caller no reply: its own call
timeout ends with outcome unknown. Re-sending `started` or `finished` for the
same accepted native identity is accounting-idempotent and cannot reset the
acceptance baseline or settle the same CPU twice.

The dropped-refusal map accepts registered module ids only and retains at most
1,024 labels. A consumer connection's dropped busy refusal, or a refusal dropped
for a registered module beyond that label cap, increments the scalar
`agent_command_refusals_dropped_unlabelled` instead of growing the map. Busy
`supervisor.list` replies use this same nonblocking, bounded refusal path.
A refused, cancelled or timed-out `started` batch leaves a coverage gap even
though its CPU remains module-side. Its cumulative agents reading stays lower:
lost lifetime CPU is never repaired. Samples spanning that gap carry uncertainty
until the root is verified gone or an accepted replay restores current coverage.
Those sample flags determine the returned hour's bound, not elapsed time alone.
A cached process walk
that began before the refused declaration cannot establish that confirmation.
Unknown liveness cannot
expire a gap. `server.describe.counters.agent_command_declaration_loss_by_module`
reports `lost_identities` and an independent `overflow` boolean. Each module
retains at most 1,024 lost identities. Overflow clears only after a successful
complete `known` reconciliation **and** one hour since the most recent overflow.
A receipt cannot acknowledge a later overflow event. A full reconciliation
includes every accepted root not verified gone after explicit starts/finishes.
A partial `known` still replaces the live set, but omitted live roots become
lost identities and the replacement cannot recover overflow. Mere contention
on the short metadata-update lock is not overflow. The verified live-root cap
returns retryable `agent_roots_full`, recording only the unaccepted roots as
coverage loss; valid peers in that batch remain accepted. A loss-only row is
emitted even at zero CPU and zero tracked live processes.
The daemon retains the validated native identity of a live root removed by a
partial replacement, so it can recognize a later resumption or confirmed exit.
These omitted-root proofs retain at most 1,024 identities; excess proofs set the
same overflow marker. A walk begun before a proof's accepted identity cannot
prune it. Resumed roots use the canonical verified identity, including when a
caller uses a different epoch-millisecond value within the platform's allowed
timestamp precision.

Oversized declaration arrays are refused before loss-metadata locking or
identity scanning. These retry rules apply to `supervisor.agent_commands`, not
to the separate decision about whether a refused `route.open` may be reopened:
`agent_roots_full` and `busy` may be retried after capacity/commit contention
clears; `duplicate_correlation` needs a fresh correlation; `declaration_closed`
needs a current supervised connection. `AgentCommandsError::is_retryable` exposes
these classes without automatically retrying. Timeout/closed-connection outcomes
may be retried for the same identities because declaration replay is idempotent;
an unknown refusal code is not presumed retryable.

An agents row is shown only after an accepted declaration in the current launch,
for inherited active commands, for attributed CPU still in the returned hour,
or for a recorded declaration loss/overflow. A module that never declared agents
does not acquire an agents row from its own process-tree uncertainty.
When the hour is unavailable, nonzero attributed CPU conservatively keeps its
row until a measured zero hour can show that no current-hour work remains.
The resource stack covers modules whose resource state is present now.
Disabling or removing a module discards its resource history, so its past-hour
contribution also leaves the stack total. A configured disabled entry may still
appear in the supervisor's module list; that does not retain its resource stack.

The daemon attributes declared live subtrees and finished CPU to
`supervisor.list.agent_commands`. **Agent CPU counts from when the daemon first
measured the declared command.** That first successful measurement establishes
its baseline: historical CPU stays in the module row and is never transferred.
If a module restart retains the same cgroup or job object, it also retains the
command baseline. When a restart instead replaces the native container,
already credited lifetime CPU remains, but a surviving old command is
rebaselined at its first successful measurement in the new domain. Its
ambiguous transition receives no new outside credit. The reading becomes a
lower bound only if that command's counter moved across the transition; a
stationary survivor does not introduce lost CPU. On Unix, when a declared root leads
its own session (`setsid`), processes in that session count too, even if they
were reparented away from the root.

`finished.cpu_ms` is an optional, **bounded classification claim**, not an
independent source of CPU. On Unix the daemon reads the declaring process's
own reaped-child CPU, including when a cgroup supplies the tree counter. Only
unclaimed increases seen after that command's last successful measurement and
no later than the earlier of its first confirmed root-exit observation and its
finish's first validation pass are eligible. That first pass takes the native
counter and process table together before attribution work; it may start as
late as the bounded ordered queue permits (four in-flight declarations, 200 ms
each for validation and reply), not at network arrival. The initial observation sequence is pinned
even if commit contention requires newer validation snapshots. No later sampler
or retry can extend its eligibility window. A finish may move at most
`max(0, cpu_ms - acceptance_baseline - already_credited)` from that module's
row to its agents row, capped by that remaining pool and funded module CPU.
The acceptance baseline excludes CPU consumed before declaration. Earlier
finishes consume the same pool once, oldest eligible first. Unclaimed increments survive zero-delta
reads for at most five minutes (`REAP_POOL_RETENTION_MS`), with a hard limit of
256 per module, oldest evicted. Ages use suspend-inclusive monotonic elapsed
time, not wall-clock labels, so a clock correction cannot stretch retention.
This lets a finish classify a tail a preceding
sampler already observed. A declarer can mislabel at most its own other reaped
children within that interval; it cannot increase measured module-plus-agents CPU or spend another
module's counter. Missing reports, unreadable reap counters, unknown acceptance
baselines, expired funding and unsatisfied claims leave CPU module-side and
mark the agents row `bound: "lower"`.
Funding excludes live module-own CPU reserved in each observation and is used
oldest eligible first, never by stealing a newer module-own minute. Its timing
metadata has the same five-minute and 256-entry limits. When that metadata has
expired, the daemon leaves the ambiguous part module-side/lower instead of
inventing a newer consumption time.

Previously measured, post-acceptance command CPU that did not receive agent
credit may still be classified at its original observation, capped by that
root's retained uncredited measurement and matching funding in that same
observation. At most 256 such entries per module survive for five minutes past
measurement; dropping an entry makes agents lower. A report's genuine new tail
may use only funding after its last successful measurement. Expired measured
CPU never becomes a new tail and cannot borrow a newer child's funding.

On Windows reports do not fund CPU: a retained identity-specific root handle
is read after exit is confirmed, re-reading if exit raced an earlier read.
If a finish or live-set replacement arrives while a Windows root is still alive,
the first validation pass freezes its last observed counters and live membership.
Later root/member work remains module-side; it is not missing CPU and does not
itself create a lower bound. Already measured/pending CPU and retained exited
descendants may still be credited once, up to the CPU captured at that snapshot
and only when the measured module/command counters cover it.
Never-sampled breakaway descendants cannot be reconstructed and make the
agents row a lower bound. The daemon measures at declaration, finish and
sampler wakes on every platform. An accepted
command with no successful measurement keeps its CPU in the module row; the
agents row includes `unmeasured_finished` so a reader can describe its CPU as
"at least" rather than interpreting a fabricated zero as complete.
Discarding an exited Windows identity with unknown final CPU or membership also
marks the agents reading lower, including an exit during its first observation.
A subsequent clean pass cannot erase that missing final counter.

A cgroup/job counter retains exited work. Windows pinned process handles
retain final CPU and fixed job membership. Each observation records the
accounting-state revision it started from; it is discarded if that state has
already changed, rather than applying the same exited CPU twice. Process-tree counters instead retain a monotone floor:
each identity's own CPU is counted once; vanished members' last counters remain
counted, and an ancestor's later reaped-children increases first absorb those
vanished counters. Only the excess adds CPU. Consequently **on macOS the
numbers are a floor**, not an exact reconstruction of orphan reaping. Linux
without a delegated cgroup has the same limit. Such tree/agents readings carry
`bound: "lower"`; an absent `bound` means exact. Memory and process count are
current readings and are not CPU bounds.

On Linux with a cgroup, the measured domain is **CPU consumed inside the
module's cgroup, plus CPU of agent commands confirmed outside it at both ends
of an interval**. For a root's prospective delta `r` and cgroup progress `c`:

- Inside at both endpoints: agents receives `min(r, c)` from new cgroup progress;
  module keeps the remainder. The combined `c` is exact for the cgroup.
- Outside at both endpoints, or mixed endpoints: the module keeps `c`, and
  agents receives only `max(0, r - c)`. The interval's total is `max(c, r)`, a
  lower bound when both deltas are nonzero. Disjoint roots share one overlap
  budget; the same cgroup CPU cannot suppress outside demand twice.

An unobserved excursion outside followed by return to the cgroup is outside
the inside/inside measured domain. Such migration requires a deliberate
`cgroup.procs` write by the module's own user; endpoint reads cannot reconstruct
it. Endpoint-based inside attribution is not proof that those CPU units were
physically inside throughout. Those same units may therefore still explain an
outside command's increment rather than prove additional outside CPU. A cumulative reading that has become a floor retains
`bound: "lower"` thereafter: a later exact interval cannot recover missing CPU.

Module CPU plus agents CPU equals measured CPU. A bounded finish may explicitly
decrease the module row by exactly the agents row's gain, never below its
prospective funding floor. Otherwise rows do not decrease. Past minutes may be
reclassified between those two rows, at the measured buckets that funded the
tail, never at the later reply time. The combined module-plus-agents CPU for any minute is unchanged;
CPU funded before the current hour creates no current-hour CPU. The unsigned
hourly deltas therefore still sum to the same measured total over identical
window endpoints. Baselines and declarations themselves create no CPU. A
finished command's confirmed inside CPU is accounted once, never replayed.
Transfer suffixes are selected by monotonic observation sequence, never by wall
time. Same-millisecond samples or a backward clock correction cannot charge an
earlier observation. Both histories and cumulative allocations are checked
before either row changes; if a part cannot move in full, neither side moves
and the agents reading is marked lower. History coverage remains wall-clock
time, independently of that ordering.

`supervisor.list` optionally includes `tree` and `cpu_last_hour` on each module,
plus response-level `daemon` and `agent_commands`. A tree reports cumulative
`cpu_ms`, current `memory_bytes`, `memory_kind`, `process_count`, `source`
(`cgroup`, `job`, `process_tree`) and up to eight `groups` ordered by memory.
Each group's `cpu_ms` is funded attributed credit earned by its currently live
identities, not their raw native counter. Agent groups start at zero at the
accepted baseline. Initial module groups may subdivide the module's initial
counter using live own CPU, largest first (ties by PID then native start), capped
by that row's measured CPU. A newly observed module-owned identity reserves its
already consumed own CPU from available module credit, rather than making that
CPU available to a finish claim. New agent identities start at zero attributed
CPU at their first accepted measurement; their historical CPU stays module-side.
The sum of
live groups never exceeds the row, which also retains retired or unattributed
history. A group's optional `bound: "lower"` follows its row and marks only CPU,
not memory. Finishing never pays the same observed or pending CPU twice; only
the bounded, previously unclassified tail may move between rows.
The old memory-only `subtree` member is replaced by `tree`. Unknown readings
are omitted. `cpu_last_hour` is `{cpu_ms, covered_ms, bound?}` across retained wall-clock
samples: 61 points cover 60 one-minute intervals. A changed process identity
credits the replacement's total; a same-process process-tree floor retains already observed departed work, so
later CPU from surviving members is not lost when the current tree sum falls.

The cumulative `bound` is sticky: missing lifetime CPU is never repaired after
a loss. The optional `cpu_last_hour.bound: "lower"` is derived from uncertainty
flags stored on exactly the samples used for that CPU figure. It is lower if
and only if any returned sample is flagged, including a process-tree minute
retained after a source switch or backward wall step. It becomes exact when
those samples leave the returned window, not merely because another clock aged
a loss marker. Retention and diagnostic overflow ages still use the
suspend-inclusive monotonic clock. A renderer draws "≥" on the hour figure
from `cpu_last_hour.bound` only, and on cumulative CPU from its cumulative
`bound`. Older published readers ignore the new optional hourly member.
Unreadable resources/identity make the entire tree unavailable, never a numeric
partial sum. `tree_unavailable` distinguishes `not_yet_measured` from
`not_running`. Skipped image probes likewise say `not_yet_measured`, not
`hash_failed`. Optional nested members tolerate omission; absent `groups`
means no live program groups. Required numeric measurements remain required
and cannot decode as zero. Readers must ignore unknown object members. The daemon's own resources
never include its reaped module children's CPU.

On Linux, `/proc` visibility also matters. A `hidepid` restriction can make a
listed PID's identity or parent unreadable. Unless it is confirmed gone, the
reader cannot exclude that identity from a tree and omits the affected reading
rather than inventing a partial numeric result. `hidepid=2` can hide other-user
PIDs from enumeration entirely; a process-table walk cannot prove their
absence behind that mount. Cross-user accounting needs a visible `/proc` view
or module containment; an uncontained walk is not an authoritative cross-user
memory census. Its CPU remains the documented process-tree lower bound.

### Request credits

Each route channel admits a bounded number of in-flight requests, set by the
target role's declared `concurrency` (`crates/subc-protocol/src/manifest.rs`):

| `concurrency` | Delivery |
| --- | --- |
| `serial` | One call in flight, strict order. |
| `module_managed` | Concurrent calls; FIFO submission within a channel, or within each lane on an opted-in route (see below). 32 credits per channel (`DEFAULT_MODULE_MANAGED_WINDOW` in `crates/subc-daemon/src/forwarding.rs`). |
| `stateless_parallel` | Fully parallel, no ordering. 1024 credits per channel, as an out-of-memory guard. |

A management surface that omits `concurrency` is treated as `module_managed`.

A client may send the optional `busy_refusals: true` member in `route.open`.
The daemon never copies this member into the module's `route.bind`. For an
opted-in route the open reply includes `request_window` (the total credits)
and, only for `module_managed`, `watch_window` (24). These reply members are
absent unless the client opted in. Older daemons ignore the request member;
a reply without `request_window` means the route is not opted in.

On an opted-in route, a REQUEST never waits for a credit: the daemon refuses
it immediately with `route_busy` if admission is full, without forwarding it.
A bit-7 watch on `module_managed` consumes one total credit and one watch
credit, so at most 24 watches may hold the unchanged 32-credit window. Serial
and stateless-parallel routes have no separate watch limit. Routes without
the opt-in retain their existing credit waits and ordering.

On opted-in `module_managed` routes, submission is FIFO within each lane
(ordinary requests and watches), not across lanes. An ordinary request may
pass an earlier queued watch. A refused request retried while another sender
shares the route's credits may arrive after later submissions. Serial routes
retain strict submission order; stateless-parallel routes remain unordered.

## 5. Error body and error codes

Every daemon-authored `ERROR` frame carries this JSON body
(`ErrorBody` in `crates/subc-protocol/src/lib.rs`):

```json
{ "code": "module_warming", "message": "human-readable text", "detail": { } }
```

`detail` is optional and omitted when absent. It is machine-readable, defined
per code, and never carries secrets. Codes are open strings; the ones below are
the daemon-owned codes defined in `subc_protocol::error_codes`.

### 5.1 Retryable and terminal `route.open` refusals

`error_codes::is_retryable_route_open` is the single predicate both SDKs call.
The golden table `crates/subc-protocol/tests/golden/decision_tables.json`
(`route_open_retryable`) records every code's verdict, and
`route_open_retry_predicate_matches_the_decision_table` (`crates/subc-protocol/tests/golden_json.rs`) holds the predicate to it.

Retryable within the caller's own deadline:

| Code | Meaning |
| --- | --- |
| `module_reloading` | The target is draining for a reload, restart or disable. On the data plane this is a pre-send guarantee: the module never received the request, so it may be re-sent once the route is reopened. Test: `supervisor_reload_rejects_new_work_during_drain` (`crates/subc-core/tests/forwarding.rs`). |
| `module_warming` | The target is supervised and starting but not yet registered, has declared itself not ready, or is missing a required capability (`detail.reason: "required_capability_unprovided"`). |
| `target_unavailable` | The target cannot take this route now: it is supervised but not live, it does not provide the requested role, it has no live connection, its bind admission is full, or the bind relay could not be sent or was abandoned. |
| `module_timeout` | The target did not acknowledge `route.bind` within its relay budget (default 12 s, `route_bind_relay_timeout_ms`), or its bind-relay breaker is open after three consecutive timeouts (open for 20 s, then one probe is let through). |
| `scope_not_synced` | The scope's owner has not synced since this daemon started. |
| `scope_changed` | The scope changed between admission and the bind commit; nothing was sent. |

Every other code is terminal for the target as named, including any code this
crate does not know. The terminal codes the daemon emits on `route.open` are
`unknown_module` (no module of that id is registered or supervised here),
`module_removed` (removed from the configuration recently),
`module_no_protocol` (declared `protocol: "none"`), `invalid_request` (the
`detail.field` names the malformed field), `invalid_project_root`,
`bad_consumer_identity` (the launch-nonce claim does not match),
`capability_forbidden`, `admission_facts_not_permitted`,
`admission_facts_target_not_allowed`, `op_not_allowed` (the target did not
grant `route.bind`), `route_limit` (no free route channel), `forwarding_error`,
`target_flow_unsupported`, `target_agent_run_unsupported`, and the `scope_*`
admission refusals in [`scopes.md`](scopes.md) section 5a, including
`scope_bind_proof_required` and `scope_bind_proof_mismatch`. This is the list
in `crates/subc-protocol/tests/golden/decision_tables.json`.

`scope_epoch_in_use` is also terminal in that table. It is a per-record
`scope.sync` / `scope.apply` refusal, not a `route.open` refusal: an idle-only
epoch or bind-proof replacement found bound routes, in-flight binds or
connection-held presence under the live epoch (see `scopes.md` section 4).

`route_busy` is not a `route.open` refusal. It is retryable on the same
established route, not by reopening it, and is not route-dead.

### 5.2 Established-route death

`error_codes::is_established_route_dead` answers "evict this route, reopen it
and resend once". It is true for exactly `unknown_channel` and
`stale_route_epoch`, the two codes the router emits before a frame reaches a
module (`RouterError::to_error_frame` in `crates/subc-daemon/src/router.rs`).
Test: `established_route_dead_predicate_matches_the_decision_table` (`crates/subc-protocol/tests/golden_json.rs`).

### 5.3 Other daemon codes

`route_busy` is a daemon-origin ERROR on the refused REQUEST's correlation.
The daemon never forwarded that request. Its detail is
`{reason, request_in_flight, watch_in_flight, request_window, watch_window?}`;
`reason` is `no_credit`, and `watch_window` is present only on
`module_managed`. A trusted refusal may be retried with backoff on the same
route until the caller's deadline; it never invalidates the route.
`error_codes::is_route_busy_retryable(flags, code, route_opted_in)` is the
shared trust predicate. The `route_busy_retryable` golden table in
`crates/subc-protocol/tests/golden/decision_tables.json` pins every combination
of code, daemon origin and opt-in. `route_busy` is false in the existing
`is_retryable_route_open` and `is_established_route_dead` predicates and their
tables: neither reopening nor evicting the route is appropriate.

The scope registration codes (`scope_sync_*`, per-record `scope_*`) are listed
in [`scopes.md`](scopes.md). The `operator.confirm` codes
(`operator_declined`, `operator_presence_unavailable`,
`operator_summary_invalid`, `operator_request_not_permitted`) are in
[`control-plane.md`](control-plane.md) section 5. Activity ledger codes are in
[`activity-ledger.md`](activity-ledger.md).

## 6. What is and is not guaranteed

Guaranteed:

- The header layout, frame kinds and flag positions above, for envelope
  version 2.
- A refusal the daemon authors carries an `ErrorBody` with a stable `code`.
- `module_reloading`, `unknown_channel`, `stale_route_epoch` and trusted
  `route_busy` on a data-plane request mean the module did not receive it.
- The credit-wait guarantee is per connection only if every route on that
  connection opted in. Other reader waits (including module queue sends,
  control operations and client reply sends) retain their existing behavior.

Not guaranteed:

- Delivery of a request after its route closes. A `GOODBYE` or `route.closed`
  while a request is in flight leaves its outcome unknown.
- Any meaning for the expedite and sheddable classes beyond decoding and
  forwarding.
- That an unknown error code is retryable. Treat it as terminal.
