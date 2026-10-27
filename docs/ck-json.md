# ck and ckni output contracts

`ckni` is the non-interactive name of the `ck` executable. Use it for the
versioned JSON contract below. `ck --json <command>` retains its original
piped JSON bytes and shapes, even on a terminal: no ANSI, progress, or implicit
wait for a module to become healthy. An action returns its acknowledgement.
Place `--json` before the command (built-ins also accept it in their tail).

## Plain output coverage

Only **`health` (overview), `module list`, and `activity`** have ckni tables:
headers, aligned columns, words instead of glyphs, fixed units, `-` for absent
values, no color, no list trimming, and diagnostic detail only with `--verbose`.

Every other built-in prints piped `ck` plain text: `health <module>`, `module
status`, `module logs`, `module stderr`, `module terminals`, `module rescan`,
`module release`, `module restart` (including `--swap`), `module stop`, `module
start`, `routes`, `catalog`, `provenance`, `daemon`, `daemon triage`, `daemon
lint`, `machine show`, `machine adopt`, `quota`, `setup`, `upgrade`, help and
version. These are not covered by the three-table formatting guarantees.
ckni never prompts; setup refuses with a usage error where ck would ask.

## JSON conventions and exceptions

For the schemas below, an existing object gains a top-level string `schema`;
an existing array becomes `{"schema":"…/v1","entries":[…]}`. All original
fields and nested values are retained for raw JSON payloads, including optional
fields introduced by a newer daemon. Activity and catalog output include only
fields this version of ck knows; extra fields from a newer daemon may be omitted.
Objects are pretty-printed except in streams. Object key order
is not a contract. Null, absent, zero, and empty collections are distinct.
Consumers should ignore unknown fields. A schema version identifies the CLI
envelope, not the version of a module's metrics or nested management payload.

With `activity --follow` or `module logs <module> -f`, stdout is JSONL:
one complete object per line, including the initial entries, and each object
has `schema`. Empty streams emit no placeholder. Errors and log filter counts
go to stderr, never into the stream. Interrupting a stream stops watching; it
does not undo recorded activity. Activity emits the initial query newest first,
then follows new matching entries, including entries recorded while the initial
query was being printed.

The following commands have no JSON form and are refused before execution,
with exit 2 and `ckni: usage: <command> has no JSON form` on stderr (followed
by the help next step). Bare `ckni --json` is help and is refused too.

| Command | JSON classification |
| --- | --- |
| `setup` | no JSON form |
| `upgrade` | no JSON form |
| `daemon lint` | no JSON form |
| `help` | no JSON form |
| `version` | no JSON form |

The following outputs are exceptions to the versioned JSON forms:

- `--ck-build-shape` and `--ck-cli-face-reserved-names`: machine probes read
  by setup; their bytes are identical under both executable names, even with
  `--json`.
- The terminal app: bare ckni is refused with usage, not launched.
- External faces: pass-through, as described below.
- `module restart` (including `--swap`), `module stop` and `module start`:
  retain piped ck output in both plain and JSON modes. JSON has **no schema
  added**: `{op:"supervisor.ack", module_id,
  applied, state, daemon, module}`. `daemon` is the responding connection-file
  path and `module` is its roster record (or `{}` when absent). Acknowledged
  restart/start/stop is not a promise of eventual healthy/stopped state.

**External faces (`ck-<domain>` executables) are not covered by these guarantees.**
Both names pass through their output, ANSI, JSON, stderr and exit status
unchanged. Flags after an external domain are passed verbatim to that face;
a dispatcher `--json` does not wrap or reinterpret its output.

## Schemas

The shapes below use `string`, `number`, `boolean`, `null`, `[]` (array), and
`{}` (object) as type notation, not literal example output. `?` denotes an
optional field. Most daemon-backed objects keep their existing `op` field.
`JSON` means any JSON value. Named record labels refer to shapes described in
this document. No table truncation or terminal escaping changes JSON data.

### `ck.module.list/v1`

`module list`: `{schema, op:"supervisor.list", generation:number, modules:[Module]}`.
`Module` is the complete supervisor roster entry: `module_id`, `state`,
`enabled`, `live`, `health`, configured enablement, restart counts and policy,
drain budget, exit/retry/resource data, and pending-reload evidence when the
daemon supplies them. The JSON is the roster, not the joined ckni plain table;
it does not invent PID, uptime or build fields from other sources.

### `ck.health/v1`

`health`: `{schema, op:"supervisor.health", generation:number, modules:[Health]}`.
A health record carries `module_id`, `status` and optional detail, metrics,
probe timing, counts and policy fields. Metrics retain the module's own JSON
shape and units. An empty roster produces `modules:[]`.

### `ck.health.detail/v1`

`health <module>`: `{schema, op:"supervisor.health_probe", module_id:string,
status:"ok"|"degraded"|"failing", detail?:string, metrics?:JSON}`. This probes
one module rather than selecting the overview's cached record. Metrics are
module-defined JSON. An unknown module is an error on stderr.

### `ck.activity/v1`

`activity`: `{schema, entries:[activity_entry]}`. With `--follow`, each line is
`{schema, ...activity_entry}` instead. An entry carries `daemon_incarnation:string`,
`seq:number`, `at_ms:number`, `kind:string`, `module:string`, `summary:string`,
`details:{[key:string]:JSON}`, optional `subject:{type:string,id:string}` and
`actor:{principal:principal, scope_owner?:principal, agent_id?:string,
run_id?:string, flow_id?:string, project_root?:string, workdir?:string,
project_id?:string}`. A `principal` is `{kind:"reserved",module_id:string}`,
`{kind:"direct"}` or `{kind:"unverified"}`. Arbitrary details are retained
without rendering table summaries. Pagination and subscription cursors are not
printed.

### `ck.module.status/v1`

`module status <module>`: `{schema, module:Module, health:Health|null,
catalog:catalog_entry|null, last_exit_reason:string|null}`. The exit reason is
the latest matching terminal record's disposition detail, when available. A
module missing from the supervisor is refused.
The catalog record describes wire registration/readiness and may be absent
even when a process is running.

### `ck.module.logs/v1`

`module logs` (no module): `{schema, files:[{module_id:string, lane:string,
path:string, size:number, last_timestamp:string}]}`. Size is bytes. An empty
census has `files:[]`. `-f` without a module still returns the census,
as one JSON object; it does not start a follower.

`module logs <module>`: `{schema, lines:[log_line]}`. An empty log has
`lines:[]`; an unknown module is refused. `log_line` is `{source:string,
line:string, timestamp:string|null, time_inherited:boolean,
level:"TRACE"|"DEBUG"|"INFO"|"WARN"|"ERROR"|null, tag:string|null}`.
`timestamp` is the parsed or inherited
UTC time; raw lines without a time keep null. In follow mode each initial or
appended line is `{schema, ...log_line}`, not a batch with a `lines` field.

### `ck.module.stderr/v1`

`module stderr <module>`: `{schema, op:"supervisor.stderr_tail",
module_id:string, capture:{state:"captured"|"incomplete"|"not_captured",
reason?:string}, entries:[stderr_entry], dropped_lines?:number}`.
Entries are `{kind:"line",text:string,truncated?:boolean,at_ms?:number}` or
`{kind:"process_start"}`. False `truncated`, absent timestamps and zero
`dropped_lines` are omitted. Raw capture lines are not reformatted.

### `ck.module.terminals/v1`

`module terminals <module>`: `{schema, op:"supervisor.terminals",
module_id:string, daemon_started_at_ms:number, entries:[terminal_entry],
dropped?:number, journal_skipped_lines?:number, journal_read_errors?:number,
journal_write_failures?:number}`. Zero counters are omitted. Entries carry
`at_ms:number`, `disposition:string`, and optional `daemon_incarnation:string`,
`exit_code:number`, `exit_signal:number`, `exit_kind:string`, and
`disposition_detail:string`. Known dispositions include `stopped`, `disabled`,
`failed`, `restarting`, `daemon_shutdown`; exit kinds include `clean`, `crash`,
`deliberate_severance`. Unknown wire strings remain intact.

### `ck.module.rescan/v1`

`module rescan [--dry-run]`: `{schema, op:"supervisor.rescan", added:[string],
removed:[string], changed_pending_reload:[string], unchanged:number,
enabled_changes?:[string], preview?:true, restart_required?:[string],
capability_warnings?:[string]}`. Empty optional arrays and false `preview` are
omitted. `unchanged` is a count, not a list. `--dry-run` previews what a rescan
would change and applies nothing. Extra result fields from a newer daemon are
retained without changing this envelope.

### `ck.module.release/v1`

`module release <module>`: `{schema, op:"supervisor.ack", module_id:string,
applied:boolean, state:string, daemon:string, module:Module|{}}`. This is the
same acknowledgement enrichment as the exempt actions, but with a schema.

### `ck.routes/v1`

`routes [<module>]`: `{schema, op:"supervisor.routes", modules:[route_module]}`.
Each `route_module` is `{module_id:string,routes:[route]}`. A `route` is
`{consumer:{kind:"reserved",module_id:string}|{kind:"direct",connection_id:number},
age_ms:number,draining:boolean,drain_reason?:string}`.

### `ck.catalog/v1`

`catalog [<module>]`: `{schema, entries:[catalog_entry]}`. The optional module
argument filters entries; a missing named module prints an empty entries
array and exits nonzero. These are wire-registration records, not the
supervisor roster: identity/version/protocol, operations, routing readiness,
capabilities, signals and role declarations are retained when supplied.

### `ck.provenance/v1`

`provenance <module>`: `{schema, op:"supervisor.provenance", daemon,
modules:[module_provenance]}`. Daemon-observed process, running-image evidence,
and module-declared build/provenance retain their tagged wire objects and
unavailable reasons; absence is not converted into invented build strings.

### `ck.daemon/v1`

`daemon`: `{schema, op:"server.describe", protocol_ver:number, subc_ops:[string],
capabilities:[string], connected_clients:number, counters?:JSON,
build_git_sha?:string, build_lock_digest?:string,
capability_requirements?:[{consumer:string, capability:string,
need:"required"|"optional", verdict:"provided"|"pending"|"never_provided",
episode_seq:number, config_satisfiable:boolean, runtime_available:boolean,
detail:string}], machine_id?:string}`.
Unlike plain text, this does not add the PID/version read from the connection
file. Like piped ck, it omits **`config_path`**; other fields are retained.

### `ck.daemon.triage/v1`

`daemon triage`: `{schema, run_dir:string, start_lock:{candidates:[object]},
connection_file:object, process_liveness:object, log_tail:object,
verdict:{status:"daemon-appears-down"|"daemon-state-ambiguous"|"daemon-appears-live",
reason:string}}`. The nested objects have these shapes:

- Each `start_lock.candidates` item is one of:
  - `{path:string,status:"present",size_bytes:number,mtime:object}`;
  - `{path:string,status:"absent",finding:"start-lock absent"}`;
  - `{path:string,status:"skipped",skipped:string}`.
- `connection_file` is `{candidates:[object],status:"absent-or-unusable"}`
  when no connection file can be selected. Otherwise it is
  `{candidates:[object],selected_path:string,fields:object,port?:JSON,
  daemon_id?:JSON,wire_version?:JSON,pid?:number}`.
  Each candidate is one of:
  - `{path:string,status:"absent",finding:"connection file absent"}`;
  - `{path:string,status:"skipped",skipped:string}` when file metadata cannot
    be read, or the same fields plus `size_bytes:number` and `mtime:object`
    when the file's contents cannot be read;
  - `{path:string,status:"present",size_bytes:number,mtime:object,
    json:"invalid",finding:string}`;
  - `{path:string,status:"present",size_bytes:number,mtime:object,
    json:"valid",fields:object}`.
  Both the selected file's `fields` and a valid candidate's `fields` are
  `{port:{present:boolean,value:JSON},daemon_id:{present:boolean},
  wire_version:{present:boolean}}`. The selected file's outer `port`,
  `daemon_id` and `wire_version` retain their reported JSON values, except
  that a byte-array `daemon_id` becomes a hex string. `pid` is included only
  when it is an unsigned integer.
- Each `mtime` above is `{status:"known",age_seconds:number}` or
  `{status:"skipped",skipped:string}`.
- `process_liveness` is `{status:"skipped",skipped:string}` when no PID is
  available, or one of `{pid:number,status:"skipped",skipped:string}`,
  `{pid:number,status:"dead",executable:{status:"not-running"}}`, and
  `{pid:number,status:"live",executable:{status:"match"|"mismatch",
  observed:string,expected:string}}`.
- `log_tail` is one of `{path:string,status:"absent",finding:"log absent"}`,
  `{path:string,status:"empty",finding:"log empty"}`,
  `{path:string,status:"skipped",skipped:string}`, and
  `{path:string,status:"present",modified_at_ms?:number,lines:[string],
  tail_lines:number,summary:string}`.

Connection-file secrets are never included. The report may be printed
successfully even when the command exits nonzero to reflect a down or ambiguous
daemon.

### `ck.machine.show/v1`

`machine show`: `{schema, file:{path:string,machine_id:string|null,error?:string},
daemon:{running:false}|{running:true,pid:number,machine_id:string|null},
differ:boolean}`. `differ` is true only when both ids are known and unequal;
otherwise false. This command works without a daemon.

### `ck.machine.adopt/v1`

`machine adopt <id> [--force]`: `{schema, path:string, machine_id:string,
previous:string|null, takes_effect:"next_daemon_start", daemon_running:boolean,
daemon_serves:string|null}`. `previous` is null if the old file could not be
read. Without `--force`, a reachable daemon refuses adoption. With `--force`,
the stored id changes, not the identity of the already-running daemon.

### `ck.quota/v1`

`quota [<provider>]`: `{schema,result:[JSON]}`, the quota provider's existing
response with `schema` added. A top-level array is not accepted.
The array's contents belong to the quota provider rather than a fixed ck
record shape: provider/account records retain usage windows, spend pools,
identity/status and freshness/collection diagnostics exactly as reported.
`--verbose` and `--redact` are text-rendering flags, not JSON
transformations; the JSON retains all accounts and identity fields. The
provider argument validates a provider id but does not filter this raw payload.

## Shared nested records

- **Module**: required `module_id:string`, `state:string`,
  `enabled:boolean`, `live:boolean`, `protocol:"subc"|"none"`, `health:string`,
  and nullable `last_probe_ms:number|null`. Optional fields include
  `waiting_for_previous_pid`, `next_retry_at_ms`, `configured_enabled`,
  `launch_nonce_env`, `launch_nonce_source`, `pending_reload`, `last_exit_code`,
  `last_exit_signal`, `last_exit_ms`, `last_exit_kind`, `restart_count`,
  `max_restarts`, `lifetime_restarts`, `spawn_generation`, `restart_window_secs`,
  `drain_timeout_ms`, `restart_backoff_ms`, `restart_max_backoff_ms`, `resources`.
  Counts, PIDs, milliseconds and seconds are numbers; enablement/nonce flags
  are booleans. `pending_reload` and `resources` are described below.
- **Health**: `module_id:string`,
  `status:"ok"|"degraded"|"failing"|"unresponsive"|"unknown"`, optional
  `detail:string`, nullable `metrics:JSON|null`, `consecutive_failures:number`,
  `late_answer_count:number`, optional `last_late_answer_latency_ms:number`,
  nullable `last_action` and `last_action_ms`, optional `last_probe_ms:number`.
- **catalog_entry**: `{module_id:string, ready:boolean,
  not_ready?:{reason:string,capability?:string}, module_version?:string,
  roles:[role], control_ops:[string], capabilities?:capabilities,
  self_signals?:[self_signal], events?:[event], rich_ui?:rich_ui,
  operation_delivery?:{[operation:string]:string},
  operation_payload?:{[operation:string]:{max_bytes:number}},
  flow_operations?:[string],
  role_declarations?:{[role:string]:object}, nonce_verified?:boolean}`.
  The `role`, `capabilities`, `self_signal`, `event` and `rich_ui` shapes are
  described below. An older daemon that omits `ready` is shown as `ready:true`.
  Empty operation/role maps and false `nonce_verified` are omitted.
  `operation_delivery` recognizes `"watch"`; other string values are retained.
  `flow_operations` lists the operations served to flow-scoped calls; absent
  means unknown, not none (see docs/scopes.md).
  Role-declaration object contents belong to the declaring module.
- **Provenance**: `daemon` carries
  `{daemon_build:{build_git_sha?:string,build_lock_digest?:string},
  daemon_observed:{pid?:number,started_at_ms?:number,running_image:object}}`.
  Each `module_provenance` record is `{module_id:string,
  module_declared:{status:"reported",build:object}|{status:"unverifiable"},
  daemon_observed:{launch_nonce_source?:string,pid?:number,spawned_at_ms?:number,
  spawned_from?:string,running_image:object}}`.
  Build fields include optional `build_git_sha`, `build_git_sha_absence_reason`,
  `build_lock_digest`, `wire_crate_version`, `store_schema_version`,
  `launch_nonce_source`. Running-image evidence retains its method/status
  tags, platform-specific fields and unavailable reasons. No digest is invented
  for a platform that reports file identity instead.

#### Catalog roles and declarations

Each `role` object in a catalog entry's `roles` array has one of these shapes,
selected by its `role` field:

- `{role:"tool_provider", tools:[{name:string,description?:string,
  execution_mode:"pure"|"mutating"|"unfenceable",schema:JSON}],
  identity_scope:["session"|"project"],
  concurrency:"serial"|"module_managed"|"stateless_parallel",
  emits_push:boolean, sub_supervises:boolean}`.
- `{role:"pipeline_stage", stage:"transform"|"codec"|"auth",
  applies_to:{provider:string,model:string}, interface:string,
  declares_frozen_floor:boolean, needs_signals:[string], conformance_class:string}`.
- `{role:"management_surface", operations:[{name:string,kind:"query"|"mutate",
  description?:string}], config_schema:JSON,
  observability:[{name:string,kind:"snapshot"|"stream"}],
  identity_scope:["session"|"project"],
  concurrency:"serial"|"module_managed"|"stateless_parallel"}`.
- `{role:"internal_service", service_id:string, transport:"bulk",
  agent_facing:boolean, operations:[string]}`.

Each `identity_scope` item is either `"session"` or `"project"`. A tool's
`schema` and a management surface's `config_schema` are module-defined JSON
values, distinct from the top-level version string.

The remaining catalog declarations are:

- `capabilities`: `{provides:[string], requires:[{capability:string,
  need:"required"|"optional"}], must_never_reach:[string]}`.
- Each `self_signal` in `self_signals` is `{name:string,kind:string,
  effect:"observe"|"mutate",anchored_to:JSON,cadence?:object,
  domain?:string,note?:string}`. Known `kind` values are `keepalive`, `busy`,
  `poller`, `cron`, `sweep`, `watchdog` and `heartbeat`; other strings are
  retained. `anchored_to` is `"fixed_interval"`,
  `{"event":{"event":string}}`, or `{"health_gauges":{"gauges":[string]}}`.
  `cadence` is `{"literal":{"interval_ms":number}}` or
  `{"derived":{"source":string}}`.
- Each `event` in `events` is `{name:string,version:number,headers:[string],
  summary?:string}`.
- `rich_ui`: `{schema:string,manifest_digest:string,bytes:number,chunks:number}`.

#### Pending reload and running images

`pending_reload` is `{path:object,image:object,spec?:object}`:

- `path` is `{status:"match"}`, `{status:"mismatch",configured:string,
  spawned_from:string}`, or `{status:"unavailable",reason:string}`. Known
  unavailable reasons are `not_running` and `spawned_path_unavailable`.
- `spec` is `{status:"match"}`, `{status:"mismatch"}`, or
  `{status:"unavailable",reason:string}`. Known unavailable reasons are
  `not_running` and `spawned_spec_unavailable`.
- `image` is `{status:"match",evidence:image_evidence}`,
  `{status:"mismatch",running:image_evidence,disk:image_evidence}`, or
  `{status:"unavailable",reason:string}`. Provenance's `running_image` uses
  these same shapes.

An `image_evidence` object is one of
`{method:"linux_proc_sha256",digest:string}`,
`{method:"macos_spawn_inode",device:number,inode:number}`, and
`{method:"windows_spawn_file_id",volume_serial_number:number,file_id:[number]}`.
The Windows `file_id` has 16 byte values. Unknown agreement statuses and
evidence methods retain their JSON fields rather than being relabeled.

`resources` is `{status:"measured",memory_bytes:number,memory_kind:string,
swap_bytes?:number,cpu_user_ms:number,cpu_system_ms:number}` or
`{status:"unavailable",reason:string}`. Known `memory_kind` values are
`phys_footprint`, `resident_set` and `windows_working_set`; known unavailable
reasons are `not_running`, `unsupported_platform`, `unreadable` and
`process_identity_unconfirmed`.

## Errors and compatibility

ckni errors are `ckni: <code>: <message>` on stderr, followed by a next-step
line when available. They are not JSON success envelopes. Exit codes are 2
for usage/config errors, 3 for daemon discovery/connection errors and 1 for
other failures. A decoded refusal preserves its code, such as
`fed_not_authorized`; reports that already rendered their error get no extra
line. External faces retain their own exit codes. `ck --json` output keeps its
existing bytes and shapes, including when run on a terminal.
