# Changelog

## 0.3.3

- Updates the SDK, daemon and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. No other behavior change.

## 0.3.2

- A credential census read interrupted by a reconnect of ck-bus's own bus connection is replayed instead of timing out, within the same five-second budget. A census that cannot be read for any other reason (its stream is missing, or the broker returns an error or no reply in time) still makes the issuance fail with `ckbus_census_unavailable`; it is never treated as an absent entry.

## 0.3.0 — 2026-10-02

- Update Rust client and control dependencies to 0.25 and 0.27 for per-route close reasons and channel-addressed lifecycle pushes.

## 0.2.1 — 2026-10-01

- Takes `subc-protocol` 0.28.0, whose `ToolCallRequest` gains an `origin` field (see that crate's changelog), and the matching minor releases of the crates built on it: `subc-client-rs` 0.24, `subc-control` 0.26, `subc-transport` 0.9 and `subc-daemon` 0.27. No other change (a test sets the new `role_versions` route.open field to `None`).
