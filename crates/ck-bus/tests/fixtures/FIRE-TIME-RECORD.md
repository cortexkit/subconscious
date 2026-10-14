# ck-bus slice-0 fire-time record

Recorded from subconscious `e3ecd52911108d1b1e337b8afa0018fee55ca335` while the
slice was being authored on 2026-09-20. The live in-process daemon reported
`server.describe.build_git_sha = e3ecd52911108d1b1e337b8afa0018fee55ca335-dirty`.

## Source sections read

The implementation was checked against `docs/ck-bus.md`:

- Constraints: Repository and dependencies; Acceptance test layout; Module declaration and
  acceptance fixture; Sequencing and file fences, including the row/slice/test-file table;
  and Gates.
- Acceptance ladder: preamble, completeness and serving-side paragraphs, gating-ladder
  legend, Module declaration and registration row, and its row-contents paragraph.
- Chair rulings: R3 (serialized integration ref and disjoint fences), R6
  (`supervisor.provenance` pid source), and R10 (real route-target ids with harness stubs).

## Daemon and control-wire records

- `crates/subc-daemon/Cargo.toml`: declared version `0.20.5`.
- `crates/subc-control/src/lib.rs::SupervisorModuleProvenance` fields:
  `module_id`, `module_declared`, `daemon_observed`.
- `crates/subc-control/src/lib.rs::SupervisorObservedProcess` fields: optional `pid`,
  optional `spawned_at_ms`, optional `spawned_from`, and `running_image`.
- `crates/subc-control/src/lib.rs::CatalogEntry` fields: `module_id`, `ready`, optional
  `module_version`, `roles`, `control_ops`, optional `capabilities`, and optional
  `self_signals`. The declaration arm reads advertised operations from `control_ops`.
- `crates/subc-control/src/lib.rs::TerminalEntry` fields: optional `daemon_incarnation`,
  optional `exit_code`, optional `exit_signal`, completion timestamp `at_ms`,
  `disposition`, optional `exit_kind`, and optional `disposition_detail`.
- `crates/subc-control/src/lib.rs::SupervisorHealthEntry` fields: `module_id`, `status`,
  optional `detail`, optional `metrics`, `consecutive_failures`, `late_answer_count`,
  optional `last_late_answer_latency_ms`, optional `last_action`, optional
  `last_action_ms`, and optional `last_probe_ms`.
- Module-side health is `subc_protocol::session::ModuleControlResponse::HealthCheck` with
  `status`, optional `detail`, and optional open-JSON `metrics`; statuses are `Ok`,
  `Degraded`, and `Failing`. The initial skeleton writes `Failing`, detail
  `bus.health.down`, and byte-exact `metrics.class = "Unavailable"`.
- `crates/subc-daemon/src/control.rs::handle_supervisor_health_probe` relays probe metrics
  whole. `crates/subc-daemon/src/supervise.rs::handle_health_report` calls
  `truncate_health_metrics` before storing the cached supervisor-health entry. The
  truncation asymmetry therefore still holds.
- `crates/subc-daemon/src/supervise.rs::handle_health_report` resets
  `consecutive_failures` for every answered report and dispatches an answered unhealthy
  status through its configured action. The committed `ckbus` declaration fixes both
  unhealthy actions to `report`; `apply_l3_health_action`'s `Report` branch logs and does
  not restart. This discharges `health-down-escalation-unpinned` for this declaration.
- `ClientControlRequest::RouteOpen.consumer_identity` carries optional
  `ConsumerIdentity { module_id, launch_nonce }`.
  `crates/subc-daemon/src/control.rs::route_open_principal` stamps
  `Principal::Reserved { module_id }` only after
  `SupervisorHandle::spawned_consumer_authorized`; absence stamps `Principal::Direct`,
  and an altered claim refuses `bad_consumer_identity`. The callee observes the result at
  `subc_client_rs::RouteBindRequest.principal: Option<Principal>`.

## Shared-path and naming records

The module directory authority is `cortexkit-store-types` `0.2.2` at commons
`4f09c7c7c7f86394d21abde6ed3f97b582ee4d28`:
`cortexkit_store_types::module_data_dir("ckbus")`. The returned directory is used as-is
and must be absolute.

This corrects settled-spec lines 90, 149 and 202, which name `cortexkit-paths` 0.1.1.
Fire-time inspection proved that crate exports project-root identity only. The operator
ruling deletes `paths-crate-function-absent`, keeps `cortexkit-paths` out of this crate,
and substitutes the store-types authority above.

At the pinned commons revision, `cortexkit-bus-naming` 0.1.0 provides:

- `AccountNames::consumer_name("ckbus_dead")` -> `c_ckbus_dead`;
- `AccountNames::process_record_name(module_id, generation, epoch)` ->
  `nats.{module_id}.g{generation}.e{epoch}`;
- `AccountNames::system_account_record_name()` -> `nats.sysaccount.{acct}`.

It does not provide constructors for `nats.ckbus.self` or the census-key grammar.
Those two names therefore carry `naming-constructor-absent`; the owed change belongs in
commons and is not guessed or emitted by this slice.

## Harness stubs and reply-shape condition

The acceptance daemon registers harness-owned tool providers under the real ids.
Claustrum's fire-time vocabulary at checkout `260b131` is exactly:

- `credential.get`
- `credential.get_scoped`
- `credential.sign`
- `credential.public_key`
- `credential.list_scoped`
- `credential.status`
- `credential.report_auth_failure`

Callosum registers exactly `callosum.hub_read`; that spelling is foundation-backed at
checkout `fc0a595`, not confirmed from a callosum implementation site.

No authoritative present, confirmed-absent, denied, or clamped reply bodies were
available. The shape table is therefore empty and every advertised operation refuses
with `stub_reply_shape_unrecorded`, naming its module and operation. Exact condition:

> stub-reply-shape-unrecorded — the harness stub for <module> has no recorded reply body
> for <op>, so the row cannot gate on a served reply. The row records this name and says
> which op it stopped on.

The reply bodies are owed by the operator. The table is data-backed so the follow-up adds
recorded rows rather than changing dispatch control flow. A route with no launch claim
reaches the stub as `Principal::Direct` and is refused
`harness_stub_principal_refused`; an altered claim is refused by the daemon as
`bad_consumer_identity` before reaching the stub.

The R10 index still lists older unqualified spellings such as `get` and `sign`, while the
folded serving-side constraint and the current claustrum read use qualified
`credential.*` names. The fire-time verified qualified names above are registered; no
alias is advertised.

## Foundation disposition

`foundation/nats-message-plane-foundation.md` is a byte-for-byte copy from prefrontal
commit `b9e827c69d98fb0c1241925961ebb5184752a25a`, SHA-256
`bc306d7478134fb02b3ac5c5cc8271e52b34cbeffd95fb1a999ed5d6916ef657`, re-vendored by
slice 2 (`foundation/SOURCE`). Slice 0 had vendored `76a910737`, which predates the
2026-09-23 credential-custody amendment. Every row is
mapped to a ck-bus acceptance row or a named exclusion in
`foundation/DISPOSITION-MAPPING.md`, discharging
`foundation-disposition-table-unquoted`.
