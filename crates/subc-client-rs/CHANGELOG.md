# Changelog

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
