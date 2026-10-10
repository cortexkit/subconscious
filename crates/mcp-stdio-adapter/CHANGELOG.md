# Changelog

## 0.1.12

- Moves to `subc-protocol` 0.30 and `subc-client-rs` 0.27. No behaviour change.

## 0.1.11

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.1.10

- Updates the SDK and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. No other behavior change.
