# Changelog

## 0.20.51

- Keep module compatibility floors enforced when repairing an existing core configuration.

## 0.20.48 — 2026-10-01

- Takes `subc-daemon` 0.27.0, a minor release that follows `subc-protocol` 0.28.0 (its `ToolCallRequest` gains an `origin` field).
- `fake-aft-stub` records the bind's `role_versions` in its `attach` event, as it does `consumer_capabilities`.
