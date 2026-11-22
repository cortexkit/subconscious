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
the operations in section 5, the `activity.*` operations, `app.open`, and
`connection.describe`; any other request
from it is refused `unsupported_control_frame` (`handle_control_frame_timed` in
`crates/subc-daemon/src/control.rs`). A module that calls other modules does
so over a second, client connection. Test:
`privileged_registered_module_restart_is_unsupported_in_both_modes` (`crates/subc-daemon/src/operator_confirm/tests.rs`).

| Op | Purpose | Notes |
| --- | --- | --- |
| `server.describe` | Protocol version, advertised ops and capabilities, connected client count, counters, build commit and lock digest, capability-requirement status, machine id. | `build_git_sha`, `build_lock_digest` and `machine_id` are absent from daemons that predate them. The daemon's version is in the connection file and the handshake, not here. |
| `app.open` | Ask the daemon to start a known installed operator app. | Unprivileged; starting an app grants nothing. Standalone request/reply bodies, not variants of the exhaustive control enums. |
| `connection.describe` | Read the daemon's facts about the caller's own connection. | Unprivileged, never prompts, and cannot inspect another connection. Standalone request/reply bodies. |
| `catalog.list` | Registered modules with their manifests' roles, capabilities, readiness, optional `display_name` and optional `rich_ui` descriptor. Optional `module_id` narrows the list. | An unregistered id gives an empty list, not an error. This is the wire registry, not the supervised roster. `display_name` is copied from the manifest and omitted when undeclared; apps fall back to the bare `module_id`. It is already-trimmed plain text of 1–40 Unicode scalar values, with no Cc controls or bidi formatting characters (U+061C, U+200E, U+200F, U+202A–U+202E, U+2066–U+2069); invalid names refuse HELLO as `invalid_manifest`, never silently trimmed or truncated. `rich_ui` is opaque and fixed for the registration's lifetime; `nonce_verified: true` appears only when HELLO passes the launch-nonce check required for a reserved module or swap candidate. Optional fields are omitted when absent/unverified, preserving legacy entry bytes. |
| `catalog.watch` | Follow catalog generations on channel 0. | The first `STREAM_DATA` reports the captured generation; later advances may coalesce behind backpressure. Cancel by correlation id and refresh the catalog after reconnect. |
| `route.open` | Open a route to a target (section 2.1). | Reply: `route_channel`, `route_epoch`. |
| `route.poll` | Read a bound route's last status (`kind: "status"`) or whether its module process is live (`kind: "liveness"`). | Answered from the daemon's own state; the module is never asked. |
| `supervisor.list` | Supervised modules with state, health, restart counts and policy. | Each entry's optional `configured_enabled` is from the approved generation when stamped, otherwise the daemon's loaded configuration. Runtime `enabled`, health and launch observations remain in-force supervisor state, not an assertion that approval activated it. Optional [`subtree`](module-subtree-memory.md) reports memory including child processes beside the unchanged per-process `resources`, with an explicit source and process count or unavailability reason. |
| `supervisor.health` | Cached health for every supervised module. | |
| `supervisor.health_probe` | Send one fresh `health.check` to a module and return its answer. | Refused `health_not_advertised` when the module did not list `health.check` in its HELLO `control_ops`. |
| `supervisor.routes` | Live routes served by one module or every module. | Answered from the forwarding table; safe during a drain. |
| `supervisor.provenance` | Daemon-observed process facts beside module-declared build facts. | |
| `supervisor.stderr_tail` | Retained stderr for one module, bounded by `max_lines` and `max_bytes`. | |
| `supervisor.terminals` | Retained process exits for one module. | Read off the supervisor command channel, so it answers even when supervision of that module has failed. |
| `supervisor.spawn_snapshot` | The live supervised processes and the spawn-event cursor, read atomically. | |
| `supervisor.spawn_subscribe` | Replay spawn events after `since`, then stream new ones. | Refusals: `spawn_cursor_incarnation_mismatch`, `spawn_cursor_too_old`, `spawn_subscriber_lagged`. Tests: `spawn_snapshot_then_subscribe_has_no_overlap_or_gap` (`crates/subc-core/tests/supervision.rs`), `spawn_subscribe_refuses_stale_and_foreign_cursors_with_typed_details` (`crates/subc-core/tests/supervision.rs`), `lagged_spawn_subscriber_receives_a_terminal_lagged_error_after_its_queued_frames` (`crates/subc-daemon/src/supervise/spawn_subscriber_lag_tests.rs`). |
| `supervisor.restart` | Drain and restart a module. Optional `drain_timeout_ms` overrides the drain budget for this restart; `0` tears down without waiting. | Privileged. |
| `supervisor.swap` | Blue/green restart: start a replacement beside the running process and cut over when it reports ready. Optional `ready_timeout_ms` (default 100 s). | Privileged. Refused unless the module is configured `overlap: "safe"`. Test: `swap_of_an_exclusive_module_is_refused_and_spawns_nothing` (`crates/subc-core/tests/forwarding.rs`). |
| `supervisor.reload` | Drain a running module's routes, stop its process and spawn it again; routes close with reason `reload`. | Privileged. Refused for a disabled module. |
| `supervisor.set_enabled` | Stop and disable, or enable and start, a module. Enabling a module that is down after its crash budget ran out starts it again. | Privileged. Held in memory only, not written to `subc.jsonc`. Unstamped, an applying rescan restores configured enabled for every module. Stamped rescans cannot apply configuration. |
| `supervisor.rescan` | Unstamped, re-read `subc.jsonc` and reconcile the supervised set. `preview: true` computes changes without applying them. | Privileged unless preview. Stamped apply returns `config_gated` before the legacy prompt and still observes files as a proposal; file preview compares against approved entries and records a proposal. |
| `supervisor.rescan_checked` | Preview client-supplied candidate text, or apply a file whose hash and staged generation match a preview. | Candidate preview records nothing. Stamped checked apply returns `config_gated` before the legacy prompt, with the existing request shape unchanged. |
| `supervisor.release_reserved` | Forget the retained reserved id of a retired module, so another process may register it. | Privileged. While stamped, membership comes from the approved generation, not a proposed file re-add. |
| `activity.record`, `activity.query`, `activity.subscribe` | The activity ledger. | Accepted from client and module connections; who may call each is in [`activity-ledger.md`](activity-ledger.md). |

A successful privileged operation that has no other result answers
`supervisor.ack` with `module_id` and `applied`.
Lifecycle acknowledgements are unchanged; they do not carry configuration
values. To compare a start or stop with the configuration, read the target's
`supervisor.list` entry after the acknowledgement. Its optional
`configured_enabled` is the module's `enabled` value in the approved generation
(or loaded configuration while unstamped), not a fresh disk read. The field is absent when there is no
configured entry (for example, a daemon without a config file), and on older
daemons. Absent values are omitted rather than serialized as `null`, preserving
older list-entry bytes.

### Module settings

The additive `config.settings/v1` capability serves `config.modules`,
`config.schema`, `config.show` and `config.watch` to authorized local settings
readers without prompting. `config.show` returns approved effective values and
layer documents on a stamped install; unstamped reads identify live, unreviewed
file settings as generation zero. The `config.settings.projects/v1` capability
adds workspace and project settings. These operations remain on channel 0 with
their existing admission checks; a `project_id` selects settings, not caller authority.

On a stamped install (after a person approves a configuration stamp),
`config.get` normally serves the approved active and
deliverable pending views. If the connection's declaration matches neither
approved declaration, the daemon serves only its last approved active view,
with the same generation, effective values, layers and sources. It adds the
optional reply field `declaration_pending: { plan_digest?: string }`: `{}` means
no plan yet, while `plan_digest` names a pending plan carrying the connection's
declaration. Unknown members are ignored. Missing keys use the module's schema
defaults. The source remains the daemon's approved store, not a module-owned
copy that could be edited without approval.

The active view may carry a connection-local attempt for `config.applied` to
ACK catch-up. This does not activate a generation or change approved bytes.
`config.status` exposes a module-keyed `declaration_pending` map; settings module
rows and `config.show` expose the same optional marker. After an ACK, status
reports the module running its last approved settings, not failed activation.
Approval of the new declaration resumes normal pending-generation delivery.
Without an approved active view, the existing `config_gated` error remains
(`no_approved_config` availability in SDKs); it never authorizes file reads.
While the approved store is in recovery or has failed its integrity check, the module is refused as before; the fallback never serves a view the daemon cannot vouch for.

**Settings schema annotations.**

The [config vocabulary](../crates/subc-config/vocabulary.schema.json) defines the
`x-ck-*` annotations admitted by `config.declare`, including `x-ck-layers`,
`x-ck-apply`, `x-ck-kind` and the `x-ck-options` picker metadata.
`config.schema` preserves admitted annotations in the module's normalized schema.

A managed value can list scalar types (`string`, `number`, `integer`, `boolean`).
An overlay replaces it and may use a different listed type. For arbitrary JSON
shapes, declare `x-ck-kind: json`: the user layer sets the whole field, optionally
constrained by supported JSON Schema assertions, and review shows whole old and
new values. The field's own settings key is managed; keys inside it are not.
Workspace and project layers cannot set the field. A default, if declared, must
validate. A top-level `null` value for the field is refused, but nested nulls are
allowed. The [config crate guide](../crates/subc-config/README.md#schemas-and-layers)
lists supported assertions, bounds, and annotations forbidden inside opaque JSON.

An optional `x-ck-setup` on a top-level, named object property marks a settings
section as a feature the setup wizard can offer:

```json
"x-ck-setup": {
  "feature": "journal_reads",
  "order": 10,
  "recommended": {"/journal/read": true}
}
```

All three members are required, and no others are allowed. `feature` is a stable
module-local id matching `^[a-z][a-z0-9_]{0,63}$`, unique across the module's
sections. `order` is an integer; lower values sort first, with ties sorted by
`feature`. The annotation carries no wording: the wizard uses the section's own
`title` and `description`. There is at most one annotation per section. Root,
nested, array-item, pattern-property and union-branch annotations are refused;
keeping sections at one level prevents overlapping features.

`recommended` is an object, possibly empty. Its keys are JSON Pointers from the
module settings **root**, not from the section. Every pointer must resolve through
named `properties` to a field inside that section; nested named descendants are
allowed, but structural object targets, array-item targets, pattern-property
targets and unresolved discriminated `oneOf` targets are refused. Values must pass the field's schema
validation, without checking membership in dynamic `x-ck-options` catalogs,
just as declaration defaults do.
A target whose schema contains `x-ck-kind: credential_ref` anywhere is forbidden,
including an empty array with credential-bearing items: credentials are chosen
with the picker, never proposed by setup. Malformed annotations, duplicate feature
ids, invalid or unresolved pointers and invalid values refuse the declaration with
`schema_invalid` at the section's settings path.

Recommendations are only wizard proposals. They are not defaults and do not
change any value applied by the daemon. See the
[setup schema sample](../vectors/config-settings-v1/schema-setup.reply.json),
generated by the daemon's `config.schema` handler.

`config.watch` opens with a `resync` containing module rows and `stamped`, then
sends `change` events when a row's settings metadata changes. A later `resync`
replaces the whole state when the module set or stamp changes. Reopening always
starts with another complete resync. Each connection can have at most four open
`config.watch` subscriptions. A fifth is refused with `too_many_watches`.

Set `projects: true` to include rows for registered projects with a stored tuple,
a proposal, or a present settings layer. Rows are sorted by `(module_id, project_id)`,
with the user row first for each module. Project rows and their `change` events
carry `project_id` and, if known, `workspace_id`. When a row disappears (for
example, the registry drops a project), a `resync` replaces the old rows.
Without `projects: true`, both the rows and events stay user-only, as does
`config.modules`.

Module rows, watch changes and successful `config.show` replies may carry
`file_error: {code, message}`. It describes a file observation failure, for
example `value_invalid` or `input_unavailable`, independently of `status` and
its existing precedence. A project file error may also include `layer`
(`"project"` or `"workspace"`) and `path` (the absolute settings file path as
resolved by the daemon). These fields are omitted when the failure does not
identify a single file, and are not added to user-level file errors. A shared
workspace file can produce a row for each project that reads it, with the same
layer and path. A tuple validation failure affects only that tuple.
Approved effective values, sources and layer documents are
not replaced by the broken file. One-shot `config.modules` and `config.show`
reads report this diagnostic without a settling grace.

A watch confirms the same failure on two observations separated by its
two-second polling interval before exposing `file_error`. Repeated publication
wakes do not shorten that grace. A single incomplete save keeps the last good
row silently. A newly opened watch with a failing file waits for the same short
grace before its initial resync. Persistent bad files do not close the watch:
once the file parses again, the next event omits `file_error` and carries the
current row. Other refusal classes retain their existing behavior.

An unstamped `config.show` has no approved view to fall back to: if its live file
cannot be parsed, the call keeps the file's own refusal code and message, the
same pair a watch reports in `file_error`. Watch rows contain metadata, not
effective settings values. An unstamped page keeps a previously loaded good
view when available; a module that has never parsed has no values to display.
The daemon does not fill that missing view with defaults.

The Rust client exposes `ConfigFileError` and the optional fields on typed
settings replies and events. Older readers may ignore the additive members.
See the [settings wire samples](../vectors/config-settings-v1/README.md) for
ordinary reads, file diagnostics and edit-plan requests.

**Project views and cutoffs.** `config.show {module_id, project_id, through?}`
returns the selected project's merged values and their sources. Precedence is
defaults, user, workspace, then project. Workspace and project files live at
`<registry root>/.cortexkit/<module_id>.jsonc`; module declarations cannot supply
repository roots. If a project's root also serves as its workspace root, that
file counts once, as the project's layer for that project. Other workspace
members can still read it as their workspace layer. `layers` lists present files:
user layers carry no IDs, workspace layers carry `workspace_id`, and project
layers carry `project_id`.

An optional `through` (`default`, `user`, or `workspace`) selects an inclusive
cutoff. On stamped installs these views come from the project's own approval,
not the module's current user generation. A dropped project still shows its
stored tuple, even during a registry outage. Without a stored tuple, a known
layerless project shows the user view. Without `project_id`, show keeps its
user-only behavior, including for `through: "workspace"`.

### Configuration plans and approval

The opt-in `config.gate` capability advertises standalone configuration bodies,
not new variants of the legacy exhaustive client control enums:

| Op | Result |
| --- | --- |
| `config.status` | Stamp state, integrity mode, proposals, missing approval surfaces, platform warnings, approved-but-inactive differences, optional settings file diagnostics and optional `root_io` health. |
| `config.stamp`, `config.plan`, `config.unstamp`, `config.rollback` | A reviewable plan and digest; none commits configuration. |
| `config.apply` | Requests approval of that digest, then reports `generation` and `restart_required` (always serialized; absent on older replies means false). |
| `config.history` | Retained generations and summaries of pruned history. |

`ck config plan` followed by `ck config apply <digest>` replaces edit-and-rescan
on a stamped daemon. The terminal is not a trusted surface and a client's claim
to have reviewed a diff grants no authority. Stamp approval is desktop-only.
Other plans require a trusted desktop or phone review unless the entire change
fits the OS prompt: the daemon-generated description must remain unaltered,
with no sanitizer substitutions or target truncation, in the 200-character
summary (target names are limited to 64 characters). Such a small change may
use the interactive OS prompt alone. Stamp, ordinary unstamp, rollback,
declaration changes and program changes are never small. Recovery unstamp is
the exception, using the fixed sentence “Config files become authoritative again”. No legacy
`privileged_operator_prompt: "log"` policy or operator-app shortcut approves a
plan. Unavailable review returns `approval_surface_unavailable`. If the phone
approval service answers the consent request with an error whose code is
`approval_surface_unavailable`, `config.apply` returns that code too: no
generation is created and the plan stays pending. Other phone error codes,
declined approval and unanswered cards return `approval_declined`. Changed plan
inputs return `plan_stale`, without supplying a new digest.

**Project-tuple plans.** A capture plan (`config.plan` without `edits`) checks
registered roots for external changes. An edit names `layer: "workspace"`
and a `workspace_id` or member `project_id`, or names `layer: "project"` and
`project_id`. A workspace edit covers every project that reads the file, even
if some projects read the physical file as a project layer. Stamped edits start
from approved bytes. They neither adopt nor overwrite a separate, unapproved
disk change. The daemon computes each candidate `(module_id, project_id)` view
from its ordered layers and checks the module's `config.review` reply against
it. Project review snapshots carry `project_id`, including during the first
review when `base` is `null`; the workspace snapshot also carries `workspace_id`.
The plan binds exactly the changed tuples, including ones whose layer bytes or
cutoffs change without an effective field delta.

Before apply the daemon takes a fresh registry snapshot of affected readers and
rechecks their membership, roots, and the reviewed files. Changes in that set
refuse the apply with `config_changed_since_review`; unrelated registry changes
do not. `config.show` returns the same module-wide `base_generation` token with
or without `project_id`; approval of any tuple advances that token at commit,
not at ACK. On stamped installs, approval commits to the store before file
write-back. Dirty targets or timed-out writes can report `write_back_behind`.
Reconciliation adopts a late write only if the current bytes match the newest
intent. Unstamped edits apply directly and can touch only one physical file.

**Root I/O and refusals.** Registry-root work uses four dedicated workers and
a FIFO queue of 16 jobs. Each root has at most one outstanding job. A single
10-second deadline covers queueing and execution; expired queued jobs never
start, and a worker running past its deadline stays charged until it returns.
`config.status` adds `root_io: {stuck_roots, charged_workers}` when a job is
stuck, with canonical root paths sorted. Bare user reads and delivery of
stored views do not wait on this pool.

Capture root failures appear in `view.skipped`, not `view.refusals`. A changed
file that a skipped project also reads is held for all readers until another
capture; independent changes can proceed. `skipped` notices do not change the
plan digest. In edit plans, path and root failures appear in the blocking
`view.refusals` instead (with `project_id` and `workspace_id` where applicable).

`config.status` reports refused settings files in `status.file_errors`, sorted
by `(module_id, project_id)`. Each row has `module_id`, `code`, and `message`;
project rows also have `project_id` and, when known, `workspace_id`. The optional
`layer` and `path` fields appear directly on the row when the daemon has
identified a particular project or workspace settings file. The list is
omitted when empty. Like `config.show`, this one-shot read does not apply the
watch's save-settling grace. If project observations are incomplete, the reply
keeps any rows already read and adds `status.file_errors_incomplete` with a
`reason` such as `project_registry_unavailable` or `config_unavailable`. An
absent list with this marker does not mean all settings files are valid.
`ck config status` prints these diagnostics, including `layer` and `path`, in
both human and JSON output.

| Code | Meaning |
| --- | --- |
| `layer_symlink`, `layer_not_regular`, `layer_hardlinked` | A link, reparse point, non-regular file, or hard-linked file violates retained-root checks. |
| `layer_too_large` | A layer file exceeds 1 MiB. |
| `layer_root_invalid`, `layer_root_ambiguous` | The root is invalid, missing from the eligible set, or cannot be selected unambiguously. |
| `project_apply_kind_unsupported` | A workspace- or project-sourced field requires a non-live apply action. |
| `credential_layer_unsupported` | A workspace or project layer sets a section containing a credential reference field. Such sections are user-layer only. |
| `unknown_project`, `unknown_workspace`, `workspace_mismatch` | The selection is unregistered, the workspace has no eligible root or member, or the IDs disagree. |
| `edit_spans_files` | An unstamped edit plan touches more than one file. |
| `project_registry_unavailable` | The registry could not be consulted; an outage is not an empty registry. |
| `config_unavailable` | Required root I/O is unavailable (for example, the queue is full or the deadline elapsed), or the result of an unstamped write is unknown. A module's unstamped project `config.get` waits for a busy root until its root I/O deadline; edits can refuse a busy root immediately. |

Selection and availability failures in edit requests are `ERROR` frames.
Layer and field refusals are rows in the plan view. A malformed control body
still yields `invalid_control_body`, and invalid parameters yield `invalid_params`.

Approval of daemon entries is not live reconciliation. The intended activation
boundary is the next deliberate daemon start; status compares approved entries
with the supervisor's in-force configuration and reports known differences by
`module_id` and `entry` in `approved_but_inactive`. Sensitive values are
fingerprinted, not printed. A `restart_required: true` apply reply means the
applied generation includes such a deferred daemon configuration change.
Startup still reads live `subc.jsonc` until approved-snapshot bootstrap is
available, so a restart is not itself evidence of approved configuration.

Unstamp validates the files before planning and, after approval, reconciles
them immediately as an Execute rescan would. It cancels proposals, keeps store
history and notifies opted-in modules whose served file view changed. See
[`operating-the-daemon.md`](operating-the-daemon.md#integrity-and-platform-limits)
for integrity modes, recovery and the stopped-daemon, data-home relocation,
keychain deletion, old-binary and program-identity platform gaps. Release builds
currently cannot approve a stamp; development approval does not enable release
surfaces.

### Agent policy trust root

These channel-0 requests reject unknown fields. `server.describe` advertises the
operator ops; `HELLO_ACK` advertises the module ops.

| Op | Caller | Body fields | Reply fields |
| --- | --- | --- | --- |
| `policy_root.get` | Local operator client or registered module | none | `pin`, `generation` |
| `policy_root.set` | Local operator client | `key_id`, `public_key_hex` | `digest`, `state: "pending"` |
| `policy_root.rotate` | The vault only: principal `reserved:claustrum`, a module the daemon launched and verified by its launch nonce ([principal.md](principal.md)) | `statement_b64`, `signature_hex` | `digest`, `state: "pending"` |
| `policy_root.recover` | Local operator client | `body_b64` | `digest`, `state: "pending"` |

`pin` is either `null` or `{key_id, public_key_hex, retired_key_ids}`. `generation`
is the approved configuration generation, or zero on an unstamped install. The
pin is not a secret; an enforcer should re-read it when verifying a policy.

Rotation signs `ck-policy-root-rotation/v1` followed by a zero byte and the body
encoded using the JSON Canonicalization Scheme (RFC 8785). Recovery submits the vault's canonically encoded
`ck-policy-root-recovery/v1` body without an old-root signature. Body encoding is
standard padded Base64; signatures and public keys use lowercase hex. A pending
move refuses a second with `policy_root_change_pending`. An already-used
destination refuses with `policy_root_key_reused`.

`policy_root.set` refuses a valid pin (`policy_root_pin_exists`), but may replace
a malformed or null section. A missing ledger on an install with pin history,
or one that no longer extends its stamped checkpoint, is refused with
`policy_root_ledger_unavailable`.

The digest is an ordinary configuration plan digest. Apply it with
`ck config apply <digest>`. On stamped installs, follow the existing configuration
review process. On unstamped installs, apply requires interactive OS presence;
the module-facing `operator.confirm` operation is unchanged. A pending reply
means the pin has not changed: the vault keeps signing with the old key until
`policy_root.get` reports the new pin. A direct `config.plan` edit targeting the
daemon section is refused.

### `app.open` and `connection.describe`

`{"op":"app.open","app":"<id>"}` replies with
`{"op":"app.open","status":"launched","pid":4242}` or
`{"op":"app.open","status":"already_running","pid":4242}`.
The refusal checks are, in order: `app_open_not_used` (macOS),
`app_open_disabled`, `app_unknown`, `app_not_installed`, and
`app_verification_failed`. The already-running success check precedes the
installation check. The Rust client's `SubcConsumer::app_open` returns the open
`AppOpenReply` status, or `AppOpenError::Refused { code, message }` for any
daemon error frame; other failures are `AppOpenError::Call`. Future status
strings and refusal codes are preserved. Handler-produced examples and the
regeneration command are in [`app-open-v1`](../vectors/app-open-v1/README.md).
Test: `app_open_vectors_match_real_handler_bytes` (`crates/subc-daemon/src/app_open/tests.rs`).

`{"op":"connection.describe"}` replies with
`{"op":"connection.describe","local":false}` for loopback TCP, or
`{"op":"connection.describe","local":true}` for the daemon's local listener
(Unix socket or Windows named pipe). When recognised, the reply also includes
`"operator_app":{"app":"<id>","pid":4242}`. That value is computed by the
same function as `route.bind`'s operator-app delivery decision; bind delivery
additionally requires the target's `operator-app/v1` capability. The field is
omitted when no app would be stamped. The production app floor currently
admits no build, so every production connection omits it.

These are facts at the call, not a lifetime promise. Revalidation can withdraw
recognition, and a reconnect establishes a different connection. No request
field can select another connection. `SubcConsumer::describe_connection`
returns `Some(ConnectionFacts)` on a supporting daemon; only the client
dispatcher's `unknown_control_op` refusal returns `None` for an older daemon.
An absent result means the daemon doesn't support this query; it says nothing about whether the connection was recognised. All other failures remain errors.
Tests: `connection_describe_wire_shapes` (`crates/subc-control/src/connection_describe.rs`),
`operator_app_bind_is_gated_independently_and_the_stamp_survives_route_commit` (`crates/subc-daemon/src/peer_manifest_tests.rs`),
`describe_connection_decodes_facts_and_only_unknown_control_op_is_older` (`crates/subc-client-rs/src/consumer/connection_ops_tests.rs`), and
`describe_connection_reports_tcp_and_local_listener_on_real_daemon` (`crates/subc-client-rs/tests/real_daemon.rs`).

Phone build attestation is a separate bind-only peer fact: a target declaring
`peer-build/v1` receives `peer_build: "production"` only for an opener attested
as `reserved:callosum` whose build claim was exactly `production`. No other
value is sent; absent claims, other values, other openers, and targets without
the capability omit the key, never send `null`. The raw `build_class` key never
appears in a bind, and targets without the capability keep their existing bind
bytes. HELLO_ACK `subc_capabilities` advertises `peer-build/v1`. See
[route-bind peer facts](module-surface.md#4-routes-routebind).

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
  `route_open_refuses_consumer_identity_that_fails_spawn_attestation` (`crates/subc-daemon/src/control/control_tests.rs`)). When absent, the opener is a direct
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

Availability refusals for a supervised target include `detail.state`, the
supervisor's module-state string (for example `starting`, `stopped`, `failed` or
`disabled`), and `detail.expected_back`, the daemon's own verdict that the target
is expected to serve again without operator action. For a lost process or
connection, a true verdict means the module is enabled and restarting, has a
respawn pending, or has restart budget left. It is not a promise that recovery
will succeed. False means
the daemon does not expect to bring the module back on its own; an absent verdict
means unknown, including when the supervisor's recovery state cannot be read.

These details accompany supervised-but-unregistered `module_warming` and
`target_unavailable` replies, including waiting for a previous process, and the
registered-target checks for an inactive registration, a non-live supervisor
process, no forwarding connection, and `module_reloading`. A
`module_warming` refusal with `detail.reason: "declared_not_ready"` keeps that
reason and adds the recovery details for supervised targets. Targets that
registered without a supervisor entry omit both new fields.
For a supervised target, `declared_not_ready` always reports `expected_back: true`:
the registered process can become ready without restarting. A supervised-but-unregistered
`module_warming` also reports true, because the process is starting, running or
restarting and only has to register. A `module_reloading` refusal also reports
true, whether the daemon is draining the module or the supervisor is replacing its
process, because the planned restart, reload or
swap is already replacing the process and does not spend crash-restart budget.
An exhausted crash budget does not change these verdicts; if the replacement
fails, later opens can report `failed` and `expected_back: false`.
`required_capability_unprovided` keeps its reason and capability details without
`expected_back`: recovery depends on another module. `unknown_module`,
`module_removed` and `module_no_protocol` are unchanged.

## 3. Daemon pushes to clients

| Op | When | Fields |
| --- | --- | --- |
| `route.closing` | A module's routes are about to close. | `module_id`, `channels` (this connection's only), `reason`. |
| `route.closed` | The routes have closed. | As above, plus `drained` (whether in-flight work finished), `abandoned` (pending binds forced down), `excluded_subscriptions`, and `terminal` (whether the daemon will leave the module down until an operator acts). |

An absent `terminal` means unknown, as with older daemons. The Rust SDK preserves
the optional flag on route-end `OutcomeUnknown` and `NotSent` errors through
`CallError::terminal()`, alongside the unchanged `close_reason()`. The flag is
informational: restart and reload reasons still classify as `MayReopen`.

`reason` is one of `reload`, `restart`, `disable`, `crash`,
`capability_denied`, `scope_ended`, `scope_carrier_removed`,
`scope_delegation_changed`, `scope_parent_ended` (`RouteCloseReason` in
`crates/subc-protocol/src/lib.rs`). Tests:
`route_closed_after_unexpected_module_loss_reports_crash` (`crates/subc-core/tests/daemon_shutdown.rs`), and
`route_closed_crash_at_max_is_terminal` (`crates/subc-core/tests/forwarding.rs`).

## 4. Daemon requests and commands to modules

| Op | Kind | Purpose |
| --- | --- | --- |
| `route.bind` | Request | Bind a client route to a module channel. Carries `route_channel`, `epoch`, `target`, `identity`, the daemon-stamped `principal`, `consumer_capabilities`, `role_versions`, `admission_facts`, the scope stamp and the call mode. Success is an empty `route.bind` reply; a refusal is an `ERROR`, whose code and message reach the opener unchanged (test `route_open_module_rejection_uses_daemon_counter_key` (`crates/subc-daemon/src/control/control_tests.rs`)). |
| `health.check` | Request | Ask the module for its health. Reply: `status` (`ok`, `degraded`, `failing`), optional `detail` and `metrics`. Sent only to modules that declared the op in HELLO `control_ops`. |
| `config.review` | Request | Review a candidate settings tuple against its approved view. The daemon checks the returned fields before including the tuple in a plan. |
| `projects.enumerate` | Request | Read Entorhinal's authoritative project/workspace registry for configuration planning and membership refresh. Requires the nonce-verified reserved `entorhinal` principal, readiness and HELLO `control_ops` opt-in. |
| `module.draining` | Command | Tell the module its routes are draining, with `reason` and an absolute wall-clock `deadline_ms`. Treat the deadline as "no later than". |

What a module does with these is in [`module-surface.md`](module-surface.md).

`projects.enumerate` uses the same channel-0 request/response framing as
`consent/v1`, not a routed capability request. The request is
`{"op":"projects.enumerate"}`. Success is
`{"op":"projects.enumerate","result":<EnumerateReply>}`, where `EnumerateReply`
is the `cortexkit-project-identity` 0.1.5 shape (projects with `rootRecords` and
workspaces with `root` and `fileId`). Refusals use an `ERROR` frame containing
`{"code":"unavailable","message":"store not open yet"}`, for example, rather
than a successful response with an error field. The daemon maps refusals to
`project_registry_unavailable`, preserving the message. Queue admission and
reply share one 10-second deadline; a timeout refuses planning with the same
code. A stamped daemon persists the last successful membership snapshot. Capture
and edit plans, apply, and the delivery worker's periodic refresh update it.
Failed enumerations leave it unchanged. Stamped `config.get` uses that record
and the approved store without enumerating or opening a repository root. A
confirmed drop prevents further delivery, removes opted-in watch rows, and leaves
stored approvals intact for `config.show`.

`daemon.shutdown_intent` is a client-originated channel-0 request with
`{"intent":"restart"}`. It uses the same client admission as `supervisor.restart`
without an operator prompt. Other or missing intents are refused with
`invalid_params`. The reply contains `intent: "restart"` and a wall-clock
`expires_at_ms` for display. The daemon itself measures the 120 seconds on a
clock that keeps counting while the computer sleeps, and each accepted
announcement replaces the previous one. It does not
stop the daemon itself.

## 5. Module requests to the daemon

A module sends these on the connection it registered with `HELLO`; the same
request from a connection that has not registered is refused `not_registered`.
`HELLO_ACK.subc_ops` lists them together with the client operations of
section 2. The module's own HELLO `control_ops` declares the optional control
operations it serves: every registration is treated as granting `route.bind`
and `route.status` (`MODULE_BASELINE_CONTROL_OPS` in
`crates/subc-daemon/src/control/module_operations.rs`), and the daemon sends `health.check` only
to a module that lists it.

| Op | Kind | Purpose |
| --- | --- | --- |
| `route.status` | Push | Report a status string for one of the module's routes, read back by `route.poll`. |
| `catalog.update` | Request | Replace the module's `provides` roles, optionally its capability declaration, and optionally its readiness, without re-registering. Test: `catalog_update_refreshes_catalog_without_disrupting_bound_routes` (`crates/subc-core/tests/catalog_update.rs`). |
| `catalog.update_role_declarations` | Request | Replace the module's complete `role_declarations` map, or clear it with `{}`, without changing provider roles, capabilities or readiness. Served since daemon 0.44.9. |
 | `supervisor.live_roots` | Request | Count live and pending routes per canonical project root, also including the workdir of each bound scoped route. A route contributes once when both paths match. |
 | `policy_root.get` | Request | Read the current agent policy trust root and its generation. |
 | `policy_root.rotate` | Request | Propose a signed trust-root rotation from the launch-verified vault. |
| `scope.sync`, `scope.apply`, `scope.describe` | Request | Register, change and read scopes. See [`scopes.md`](scopes.md). |
| `scope.watch` | Subscription | Snapshot and follow the lifetime of selected scope epochs (below). |
| `config.declare`, `config.get`, `config.applied` | Request | Declare a settings schema, fetch an approved or live view, and acknowledge a delivered attempt. |
| `operator.confirm` | Request | Ask the person at the machine to confirm one in-flight write (section 5.1). |

**Project settings delivery.** Modules may receive `config.changed {project_id}`
after declaring on a delivery connection, even if they have not yet requested
that project. The push is a hint: fetch the view with `config.get {project_id}`.
For a deliverable view with an attempt, send `config.applied` with the same
project ID and attempt. A pending project generation waits for its reviewed
user generation to be active with no pending user generation. User ACK wakes
waiting projects; a failed user basis fails its dependent pending project views.
A project NACK (`config.applied` with `result: "nack"`, the module refusing the view) keeps that
project's last active view and does not affect other tuples, route policy, or
process recovery.

A known project without its own tuple receives the active user view with the
requested `project_id` and no attempt. On an unstamped install a project get
reads the current registry entry and layer files, validates the tuple without
`config.review`, and returns `generation: 0` and `status: "unstamped"`; fallback
requires that the applicable workspace and project files are absent. An
attempt-less view does not need an acknowledgement. Every two seconds the
unstamped poller checks requested projects for file and registry changes. A
valid change notifies that project's requesting connections; a failure leaves
its last good view and can report `file_error` on its watch row. A bare
`config.get` does no project root I/O.

### Scope lifecycle watches

On a module's registered connection, send a channel-0 subscription `REQUEST`:

```json
{"op":"scope.watch","scopes":[{"scope_ref":{"owner":{"kind":"reserved","module_id":"owner"},"ref":"session"},"epoch":7}]}
```

The daemon advertises `scope.watch` in `HELLO_ACK.subc_ops`. The request is an
immutable set of at most **256 selectors**, and each connection may hold at most
**16 active scope watches**. Either limit refuses with `scope_watch_limit_exceeded`;
a repeated active correlation is `duplicate_subscription`. Duplicate selectors
count toward the input limit but appear only once in the snapshot. To add to or
replace a set, cancel the old watch and reopen; no update operation exists.

The first frame is a nonterminal `RESPONSE` (without `LAST`), with the same
channel, epoch zero and correlation id:

```json
{"op":"scope.watch","scopes":[{"scope_ref":{"owner":{"kind":"reserved","module_id":"owner"},"ref":"session"},"epoch":7,"status":"live"}]}
```

Every selected epoch reports `live`, `ended`, `superseded` (also `by_epoch`), or
`unknown`. The snapshot and subscription are captured atomically. Later terminal
transitions are `STREAM_DATA` on the same correlation:

```json
{"op":"scope.watch","event":"superseded","scope_ref":{"owner":{"kind":"reserved","module_id":"owner"},"ref":"session"},"epoch":7,"by_epoch":8}
```

`ended` means the scope ended, including expiry; `superseded` means a newer epoch
of that ref became live. `by_epoch` is absent for `ended`. Each selector gets at
most one push, retaining its first terminal event even if egress is blocked and
the ref changes again. Already-terminal snapshot entries are not pushed again.
Same-epoch changes, including re-parenting a worker under a moved head, do not
end that worker and produce no event for it. See [`scopes.md`](scopes.md#5-reading-scopedescribe-and-scopewatch).

Reader admission is exactly `scope.describe`'s: a registered module connection
is required, and its existing per-owner reader gate is reused. A denied selector
is always `unknown` and emits no events, before or after its scope ends. Unknown
also covers a ref the daemon has never seen, lost tombstones after eviction or a
restart, and an epoch newer than any known one; it is not evidence of an ending.

As with `catalog.watch`, channel-0 `CANCEL` aborts the task without an ordinary
reply, connection cleanup removes its watches, and neither watch consumes a
routed request/watch credit. Producers never wait for egress while holding scope
or forwarding locks. Reconnect ends the feed: register again and open a new
watch to get a fresh snapshot; there is no replay cursor or persistent history.
Rust modules use `ModuleHandle::scope_watch`, inspect `ScopeSubscription.initial`,
and read pushes with `next`; dropping or unsubscribing cancels. This is not a
consumer-connection API. The TypeScript provider currently has no registered
scope reader/stream API, so its scope watch SDK remains a follow-up.

Tests:
- `scope_watch_wire_shapes_require_op_and_accept_future_fields` (`crates/subc-control/src/scope_watch.rs`).
- `scope_watch_live_scope_gets_ended_exactly_once` (`crates/subc-daemon/src/scope_watch_tests.rs`).
- `scope_watch_slow_consumer_retains_first_ended_and_superseded_events` (`crates/subc-daemon/src/scope_watch_tests.rs`).
- `scope_watch_child_survives_head_epoch_move_without_push_or_route_drain` (`crates/subc-daemon/src/scope_watch_tests.rs`).

### Live role-declaration updates

Send a channel-0 `REQUEST` on the module's own HELLO-registered connection:

```json
{"op":"catalog.update_role_declarations","role_declarations":{"media-provider/v1":{"engines":["image"]}}}
```

The map replaces the whole registered map, not individual entries; keys absent
from the replacement disappear. `"role_declarations": {}` clears it. After the
daemon replies `{"op":"catalog.update_role_declarations"}`, `catalog.list`
serves the replacement (an empty map is omitted from catalog entries). The body
does not name a module: ownership comes only from the registered connection.
`catalog.update` still leaves role declarations unchanged.

The daemon advertises `catalog.update_role_declarations` in `HELLO_ACK.subc_ops`.
Check that exact op, not `catalog.update`: older daemons refuse an unknown op
rather than silently accepting an unknown member of `catalog.update`.
Rust's `ModuleHandle::update_role_declarations` returns
`CatalogUpdateError::NotSupported` without sending when the op is not advertised.

Refusals are channel-0 `ERROR` frames and leave the previous map unchanged:

| Code | Meaning |
| --- | --- |
| `not_registered` | The request is not on the module's own registered connection. |
| `invalid_control_body` | The request body is malformed, including a missing `role_declarations` member. |
| `invalid_role_declarations` | The member is not an object, a key violates capability-name grammar, a value is not an object, or the whole map exceeds 16384 compact JSON bytes. The error message names the failed rule. |

The map follows the same shape, key and byte-limit rules as HELLO. Unlike HELLO,
which drops a bad map with a warning and continues registration, live updates
are refused. Role-specific object contents remain opaque to the daemon. The Rust
SDK preserves validation refusals as `CatalogUpdateError::Rejected(ErrorBody)`.

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

The daemon advertises capabilities to modules in `HELLO_ACK.subc_capabilities`
and to clients in `server.describe.capabilities`. Both advertise the settings
capabilities under the same conditions. Daemons before 0.50.33 advertised them
only in `server.describe`.

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
| `config.settings/v1` | A config runtime is active and channel-0 settings reads and watches are available to authorized readers. |
| `config.settings.projects/v1` | Project settings are enabled in that runtime. Workspace and project edits, project show and delivery, opted-in project watch rows, and the project-specific refusal codes are supported. |

A client checks for a capability before sending a field or op that depends on
it. Modules supporting older daemons must check `server.describe` when
`HELLO_ACK` omits a settings capability.
