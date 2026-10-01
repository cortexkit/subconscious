# Changelog

## 0.28.0 — 2026-10-01

- Breaking: `ToolCallRequest` gains `origin: Option<CallOrigin>`, so a struct literal must now set it (`ToolCallRequest::new` sets `None`). The member is omitted on the wire when `None` and decodes as `None` when absent, so bodies without it are unchanged in both directions.
- New `CallOrigin { carrier: Principal, call_key: String }` (`#[non_exhaustive]`, built with `CallOrigin::new`): the caller behind a relayed call, with `carrier` in the same tagged form the daemon stamps on a route. It is for attribution only; a provider must never grant or refuse anything because of it.
- New `validate_call_origin`, which checks `origin.call_key` with the existing call-key bounds and reports `ORIGIN_CALL_KEY_FIELD` (`origin.call_key`) as the error's field. Every `Principal` is accepted as the carrier.
