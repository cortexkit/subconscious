# Control plane

Channel 0 of every connection is the control channel. Requests on it are JSON
bodies tagged by an `op` field; replies use the same `op` on a `RESPONSE`
frame, and refusals are `ERROR` frames carrying an `ErrorBody`
([`wire-contract.md`](wire-contract.md) section 5). This document lists every
operation, who may send it, and which test pins its shape.

Client-facing shapes live in `crates/subc-control/src/lib.rs` and
`crates/subc-control/src/activity.rs`. Module-facing shapes live in
`crates/subc-protocol/src/session.rs` and `crates/subc-protocol/src/scope.rs`.
The daemon dispatches them in `crates/subc-daemon/src/control.rs`.

Shape tests: every client request, response and push in sections 2 and 3 has a
golden file in `crates/subc-control/tests/golden/`, checked by
`control_wire_shapes_match_golden_json_and_round_trip` (`crates/subc-control/tests/golden_json.rs`). Every module-facing body in
sections 4 and 5 has a golden file in `crates/subc-protocol/tests/golden/`,
checked by `protocol_wire_shapes_match_golden_json_and_round_trip` (`crates/subc-protocol/tests/golden_json.rs`).

## 1. Rules that hold for every operation

1. **Unknown fields are ignored, except in scope records.** Request bodies are
   not `deny_unknown_fields`, so an older daemon silently drops a field it does
   not know. A sender that depends on a new field first checks that the daemon
   advertises the capability that introduced it (section 6). Scope records and
   scope selectors are the exception: they refuse unknown fields, so a scope
   attribute is never silently lost. Tests:
   `scope_records_refuse_unknown_fields_at_every_level` (`crates/subc-protocol/tests/golden_json.rs`) and
   `scope_selector_refuses_unknown_principal_fields` (`crates/subc-protocol/tests/golden_json.rs`).
2. **A new behaviour that must not degrade silently is a new op.** An unknown
   op is refused (`unknown_control_op` from a client connection,
   `unsupported_control_frame` from a module connection), while an unknown field
   is dropped. That is why blue/green restart is `supervisor.swap` and not a
   flag on `supervisor.restart`. A body that names a known op but does not
   decode is refused `invalid_control_body`.
3. **Absent is not zero.** Optional reply fields are omitted when the daemon
   has no value. A missing field means "not reported by this daemon", never
   `0`, `false` or empty.
4. **Privileged operations name their effect.** The operations marked
   *privileged* in section 2 change what runs. With
   `privileged_operator_prompt: "enforce"` in the daemon config the daemon asks
   the person at the machine to approve each one before acting, naming the
   operation and the requesting process. With the default, `"log"`, it logs
   that a prompt would have been shown and proceeds
   (`privileged_operation` in `crates/subc-daemon/src/control.rs`). Tests:
   `privileged_operator_prompt_defaults_to_log_and_accepts_both_modes` (`crates/subc-daemon/src/daemon_config.rs`),
   `privileged_restart_approval_holds_dispatch_until_provider_approves` (`crates/subc-daemon/src/operator_confirm/tests.rs`) and
   `privileged_restart_decline_never_dispatches` (`crates/subc-daemon/src/operator_confirm/tests.rs`).

## 2. Client requests

Any authenticated connection that has not registered as a module may send
these; `ck` is such a client. A connection that has sent `HELLO` may send only
the operations in section 5 and the `activity.*` operations; any other request
from it is refused `unsupported_control_frame` (`handle_control_frame_timed` in
`crates/subc-daemon/src/control.rs`). A module that calls other modules does
so over a second, client connection. Test:
`privileged_registered_module_restart_is_unsupported_in_both_modes` (`crates/subc-daemon/src/operator_confirm/tests.rs`).

| Op | Purpose | Notes |
| --- | --- | --- |
| `server.describe` | Protocol version, advertised ops and capabilities, connected client count, counters, build commit and lock digest, capability-requirement status, machine id. | `build_git_sha`, `build_lock_digest` and `machine_id` are absent from daemons that predate them. The daemon's version is in the connection file and the handshake, not here. |
| `catalog.list` | Registered modules with their manifests' roles, capabilities, readiness and optional `rich_ui` descriptor. Optional `module_id` narrows the list. | An unregistered id gives an empty list, not an error. This is the wire registry, not the supervised roster. `rich_ui` is opaque and fixed for the registration's lifetime; `nonce_verified: true` appears only when HELLO passes the launch-nonce check required for a reserved module or swap candidate. Both fields are omitted when absent/unverified, preserving legacy entry bytes. |
| `route.open` | Open a route to a target (section 2.1). | Reply: `route_channel`, `route_epoch`. |
| `route.poll` | Read a bound route's last status (`kind: "status"`) or whether its module process is live (`kind: "liveness"`). | Answered from the daemon's own state; the module is never asked. |
| `supervisor.list` | Supervised modules with state, health, restart counts and policy. | Each entry's optional `configured_enabled` is the `enabled` value in the daemon's loaded configuration. Compare it with runtime `enabled` to detect drift; it is not a stored flag. |
| `supervisor.health` | Cached health for every supervised module. | |
| `supervisor.health_probe` | Send one fresh `health.check` to a module and return its answer. | Refused `health_not_advertised` when the module did not list `health.check` in its HELLO `control_ops`. |
| `supervisor.routes` | Live routes served by one module or every module. | Answered from the forwarding table; safe during a drain. |
| `supervisor.provenance` | Daemon-observed process facts beside module-declared build facts. | |
| `supervisor.stderr_tail` | Retained stderr for one module, bounded by `max_lines` and `max_bytes`. | |
| `supervisor.terminals` | Retained process exits for one module. | Read off the supervisor command channel, so it answers even when supervision of that module has failed. |
| `supervisor.spawn_snapshot` | The live supervised processes and the spawn-event cursor, read atomically. | |
| `supervisor.spawn_subscribe` | Replay spawn events after `since`, then stream new ones. | Refusals: `spawn_cursor_incarnation_mismatch`, `spawn_cursor_too_old`, `spawn_subscriber_lagged`. Tests: `spawn_snapshot_then_subscribe_has_no_overlap_or_gap` (`crates/subc-core/tests/supervision.rs`), `spawn_subscribe_refuses_stale_and_foreign_cursors_with_typed_details` (`crates/subc-core/tests/supervision.rs`), `lagged_spawn_subscriber_receives_a_terminal_lagged_error_after_its_queued_frames` (`crates/subc-daemon/src/supervise.rs`). |
| `supervisor.restart` | Drain and restart a module. Optional `drain_timeout_ms` overrides the drain budget for this restart; `0` tears down without waiting. | Privileged. |
| `supervisor.swap` | Blue/green restart: start a replacement beside the running process and cut over when it reports ready. Optional `ready_timeout_ms` (default 100 s). | Privileged. Refused unless the module is configured `overlap: "safe"`. Test: `swap_of_an_exclusive_module_is_refused_and_spawns_nothing` (`crates/subc-core/tests/forwarding.rs`). |
| `supervisor.reload` | Drain a running module's routes, stop its process and spawn it again; routes close with reason `reload`. | Privileged. Refused for a disabled module. |
| `supervisor.set_enabled` | Stop and disable, or enable and start, a module. Enabling a module that is down after its crash budget ran out starts it again. | Privileged. Held in the daemon's memory only: it is not written to `subc.jsonc`. A daemon restart or any rescan applies the configured `enabled` again for every module, including entries that did not change. |
| `supervisor.rescan` | Re-read `subc.jsonc` and reconcile the supervised set. `preview: true` computes the change without applying it. | Privileged unless `preview` is true. |
| `supervisor.release_reserved` | Forget the reserved id of a module removed from config, so another process may register it. | Privileged. |
| `activity.record`, `activity.query`, `activity.subscribe` | The activity ledger. | Accepted from client and module connections; who may call each is in [`activity-ledger.md`](activity-ledger.md). |

A successful privileged operation that has no other result answers
`supervisor.ack` with `module_id` and `applied`.
Lifecycle acknowledgements are unchanged; they do not carry configuration
values. To compare a start or stop with the configuration, read the target's
`supervisor.list` entry after the acknowledgement. Its optional
`configured_enabled` is the module's `enabled` value in the configuration the
daemon loaded, not a fresh disk read. The field is absent when there is no
configured entry (for example, a daemon without a config file), and on older
daemons. Absent values are omitted rather than serialized as `null`, preserving
older list-entry bytes.

### 2.1 `route.open`

```json
{
  "op": "route.open",
  "target": { "kind": "tool_provider", "module_id": "aft" },
  "identity": { "project_root": "/path/to/project", "harness": "opencode", "session": "s-1" },
  "consumer_identity": { "module_id": "my-module", "launch_nonce": "..." },
  "consumer_capabilities": ["elicitation"],
  "role_versions": { "tool-provider": "v1" },
  "scope": { "owner": { "kind": "reserved", "module_id": "owner" }, "ref": "r-1", "scope_epoch": 3 }
}
```

- `target.kind` is `tool_provider`, `management_surface` or
  `internal_service` (with a `service_id`). A target that does not provide the
  matching role is refused `target_unavailable`; pipeline stages are never
  routable.
- `identity` is caller-supplied and unattested. The daemon canonicalises
  `project_root` and copies every field to the module, which may use them for
  scoping and attribution but never for authorization.
- `consumer_identity` is optional. When present, the daemon checks the nonce
  against the processes it spawned and refuses the open with
  `bad_consumer_identity` if it does not match (test
  `route_open_refuses_consumer_identity_that_fails_spawn_attestation` (`crates/subc-daemon/src/control.rs`)). When absent, the opener is a direct
  key-holder.
- `consumer_capabilities` and `role_versions` are unverified declarations
  copied to the module. A malformed `role_versions` is refused as
  `invalid_request` with `detail.field: "role_versions"`.
- `admission_facts` is accepted only from the configured carrier module and
  only toward configured targets.
- `scope` opens the route under a scope; the rules are in
  [`scopes.md`](scopes.md) section 4.

Retry rules for the refusals are in [`wire-contract.md`](wire-contract.md)
section 5.1.

## 3. Daemon pushes to clients

| Op | When | Fields |
| --- | --- | --- |
| `route.closing` | A module's routes are about to close. | `module_id`, `channels` (this connection's only), `reason`. |
| `route.closed` | The routes have closed. | As above, plus `drained` (whether in-flight work finished), `abandoned` (pending binds forced down), `excluded_subscriptions`, and `terminal` (whether the daemon will leave the module down until an operator acts). |

`reason` is one of `reload`, `restart`, `disable`, `crash`,
`capability_denied`, `scope_ended`, `scope_carrier_removed`,
`scope_delegation_changed`, `scope_parent_ended` (`RouteCloseReason` in
`crates/subc-protocol/src/lib.rs`). Tests:
`route_closed_after_unexpected_module_loss_reports_crash` (`crates/subc-core/tests/daemon_shutdown.rs`), and
`route_closed_crash_at_max_is_terminal` (`crates/subc-core/tests/forwarding.rs`).

## 4. Daemon requests and commands to modules

| Op | Kind | Purpose |
| --- | --- | --- |
| `route.bind` | Request | Bind a client route to a module channel. Carries `route_channel`, `epoch`, `target`, `identity`, the daemon-stamped `principal`, `consumer_capabilities`, `role_versions`, `admission_facts`, the scope stamp and the call mode. Success is an empty `route.bind` reply; a refusal is an `ERROR`, whose code and message reach the opener unchanged (test `route_open_module_rejection_uses_daemon_counter_key` (`crates/subc-daemon/src/control.rs`)). |
| `health.check` | Request | Ask the module for its health. Reply: `status` (`ok`, `degraded`, `failing`), optional `detail` and `metrics`. Sent only to modules that declared the op in HELLO `control_ops`. |
| `module.draining` | Command | Tell the module its routes are draining, with `reason` and an absolute wall-clock `deadline_ms`. Treat the deadline as "no later than". |

What a module does with these is in [`module-surface.md`](module-surface.md).

## 5. Module requests to the daemon

A module sends these on the connection it registered with `HELLO`; the same
request from a connection that has not registered is refused `not_registered`.
`HELLO_ACK.subc_ops` lists them together with the client operations of
section 2. The module's own HELLO `control_ops` declares the optional control
operations it serves: every registration is treated as granting `route.bind`
and `route.status` (`MODULE_BASELINE_CONTROL_OPS` in
`crates/subc-daemon/src/control.rs`), and the daemon sends `health.check` only
to a module that lists it.

| Op | Kind | Purpose |
| --- | --- | --- |
| `route.status` | Push | Report a status string for one of the module's routes, read back by `route.poll`. |
| `catalog.update` | Request | Replace the module's `provides` roles, optionally its capability declaration, and optionally its readiness, without re-registering. Test: `catalog_update_refreshes_catalog_without_disrupting_bound_routes` (`crates/subc-core/tests/catalog_update.rs`). |
| `supervisor.live_roots` | Request | Count live and pending routes per canonical project root, also including the workdir of each bound scoped route. A route contributes once when both paths match. |
| `scope.sync`, `scope.apply`, `scope.describe` | Request | Register, change and read scopes. See [`scopes.md`](scopes.md). |
| `operator.confirm` | Request | Ask the person at the machine to confirm one in-flight write (section 5.1). |

### 5.1 `operator.confirm`

A module serving a route may ask the person to confirm a write before doing
it. The daemon accepts the request on any connection but refuses it unless the
conditions under `operator_request_not_permitted` below hold. The request names a `summary` and the caller's `route_channel` and
`route_epoch` on the module's connection. Only `outcome: "confirmed"` confirms.
Refusals:

| Code | Meaning |
| --- | --- |
| `operator_declined` | Declined or withdrawn. `detail.reason`: `person`, `backoff`, `route_closed`, `module_closed`, `caller_cancelled`. A module `backoff` refusal also includes `detail.retry_after_ms`, the rounded-up milliseconds until its 30-second `(module, principal)` cooldown ends; older daemons may omit it. |
| `operator_presence_unavailable` | The prompt could not be shown or completed. `detail.reason`: `no_presence`, `timeout`, `module_limit`, `queue_full`, `queue_wait`, `provider_stuck`, `unsupported_platform`, `provider_error`. |
| `operator_summary_invalid` | The summary breaks the daemon's summary rules. |
| `operator_request_not_permitted` | The asking connection is not a nonce-proven module serving an open route with a verified opener. |

The prompt implementation is `crates/subc-daemon/src/operator_confirm.rs` and
the operating-system prompts are in `crates/subc-presence`.

## 6. Advertised capabilities

The daemon lists the same capabilities in `HELLO_ACK.subc_capabilities` and
in `server.describe.capabilities` (`subc_capabilities` in
`crates/subc-daemon/src/control.rs`):

| Capability | Meaning |
| --- | --- |
| `manifest_registration_v1`, `channel_lifecycle_v1`, `ping_pong_v1`, `session_attach_v1`, `admission_facts_relay_v1` | Baseline features every current daemon has: module registration by `HELLO`, the route lifecycle, `PING`/`PONG`, `BindIdentity` on `route.open`, and the admission-facts relay. They carry no further contract; a client does not need to check them. |
| `scopes/v1` | `route.open` accepts `scope`, and `scope.*` ops exist. |
| `scope-workdir/v1` | Scope records accept optional `workdir` and `project_id`, fixed per epoch and not authority-gated. Binds carry them only to targets whose manifest `capabilities.provides` declares this capability; older targets' bind bytes stay unchanged. Scoped activity entries include them, and bound scoped workdirs contribute to live roots alongside bind roots. See [`scopes.md`](scopes.md) for validation and epoch refusal codes. |
| `route-role-versions/v1` | `route.open` accepts `role_versions` and copies it to the bind. |
| `catalog-events/v1` | `catalog.list` carries each module's declared `events`, so an entry without them means the module declares none. |
| `scope-bind-proof/v1` | Scope records may carry `bind_proof_sha256`, and scoped opens must present the proof. |
| `scope-bind-proof/v2` | Bind proofs are required only from non-attested openers; reserved modules are exempt. Advertised alongside v1. |
| `scope-replace-if-idle/v1` | Scope records may request atomic idle-only epoch/proof replacement; scoped opens may hold connection presence across route closures. |
| `activity-ledger/v1` | The `activity.*` ops exist. |

A client checks for a capability before sending a field or op that depends on
it. Absence of a capability means the daemon does not support it.
