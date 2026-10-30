# What a module sees

A module is a separate process that the daemon supervises and routes to. This
document describes the module's side of that relationship: how it is started,
how it registers, what it receives when a client opens a route to it, how its
health is checked, and how it is stopped and replaced. The frame format is in
[`wire-contract.md`](wire-contract.md) and the channel-0 operations are in
[`control-plane.md`](control-plane.md).

Most modules use an SDK, which implements everything below:
`crates/subc-client-rs` for Rust and `clients/subc-client`
(`@cortexkit/subc-client`) for TypeScript. A module that implements the wire
itself must carry the behaviours listed in
[`client-affordances.md`](client-affordances.md).

## 1. Configuration

A module is supervised when it has an entry under `modules` in
`<config home>/cortexkit/subc.jsonc` (`crates/subc-daemon/src/daemon_config.rs`):

```jsonc
{
  "version": 1,
  "modules": {
    "example": {
      "program": "/path/to/ck-example",
      "args": [],
      "env": { "EXAMPLE_MODE": "fast" },
      "enabled": true,
      "reserved": false,
      "protocol": "subc",          // or "none"
      "overlap": "exclusive",      // or "safe"
      "drain_timeout_ms": 30000,
      "route_bind_relay_timeout_ms": 12000,
      "restart": { "max_restarts": 3, "window_secs": 600, "backoff_ms": 100, "max_backoff_ms": 30000 },
      "health": { "cadence_ms": 30000, "deadline_ms": 5000, "failure_threshold": 3,
                  "on_degraded": "report", "on_failing": "report", "critical": false }
    }
  }
}
```

The values shown are the defaults; every key except `program` is optional.
The meaning of each timeout, and the keys that may also appear at the daemon
root, are in [`daemon-config.md`](daemon-config.md).

- `reserved: true` means only the process the daemon spawned may register this
  module id (section 3). Use it for modules that hold authority, such as a
  credential vault. `reserved_prefixes` extends the protection to every id
  under a prefix. `reserved: true` together with `protocol: "none"` is refused
  when the configuration is read, because such a module never registers. Test:
  `reserved_true_with_protocol_none_is_refused_with_the_reason` (`crates/subc-daemon/src/daemon_config.rs`).
- `protocol: "none"` declares a program that speaks no subc wire. The daemon
  supervises its process and nothing else: it passes no `--subc` argument and
  no launch nonce (it still sets `SUBC_MODULE_ID`), never sends it
  `health.check`, and refuses every `route.open` to it with
  `module_no_protocol`. Its health can instead be probed over plain HTTP by
  setting `health.http` to a loopback `http://` URL; that key is refused on a
  `protocol: "subc"` module (test
  `http_health_refuses_non_loopback_https_and_wire_modules` (`crates/subc-daemon/src/daemon_config.rs`)).
  A clean exit the daemon did not ask for counts as a crash. Tests: `route_open_to_a_protocol_none_module_is_refused_as_terminal_and_counted` (`crates/subc-core/tests/forwarding.rs`)
  and `a_protocol_none_module_is_never_probed_while_its_subc_twin_is_restarted` (`crates/subc-core/tests/forwarding.rs`);
  `protocol_none_unrequested_clean_exit_restarts_as_a_crash` (`crates/subc-daemon/src/supervise.rs`).
- `overlap: "safe"` permits a blue/green swap (section 7). The default,
  `exclusive`, refuses it, because most modules are single writers to their
  store.

## 2. Start-up

The daemon starts a `protocol: "subc"` module as
`<program> <args> --subc <connection-file>` with this environment
(`apply_wire_spawn_args_for_role` and `apply_child_env` in
`crates/subc-daemon/src/supervise.rs`):

| Variable | Value |
| --- | --- |
| `SUBC_MODULE_ID` | The configured module id. Register under exactly this id. |
| `SUBC_LAUNCH_NONCE_FD` | macOS and Linux: `3:<inode>`. Descriptor 3 is a pipe holding the one-time launch nonce. |
| `SUBC_LAUNCH_NONCE_PIPE` | Windows: the name of a one-time named pipe that serves the nonce to this process only. |
| `SUBC_LAUNCH_NONCE` | Windows only, while modules move to the named pipe: the nonce itself. Never set on macOS or Linux. |
| `SUBC_SPAWN_ROLE` | `swap_candidate` when the process is the replacement in a swap; absent otherwise. It selects a longer warm-up budget and grants nothing. |
| `CK_LOG`, `CK_LOG_MAX_AGE_DAYS`, `CK_LOG_ALARM_SEGMENT_MB` | Log filter and retention from the module's `log` block, when one is configured. |
| Entries from `env` | As configured. |

A fresh nonce is minted for every spawn. A module reads it once with
`subc_os::launch_nonce()` (Rust) or its SDK, which reads and closes the
descriptor and caches the value. See [`launch-nonce.md`](launch-nonce.md).

## 3. Registration: `HELLO` and `HELLO_ACK`

After the transport handshake, the module sends `HELLO` on channel 0 with a
`ModuleHelloBody`: its manifest, `protocol_ver` (must be `2`), the optional
module-control ops it serves (`control_ops`, for example `["health.check"]`),
and its `launch_nonce`.

The daemon refuses the `HELLO` with an `ERROR` when
(`handle_hello` in `crates/subc-daemon/src/control.rs`):

| Code | Cause |
| --- | --- |
| `invalid_hello` | The body does not parse, the connection is already registered, or the connection has already opened client routes (a connection is either a client or a module, never both). |
| `invalid_manifest` | `protocol_ver` differs between body and manifest, the id is empty, or provenance, self-signal, or rich-UI descriptor data is malformed. Rich-UI refusals name the malformed `rich_ui` field. |
| `invalid_capability_grammar` | The capability declaration breaks [`capability-grammar.md`](capability-grammar.md). |
| `invalid_event_declaration` | An event declaration is malformed. |
| `version_unsupported` | `protocol_ver` is not `2`. |
| `reserved_module` | The id is reserved (or under a reserved prefix) and the nonce does not match the one the daemon gave the process it spawned. Test: `reserved_module_hello_requires_matching_launch_nonce` (`crates/subc-daemon/src/control.rs`). |
| `swap_token_invalid` | A swap is open for this id and the nonce is not the candidate's. |
| `reserved_capability` | The manifest claims a capability reserved to another module id. |
| `duplicate_module_id` | Another registration already holds the id. |
| `invalid_module_id` | The id is unsafe to use in a path (for example it contains a path separator). |

On success the daemon answers `HELLO_ACK` with `negotiated_ver`, the ops it
serves to modules (`subc_ops`), its capabilities (`subc_capabilities`,
[`control-plane.md`](control-plane.md) section 6), the module's storage
descriptor when central storage is configured, and the machine id. An absent
`storage` means no managed storage is configured; an absent `machine_id` means
the daemon predates it, never that the machine has none.

### 3.1 The manifest

`ModuleManifest` (`crates/subc-protocol/src/manifest.rs`) declares:

- `module_id`, `module_version`, `protocol_ver`.
- `provides`: roles the module serves. The set is closed:
  `tool_provider` (tools with schemas, execution mode and concurrency),
  `management_surface` (operations, config schema, observability),
  `internal_service` (a named service), and `pipeline_stage`, which is never
  routable. A client can open a route only to a role the module provides.
- `consumes`: what the module calls (`tool_client`, `llm_client`,
  `service_client`).
- `capabilities`: `provides`, `requires` (each `required` or `optional`) and
  `must_never_reach`. The daemon refuses a route whose opener's
  `must_never_reach` covers the target (`capability_forbidden`), reports
  unmet required capabilities in `server.describe`, and while a required
  capability is unprovided refuses routes to the module with the retryable
  `module_warming` (`detail.reason: "required_capability_unprovided"`). See [`capability-grammar.md`](capability-grammar.md)
  and [`machine-id-and-required-capabilities.md`](machine-id-and-required-capabilities.md).
- `ready`: absent or `true` means routable now. `false` means registered but
  warming: `route.open` is refused with a retryable code until the module sends
  `catalog.update` with `ready: true`. Test:
  `declared_not_ready_refuses_without_relay_until_catalog_update_marks_ready` (`crates/subc-core/tests/catalog_update.rs`).
- `events`, `self_signals`, `provenance` (build commit, lock digest, wire crate
  version, launch-nonce source) and `trust_tier`: descriptive, relayed to
  readers, and not used for routing.
- Optional `rich_ui`: an opaque descriptor with `schema` (1–64 bytes from
  `[a-z0-9._/-]`), `manifest_digest` (`sha256:` plus 64 lowercase hex digits),
  `bytes` (1–16,777,216), and `chunks` (1–1,024). The daemon checks these
  bounds and lexical forms at HELLO and rechecks the descriptor stored for the
  registration before applying `catalog.update`. It does not fetch or interpret
  the described manifest. The descriptor remains fixed for the registration's
  lifetime: `catalog.update` cannot accept or change it.

## 4. Routes: `route.bind`

When a client opens a route, the daemon sends the module a `route.bind` on
channel 0 and waits up to `route_bind_relay_timeout_ms` (default 12 s) for the
reply, answering the opener `module_timeout` if none comes. After three
consecutive full-budget timeouts, the daemon refuses new routes to that module
with `module_timeout` for 20 seconds without relaying, then lets one probe through
(`DEFAULT_ROUTE_BIND_BREAKER_THRESHOLD` and `DEFAULT_ROUTE_BIND_BREAKER_COOLDOWN`
in `crates/subc-daemon/src/control.rs`).

The bind carries:

| Field | Who vouches for it | Use it for |
| --- | --- | --- |
| `route_channel`, `epoch` | The daemon. | Stamping every frame on this route. |
| `principal` | The daemon. `reserved` with a `module_id` when the opener proved a launch nonce the daemon minted; `direct` when the opener presented none. `unverified` is reserved vocabulary the daemon does not send. | Authorization. It is the only caller fact a module may grant privilege on. Absent means unattested. |
| `scope` | The daemon, from the scope owner's synced record. | Acting under the scope's authority; fixed for the route's life. See [`scopes.md`](scopes.md). |
| `call_mode` | The daemon, sent only to modules providing `call-modes/v1`. | Choosing call semantics. Refuse an unknown mode. |
| `identity` (`project_root`, `harness`, `session`, `project_id`) | Nobody. The caller chose it. | Scoping and attribution, never authorization. |
| `consumer_capabilities`, `role_versions` | Nobody. | Choosing which optional behaviours and role versions to speak. |
| `admission_facts` | The configured carrier module. | Whatever the carrier and target agree. |

Reply with an empty `route.bind` response to accept. Reply with an `ERROR` to
refuse; the daemon relays it to the opener unchanged. Nothing is forwarded on
the route until the module accepts. See [`principal.md`](principal.md) for the
principal's full contract.

A module may send requests to the client on a bound route; see
[`reverse-requests.md`](reverse-requests.md).

Module-to-client delivery shares a bounded queue across the client's routes.
If it is full, an atomic backlog age older than 250 ms closes the client
immediately; otherwise the daemon waits only for the remaining part of that
250 ms grace, closing if no room frees up. Keep pushes and replies small and
expect disconnects from stopped readers. GOODBYE and oversized-reply replacement
errors use the same grace; frames too large to fit alone have separate handling.
See [client connection limits](concurrency-and-time-limits.md#12-client-connections).
The age follows the writer's current frame (or the frame that made the queue
non-empty); it can be one frame older than the true head between writes.

## 5. Health

The daemon probes only modules that list `health.check` in `control_ops`;
an operator probe (`supervisor.health_probe`) of any other module is refused
`health_not_advertised`. Every `cadence_ms` it sends
`health.check` and waits `deadline_ms`. After `failure_threshold` consecutive
missed deadlines the module is unresponsive, and the daemon restarts it. A
reply's `status` is `ok`, `degraded` or `failing`; `on_degraded` and
`on_failing` choose `report`, `restart` or `alert`. A health-triggered restart
spends the crash budget like a crash. `metrics` larger than 16 KiB are
replaced in the stored report by `{"truncated": true, "original_bytes": N}`;
the status still applies (`MAX_HEALTH_METRICS_BYTES` in
`crates/subc-daemon/src/supervise.rs`).

The reply must come from in-memory state and must not wait on anything that a
busy module also waits on. [`health-path-rule.md`](health-path-rule.md) states
that rule, and [`health.md`](health.md) the probing and escalation design.

A module that has never been probed reports no `last_probe_ms`; that means
"never probed", not "probed long ago".

## 6. Crashes and the restart budget

A module that exits unexpectedly is respawned after a backoff that starts at
`backoff_ms` and grows tenfold with each recent crash, capped at
`max_backoff_ms` (`delay_for_restart` in `crates/subc-daemon/src/supervise.rs`). At most `max_restarts`
crashes are restarted within `window_secs`; past that the module stays down
and `route.closed` reports `terminal: true`. An operator restart, reload or
re-enable clears the crash count. Tests: `route_closed_crash_at_max_minus_one_is_non_terminal` (`crates/subc-core/tests/forwarding.rs`)
and `route_closed_crash_at_max_is_terminal` (`crates/subc-core/tests/forwarding.rs`).

If the daemon itself dies, its next start ends every child recorded in the
previous daemon's `live-children.json` that is still the same process before
spawning new ones ([`data-model.md`](data-model.md) section 3).

## 7. Stopping, draining and swapping

### Daemon shutdown

On macOS and Linux, a graceful daemon shutdown tries to send every module
connection `module.draining` with the shutdown's absolute `deadline_ms`, before
GOODBYE and EOF. Delivery is best effort within a short notice budget. On Windows
the daemon sends no shutdown notice; the module's connection just ends.

The reason is `restart` when a planned restart was announced within the last 120
seconds, measured on a clock that keeps counting while the computer sleeps. `ck
upgrade` and `ck setup` announce before they restart the daemon, and scripts can
announce with the script-only command `ck daemon announce-restart`. A later
announcement replaces an earlier one. Without a live announcement the reason is
`disable`: the daemon cannot promise that the module is coming back.

Modules should stop accepting work and seal for shutdown on either notice, but
may preserve long-running work across `restart` as they do for a module restart.
Treat `disable` as a stop. A connection that ends without `module.draining`,
because the notice failed, the platform sends none, or the daemon died, must
also be treated as a stop. Only the module's notice reason depends on the
announcement: clients' `route.closing` and `route.closed` still carry `restart`,
and the shutdown drain, GOODBYE and EOF sequence is unchanged.

### Module lifecycle

- **Drain.** Before a restart, reload or disable, the daemon sends
  `module.draining` with an absolute `deadline_ms`, sends clients
  `route.closing`, refuses new requests with `module_reloading`, and waits up to
  `drain_timeout_ms` (default 30 s) for in-flight requests to finish. It then
  sends `route.closed` to clients and a `GOODBYE` per route and for the module
  itself. An idle module is not kept waiting.
- **Stop.** A registered module is expected to exit on its own after the
  module `GOODBYE`; the daemon sends it no signal, because a signal would race
  that stop. A `protocol: "none"` module, or a module that has not registered,
  receives `SIGTERM` on macOS and Linux instead. On Windows nothing is sent.
  Whatever is still running when the drain budget ends is killed
  (`request_graceful_stop` in `crates/subc-daemon/src/supervise.rs`). Test:
  `protocol_none_teardown_sigterms_first_and_only_kills_a_child_that_ignores_it` (`crates/subc-core/tests/forwarding.rs`).
- **Swap.** `supervisor.swap` starts a second process with
  `SUBC_SPAWN_ROLE=swap_candidate` and its own nonce. The candidate registers
  beside the running process but receives no routes until it reports ready
  (manifest `ready`, or `catalog.update`). The daemon then sends new routes to
  the candidate and drains the old process. If the candidate fails or misses
  its ready budget, the old process keeps serving. See
  [`module-readiness-and-swap.md`](module-readiness-and-swap.md).

## 8. On macOS

Each supervised module, including `protocol: "none"` programs, is launched as
its own responsible process, so privacy prompts (Screen Recording, Files and
Folders, Full Disk Access and others) name the module rather than the daemon.
See [`macos-privacy-identity.md`](macos-privacy-identity.md).
