# Changelog

## 0.10.1

- Hold federation watches until a decision so ck-bus restarts leave the hub and its leaf links running.
- Recheck JWT renewal against the wall clock after sleep, and refuse to sign leaf connects with a JWT expiring within a minute.
- Revoke rotated leaf keys immediately and retain recent keys so removing a computer revokes them all.
- Skip damaged census rows at boot and replace them on issuance, revoking recoverable keys.
- Retry failed delivery-reader moves even when the module catalog has stopped changing.
- Drop stale connection targets when the system connection reaches a different NATS server.
- Reconcile hub stream limits in place, preserving stored messages and warning about immutable drift.

## 0.10.0

- Require cortexkit-bus-naming 0.5.4. Reconcile ROOM, WAKE and PEER in place with seven-day retention, discard-new, and exact inbound federation bindings. Provision the hand-off and shared hub streams and the federation-only readers.
- Apply box-account publish denies to every user. Revoke changed logins at boot so clients fetch the new permissions, including cerebellum's raw run-event subscriptions.
- Issue and renew scoped hub credentials for eligible modules over the existing local server, with separate census entries and access limited to each module's hub streams.
- Move the delivery readers and session-liveness bucket to the sole registered prefrontal authority, in either direction, without blocking bus startup. Preserve work-queue messages and resume ROOM after the old acknowledgement floor.
- Restrict each federation-only WAKE and PEER reader to its named authority. Keep those readers out of the agent-durable list.

## 0.9.1

- `fed.remote_state` reports `connected_via_bridge` only when the live leaf link's peer is
  the relay bridge's port. An open bridge alone no longer marks a direct link as bridged.

## 0.9.0

- Require cortexkit-bus-naming 0.5.0: prefrontal owns the delivery and EVENT
  readers and session-liveness bucket. Keep accepting both attested authority ids
  for rollback, and keep other modules' filters on both publisher names.
- Move empty `m_prefrontal-core` delivery readers to `m_prefrontal` on INBOX,
  CORE_WORK and ROOM. Refuse startup with the stream and pending/ack-pending
  counts in health if any old reader still has work; leave its cursor intact.
- After all delivery readers are provisioned, delete the retired
  `prefrontal-core.session-liveness` bucket. A refused move leaves the old bucket
  and its entries untouched. Keep the old EVENT durable for later retirement.
- Refresh the permission and provisioning goldens from naming 0.5.0; resolve
  cortexkit-bus-nats 0.5.2 and cortexkit-bus-inmemory 0.4.1 for its naming support.

## 0.8.1

- Require cortexkit-bus-naming 0.4.8 so event and live readers accept both
  `prefrontal-core` and `prefrontal` publisher subjects before the module rename.
- Accept either daemon-attested reserved id for delivery authority and agent-durable
  operations. Keep all existing durables, streams, buckets and subjects; only the
  authority's own event and live publish prefixes follow its attested id.

## 0.8.0

- Require cortexkit-bus-naming 0.4.7; use the `aft`-specific send-state filter
  and remove the `astrocyte` payer-row reader from the permission fixtures.
- Require cortexkit-bus-naming 0.4.6, whose HUB_IDLOG refuses an append when
  storage is full instead of dropping its oldest entry.

- Add this machine's side of the leaf link to the user's hub. `ckbus.leaf_watch`
  (a watch) sends the local server's wrapper `{generation, remotes}` on open and
  after each change, and `ckbus.leaf_sign` signs the hub's 15-character connect
  nonce with the vault's `signing:bus-leaf:1`, answering `{jwt, signature_b64}`.
  Both are admitted only to `reserved:nats-server`.
- Follow callosum's hub row (`callosum.hub_watch`, with `callosum.hub_read` every
  60 s while the watch is down). Exactly one hub is allowed. A peer hub is dialled
  at its candidate addresses with its SPKI pin, falling back to a relay bridge from
  `callosum.leaf_bridge_open` after a failed round. A self hub waits for the local
  hub, and the hosted hub is not dialled yet.
- Get the leaf JWT from the hub computer (`ckbus.hub_leaf_issue` on
  `fed:<fingerprint>:ckbus`), retrying from 1 s up to 5 min. Renew it at half its
  life and bounce the leaf connection once at a quiet moment (within 1 h), and
  request a new one when the leaf key changes.
- Create the federation account `fed_<machine id>` (recorded in
  `fed_account.json`, JetStream enabled, no streams or signing key yet) once a hub
  is named. The leaf remote binds only that account and denies `fedlocal.>` and
  `fedout.>` both ways.
- The system user may read the server's `LEAFZ` report (from cortexkit-bus-naming
  0.4.5's system grant, which ck-bus now pins), so the link's state and
  connection id are known. Health gains `fed.hub_kind`, `fed.remote_state` and
  `fed.jwt_expires_at_ms`; every existing health field is unchanged.
- Add the hub computer's side. When callosum's hub row names this computer
  (`source: "self"`), and again only after a hub key rotates, hub setup signs the
  hub operator, its system account and the user's hub account with the vault's
  `signing:bus-hub-operator:1`, issues the listener certificate for
  `signing:bus-hub-tls:1` (kept in `hub.json`, so restarts serve the same bytes),
  and writes `<cortexkit data dir>/nats-hub/server.conf` (full resolver, JetStream
  domain `hub`, `leafnodes { no_advertise: true }`, one loopback admin listener on
  17422). Once the hub instance answers, it pushes the hub account and creates
  `HUB_IDLOG`, `KV_residence` and `OBJ_cuts`.
- `ckbus.hub_server_watch` (a watch, `reserved:nats-hub`) sends `{generation}`, plus
  `hub_listener: {port: 7422, cert_der_b64}` once setup has written the
  configuration; the listener is withdrawn when this computer stops being the hub.
  `ckbus.hub_tls_sign` (`reserved:nats-hub`) signs TLS 1.3 server
  `CertificateVerify` content with the hub TLS key, at most 20 a second with a
  burst of 40. `ckbus.hub_identity_read` (`reserved:callosum`) answers
  `{spki_sha256, leaf_port}`, or `{hub: null}` on a computer that is not the hub.
- `ckbus.hub_leaf_issue` answers only callosum's forward from a live paired
  computer (harness `fed-computer:<fingerprint>`). It takes that computer's
  machine id from `callosum.peer_keys_read`, signs a 24 h leaf JWT with
  `signing:bus-hub-account:1` carrying that box's leaf grant, creates its
  `HUB_INBOX_{box}`, and records the issue under the fingerprint. The hub
  computer's own leaf gets its credential the same way, locally.
- Follow `callosum.paired_devices_watch`. A computer named by a tombstone or gone
  from the list has its leaf key revoked in the hub account, which the hub server
  applies by closing that leaf connection; a later issue for it is refused.
- Declare the `peer-sharing/v1` share `hub`, holding only `ckbus.hub_leaf_issue`,
  for computers. Health gains `fed.hub_setup`, `fed.hub_setup_error`,
  `fed.hub_leafs_connected` and `fed.hub_issued`.

## 0.7.4

- Require subc-client-rs 0.41.2, subc-control 0.42.0, subc-daemon 0.50.0 and cortexkit-bus-testkit 0.9.4 for the settings release.

## 0.7.3

- Adopt cortexkit-bus-naming 0.4.4: the `astrocyte.multiplier` bucket takes
  values up to 64 KiB (8 MiB in total), up from 8 KiB, so per-plan multiplier
  entries of 14 to 16 KiB fit. An existing bucket's limits are raised in place
  at boot; its keys and history stay.

## 0.7.2

- Record a digest of each issued module grant in the census (ck-bus's bucket
  of issued bus credentials). On restart, revoke only credentials whose
  permissions changed or were removed, including older entries without a
  digest, in one batched account update before readiness. Unchanged
  grants keep their connections; credential retirement leaves durable cursors
  and stored data intact.

## 0.7.1

- Adopt cortexkit-bus-naming 0.4.3: the retired `prefrontal-routing` id gets no permissions or durables, and Cerebellum's display event filter follows tool-display's `display_record_ready` name.

## 0.7.0

- Adopt cortexkit-bus-naming 0.4.2, cortexkit-bus-nats 0.5.0 and
  cortexkit-bus-inmemory 0.4.0. Provision the seven declared module KV buckets,
  reconciling their limits in place without recreating stored keys. Refuse
  incompatible immutable backing-stream settings with a named error.
- Bound each module bucket's watch consumers to eight times its owner-and-reader
  count. Reconcile an existing cap in place, as supported by NATS 2.15.0, without
  recreating the stream or keys. Return durable-plan rows and the orphan count as
  a typed result on the boot path.
- Grant module bucket writes only to their owner and bucket-scoped read/watch
  APIs only to owners and declared readers; keep census isolation unchanged.
  Refresh the local permission golden for these grants and the expanded event
  consumer table, including multiple owner-namespaced durables per module.
- Include naming 0.4.2's additional event readers and view buckets, including
  astrocyte attribution facts, and regenerate permission and provisioning fixtures.
- Report retired module durables as orphaned in the bootstrap plan log and health
  detail without deleting their cursors. Keep `m_prefrontal-routing`, which is
  still declared by naming. Correct the documented message-plane count to nine.

## 0.6.12

- Require Rust client 0.41.0 for the public registration-channel adoption API.
- Raise the daemon requirement to 0.49.0 for opt-in phone build facts on route
  binds, and align the co-bumped protocol and Rust client requirements.
- Raise subc-control to 0.41 and subc-daemon to 0.49 for scope lifecycle watches, with matching minimum dependency versions for the Rust SDK and sibling crates; no direct behavior changes.

## 0.6.11

- Raise the test-only subc-daemon requirement to 0.48.0 for typed script entries.

## 0.6.10

- Raise the Rust client requirement to 0.40.2 and testkit to 0.9.2 for process-local
  module re-exports.

## 0.6.9

- Raise the Rust client requirement to 0.40.1 and testkit to 0.9.1 for additive
  module control hooks.

## 0.6.8

- Raise daemon and control test dependencies for script-aware module launches.

## 0.6.7

- Dependency-only upgrade to client/control 0.40, daemon 0.46 and testkit 0.9;
  the binary uses these types internally and bus behavior is unchanged.

## 0.6.6

- Let the SDK attest supervised vault calls implicitly, using a connection
  claim or the legacy launch nonce instead of a conflicting explicit identity.
  This restores bootstrap when the configuration gate is not stamped but the
  module uses the private registration channel.
- Require client 0.39.1 for launch-nonce provenance diagnostics and daemon
  0.45.1 for the native Windows admission fixtures.

## 0.6.5

- Raise SDK and test-helper requirements to client/control 0.39, protocol
  0.37, transport 0.18, daemon 0.45 and bus-testkit 0.8. This binary-only
  dependency update keeps bus behavior unchanged and does not enable stamping.

## 0.6.4

- The routing module's new id, `striatum`, gets the same bus access as `prefrontal-routing`, from the shared bus naming tables (cortexkit-bus-naming 0.3.4) that define each module's event durable and live-hint reads: its own event durable `m_striatum` reading Insula's quota events, the runners' content-free `step.finished.v1` live hints, and the standard participant grants for its own id. Bootstrap creates `m_striatum` beside `m_prefrontal-routing`, which is unchanged. The new id stays reserved and disabled in the daemon until the routing module moves.

## 0.6.3

- `m_prefrontal-core` also reads Basal's fifth flow-instance lifecycle event, `event.basal.flow_instance_rescheduled.v1` (cortexkit-bus-naming 0.3.3). Bootstrap adds the subject to the existing durable's filter in place and keeps its position in the stream. Basal already publishes under `event.basal.>`; core's delivery-authority grant gains the matching content-free hint subject, `live.basal.flow_instance_rescheduled.v1`, as for the other four.

## 0.6.2

- Require subc-client-rs 0.38.0 for scoped managed calls and watches. This dependency-only patch keeps wire behavior unchanged.

## 0.6.1

- Refresh daemon/control and Rust-client requirements for the extensible supervisor resource API. This dependency-only release keeps bus wire behavior unchanged.

## 0.6.0

- Adopt the coordinated protocol 0.36 scope-type dependency family. **Rust migration:** add a wildcard arm to any `ScopeKind` match; `ScopeKind` is now non-exhaustive and includes `Federated`. Existing wire traffic to non-declaring providers is unchanged.

## 0.5.6

- Raise dependency floors for the resource-accounting wire release.

## 0.5.5

- Adopt cortexkit-bus-naming 0.3.2, replacing the routing change in 0.5.4,
  which was never placed (deployed). `prefrontal-routing`'s durable returns to Insula quota
  events only; its exact runner step hints are plain live subscriptions, with no
  additional JetStream rights. Bootstrap updates existing durable filters in
  place, preserving their delivered positions.
- Derive live subscriptions from both code-reviewed reader allowlists and refresh the
  permission goldens. The removed `thalamus-pi` runner no longer contributes any
  reader or permission rows; existing orphaned durables are not deleted.

## 0.5.4

- Adopt cortexkit-bus-naming 0.3.1 so `prefrontal-routing` can read the exact
  `broca.step.finished.v1` and `thalamus-harness.step.finished.v1` events and live
  hints for per-model speed statistics. Bootstrap updates its existing event
  durable's filters in place, preserving its delivered position; other live
  subjects remain refused.

## 0.5.3

- Test only: the revocation tests' severed-socket check waits for the client's
  `disconnected` event instead of polling its connection state. A kicked client
  whose credential is still valid reconnects at once, so the state poll could
  miss the disconnect and fail at random. No change to the binary.

## 0.5.2

- Move the subc-daemon dev-dependency requirement to 0.40.0; no behavior change for the binary.

## 0.5.1

- Adopt compatible dependency requirements for the updated route-open control types; no behavior change.

## 0.5.0

- Adopt cortexkit-bus-naming 0.3.0, bus-trait 0.3.1, bus-nats 0.4.0 and
  bus-inmemory 0.3.2. Bootstrap creates INBOX, CORE_WORK and DELIVERY_DEAD and
  the durables specified by bus-naming, including `m_prefrontal-core` on the two command
  streams and `c_ckbus_dead` on both dead-letter streams.
- Extend the existing stream-limit reconciliation path: EFFECT loses its 24-hour
  age eviction and gains a 16 KiB message cap through an in-place update that
  preserves messages and stream identity. Both dead-letter streams explicitly
  use a 120-second duplicate window. Unset message caps and duplicate windows
  are omitted from the wire configuration, not sent as zero.
- Refresh the vendored permission golden and provenance from bus-naming 0.3.0:
  only new delivery publish, durable and refusal rows. The live-only golden stays
  unchanged because these streams do not carry events or live hints. The minor
  bump marks the new reliable-delivery provisioning contract
  and adoption of the breaking shared naming API.

## 0.4.22

- Complete revocations whose recorded kick targets belong to an old server
  instance instead of retrying them forever on a missing responder. Compare targets
  with the connected system server's ID and log skipped targets; unknown server IDs
  and other kick failures still defer.

## 0.4.21

- Take cortexkit-bus-naming 0.2.10: bootstrap provisions `m_cerebellum`, an
  event reader on the runners' exact `turn_started.v1` and `turn_ended.v1`
  subjects from `thalamus-harness` and `thalamus-pi`. The browser module's
  credential can pull that durable and subscribe to the same four subjects as
  live hints. No existing durable's filters change.

## 0.4.20

- Take cortexkit-bus-naming 0.2.9: bootstrap provisions the Pi runner's event
  reader, `m_thalamus-pi`, on the compaction provider's `compaction.>` events,
  adds the Pi runner's `compaction.>` events to `m_magic-context`'s filters and
  its `step.finished.v1` subject to `m_astrocyte`'s. Existing durables keep
  their cursors when their filters change.

## 0.4.19

- Declare `bind-no-project/v1`. ck-bus never reads a project from a route's bind,
  so the daemon now sends it none instead of filling the `/` placeholder.

## 0.4.18

- Upgrade to protocol 0.34 and Rust client 0.33. Credential routes use project-free identities instead of attributing the connection-file directory as a project.

## 0.4.17

- Test-only: prove that health answers while a broker call is still pending,
  rather than asserting a 500 ms ceiling on health-response latency.
- Test-only: give the reply-permission probe's healthy control its own round
  budget, independent of the production probe's short failure-detection round.
- Health summaries use plain sentences instead of untranslated detail keys.

## 0.4.16

- Test-only: the activity-identity test's relay treats a reset or broken
  pipe from the exiting supervised reader as the end of the connection, as
  it already treated EOF. On Linux the reader's socket can close with unread
  bytes and reset, which panicked the relay and failed the test. No module
  change.

## 0.4.15

- Builds against subc-daemon 0.38.0. No change to the module itself.

## 0.4.14

- Take `cortexkit-bus-naming` 0.2.8, whose reader table gives `astrocyte` the
  runners' `step.finished.v1` events. Live subscriptions mirror the event
  reader's filters, so Astrocyte may now subscribe to
  `ck.{acct}.live.{broca,thalamus-harness,thalamus}.step.finished.v1`; before,
  those subscriptions were refused.

## 0.4.13

- Live subjects, `ck.{acct}.live.<module>.<kind>`: plain publishes, delivered only to
  current subscribers and captured by no stream. A credential publishes live only on
  its own module's subjects, and its live subscriptions mirror its event durable's
  filters exactly, so the live root never widens what a module can observe.

## 0.4.12

- Take subc-client-rs 0.32: a module handler panic now fails serving instead of being silently ignored. After the module handshake completes, daemon disconnects remain clean exits on every OS, with EOF/reset reported through the connection-end hook; handshake disconnects remain errors.

## 0.4.11

- Takes subc-protocol 0.33 and the matching SDK, daemon, control, transport and bus-testkit releases (`ToolCallRequest` gains the optional `output_refs` member). No behavior change.

## 0.4.10

- Test-only: initialize the new optional scope folder/project activity fields in mirror fixtures. Runtime behavior is unchanged.

## 0.4.9

- Test-only: make the activity mirror backoff test independent of real time before Tokio time is paused, and verify the configured retry delay.

## 0.4.8

- Fixed: the daemon's activity ledger never reached the bus. The bus module's grant did not allow publishing `ck.{acct}.event.subc.activity.v1`, so the server refused every copy and the mirror retried every 15 s without storing anything. The grant (from `cortexkit-bus-naming` 0.2.7) now allows exactly that subject, and a live-server test checks that the copy is stored and that publishing another module's events is still refused.
- `m_prefrontal-core` also reads Basal's four flow-instance lifecycle events (`event.basal.flow_instance_created.v1`, `_extended`, `_ended`, `_expired`). Bootstrap widens the existing durable in place and keeps its position on Broca's events.

## 0.4.7

- Create the `m_thalamus` durable reader so the Claude Code gateway can read Magic Context's compaction notices, and widen `m_magic-context`'s filter to also read `event.thalamus.compaction.>`. Bootstrap's create-or-update widens the existing durable in place, so Magic Context keeps its reading position. Takes `cortexkit-bus-naming` 0.2.6.

## 0.4.6

- Create durable readers for compaction events before modules connect:
  `m_magic-context` reads thalamus-harness and broca compaction events, while
  `m_thalamus-harness` and `m_broca` each read magic-context compaction events.
- Use the commons-maintained tryhard 0.5.2 copy, which omits Tokio's test clock
  from normal dependencies so production bus binaries do not link test-only time.

## 0.4.5

- Take cortexkit-bus-naming 0.2.4, whose event-consumer table adds
  `prefrontal-routing` reading insula events: bootstrap creates its durable
  `m_prefrontal-routing` and issues its own-durable pull, ack and info grant.

## 0.4.4

- Take cortexkit-bus-naming 0.2.3: bootstrap the closed event-consumer table's
  `m_prefrontal-core` (broca events) and `m_astrocyte` (insula events), and issue
  their own-durable pull, ack and info grants. Event names support 1–4 tokens.
- Reconcile module consumers in place, retaining acknowledgement floors and
  refusing immutable configuration changes without deleting a durable.
- Raise existing event streams' per-subject cap from 10,000 to 100,000 in place,
  preserving stored messages and logging the old and new limits.
- Take `cortexkit-bus-trait`, `cortexkit-bus-inmemory` and `cortexkit-bus-nats` 0.3.0 from crates.io instead of a commons git revision, so the build carries one copy of `cortexkit-bus-naming` (0.2.3) rather than also its 0.2.1 through the git-pinned crates.

## 0.4.3

- Move to subc-control 0.32 and subc-client-rs 0.30. No behavior change.

## 0.4.2

- Reuse cortexkit-bus-testkit for acceptance tests' NATS bootstrap, signing
  fixtures, ckbus configuration and machine ids instead of maintaining a private
  broker rig. Existing acceptance daemons remain in-process.

## 0.4.1

- Basal (the flow engine) may publish its own module events on
  `ck.{acct}.event.basal.>`, as every other participant may on its own. Its
  first lifecycle event was refused as a permissions violation, and the server
  closed the connection its event reader shares, so the reader delivered nothing.
  It still cannot publish another module's events.
- Take the commons bus crates at 0.3.0 (bus-naming 0.2.1). Publishes now return a
  receipt that reports a deduplicated publish; bus behavior is otherwise unchanged.

## 0.4.0

- Upgrade the Rust SDK and protocol dependencies for non-exhaustive scope selectors and per-scope bind proofs. Bus behavior is unchanged.

## 0.3.18

- Raise the daemon test dependency to `subc-daemon` 0.35, whose public config types gained the `privileged_operator_prompt` setting. Bus behaviour is unchanged.

## 0.3.17

- Reinstate the activity-ledger mirror on `ck.<account>.event.subc.activity.v1`, preserving each entry's `(daemon_incarnation, seq)`. Advance `activity_cursor.json` only after broker storage acknowledgement, and back off and resume from it on failure or restart.
- Start fresh or damaged cursors at query head without backfill. When the daemon refuses a saved cursor or ends a feed that fell behind, replay the entries it still keeps in its ledger file, in sequence order, then follow from its current head. Entries the daemon already deleted under its 30-day or size limit can't be replayed; the mirror logs each such gap with its first and last sequence numbers.
- Share the vault's supervised launch-identity helper with the mirror preflight. SDK activity reads carry `reserved:ckbus`; missing identity warns once about unverified local reads, and nonce-read errors stop initialization.
- Test-only: tests wait for the mirror's checkpoint signal instead of reopening the cursor file, which on Windows made the checkpoint's file replacement fail.
- Scope test mutex guards before asynchronous waits so all-targets strict clippy accepts the mirror tests.

## 0.3.16

- Withdraws 0.3.15, which never shipped: the activity ledger's bus copy returns in a later release, once it passes on Windows. The code is identical to 0.3.14.

## 0.3.14

- Test-only: use paused Tokio time to check that health checks respond while a probe is hung, without measuring elapsed wall-clock time.

## 0.3.13

- Update the internal bus module to `subc-protocol` 0.31 and `subc-client-rs` 0.28, with control, transport and daemon test dependencies upgraded together. Bus consumers need no request changes; this dependency update does not declare `call-modes/v1` or implement provider mode handling.

## 0.3.12

- Expect the shared accessor's named-pipe nonce source in supervised Windows provenance tests; Unix still expects the descriptor source.

## 0.3.11

- Use `cortexkit-lease` for durable replacement of state and install files.

## 0.3.10

- Moves to `subc-protocol` 0.30 and `subc-client-rs` 0.27. No behaviour change.

## 0.3.9

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.3.8

- Build time only: the build script watches the workspace `Cargo.lock` only when it exists. Built from a registry or vendored copy there is no workspace lock two levels up, and cargo treats a missing watched path as always changed, so every consumer rebuilt this crate on every build.

## 0.3.7

- Build time only, no runtime change: the build script, which stamps the git commit into the binary, no longer recompiles the crate on every cargo run in a git worktree. It watched `.git/HEAD` and `.git/refs` paths that don't exist when `.git` is a worktree's file, and cargo treats a missing watched path as always changed. It now resolves paths with `git rev-parse --git-path` and watches only the existing HEAD, current branch ref, `packed-refs` and index, so a rebuild happens only when the checked-out commit or the index changes.

## 0.3.6

- `install-apply --conf-only` takes two new flags, which `ck setup` uses to upgrade an existing install. `--keep-existing` keeps a monitoring listener that is already on another port: it reports `"status": "kept"` and a `health_url` for that port instead of refusing, and it cannot be combined with `--monitor-port`. `--dry-run` writes nothing and reports `"would apply"` where a write would happen. Without these flags, behaviour is unchanged. A non-loopback listener is still refused under every flag.

## 0.3.5

- Update the test daemon dependency to subc-daemon 0.32 whose macOS launches give each supervised module its own privacy identity.

## 0.3.4

- `install-apply` enables loopback-only NATS HTTP monitoring (`--monitor-port`, default 18222), refuses a collision with the client port, and reports the plain `/healthz` URL for daemon health probes. `--conf-only` upgrades the monitoring line in an existing rendered `server.conf` without rewriting JWTs, keys or resolver data, preserving its mode and leaving identical configurations untouched.

## 0.3.3

- Updates the SDK, daemon and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. No other behavior change.

## 0.3.2

- A credential census read interrupted by a reconnect of ck-bus's own bus connection is replayed instead of timing out, within the same five-second budget. A census that cannot be read for any other reason (its stream is missing, or the broker returns an error or no reply in time) still makes the issuance fail with `ckbus_census_unavailable`; it is never treated as an absent entry.

## 0.3.0 — 2026-10-02

- Update Rust client and control dependencies to 0.25 and 0.27 for per-route close reasons and channel-addressed lifecycle pushes.

## 0.2.1 — 2026-10-01

- Takes `subc-protocol` 0.28.0, whose `ToolCallRequest` gains an `origin` field (see that crate's changelog), and the matching minor releases of the crates built on it: `subc-client-rs` 0.24, `subc-control` 0.26, `subc-transport` 0.9 and `subc-daemon` 0.27. No other change (a test sets the new `role_versions` route.open field to `None`).
