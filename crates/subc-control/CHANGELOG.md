# Changelog

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
