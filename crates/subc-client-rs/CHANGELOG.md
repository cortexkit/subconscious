# Changelog

## 0.32.12

- A `module_reloading`, `unknown_channel` or `stale_route_epoch` carrying `FLAG_DAEMON_ORIGIN` now proves non-delivery only when the connection's daemon is 0.37.15 or later, the first release that strips the flag from module frames. The version is the `daemon_ver` in the `ConnectionInfo` the handshake authenticated against: the handshake proves that file's `daemon_id` belongs to the connected daemon, though its proof does not cover `daemon_ver` itself. Before 0.37.15 the data-plane proof is never claimed: even marked refusals count as possibly forwarded, and final refusals remain `CallError::Module`. Unrecognized version strings (including pre-release and build suffixes) are also untrusted. The existing resend and eviction policy is unchanged.
- A managed call that encounters a stale route handle after an earlier attempt was sent now returns typed `NotSent(BeforeQueue)` with the whole-call `never_forwarded()` result and the stale handle in its message. A stale handle on the first attempt is unchanged. Writer-capacity timeouts classify pending entries by writer acceptance, preserving `OutcomeUnknown` if an entry was accepted. No public signatures or enum variants change.

## 0.32.11

- Add the non-exhaustive `NotSentCause` and `CallError::not_sent_cause()` for every crate-built `NotSent`, preserving route-open refusal bodies and route-close reasons. `CallError::never_forwarded()` reports whether **all** attempts of a managed call are proven not forwarded, not just its final attempt. A data-plane `module_reloading`, `unknown_channel` or `stale_route_epoch` proves non-delivery only with `FLAG_DAEMON_ORIGIN`; unmarked replies (including older daemons' replies) remain conservative. Both accessors return `None` for other error variants and externally constructed untyped `NotSent` sources.
- A final daemon-marked data-plane `module_reloading`, `unknown_channel` or `stale_route_epoch` now returns `CallError::NotSent` with `DaemonRefusedBeforeForward { code }`, instead of `CallError::Module`. Callers matching `Module` for those codes must also handle `NotSent`, but only daemons marking their errors produce this category change. Without the flag the original `Module(body)` is preserved. The existing single-resend and eviction decisions are unchanged, as are sent calls that lose their route (`OutcomeUnknown`). No existing public signatures or `CallError` variants change.

## 0.32.10

- Add `OperatorConfirmError::PresenceUnsupportedPlatform`, returned when the daemon answers `operator_presence_unavailable` with `detail.reason` `unsupported_platform`: it has no operator prompt on this platform (today Linux) and showed none. Before, this came back as `PresenceUnavailable`, the same variant as a failed or timed-out prompt, a dropped connection or an unknown reply, so a module couldn't tell "no prompt exists here" from "the prompt broke". Every other `operator_presence_unavailable` reply is still `PresenceUnavailable`. Like every variant, the new one refuses the write by default; a module may treat it differently only where its owner has ruled that a confirmation typed by a person in a local app counts on such a platform. The enum is `#[non_exhaustive]`, so this is additive.

## 0.32.9

- Add `ModuleHandle::confirm_enrollment(summary, caller_route)`, which sends the same `operator.confirm` request as `confirm_operator` with `kind: "enrollment"`, sharing its 290-second deadline, withdrawal-on-drop and error mapping. The daemon never approves an enrollment without an operator prompt, though it may still refuse one before prompting; a daemon built against subc-protocol before 0.33.5 ignores the kind and treats it as a plain `confirm_operator` request.
- Require subc-protocol 0.33.5, the first release with `OperatorConfirmKind`. 0.32.8 already used that type while still declaring 0.33.0, so it failed to build against an older locked 0.33 release.

## 0.32.8

- Add `ModuleHandle::confirm_operation(summary, caller_route, operation)`, which sends the same `operator.confirm` request as `confirm_operator` with `kind: "operation"` and the operation name set, sharing its 290-second deadline, withdrawal-on-drop and error mapping. An empty operation name is refused locally as the new `OperatorConfirmError::OperationInvalid` without sending. Any other name is sent as is; a name the module did not declare in its manifest `confirm_operations` still prompts, and `Ok(())` authorises only this one write.

## 0.32.7

- Add two-step sends on an open consumer route: validate bytes with `RequestBody::new`, await `SubcConsumer::reserve` outside a caller-owned database transaction to acquire route credit, writer capacity and a correlation id, then call synchronous, infallible `RequestReservation::send` inside the transaction that records the send. The returned `PendingReply` uses the same results and errors as `SubcConsumer::request`. Dropping an unsent reservation releases its resources and sends nothing; a request queued after its route closes is skipped by the writer, never written to the wire. Queuing is not delivery: a connection-loss error cannot establish whether the module saw the request, so callers must include their own idempotency key in the body for safe retries. Existing public signatures are unchanged.

## 0.32.5

- Tests only: removed a source-shape test of the TCP connector; the real IPv4, IPv6 and localhost connector tests remain. No behaviour change.

## 0.32.4

- Treat a daemon reset after a validated HELLO_ACK as a connection end even if the module's `on_hello_ack` callback is still pending. Serving returns `Ok` and calls `on_connection_end(Reset)` once, instead of sometimes returning a raw frame I/O error. Disconnects before a valid HELLO_ACK remain registration errors. Reader and writer paths now share the reset/abort/broken-pipe classifier; a late flush disconnect after EOF, GOODBYE or a module stop still preserves the first connection-end reason. Frame I/O errors no longer repeat the `frame I/O error:` prefix. No API change.

## 0.32.3

- A module that ends its own daemon connection now closes it in order, so the daemon reads the module's GOODBYE and then a clean EOF instead of a connection reset. Before, the module dropped its socket while frames the daemon had sent after the stop were still unread, and closing a TCP socket with unread data makes the OS send a reset. On Windows the daemon's read then failed with `ConnectionReset` (10054) before it saw GOODBYE; Linux passed only by timing. After the last frame is flushed, serving now shuts down the socket's sending side and keeps reading and discarding until the daemon closes, bounded by the existing writer drain budget, so a daemon that never closes cannot hold a stop past that budget. This applies to a `ModuleStopHandle` stop (during or after the handshake), an internal close and a module-side error. When the daemon ended the connection (EOF, GOODBYE or a reset), nothing is drained. A test stand-in daemon that keeps its end open after the module closes now holds `serve` until the drain budget; close the stand-in's end, as a real daemon does, to end it at once. No API change.

## 0.32.2

- Keep invalidated consumer routes readable until their last pending call or subscription settles, or the daemon closes the route. New calls open a fresh route, but sibling calls still receive their own drain refusals or real replies on the old route. Only a call that receives a not-forwarded refusal resends, within its existing retry budget and deadline. Once the old route's last call settles, including after a timeout, a dropped call or the connection ending, release what the client kept for it: the entry that routes the daemon's frames for that route to its calls, and the table of requests the module may send back on it.

## 0.32.1

- Version 0.32.0 was never published, because the main branch failed CI before release. This release carries every 0.32.0 change below, plus the daemon-disconnect fix.

## 0.32.0

- **Breaking:** `SubcModuleError` is now non-exhaustive. Add a `_` arm anywhere an existing module matches it exhaustively. `serve` now sends a fixed, redacted correlated `internal` error and returns `SubcModuleError::HandlerPanic` on a request-handler panic, rather than silently losing the reply and continuing to serve. A terminal reply that cannot be queued also fails the connection.
- After the module handshake completes, a daemon disconnect ends serving cleanly on every OS: read reset/abort or a writer disconnect while the reader is still open calls `on_connection_end(ConnectionEnd::Reset)` and returns `Ok`, just as EOF calls the hook with `Eof`. Windows can report a killed daemon as a reset where Unix reports EOF; a module wanting a different exit code for an unannounced end decides in its hook. Disconnects before HELLO_ACK remain handshake errors. GOODBYE and module stop remain orderly, and late-flush disconnects after an orderly end are ignored. After EOF, undelivered replies are logged once by count, without their contents.
- Connect using host/port pairs, supporting bracketed and unbracketed IPv6 as well as IPv4 and hostnames.
- Response and Error frames above the delivered-frame limit (header included) now become a small correlated `frame_too_large` Error with `{size, limit}` detail. Exactly-at-limit replies are unchanged; no body is truncated.
- Add `ServeOptions` with default/builders and a cloneable `ModuleStopHandle`. Create the stop handle before calling `ServeOptions::serve` or `serve_with` to cover connection establishment and the handshake. A stop finishes accepted handlers within the drain budget, sends GOODBYE and flushes; keep awaiting the serving future for teardown. The configurable writer/stop drain budget defaults to two seconds and should remain below the daemon's per-module stop budget (capped at 25 seconds). Existing free `serve*` signatures and defaults are unchanged.

## 0.31.1

- Add `SubcConsumer::supervisor_rescan_checked(preview, candidate, expect)`, which sends `supervisor.rescan_checked` and returns the daemon's `SupervisorRescanResult`. A preview sends the candidate config text and changes nothing; an apply sends the `expect` value from that preview. A preview is retried on transport failures like the other reads, while an apply is sent once. A daemon refusal comes back as `CallError::Module` with its code and message. No existing method changes.

## 0.31.0

- Minor bump because this crate re-exports subc-protocol types, and subc-protocol moves to 0.33.0 (`ToolCallRequest` gains the optional `output_refs` member, breaking for struct literals). Also takes subc-control 0.33 and subc-transport 0.14, which moved for the same reason. Move direct protocol dependencies together with the SDK. No other change.

## 0.30.13

- Add `SubcConsumer::call_framed_with_reply` and `call_framed_cancellable_with_reply`, taking an object head plus raw payload and returning `CallReply` with existing cancellation, deadline and retry semantics. Requests carry `FLAG_BINARY`; existing methods are unchanged.
- Add `RequestCtx::request_payload()` to decode flagged requests into an owned JSON head and borrowed unchanged payload, return `None` for unflagged requests, or a named `FramedBodyError` for malformed bodies. Handlers call it only on payload-declaring management operations and turn decoding errors into refusals before performing work. Tested with a 1 MiB NUL/non-UTF-8 payload through the real daemon.

## 0.30.12

- Add `RequestCtx::mark_binary_reply()` to declare a terminal `HandlerOutcome::Response` body as raw bytes with `FLAG_BINARY`. The mark is shared by clones of one request's context, never by other requests, and does not change errors or stream frames. Bodies are sent unchanged with or without the mark, and the delivered-frame size limit still includes the header. No `HandlerOutcome` variant was added.
- Add `SubcConsumer::call_with_reply` and `call_cancellable_with_reply`, returning a non-exhaustive `CallReply` with `body()`, `into_body()` and `is_binary()` accessors. Relays can preserve the terminal Response's binary declaration without guessing from its bytes. Existing body-only call APIs keep their signatures and behavior.

## 0.30.11

- Expose session folder/project through the re-exported scope record builders for scope sync/apply and scope stamps for bind readers. Requires subc-protocol 0.32.11 and subc-control 0.32.5.

## 0.30.10

- Use subc-control 0.32.4 and subc-protocol 0.32.10 so typed `catalog.list` responses decode the optional rich-UI descriptor and nonce-verification fact carried by the newer daemon. Older catalog replies remain valid with both fields absent.

## 0.30.9

- A served module can keep requests that are held for minutes from taking every handler slot. Until now every data request waited, in arrival order, for one of 64 slots and held it until its handler returned, so a burst of long requests left short ones queued behind them where the daemon could not see them. Implement the new `ModuleHandler::request_pools` and return `RequestPools::new().with_long_held(limit, classifier)`: the classifier gets each request's raw body before the request takes a slot and returns a `RequestClass` (`RequestClass::long_held()` or `RequestClass::default_pool()`, optionally `.with_op("label")`). Long-held requests then wait only for one of `limit` slots of their own, and the default pool's 64 slots stay free for everything else. The classifier runs on the connection's frame reader, so keep it a cheap peek at the operation name; a panic sends the request to the default pool. A module that does not implement `request_pools` keeps the single default pool of 64 slots.
- For every module: a request that has waited two seconds for a slot is logged once at WARN while it is still waiting, with its operation label (`<none>` when it has none), pool, the pool's slots in use and limit, and its route channel and corr; never its body. While the connection is open, a request cancelled while it waits for a slot is now answered `cancelled` at once, without running its handler, instead of after a slot frees. When the connection ends, waiting requests still end without a reply.
- `ModuleHandle::request_pool_snapshots` returns each pool's slots in use, limit, waiting requests and oldest wait (`RequestPoolSnapshot`). Health saturation is reported per pool; with a long-held pool declared each sentence names its pool (`request dispatch saturated in the long_held pool: ...`), and without one the wording is unchanged. Health checks still never wait for a slot.
- `RequestPool`, `RequestClass`, `RequestPools` and `RequestPoolSnapshot` are `#[non_exhaustive]`.

## 0.30.8

- Mark every held subscription request with `FLAG_SUBSCRIPTION` so module drains exclude it from their quiescence wait; ordinary requests remain unmarked.

## 0.30.7

- `ServerDescription` gains `config_path`, the config file the daemon reads as `server.describe` reports it (absent from older daemons and when the daemon's path is relative). Requires subc-control 0.32.3.

## 0.30.6

- Add typed `SubcConsumer` calls for the supervisor operations that had none: `supervisor_health`, `supervisor_health_probe` (complete metrics, which the cached view may replace with a size marker), `supervisor_provenance`, `supervisor_routes`, `supervisor_stderr_tail`, `supervisor_terminals`, `server_describe`, and the lifecycle requests `supervisor_restart` and `supervisor_set_enabled` (what `ck module restart|stop|start` send). Reads retry transport failures until the call deadline like `supervisor_list`; the lifecycle requests are sent once and never retried, since a request with an unknown outcome may already have acted. Refusals keep the daemon's code in `CallError::code`.

## 0.30.5

- `ModuleHandle::confirm_operator` distinguishes a daemon `detail.reason` of `backoff` as `OperatorConfirmError::Backoff { retry_after_ms }`, preserving the remaining cooldown when supplied. Other operator declines remain `Declined`.

## 0.30.4

- Expose capability-gated `ModuleHandle::scope_served_by` with an open answer and non-exhaustive reply; queries read only the registered owner namespace.

## 0.30.3

- `SubcConsumer::call` and `call_cancellable` now resend once after a data-plane ERROR `module_reloading`. The daemon answers a request on an established route with that code only while the module drains for a restart, reload or disable, and before forwarding it, so the module never received the request. The call evicts the cached route, reopens it (the open path already retries the `module_reloading` / `module_warming` refusals it meets until the replacement process registers) and resends, so a call that lands in a drain window succeeds on the new process instead of failing. The resend shares the existing single resend budget with `unknown_channel` and `stale_route_epoch` (a call never resends twice; a second refusal is returned as `CallError::Module`), happens only within the call deadline, and is skipped once the call is cancelled. `error_codes::is_established_route_dead` is unchanged, because other consumers map it to their own verdicts; the client checks `module_reloading` separately, in a private `is_data_plane_drain_refusal`. Subscriptions are unchanged.

## 0.30.2

- Require subc-protocol 0.32.6 and expose attribution-only `on_behalf_of_agent` through the existing `RouteBindRequest.scope.attributes` and `ScopeDescribeReply.scope.attributes` stamps. Bind handlers and describe callers receive it only when their manifest provides `scope-on-behalf-of/v1`; it is never agent authority, consent or a per-agent handle grant.

## 0.30.1

- Add `Subscription::ended()` and the non-exhaustive `SubscriptionEnd` enum to retain terminal Response bytes, module `ErrorBody`, route close reasons, connection loss, cancellation and event-channel backpressure. The independent, repeatable waiter does not consume the existing one-shot `closed()` future or read frames; `closed()` keeps its existing `Result<(), CallError>` behavior.

## 0.30.0

- Move to subc-control 0.32. Breaking because the re-exported `PendingReloadVerdict` gained the `spec` field; no other change.

## 0.29.3

- Add `SubcConsumer::request_cancellable` (open route handle) and `call_cancellable` (managed route with `CallOptions`), returning an explicit `CancelHandle` and the unary reply future. Cancellation sends at most one empty-body Cancel with the request's channel, epoch and correlation, without discarding a late Response body or hiding a provider Error. Dropping either value sends no Cancel; dropping the future removes its pending registration. `CancelOutcome` distinguishes settled requests, closed routes and lost connections. A cancelled managed call does not retry a dead-route refusal.

## 0.29.2

- Re-export protocol 0.32.2 scope builders for capability-gated idle-only replacement and connection-held presence. `scope_epoch_in_use` is terminal in the shared contract table.

## 0.29.1

- Expose the daemon's HELLO_ACK capability list through `ModuleHandle::daemon_capabilities` and `daemon_supports`. Scope owners should check `scope-bind-proof/v1` before setting `bind_proof_sha256`.

## 0.29.0

- **Breaking dependency upgrade** to subc-protocol 0.32. `ScopeSelector` is now `#[non_exhaustive]`: build it with `ScopeSelector::new(owner, scope_ref, scope_epoch)` and add a bind proof with `.with_bind_proof(Some(proof))`. `CallError::scope_bind_proof_refusal` returns `Required` or `Mismatch` for the two terminal refusals; neither is retried. Route cache keys tell different proofs apart without storing the proof text. A scope registered without a hash ignores the proof.

## 0.28.4

- Add `SubcConsumer::supervisor_list` and typed `SupervisorListResponse` so modules can read every configured module's enabled flag and supervisor state.

## 0.28.3

- Keep the daemon's route.open refusal when the retry deadline expires while a later retry is still waiting for its answer. Before, a refusal such as `module_warming` was dropped in that case, and the caller got only "route.open did not complete (request on channel 0 timed out at its deadline)" with no `route_open_refusal()`. This happened when a busy daemon answered the final retry a few milliseconds after the deadline. The error is now `NotSent` and carries the refusal, as it already did when the deadline expired between attempts.

## 0.28.2

- Let route options add validated consumer-capability declarations alongside reverse-request handler families. The merged, sorted list is forwarded to the provider bind and kept in the route cache identity; declarations such as `content/v1` remain unverified and grant no privilege.

## 0.28.1

- Add `ModuleHandle::record_activity`, checking `activity-ledger/v1` before sending and returning typed `ActivityRefusal` classifications with the daemon's original error body. Recording does not retry after timeout or connection loss because the append may already be durable.
- Add `SubcConsumer::query_activity` and `subscribe_activity`, preserving query entries, continuation cursor and head. Both fill the supervised consumer identity from the launch environment. `ActivitySubscription` yields durable entries and exposes cursor, lag and other daemon refusal codes through `ActivityStreamError`.

## 0.28.0

- **Breaking.** Requires `subc-protocol` 0.31, `subc-control` 0.30 and `subc-transport` 0.12; upgrade direct dependencies together with the SDK. Replace `ScopeStamp` literals with `ScopeStamp::new` and its `with_*` setters, and account for optional `call_mode` in raw bind constructors and exhaustive patterns. `RouteBindRequest` exposes `call_mode`, and the SDK re-exports `CallMode` and `CALL_MODES_CAPABILITY`. Providers must refuse `CallMode::Unknown` and declare `call-modes/v1` only when they read every bind's mode and handle all three known modes.
- When an older daemon omits `call_mode`, the SDK derives it from the bind's scope: a run is codemode, a flow is flow, and a plain or absent scope is classic. `with_scope` derives the same mode in provider tests; call `with_call_mode` afterwards to override it.
- Targets declaring `call-modes/v1` receive the run deadline in `ScopeStamp::expires_at_ms`; `scope_describe` also preserves it. Targets declaring only `agent-run-scopes/v1` or `flow-scopes/v1` still admit the matching non-delegating run or flow binds, but receive neither `call_mode` nor `expires_at_ms` so older readers continue to work.

## 0.27.2

- Use the shared Windows named-pipe accessor and its process cache for HELLO and route-open; Unix remains on the inherited descriptor.
- Require the corrected Windows pipe reader and add a real supervised-module test asserting both daemon-observed and HELLO-declared `pipe` sources. Environment fallback can keep registration alive without making a broken pipe pass this test.

## 0.27.1

- Expose module-declared bus events via the SDK's re-exported `CatalogEntry`, and verify SDK readback against a real daemon.

## 0.27.0

- Add `ModuleHandle::scope_apply(generation, upsert, end)`, returning `ScopeApplyReply`. It returns `ScopeCallError::NotSupported` without sending anything when the daemon does not advertise `scope.apply`; a daemon that does also accepts `expires_at_ms`.
- **Breaking.** Requires `subc-protocol` 0.30 and `subc-control` 0.29 (it re-exports `CatalogEntry`).

## 0.26.4

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.26.3

- Add the opt-in, tests-only `test-support` feature and `test_support::ModuleHarness`. Module tests can run their real serve handler over an in-memory stand-in daemon connection, bind routes, send requests, inspect replies and channel-0 traffic, and script each `operator.confirm` answer (confirmation, all four refusals with `detail.reason`, unknown operation, or silence). Use this feature only on a dev-dependency. Silence/deadline tests can use Tokio's paused clock without changing the production confirmation timeout.
- Add `serve_from_env_with_handle(manifest, handler)`, which uses `serve`'s argv, module-id environment and launch-secret handling but returns the live `ModuleHandle` and serve future. `serve` now delegates to it, so modules that need the handle do not have to copy private launch parsing.

## 0.26.2

- Add `ModuleHandle::confirm_operator(summary, caller_route)`, which asks the daemon to confirm with the person at the machine that one in-flight request may proceed (the `operator.confirm` control operation). It returns `Ok(())` only for a `confirmed` reply. Every error, including `Unsupported` from a daemon without the operation, means the module must refuse the write. It waits at most 290 seconds and never retries; dropping its future withdraws the request. Requires subc-protocol 0.29.4.

## 0.26.1

- A control request the module cannot decode, such as a `route.bind` whose scope stamp carries an attribute this SDK does not know, is refused with `invalid_request` on its own correlation id, and the module keeps serving. Before, the decode error ended the module's serve loop, so one such bind stopped the whole module and every repeat after its restart stopped it again.

## 0.26.0

- Takes subc-protocol 0.29.0 and the matching control/transport minor releases: `ToolCallRequest.preset` and `ScopeAttributes.flow_id` are optional wire fields, but this release is breaking for Rust struct literals. Move direct protocol dependencies together with the SDK.

## 0.25.3

- A caller waiting behind another caller's `route.open` for the same route keeps waiting across a reconnect instead of failing at once, so a reconnect never starts a second, competing open; a `close_route` issued during the lead caller's retry backoff still wins, and the discarded route gets GOODBYE.
- `poll_route` returns a daemon refusal as `CallError::Module` with the daemon's code, message and detail, instead of a generic send failure.
- `catalog_list` and `spawn_snapshot` pace their transport retries with the consumer's reconnect backoff, still bounded by the call deadline, so a connection file that fails immediately cannot spin the executor; closing the consumer cancels the wait.

## 0.25.2 — 2026-10-02

- Every module using the SDK's serve helper now answers `health.check` independently of data-request slots, even under request saturation. When all 64 slots are in use and the oldest queued request has waited more than two seconds, the helper reports at least `Degraded` and puts the slot count and wait age at the start of the detail. A module's own `Degraded` or `Failing` status is kept, its own detail follows the saturation note, and its metrics are unchanged. With free slots, its report passes through unchanged.

## 0.25.1 — 2026-10-02

- Add non-exhaustive `OutcomeUnknownCause` and `CallError::outcome_cause()` through typed error sources. Reply deadlines, writer failures, consumer closure, connection loss, route ends, and internal completion failures can be distinguished without matching `OutcomeUnknown` error message text. Existing error variants, message text, retry classes, and route-end reasons are preserved.

## 0.25.0 — 2026-10-02

- Add non-exhaustive `RouteEndReason` and `CallError::close_reason()` without changing existing error variants or retry classes. Reasons follow named channels, with module fallback only for legacy pushes lacking channels; reused channels clear their history.
- Breaking: `RouteCloseReason` gains the four scope variants, and the public control dependency moves to 0.27. Exhaustive reason matches must be updated. The error getter itself is additive.

## 0.24.0 — 2026-10-01

- Minor bump because this crate's public types come from `subc-protocol`, which moves to 0.28.0 (its `ToolCallRequest` gains an `origin` field; see that crate's changelog). A consumer that also depends on `subc-protocol` directly must move both together, or two incompatible copies of the protocol types would meet. Also takes `subc-control` 0.26 and `subc-transport` 0.9, which moved for the same reason.
- Breaking: `CallOptions`, `SubscribeOptions` and `CloseRouteOptions` gain `role_versions: Option<BTreeMap<String, String>>` (default `None`), sent on `route.open`, including admitted routes opened with options. It is part of the route cache key, so a legacy route and a versioned one to the same target are never shared. An empty map is treated as `None`. A daemon without `route-role-versions/v1` drops the field, and a malformed map fails the call as not sent with `invalid_request`.
- Breaking: `RouteBindRequest` gains `role_versions`, so a module built on this SDK sees the consumer's declaration in `on_bind`.
- Breaking: `RouteBindRequest` gains `scope: Option<ScopeStamp>`, the daemon's scope stamp copied from the bind unchanged, so a module served through `serve` can tell which session a route belongs to. Until now the SDK dropped it. `None` means an unscoped route or a daemon that predates scopes.
- Breaking: `RouteBindRequest` is now `#[non_exhaustive]`, so a later field is additive. Code outside this crate builds one with `RouteBindRequest::new(handle, target, identity)` and the `with_principal`, `with_consumer_capabilities`, `with_role_versions`, `with_admission_facts` and `with_scope` setters.
- New `RouteHandle::detached(channel, epoch)`: a handle that belongs to no connection, for building a `RouteBindRequest` in a module's own tests. Every operation on it that would reach a connection fails with the stale-route error (`SubcModuleError::StaleRouteHandle`, `CallError::StaleRouteHandle`, or `ReverseRequestRegistrationError::NotConsumerRoute`) and sends nothing.

## 0.23.7 — 2026-10-01

- Builds again on toolchains older than Rust 1.99. 0.23.6 replaced the deprecated
  `AtomicU64::fetch_update` with `try_update` and declared `rust-version = "1.99"`, because
  `try_update` does not exist before 1.99. The two counters now use a compare-exchange loop, which
  builds on every toolchain and is warning-free on 1.99, and the `rust-version` declaration is gone.

## 0.23.6 — 2026-10-01

- Builds warning-free on Rust 1.99, where `AtomicU64::fetch_update` is deprecated: the two counters
  use `try_update` instead. This release requires Rust 1.99 (`rust-version = "1.99"`); 0.23.7 lifts
  that requirement.

## 0.23.5 — 2026-10-01

- `ModuleHandle::scope_sync(generation, scopes)` registers the module's full scope set
  (`scope.sync`) and returns a `ScopeSyncReply` (`generation`, per-record `results` in request
  order, `ended`). `ModuleHandle::scope_describe(owner, scope_ref)` reads one scope's state
  (`scope.describe`) and returns a `ScopeDescribeReply` with every field of the daemon's answer
  (`status`, `scope_epoch`, `daemon_incarnation`, `owner_synced`, `owner_configured`, `scope`).
  Both reply structs are `#[non_exhaustive]`. Each call is gated on its own op in the
  HELLO_ACK's `subc_ops` and fails with `ScopeCallError::NotSupported` without sending anything
  when the daemon does not list it.
- New `ScopeCallError`. A daemon refusal is `Refused { code, message }` with the code exactly as
  sent, so callers match it against `subc_protocol::error_codes` (`SCOPE_SYNC_STALE`,
  `SCOPE_SYNC_NOT_AUTHORITY`, ...); `ScopeCallError::code()` returns it. A reply for a different
  op is `Protocol`. `ScopeCallError` is `#[non_exhaustive]`, so a later failure kind is additive.
- New `scope-owner` example: a supervised module that runs scope syncs and describes from a
  script, used by the real-daemon tests because only a daemon-launched module holds sync
  authority.

## 0.23.4 — 2026-09-30

- `ModuleHandler::on_draining(reason, deadline)` delivers the daemon's `module.draining` notice,
  which it sends before stopping a module (restart, reload, disable, swap, daemon shutdown). Until
  now the SDK dropped it. The hook runs on its own task, so pings and GOODBYE keep flowing while it
  works, and it is always called before the GOODBYE that ends the drain is handled. `deadline` is
  a wall-clock "no later than", never a grant. It has a no-op default, so existing handlers are
  unaffected. The trait docs describe how to hold a drain open until background work finishes (a
  `Busy` self-signal anchored to health gauges); the `echo-module` example declares one.
- A channel-0 push the SDK cannot decode is ignored, and the first on each connection is logged at
  warn through `tracing` (a new dependency); the connection stays up.

## 0.23.2 — 2026-09-30

- Add `SubcConsumer::open_route_scoped(target, identity, scope, opts)`, which opens (or reuses) a
  managed route admitted under a daemon scope, so a carrier can open its onward route for a
  session and the provider's bind is stamped with the scope. `ScopeSelector` is re-exported from
  `subc-protocol`; build its `owner` with `subc_protocol::Principal`. The managed route cache keys
  a scoped route by the whole selector (owner, ref and epoch): a route opened under one scope or
  epoch is never returned for another, and scoped and unscoped opens never share a route. After
  the route closes, the next call with the same selector reopens it under that selector.
- `scope_not_synced` and `scope_changed` refusals are retried within the call's deadline;
  `scope_ended`, `scope_not_live`, `scope_epoch_required` and `scope_not_carrier` end the call at
  once, with the code in `CallError::route_open_refusal()`. A `route.closed` push with any of the
  four `scope_*` reasons classifies as `RouteCloseDisposition::MustNotReopen`.

## 0.22.1 — 2026-09-29

- `ModuleHandler::on_connection_end(end)` reports how a served module's daemon connection ended:
  `ConnectionEnd::Goodbye` (a channel-0 GOODBYE, a planned stop), `Eof` (closed without one),
  `Reset`, or `Closed` (the module closed it through its `ModuleHandle`). `serve` still returns
  `Ok(())` for all four; the hook is called once, after in-flight requests are cancelled, and
  not when serving ends with an error. It has a no-op default, so existing handlers are
  unaffected.

## 0.22.0 — 2026-09-28

- `DEFAULT_ROUTE_RETRY_DEADLINE` is 90s instead of 30s. A module restart drains its routes (up to
  30s), waits for the old process to stop, then boots the new one; one measured restart kept
  routes refused for 62.5s. The retries still end at `CallOptions::timeout` when that comes
  first, and its default is still 30s, so a caller that wants to ride out a restart raises both.
- Retry delays are jittered ("equal jitter": half of each delay is kept, half is random), so
  routes refused together no longer retry in lock step. `RetryBackoff` is shared by route.open
  retries and reconnects, so reconnect delays are jittered too.
- At most `MAX_ROUTE_OPENS_IN_FLIGHT` (8) route.open requests are outstanding per consumer
  connection, matching the daemon's per-connection limit. Further opens, managed and admitted,
  wait their turn in order, and the wait counts against the same deadline as the open.
- When the retry deadline runs out, `CallError::route_open_refusal()` is the most informative
  refusal seen during the retries instead of the last one: `module_reloading` / `module_warming`
  first, then `delegation_not_registered`, then admission pressure (too many binds in flight),
  then anything else. The error stays `NotSent`. Its message names the refusal's `detail.reason`
  when present, and the most recent refusal after it when that differs.
- Fix a served module that never exited after a channel-0 GOODBYE (for example on
  `supervisor.restart`) until the daemon's stop budget killed it. Requests still in flight held
  the writer open. When the connection ends for any reason (GOODBYE, EOF, reset, close), every
  in-flight request's cancellation token is now cancelled, and requests still waiting for a
  handler slot are dropped.
- The serve future now waits at most 2s for the writer to flush after the connection ends, then
  aborts it, so a handler that ignores cancellation cannot keep the process alive past its stop.
- Add `ModuleHandle::closed()`, a future that resolves once the module's connection has closed,
  so a module can end its own background work, and `ModuleHandle::is_closed()`.

## 0.19.4 — 2026-09-24

- `SpawnStreamError`'s three coded variants carry `body: Box<ErrorBody>` instead of an inline
  `ErrorBody`, so the error stays small when workspace features enlarge `ErrorBody`. Reading
  `body.code` or `body.detail` is unchanged. 0.19.3 was never published.

## 0.19.3 — 2026-09-24

- Add `SubcConsumer::spawn_subscribe`, the typed `supervisor.spawn_subscribe` call on channel 0:
  it takes an optional `SpawnCursor` and returns a `SpawnSubscription` whose `next()` yields each
  `SpawnEvent` until the stream ends. The daemon's two cursor refusals and the terminal it sends a
  subscriber that fell behind come back as `SpawnStreamError::CursorIncarnationMismatch`
  (`spawn_cursor_incarnation_mismatch`, with `current_daemon_incarnation`),
  `SpawnStreamError::CursorTooOld` (`spawn_cursor_too_old`, with `oldest_retained_cursor`) and
  `SpawnStreamError::SubscriberLagged` (`spawn_subscriber_lagged`, with
  `first_undelivered_cursor`), each after every event the daemon queued before it;
  `SpawnStreamError::code()` reads the code. The three codes are exported as constants, and
  `SpawnEvent` and `SpawnEventKind` are re-exported from `subc_client_rs::consumer`.
- Dropping or unsubscribing a channel-0 subscription now sends its Cancel frame, so the daemon
  releases the spawn subscriber. Before, a Cancel was sent only for a subscription on a route.

## 0.19.2 — 2026-09-24

- No library change. The real-daemon tests now start the daemon binary with
  `SUBC_CGROUP_PLACEMENT=disabled`, so they no longer leave cgroups under the caller's own.

## 0.19.1 — 2026-09-24

- Add `SubcConsumer::spawn_snapshot`, the typed `supervisor.spawn_snapshot` call: it returns the
  daemon's `SpawnSnapshot` (live processes with their spawn generations, and the cursor), and a
  daemon refusal as `CallError::Module` with its code. `SpawnSnapshot`, `LiveSpawn` and
  `SpawnCursor` are re-exported from `subc_client_rs::consumer`, so a caller needs no direct
  `subc-control` dependency.

## 0.18.7 — 2026-09-24

- Require subc-protocol 0.25.2. Since 0.18.5 this crate calls
  `error_codes::is_established_route_dead`, which first appears in 0.25.2, but it still accepted
  0.25.0, so a consumer locked at 0.25.0 or 0.25.1 got a compile error instead of an upgrade.

## 0.18.6 — 2026-09-24

- `open_route_with_admission_facts` (and its `_and_options` form) now keeps the daemon's refusal
  code and detail, like the plain route open: read them with `CallError::route_open_refusal()`.
  Before, a refused admitted open became an uncoded `NotSent`, so a caller could not tell
  `module_warming` (retry shortly) from `admission_facts_not_permitted` (configuration).

## 0.18.3 — 2026-09-23

- `PolicyResolver` releases a subject's state once its verdicts expire: expired
  cache entries are removed, and the subject's resolver route is closed and its
  `policy.subscribe` task aborted. Before, both grew with every subject seen.
- Add `PolicyResolver::footprint` and `PolicyResolverFootprint` (entries, held
  routes, running subscription tasks).

## 0.7.2 — 2026-08-24

- Add capability-addressed provider resolution from the catalog capabilities mirror.
- Add deterministic plural resolution, singular ambiguity/unprovided errors, and local identifier validation.
