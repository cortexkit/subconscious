# Changelog

## 0.33.4

- Add `CatalogEntry::confirm_operations` and `with_confirm_operations`, mirroring the manifest's list of management operations guarded with `operator.confirm`. Absent means unknown; absence is omitted and keeps existing catalog bytes. The daemon relays it and refuses nothing on its basis.
- Add standalone `app.open` bodies, kept out of the exhaustive control enums so no existing match changes: `AppOpenRequest` (`{"op":"app.open","app":"<id>"}`), `AppOpenReply` with `launched` and `already_running` constructors, the open string enum `AppOpenStatus`, `ops::APP_OPEN`, and the refusal codes `app_open_not_used`, `app_open_disabled`, `app_unknown`, `app_not_installed` and `app_verification_failed` (`APP_OPEN_ERROR_CODES`, in the order the daemon checks them).
- Requires `subc-protocol` 0.33.5. `route.open` may carry `identity.build_class` through the protocol's `BindIdentity`; omitted when absent, so existing request bytes are unchanged.

## 0.33.3

- Add `CatalogEntry::flow_operations` and `with_flow_operations`, mirroring the manifest's list of management operations served to flow-scoped calls. Absent means the module did not say, not that it admits none; absence is omitted and keeps existing catalog bytes. `CatalogEntry` remains non-exhaustive and builder-compatible.

## 0.33.2

- Add optional `SupervisorEntry::next_retry_at_ms` and its builder method so operators can see when a failed module will retry after exhausting its restart budget. This Unix-millisecond estimate is recomputed from the daemon's monotonic countdown on each read; macOS sleep pauses the countdown. `SupervisorEntry` remains non-exhaustive, and absent retry times are omitted so existing response bytes are unchanged.

## 0.33.1

- Add `CatalogEntry::role_declarations` and `with_role_declarations`, mirroring opaque per-role objects from the registered manifest. Empty maps are omitted, preserving legacy catalog bytes. `CatalogEntry` remains non-exhaustive and builder-compatible.

## 0.33.0

- Minor bump because this crate's public types come from subc-protocol, which moves to 0.33.0 (`ToolCallRequest` gains the optional `output_refs` member). A consumer that also depends on subc-protocol directly must move both together, or two incompatible copies of the protocol types would meet. No other change.

## 0.32.7

- Add `CatalogEntry::operation_payload` and `with_operation_payload`, mirroring the manifest's operation-name-to-limit map so federation gateways can discover accepted request payloads. Empty maps are omitted and preserve existing catalog bytes. Requires subc-protocol 0.32.12.

## 0.32.6

- Add optional `configured_enabled` and `with_configured_enabled` to non-exhaustive `SupervisorEntry`. Absent values are skipped and older list entries retain their bytes. The value comes from the daemon's loaded configuration; compare it with runtime `enabled` rather than treating it as a drift flag. This is additive: lifecycle acknowledgement types and bytes are unchanged, including `ClientControlResponse::SupervisorAck` literals and field patterns used by published clients and daemons.

## 0.32.5

- Add optional `ActivityActor.workdir` and `project_id` from the route scope. Absent values preserve existing activity bytes. Code that builds `ActivityActor` with a struct literal must add the two fields.

## 0.32.4

- Add optional `CatalogEntry.rich_ui` and `nonce_verified`. Both are omitted when absent/false, preserving legacy catalog bytes. The daemon copies rich-UI metadata from the registered manifest and sets `nonce_verified` only when HELLO passes the launch-nonce check required for a reserved module or swap candidate.

## 0.32.3

- Add `ClientControlRequest::SupervisorRescanChecked { preview, candidate, expect }` (op `supervisor.rescan_checked`). `preview: true` with `candidate` parses and previews caller-supplied configuration text without writing or applying it (at most `RESCAN_CANDIDATE_MAX_BYTES`, 1 MiB) and applies nothing; `preview: false` with `expect` applies the daemon's config file as `supervisor.rescan` does, after refusing `config_changed_since_preview` when the file's SHA-256 or the staged generation (daemon incarnation and counter) no longer matches. Any other combination is refused `invalid_control_body`. Send it only to a daemon advertising `rescan-candidate-preview/v1` (`RESCAN_CANDIDATE_PREVIEW_CAPABILITY`). `SupervisorRescan { preview }` is unchanged.
- `SupervisorRescanResult` gains `module_actions` (per module `restart`, `reload`, `start` or `stop`, with the changed parts, drain budget and busy-gauge readings), `earlier_pending` and `expect`, filled only by a candidate preview. Empty and absent values are skipped, so `supervisor.rescan` replies keep their bytes. New types: `RescanExpect`, `StagedGeneration`, `RescanModuleAction`, `RescanModuleActionKind` (an open string enum), `BusyGaugeReading`.
- `ClientControlResponse::ServerDescribe` gains optional `config_path`: the config file the daemon reads, never canonicalized, sent only when absolute. Older readers ignore it.
- Code that builds `SupervisorRescanResult` or `ServerDescribe` with a struct literal, or matches `ClientControlRequest` exhaustively, must add the new fields or arm.

## 0.32.2

- Add standalone non-exhaustive `scope.served_by` request and reply bodies with an open answer enum, preserving exhaustive enum source compatibility.

## 0.32.1

- Add `CatalogEntry.operation_delivery` and `with_operation_delivery` to mirror management delivery hints from the registered manifest. Values use subc-protocol 0.32.5's open `OperationDelivery` type: unknown strings survive, and only `"watch"` on a declared query is subscribable. Empty maps are omitted so existing catalog replies retain their wire bytes. Older readers ignore the additive field.

## 0.32.0

- `PendingReloadVerdict` gains `spec: Option<LaunchSpecAgreement>`: whether the running process was started with the module's currently configured launch inputs (program, arguments, configured environment and protocol). `Match`, `Mismatch`, `Unavailable { reason }` (`not_running`, `spawned_spec_unavailable`), or `Unknown` for a status a newer daemon sends. `None` means the daemon predates the field, never agreement. Breaking for code that builds `PendingReloadVerdict` with a struct literal: add `spec`.

## 0.31.0

- **Breaking dependency upgrade.** Use subc-protocol 0.32 scope selectors with optional bind proofs. Existing selectors serialize unchanged when the proof is absent.

## 0.30.0

- **Breaking.** Requires `subc-protocol` 0.31. Public control types carry protocol types, so consumers must upgrade control and protocol together to avoid incompatible copies. Replace `ScopeStamp` literals with `ScopeStamp::new` and its `with_*` setters, and account for optional `call_mode` in raw `ModuleControlRequest::RouteBind` constructors and exhaustive patterns.
- Call-mode providers declare `call-modes/v1` only after reading every bind's mode, handling classic, codemode and flow, and refusing `CallMode::Unknown`. Legacy non-delegating run and flow binds remain supported; delegating runs require the new capability. Control operations otherwise keep their existing behavior.

## 0.29.5

- Add the `running_executable_permission_denied` running-image unavailable reason for Linux non-dumpable processes. Keep the existing unavailable wire envelope so older readers retain the new reason as an unknown string. Exhaustive Rust matches on `RunningImageUnavailableReason` must account for the new arm.
- Add the `process_hidden` unavailable reason and optional `SupervisorEntry.waiting_for_previous_pid` for a module blocked by a previous hidden instance. The previous PID is separate from the daemon's currently supervised process identity; old payloads and readers remain compatible.

## 0.29.4

- Add standalone, op-tagged activity record, query and subscribe request/response types, ledger entries, actor citations and cursors. The exhaustive `ClientControlRequest`, `ClientControlResponse` and `ClientControlPush` enums are unchanged; dispatch activity bodies using the operation name in each body's `op` field.
- Add activity operation names, refusal-code constants and the `activity-ledger/v1` capability. Query replies require a current-incarnation `head` cursor; record requests contain no caller-supplied actor.

## 0.29.3

- Add optional `launch_nonce_source` to supervisor list entries and daemon-observed module provenance for per-spawn Windows pipe consumption. Older payloads still decode, and the fields are omitted when no observation is available.

## 0.29.2

- Add `RunningImageEvidence::WindowsSpawnFileId` (volume serial number and full 128-bit file ID) and the `windows_working_set` memory kind, so Windows reports a running-image check and resource readings instead of "unavailable". Older readers decode the new evidence as unknown. `RunningImageEvidence` is not `#[non_exhaustive]`, so a downstream crate that matches it exhaustively must add the new arm.

## 0.29.1

- Add optional manifest-mirrored bus event declarations to `CatalogEntry`, omitting the field when the manifest has no event declaration.

## 0.29.0

- **Breaking.** `CatalogEntry` and `SupervisorEntry` are `#[non_exhaustive]`; build them with `new` and the `with_*` setters, so later fields are not breaking. Requires `subc-protocol` 0.30. Wire bytes are unchanged.

## 0.28.0

- Takes subc-protocol 0.29.0: `ToolCallRequest.preset` and `ScopeAttributes.flow_id` are optional wire fields, but this release is breaking for Rust struct literals. Update protocol and control dependencies together to avoid incompatible public protocol types.

## 0.27.1

- Correct the restart drain override documentation: older daemons ignore the unknown field and use their normal drain budget, rather than refusing the request. No wire or runtime change.

## 0.27.0 — 2026-10-02

- Breaking Rust API: `RouteClosing` and `RouteClosed` gain `channels: Vec<u16>`, naming the receiving connection's affected routes. Struct literals and exhaustive patterns must include the field. Older wire pushes decode with an empty list; current serialization always emits the `channels` field.

## 0.26.0 — 2026-10-01

- Minor bump because this crate's public types come from `subc-protocol`, which moves to 0.28.0 (its `ToolCallRequest` gains an `origin` field; see that crate's changelog). A consumer that also depends on `subc-protocol` directly must move both together, or two incompatible copies of the protocol types would meet.
- Breaking: `ClientControlRequest::RouteOpen` gains `role_versions: Option<BTreeMap<String, String>>`, the provider-role versions the consumer speaks on the route (`{"tool-provider": "v1"}`). It is omitted on the wire when `None`, and an empty map means the same as `None`. Send it only to a daemon advertising `route-role-versions/v1`. New golden vectors: route.open with and without it.
