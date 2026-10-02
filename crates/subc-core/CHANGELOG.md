# Changelog

## 0.20.51 — 2026-10-02

- Add an inherited-daemon-socket fixture and real-process health restart regression. Supervision fixtures isolate all three XDG directories.

## 0.20.48 — 2026-10-01

- Takes `subc-daemon` 0.27.0, a minor release that follows `subc-protocol` 0.28.0 (its `ToolCallRequest` gains an `origin` field).
- `fake-aft-stub` records the bind's `role_versions` in its `attach` event, as it does `consumer_capabilities`.
