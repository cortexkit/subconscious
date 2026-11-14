# Changelog

## 0.8.0

- Declare that the MCP adapter hosts third-party code so its origin evidence
  cannot qualify the code it hosts for the first-party mark.
- Breaking dependency upgrade to client 0.40; the public credential resolver,
  module handler and attestation errors expose SDK types. Require daemon 0.46
  for tests and upgrade direct SDK dependencies together.

## 0.7.1

- Use SDK self-attestation for vault credential reads instead of an explicit
  legacy identity that conflicts with registration-channel claims. Keep the
  credential-resolver constructor's identity argument for source compatibility.
- Require client 0.39.1 and test channel-bound credential resolution with the
  current daemon's test-support feature.

## 0.7.0

- Adopt client 0.39 and protocol 0.37. This is a breaking Rust dependency
  upgrade because the public adapter API exposes client types; update direct
  SDK dependencies together. Route-close reasons now include `AuthorityChanged`
  and are non-exhaustive. MCP framing and adapter behavior are unchanged.

## 0.6.0

- Adopt subc-client-rs 0.38.0 for scoped managed calls and watches. This is a breaking Rust dependency upgrade because the public API exposes SDK types; downstream users must upgrade their SDK dependency too. No wire behavior changes.

## 0.5.0

- Adopt Rust client 0.37 and its control 0.38 type family. Public credential-resolver construction accepts the new `ConsumerIdentity`, and handler methods/trait implementations expose the new SDK types, so this is a breaking Rust dependency release. Adapter wire behavior is unchanged.

## 0.4.0

- Adopt the coordinated protocol 0.36 scope-type dependency family. **Rust migration:** add a wildcard arm to any `ScopeKind` match; `ScopeKind` is now non-exhaustive and includes `Federated`. Existing wire traffic to non-declaring providers is unchanged.

## 0.3.0

- Raise dependency floors for the resource-accounting wire release; the public dependency types require a breaking minor version.

## 0.2.1

- Adopt compatible dependency requirements for the updated route-open control types; no behavior change.

## 0.2.0

- Breaking: upgrade public resolver and module handler types to Rust client 0.33 and protocol 0.34. Credential routes no longer claim the current directory as a project; this resolver's credential request uses the credential handle, not a project.

## 0.1.18

- Tests only: removed a duplicate manifest unit test; the executable manifest test still checks `runtime_computed`. No behaviour change.

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
