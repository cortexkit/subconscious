# Changelog

## 0.1.31

- Update supervised launch-nonce provenance tests to expect the shared accessor's named-pipe source on Windows; Unix still expects the descriptor source.

## 0.1.30

- Moves to `subc-protocol` 0.30, `subc-control` 0.29 and `subc-daemon` 0.33. No behaviour change.

## 0.1.29

- Test-only: configure the `ck-subc` daemon executable to give each supervised test module a separate macOS privacy identity.

## 0.1.28

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.1.27

- Build time only: the build script watches the workspace `Cargo.lock` only when it exists. Built from a registry or vendored copy there is no workspace lock two levels up, and cargo treats a missing watched path as always changed, so every consumer rebuilt this crate on every build.

## 0.1.26

- Build time only, no runtime change: the build script, which stamps the git commit into the binary, no longer recompiles the crate on every cargo run in a git worktree. It watched `.git/HEAD` and `.git/refs` paths that don't exist when `.git` is a worktree's file, and cargo treats a missing watched path as always changed. It now resolves paths with `git rev-parse --git-path` and watches only the existing HEAD, current branch ref, `packed-refs` and index, so a rebuild happens only when the checked-out commit or the index changes.

## 0.1.25

- Update the test daemon dependency to subc-daemon 0.32 whose macOS launches give each supervised module its own privacy identity.

## 0.1.24

- Updates the daemon and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. The gateway sends no preset because its MCP host supplies none; providers must decide explicitly what an absent preset gets.

## 0.1.23

- Preserve outcome-unknown metadata when a provider closes a dispatched request.
- Keep attached sessions alive across transient shim accept failures.
- Release completed prompt relay routes and omit combined tool names exceeding 64 characters.

## 0.1.19 — 2026-10-01

- Takes `subc-protocol` 0.28.0 and `subc-daemon` 0.27.0. The gateway sends no `origin` on the tool calls it routes, because those calls come from the host it serves rather than being relayed for another caller, and it declares no `role_versions` on the routes it opens. No behavior change.
