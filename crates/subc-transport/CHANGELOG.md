# Changelog

## 0.11.1

- Protect Windows connection-file temporaries with an explicit owner-only DACL at creation. Verify the opened file's owner and DACL before reading, refusing foreign owners, broad grants, null DACLs and unsupported ACE types as `ConnectionFileError::Invalid`. Create missing publication directories privately and refuse existing shared publication directories without changing their ACLs.

## 0.11.0

- **Breaking.** Requires `subc-protocol` 0.30. `read_frame`, `write_frame` and `FrameIoError::DecodeHeader` expose `subc-protocol` types, so their types change with it. No behaviour change.

## 0.10.1

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.10.0

- Takes subc-protocol 0.29.0: `ToolCallRequest.preset` and `ScopeAttributes.flow_id` are optional wire fields, but this release is breaking for Rust struct literals. Update protocol and transport dependencies together to avoid incompatible public protocol types.

## 0.9.1

- Validate public frames before writing any bytes, including the body-size cap and all header decode rules.
- Refuse Unix connection-file ancestors owned by users other than the effective user or root, even when their mode is not group/world writable.
- Read Unix connection files only when owned by the effective user, checking metadata on the same opened file that supplies the key.

## 0.9.0 — 2026-10-01

- Minor bump because this crate's public types come from `subc-protocol`, which moves to 0.28.0 (its `ToolCallRequest` gains an `origin` field; see that crate's changelog). A consumer that also depends on `subc-protocol` directly must move both together, or two incompatible copies of the protocol types would meet. No other change.
