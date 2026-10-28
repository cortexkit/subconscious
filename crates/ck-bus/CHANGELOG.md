# Changelog

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
