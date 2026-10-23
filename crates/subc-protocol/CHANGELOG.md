# Changelog

## 0.33.0

- **Breaking for Rust struct literals.** `ToolCallRequest` gains `output_refs: Option<OutputRefs>`, the llm-runner's output-refs descriptor: which stored set of tool-output snapshots an agent's references resolved to, so a provider can read them back from the runner. It is a top-level member beside the arguments, so the model cannot write it; a runner attaches it only to a call whose provider declares `output_refs: true`. The struct stays exhaustive, so every module that builds one with a literal must decide what to send (`ToolCallRequest::new` sets `None`). The member is omitted when `None`, so every request without it keeps its exact bytes, and the struct still accepts unknown members, so providers on 0.32 ignore it.
- New `OutputRefs`, carried opaquely: the descriptor's schema belongs to the llm-runner contract (`cortexkit-role-llm-runner` in the commons repository), and providers decode it with that crate's type. This crate checks only that it is a JSON object whose compact encoding is at most `MAX_OUTPUT_REFS_BYTES` (4,096) bytes, in `OutputRefs::new` and when decoding; refusals are `OutputRefsError` naming `OUTPUT_REFS_FIELD` (`output_refs`). The daemon never reads it.

## 0.32.12

- Add `framed_body::{encode, decode, FramedBodyError, MAX_FRAMED_HEAD_BYTES, MAX_DECLARED_PAYLOAD_BYTES}` for a bounded JSON object head and unchanged raw payload, with shared Rust/Swift vectors.
- Add the optional, empty-omitted `ModuleManifest::operation_payload` map and builder method, `OperationPayload::{new, max_bytes}`, `OperationPayloadError`, and `ModuleManifest::validate_operation_payload`. Declarations must name unary management operations and allow 1 through 8 MiB. `ManagementOperation` remains unchanged and source-compatible.

## 0.32.11

- Add optional epoch-fixed `ScopeRecord` and `ScopeStamp` workdir/project_id fields and builders, project-id validation, `scope-workdir/v1`, and named workdir/project refusals. Absent fields preserve existing JSON bytes.

## 0.32.10

- Add optional `ModuleManifest.rich_ui` and `RichUiDescriptor`, with bounded schema, digest, byte-size and chunk-count validation inputs. The field is omitted when absent, preserving existing manifest bytes; the daemon validates its shape at HELLO and relays it opaquely through `catalog.list`.

## 0.32.9

- Add `MAX_DELIVERED_FRAME_LEN` (4,194,304 bytes, the 21-byte header included): the largest frame the daemon delivers from a module to a client, whether a reply, stream frame, error, reverse request or push. A larger module frame is never forwarded: a reply, stream frame or error ends its request with `frame_too_large`, a reverse request is answered with an error to the module, and an unsolicited push closes the client connection. Client-to-module requests are still bounded only by `MAX_FRAME_BODY_LEN`. The value is published in `tests/golden/protocol_constants.json` as `max_delivered_frame_len` for the TypeScript and Swift clients to compare against.

## 0.32.8

- Add the daemon `scope-served-by/v1` capability constant; existing exhaustive control enums remain unchanged.

## 0.32.7

- Match `cortexkit-bus-naming` 0.2.6's event-name grammar in manifest validation: event names contain one to four non-empty dot-separated tokens, each 1–63 bytes and starting with a lowercase letter or digit; version-shaped levels (`v` followed by digits) are reserved. Modules must not declare dotted events until the running daemon carries this release; older daemons refuse dotted names at HELLO.

## 0.32.6

- Add optional `ScopeAttributes.on_behalf_of_agent` and its builder: attribution only, never authority or `agent_id`. Authority owners may set it using the agent token rule, but not alongside `agent_id`, `delegates`, `flow_id` or `run_id`. Changes rotate the content version and close routes. The daemon advertises `scope::SCOPE_ON_BEHALF_OF_CAPABILITY` (`scope-on-behalf-of/v1`) and includes the field in bind stamps only for targets declaring it, and in describe replies only for callers declaring it.

## 0.32.5

- Add `ModuleManifest.operation_delivery` and its builder setter: a map from management operation names to open `OperationDelivery` strings. Empty maps are omitted, preserving existing manifest bytes. `OperationDelivery::WATCH` and `is_watch()` recognize exactly `"watch"`; unknown strings round-trip without becoming subscribable.
- `ModuleManifest::is_watch_operation` requires both watch delivery and a declared management operation of kind `query`. Missing or non-query names are not watches and do not refuse HELLO. Malformed delivery maps (including any non-string value) are dropped as a whole without rejecting the manifest. Older daemons silently drop the new field, so discovery requires a daemon built against this protocol revision.

## 0.32.4

- Add terminal `error_codes::FRAME_TOO_LARGE` (`frame_too_large`) for a module reply whose header plus body exceeds the client connection's egress budget. It does not mark the established route dead: Rust and TypeScript SDKs return the request error without reopening or retrying; Swift already throws on route ERRORs without retrying. Retrying the same request could repeat module side effects and produce the same oversized reply. No envelope or protocol-version change.

## 0.32.3

- Add `manifest::CAP_SCOPE_BIND_PROOF_V2` (`scope-bind-proof/v2`): bind proofs are required only from non-attested openers; reserved modules are exempt. Owners relying on reserved modules opening without proofs should set hashes only after discovering v2. The daemon continues advertising v1.

## 0.32.2

- Add optional `ScopeRecord.replace_only_if_idle` and `ScopeSelector.hold_presence`, with builder setters and false omitted from wire bytes. Both types remain non-exhaustive. Add `manifest::CAP_SCOPE_REPLACE_IF_IDLE_V1` (`scope-replace-if-idle/v1`) covering atomic idle-only rotation and connection-held presence; send these fields only after capability discovery. Add terminal `SCOPE_EPOCH_IN_USE` (`scope_epoch_in_use`) for per-record conditional replacement refusal.

## 0.32.1

- Add `manifest::CAP_SCOPE_BIND_PROOF_V1` (`scope-bind-proof/v1`). A daemon advertising it stores scope bind-proof hashes and enforces proofs on scoped route opens; owners set `bind_proof_sha256` only when their connected daemon advertises it.

## 0.32.0

- **Breaking.** Add optional `ScopeSelector.bind_proof` and `ScopeRecord.bind_proof_sha256`. Selectors are non-exhaustive: use `ScopeSelector::new` and `with_bind_proof`. Validate hashes as 64 lowercase hexadecimal characters. Add terminal `scope_bind_proof_required` and `scope_bind_proof_mismatch` codes. Proofs are redacted in Debug/Display; unset fields preserve existing wire bytes.

## 0.31.0

- **Breaking.** Add `call_mode::CallMode` (`classic`, `codemode`, `flow`, or a preserved `Unknown(String)`), `CALL_MODES_CAPABILITY` (`call-modes/v1`), and `derive_call_mode`. Providers must refuse unknown modes. Declare the capability only when reading the mode on every bind and handling all three known modes.
- **Breaking.** `ModuleControlRequest::RouteBind` gains `call_mode: Option<CallMode>`. Rust constructors must supply it; wire readers receiving an absent field must derive the mode from the bind's scope, not always choose classic. Daemons send it only to targets declaring `call-modes/v1`, including on classic routes.
- **Breaking.** `ScopeStamp` gains `expires_at_ms: Option<u64>` and becomes `#[non_exhaustive]`: replace struct literals with `ScopeStamp::new` and the `with_*` setters. The deadline is absolute Unix wall-clock milliseconds and fixed per epoch. Both new wire fields are omitted when absent, so binds to older providers keep their existing shape.
- Run scope records may now delegate. `scope_run_id_delegates` remains a stable code for older peers, but validation no longer emits it. To migrate providers to `call-modes/v1`, this release keeps the legacy run and flow capabilities working: non-delegating run scopes can bind to `agent-run-scopes/v1` or `call-modes/v1` targets, while delegating run scopes require `call-modes/v1`. Flow scopes can bind to `flow-scopes/v1` or `call-modes/v1` targets. Unsupported targets retain the terminal `target_agent_run_unsupported` and `target_flow_unsupported` refusals.

## 0.30.2

- Recognize `pipe` as a launch-nonce provenance source for Windows named-pipe readers while preserving unknown-source compatibility.

## 0.30.1

- Add `manifest::CAP_CATALOG_EVENTS_V1` (`catalog-events/v1`), the `server.describe` capability a daemon advertises when `catalog.list` carries modules' declared `events`.

## 0.30.0

- **Breaking.** `ScopeRecord` gains `expires_at_ms: Option<u64>`, an absolute Unix wall-clock deadline in milliseconds that the daemon enforces itself: at the deadline the scope ends as if its owner had removed it, routes under it close with `scope_ended`, and that ref at that epoch can never return (`scope_expired`). The deadline is fixed for the epoch (`scope_expiry_immutable`) and at most `MAX_SCOPE_EXPIRY_AHEAD_MS` (24 hours) ahead (`scope_expiry_too_far`).
- **Breaking.** `ScopeAttributes` gains `run_id`, marking a scope as one agent run. It requires `agent_id` and refuses `flow_id` and `delegates` (`scope_run_id_without_agent`, `scope_run_id_with_flow_id`, `scope_run_id_delegates`). A route under such a scope binds only to a target that declares `AGENT_RUN_SCOPES_CAPABILITY` (`agent-run-scopes/v1`); otherwise route.open is refused with the terminal `target_agent_run_unsupported`.
- **Breaking.** New `scope.apply` module control operation (`ScopeApply` request and response variants, `ScopeEnd`, `ScopeEndOutcome`, `ScopeEndResult`): add or end individual scopes without resending the owner's whole set. It shares `scope.sync`'s generation, and needs one accepted full `scope.sync` per daemon incarnation first (`scope_sync_required`).
- `ScopeRecord`, `ScopeAttributes`, `ScopeCarrier`, `ScopeParent`, `ScopeEnd` and `ScopeEndResult` are `#[non_exhaustive]`; build them with `new` and the `with_*` setters, so later fields are not breaking. Wire bytes of existing records are unchanged.

## 0.29.4

- Add standalone, `#[non_exhaustive]` `session::OperatorConfirmRequest` and `session::OperatorConfirmReply` bodies for the module-to-daemon `operator.confirm` control operation, with constructors. The request carries the summary and the caller's route channel/epoch on the module connection; the successful reply carries `outcome: "confirmed"`. Refusals use the new `operator_declined`, `operator_presence_unavailable`, `operator_summary_invalid` and `operator_request_not_permitted` error codes. All four are terminal in the route-open retry predicate but are not route-open refusals, so the route-open decision table is unchanged. Additive: neither existing module-control enum changes, and no consumer code change is needed.

## 0.29.3

- Add optional `ModuleManifest.events: Option<Vec<EventDeclaration>>` and its builder setter. `EventDeclaration` is non-exhaustive, constructor-built, and declares a name, version, filter headers and optional consent-card summary. An absent block means undeclared; an empty list affirms no events. Existing manifests keep their wire bytes.
- Validate names against the `cortexkit-bus-naming` 0.2.0 event token rule, positive u32 versions, unique (name, version) pairs, bounded unique headers, and bounded control-free summaries. HELLO refusals name the malformed field. Headers are filter hints, never authority; the bus transports a notice and the publisher serves its body.
- Declarations are validated and retained by the new daemon but **not yet served** by `catalog.list`, `server.describe`, or `ck catalog`. Serving them needs a field in subc-control's `CatalogEntry`, which is exhaustive, so it waits for a breaking subc-control release; emitting the field on the wire without the type would leave every Rust reader silently dropping it. Readers come first: older daemons silently drop `events`, and publishers must not rely on declaration discovery before the serving daemon release is deployed.

## 0.29.2

- `CallOrigin` gains an optional `message_position: { message_id, index }`, where the call sits among the tool calls of one model message (0-based, in the order the model wrote them). A host may run one message's calls concurrently, so a provider that must follow the model's order reads it to sequence them. Absent means unknown. The id and the index travel together: a position missing either one does not decode. `validate_call_origin` also refuses a message id that breaks the call-key rule (empty, over 256 bytes, or outside printable non-space ASCII), naming the field `origin.message_position.message_id` (`ORIGIN_MESSAGE_ID_FIELD`) for the provider's `invalid_request` reply. Additive: `CallOrigin` is `#[non_exhaustive]`, an origin without a position keeps its existing bytes, and readers that predate it ignore the member.

## 0.29.1

- Add `scope::FLOW_SCOPES_CAPABILITY` (`flow-scopes/v1`). A module that declares it in `capabilities.provides` promises to recognise a scope carrying `flow_id` and to apply flow behaviour, never owner-agent behaviour. Add the terminal route-open code `target_flow_unsupported`; the shared retry predicate and the golden decision table classify it as terminal. Additive: no existing type changes.

## 0.29.0

- Breaking for Rust struct literals: `ToolCallRequest` gains optional `preset`, and `ScopeAttributes` gains optional `flow_id`. Both are omitted when absent, leaving existing absent-field wire bytes unchanged; `ScopeAttributes` still refuses unknown fields.
- `validate_preset` checks 1–64 characters of `[a-z0-9_-]` and names `preset` for a provider's `invalid_request` reply. When a call carries no preset, the provider must apply a policy it chose explicitly; it must not fall back to its most permissive preset (the one offering the most tools). A preset the provider does not serve is refused, with the preset named in the refusal, never replaced by another.
- `flow_id` identifies the scope's flow, needs no agent or delegation, and is set only by an authority owner and stamped verbatim. `validate_flow_id` shares the 1–256 printable non-space ASCII token rule with call keys and schema pins. Same-epoch changes bump the content version and drain scoped routes with `scope_delegation_changed`, like `agent_id` changes.

## 0.28.1

- Refuse unknown fields inside scope principals, including parent owners, child owners, carriers, and selectors, as required for authority-bearing scope input. Principal decoding outside scopes remains forward-compatible.

## 0.28.0 — 2026-10-01

- Breaking: `ToolCallRequest` gains `origin: Option<CallOrigin>`, so a struct literal must now set it (`ToolCallRequest::new` sets `None`). The member is omitted on the wire when `None` and decodes as `None` when absent, so bodies without it are unchanged in both directions.
- New `CallOrigin { carrier: Principal, call_key: String }` (`#[non_exhaustive]`, built with `CallOrigin::new`): the caller behind a relayed call, with `carrier` in the same tagged form the daemon stamps on a route. It is for attribution only; a provider must never grant or refuse anything because of it.
- New `validate_call_origin`, which checks `origin.call_key` with the existing call-key bounds and reports `ORIGIN_CALL_KEY_FIELD` (`origin.call_key`) as the error's field. Every `Principal` is accepted as the carrier.
- Breaking: `ModuleControlRequest::RouteBind` gains `role_versions: Option<BTreeMap<String, String>>`, beside `consumer_capabilities`: the provider-role versions the consumer declared on its `route.open` (`{"tool-provider": "v1"}`), forwarded by the daemon unchanged. Like `consumer_capabilities` it is an unverified declaration that grants nothing. Omitted on the wire when `None`; absent decodes as `None`. It is not on `BindIdentity`.
- New `session::validate_role_versions`, the shared check the daemon applies and a consumer can run first: at most `MAX_ROLE_VERSIONS` (8) entries, each role name matching `^[a-z0-9]+(-[a-z0-9]+)*$` in at most `MAX_ROLE_NAME_LEN` (64) bytes, each version matching `^v[1-9][0-9]*$`. It returns a `#[non_exhaustive]` `RoleVersionsError` whose `field()` is `ROLE_VERSIONS_FIELD` (`role_versions`).
- New `scope::CAP_ROUTE_ROLE_VERSIONS_V1` (`route-role-versions/v1`), the capability a daemon advertises when it checks and forwards `role_versions`. An older daemon drops the field silently.
- New `error_codes::INVALID_REQUEST` (`invalid_request`), terminal: a malformed request field, named in `detail.field`. The `decision_tables.json` golden lists it as terminal.
