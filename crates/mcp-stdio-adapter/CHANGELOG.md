# Changelog

## 0.1.17

- Take subc-client-rs 0.32: handler panics now fail serving for supervisor recovery. After the module handshake completes, daemon disconnects still exit cleanly on every OS, with EOF/reset reported through the connection-end hook; handshake disconnects remain errors.

## 0.1.16

- Move to subc-protocol 0.33 and subc-client-rs 0.31. No behavior change.

## 0.1.15

- Move to subc-control 0.32 and subc-client-rs 0.30. No behavior change.

## 0.1.14

- Upgrade the SDK and scope wire dependencies for per-scope bind proofs.

## 0.1.13

- Update the internal adapter to `subc-protocol` 0.31 and `subc-client-rs` 0.28. MCP stdio clients need no request changes; this dependency update does not declare `call-modes/v1` or implement provider mode handling.

## 0.1.12

- Moves to `subc-protocol` 0.30 and `subc-client-rs` 0.27. No behaviour change.

## 0.1.11

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.1.10

- Updates the SDK and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. No other behavior change.
