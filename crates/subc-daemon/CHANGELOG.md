# Changelog

## 0.27.0 — 2026-10-01

- Minor bump because this crate's public types come from `subc-protocol`, which moves to 0.28.0 (its `ToolCallRequest` gains an `origin` field; see that crate's changelog). A consumer that also depends on `subc-protocol` directly must move both together, or two incompatible copies of the protocol types would meet. Also takes `subc-control` 0.26 and `subc-transport` 0.9, which moved for the same reason. No other change.
