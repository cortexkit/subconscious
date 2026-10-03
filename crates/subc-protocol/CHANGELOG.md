# Changelog

## 0.29.0

- Breaking for Rust struct literals: `ToolCallRequest` gains optional `preset`, and `ScopeAttributes` gains optional `flow_id`. Both are omitted when absent, leaving existing absent-field wire bytes unchanged; `ScopeAttributes` still refuses unknown fields.
- `validate_preset` checks 1–64 characters of `[a-z0-9_-]` and names `preset` for a provider's `invalid_request` reply. An absent preset requires an explicit provider policy, never the most capable default; an unserved preset is refused by name, never silently substituted.
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
