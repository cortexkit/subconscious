# Changelog

## 0.20.66

- Build time only: the build script watches the workspace `Cargo.lock` only when it exists. Built from a registry or vendored copy there is no workspace lock two levels up, and cargo treats a missing watched path as always changed, so every consumer rebuilt this crate on every build.

## 0.20.65

- Build time only, no runtime change: the build script, which stamps the git commit into the binary, no longer recompiles the crate on every cargo run in a git worktree. It watched `.git/HEAD` and `.git/refs` paths that don't exist when `.git` is a worktree's file, and cargo treats a missing watched path as always changed. It now resolves paths with `git rev-parse --git-path` and watches only the existing HEAD, current branch ref, `packed-refs` and index, so a rebuild happens only when the checked-out commit or the index changes.

## 0.20.64

- Show a module's latest health detail and headline metrics in `ck module status`, including healthy modules.

## 0.20.63

- Tests run the daemon, the CLI and test modules under `ckdev-*` names, so no test process is listed as a production `ck-*` binary. No change to the shipped binaries.

## 0.20.62

- `ck setup` now health-checks nats-server on an existing bus install. It still does not install the message bus: nats-server and ck-bus are still placed with `ck-bus install-plan` and `install-apply`. When `subc.jsonc` already declares `nats-server` (`protocol: "none"`, `-c <absolute path>/server.conf`) and `ckbus` (an absolute `program`), setup first has ck-bus add the loopback monitoring listener `http: "127.0.0.1:18222"` to `server.conf` (`ck-bus install-apply --conf-only --keep-existing`). Only after that succeeds does it add `modules.nats-server.health = { http: "http://127.0.0.1:18222/healthz", cadence_ms: 30000, deadline_ms: 5000 }`. A monitoring port or `health` entry you already set is kept, and setup reports "kept your setting". A non-loopback listener is refused. An install setup cannot identify is reported and skipped. Running setup again changes nothing. To start the check, restart nats-server so it opens the listener (`ck module restart nats-server`), then run `ck module rescan`. Setup restarts neither.

## 0.20.61

- Require subc-daemon 0.32.5 so an exhausted restart budget's exit record is retained before module status reports `failed`.

## 0.20.60

- Require subc-daemon 0.32.4 so a macOS `#!/bin/sh` module is recognised as its own orphan after a daemon crash, including the system shell launcher's transition to its selected interpreter. Other process-identity checks remain strict.

## 0.20.59

- Require subc-daemon 0.32.3, which adds test-only seams for the macOS launch boundary. Shipped behaviour is unchanged.

## 0.20.58

- Require subc-daemon 0.32.2 so `ck module list` stops showing a wire module as `ok` after a failed health probe. It shows `unknown` until another report arrives, while restarts still wait for the configured failure threshold.

## 0.20.57

- Require subc-daemon 0.32.1: on macOS a module's pid briefly runs a re-executed `ck-subc` (the launch trampoline) before becoming the module, and status, provenance and resource readings now wait until the module image is confirmed instead of reporting `ck-subc`. The physical spawn/exit feed is unchanged.

## 0.20.56

- Update to subc-daemon 0.32: every macOS supervised module gets its own privacy identity, without a config switch. `ck-subc` handles its hidden launch-trampoline argument before runtime, logging, config and CLI probes; explicitly configure and probe the daemon's executable at startup. Grants to `ck-subc` no longer cover modules. Modules needing permissions must have a stable team code signature (ad-hoc rebuilds prompt again). Linux and Windows are unchanged.
- Add real-process identity, inherited direct-spawn control, nonce/group, failure, executable-roster and swap tests. Fault injection is confined to a dedicated test executable, never production config.

## 0.20.55

- Take subc-daemon 0.31.1: flow-scoped opens to a module that does not declare `flow-scopes/v1` are refused with terminal `target_flow_unsupported`.

## 0.20.54

- Updates the daemon and wire dependency cascade for subc-protocol 0.29.0 (`ToolCallRequest.preset` and `ScopeAttributes.flow_id`), a release breaking for Rust struct literals. Absent-field wire bytes remain unchanged.

## 0.20.53

- Keep module compatibility floors enforced when repairing an existing core configuration.
- Refuse incompatible setup requests with an actionable nonzero result, and let read-only upgrade checks report availability while the daemon is stopped.
- Preserve prerelease versions and the `ck-mc-<train>` build tags that the magic-context (`mc`) component reports as its version; pin each upgraded file's identity when it is placed, reconcile self-update versions, and remove rollback copies after a successful upgrade.
- Use fixed PowerShell extraction scripts and escaped service definitions; reload changed registrations without stopping live macOS jobs.
- Distinguish download failures from missing release assets, refresh incomplete dashboard cache coverage, and render release transitions consistently.
- Refuse non-ASCII reset timestamps safely and label reset clock times as UTC.

## 0.20.51 — 2026-10-02

- Add an inherited-daemon-socket fixture and real-process health restart regression, plus coverage for cancelling every restart backoff and recovering from exhausted health restart budgets. Supervision and daemon shutdown fixtures isolate all three XDG directories.

## 0.20.48 — 2026-10-01

- Takes `subc-daemon` 0.27.0, a minor release that follows `subc-protocol` 0.28.0 (its `ToolCallRequest` gains an `origin` field).
- `fake-aft-stub` records the bind's `role_versions` in its `attach` event, as it does `consumer_capabilities`.
