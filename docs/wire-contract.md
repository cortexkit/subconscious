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
3. **Daemon origin is decoded, not yet relied on.** Every decoder accepts bit 6
   (test `daemon_origin_flags_decode_and_round_trip` (`crates/subc-protocol/src/lib.rs`)). The daemon does not set it
   on its own error frames yet, so `is_established_route_dead` deliberately
   ignores it (see section 5).
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

### Request credits

Each route channel admits a bounded number of in-flight requests, set by the
target role's declared `concurrency` (`crates/subc-protocol/src/manifest.rs`):

| `concurrency` | Delivery |
| --- | --- |
| `serial` | One call in flight, strict order. |
| `module_managed` | Concurrent calls; FIFO submission within a channel. 32 credits per channel (`DEFAULT_MODULE_MANAGED_WINDOW` in `crates/subc-daemon/src/forwarding.rs`). |
| `stateless_parallel` | Fully parallel, no ordering. 1024 credits per channel, as an out-of-memory guard. |

A management surface that omits `concurrency` is treated as `module_managed`.

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

### 5.2 Established-route death

`error_codes::is_established_route_dead` answers "evict this route, reopen it
and resend once". It is true for exactly `unknown_channel` and
`stale_route_epoch`, the two codes the router emits before a frame reaches a
module (`RouterError::to_error_frame` in `crates/subc-daemon/src/router.rs`).
Test: `established_route_dead_predicate_matches_the_decision_table` (`crates/subc-protocol/tests/golden_json.rs`).

### 5.3 Other daemon codes

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
- `module_reloading`, `unknown_channel` and `stale_route_epoch` on a data-plane
  request mean the module did not receive it.

Not guaranteed:

- Delivery of a request after its route closes. A `GOODBYE` or `route.closed`
  while a request is in flight leaves its outcome unknown.
- Any meaning for the expedite and sheddable classes beyond decoding and
  forwarding.
- That an unknown error code is retryable. Treat it as terminal.
