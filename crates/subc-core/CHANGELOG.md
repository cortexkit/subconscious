# Changelog

## 0.20.92

- Ship daemon 0.36.2's atomic idle-only scope rotation and connection-held presence support; add `scope_epoch_in_use` to the shared terminal-code contract checks.

## 0.20.91

- `ck`'s fixed test clock now lives in a helper chosen at build time, so release builds read the real clock with no constant-None call that release-profile clippy refuses.

## 0.20.90

- Keep terminal-table byte-for-byte rendering deterministic by allowing the test-support CLI to use a fixed clock for relative ages.

## 0.20.89

- Upgrade the daemon and SDK wire crates for per-scope bind proofs. Add real-daemon tests, each starting its own daemon on a throwaway data, runtime and config folder, for a missing, wrong, malformed and matching proof, the owner needing the proof too, a new epoch replacing the hash, and unchanged admission for scopes registered without a hash.

## 0.20.88

- Test-only: the `ck` quota fixture uses a placeholder account address (`person@example.invalid`) instead of a real-looking email.

## 0.20.87

- Update the internal CLI and daemon executable to `subc-daemon` 0.35, which adds the `privileged_operator_prompt` config key and the matching restart-required section. Setup reports a pending `privileged_operator_prompt` edit on a live host as restart-required.
- `ck` allows an extra 290 seconds (the daemon's default operator budget) for every privileged command before reporting outcome-unknown, against any daemon and in both modes, because the wire does not say whether a request will prompt. `ck module restart` now reports outcome-unknown after 300 seconds instead of 10 when the daemon never replies; `ck` still never retries. Replies that arrive are handled as before.
- `ck` prints a one-line reason for `operator_declined` and `operator_presence_unavailable`, and "refused without asking: too many recent declines" for `detail.reason` `backoff`, then exits non-zero.

## 0.20.85

- `ck` and `ck upgrade` never offer or install a release older than what is installed, including binaries that don't report their own version (judged by a sibling from the same release that does). Cached update metadata that names a release older than the installed one predates this install, so it is refetched even inside its 24-hour lifetime; if the refetch fails, no update is shown. Replacing a damaged binary with the same release, and restoring the previous binary when an upgrade fails, still work as before.

## 0.20.84

- Make forwarding-test stub spawn helpers and replacement attachments wait for catalog publication on a separate authenticated connection, so route.open cannot race installation of the module's forwarding endpoint.

- Make the crash route-close tests wait for catalog publication and trigger crashes only after binding, rather than racing a launch-relative timer that could attach to an already restarted process. Verify that the client can reopen after the non-terminal close and that crashing the replacement exhausts the one-restart budget. The test provider supports crashing on a bound-route request.

## 0.20.83

- Add `ck activity` with agent, project, module, kind, inclusive millisecond time-range and limit filters, a UTC time/module/actor/summary table, and JSON output. `--follow` subscribes from the query's head without a handoff gap and applies the same filters to live entries; JSON follow output is one entry per line.

## 0.20.81

- Order legacy unstamped capture lines using their position relative to stamped lines in the same capture file, apply `--since` to inherited capture times, and identify inherited times in JSON.

## 0.20.80

- Update the internal CLI and daemon executable to `subc-protocol` 0.31, `subc-control` 0.30, `subc-transport` 0.12 and `subc-daemon` 0.34 for call modes and stamped deadlines. Restart the daemon before writing delegating run scopes; providers opt into `call-modes/v1` only after implementing mode handling. CLI commands are unchanged. The test provider ignores the new optional `call_mode` bind field.

## 0.20.79

- Render Linux executable permission denial as `not readable (non-dumpable)` in module status and provenance rather than as an image difference or a generic unavailable result. The image is still unavailable for verification; this does not weaken upgrade checks.
- Show `not observable (process hidden)` when procfs hides a live process, and identify the previous PID when a module is waiting for its hidden predecessor to exit.

## 0.20.78

- Require the health-triggered restart test to find exactly one terminal record immediately after observing a failed, reaped module. It then checks that the record names the exhausted restart budget and that enabling the module starts it again. An ordering violation now fails explicitly rather than indexing an empty history.

## 0.20.77

- Show daemon-observed Windows launch-nonce sources in module list, status and provenance alongside the running-image and resource reports. Update exact CLI expectations for the Windows `pipe` column and lines; Unix output stays unchanged when the observation is absent.
- Require exact `pipe` provenance from a reserved Windows module, including the daemon-observed source; Unix still requires `fd`.

## 0.20.76

- `ck module status` and `ck module list` show Windows resource readings (working set and CPU time) and the running-image check where they previously said "unavailable".

## 0.20.75

- Fix two tests broken by the Linux setup preflight: the adoption test now boxes its command runner, and the core-version-floor test skips only the preflight (through a test-support switch), since a Linux CI container has no systemd user manager.

## 0.20.74

- Refuse Linux setup before downloading or writing when the systemd user manager is unavailable, and explain how to keep the daemon running after logout.

## 0.20.73

- Fresh installers keep `ck` in `cmd/` and place only that directory on PATH; setup adopts the installed command, and the MCP gateway is dispatched as `ck subc-mcp`.

## 0.20.72

- Discover external `ck-<name>` domains from the CortexKit bin directory before searching PATH, so a domain command works as `ck <name>` without its binary being on PATH.

## 0.20.71

- On Windows, register the logon task only for the current account and update the installer session PATH immediately while notifying other processes of persisted PATH changes.

## 0.20.70

- Built on `subc-daemon` 0.33: the daemon enforces scope deadlines, serves `scope.apply`, and gates agent-run scopes on `agent-run-scopes/v1`. No CLI change.

## 0.20.69

- Test-only: the tests' helpers (temporary directories, process-liveness checks, and copies of test programs under `ckdev-` names) now come from the published `cortexkit-test-support` crate, which other CortexKit repositories also use, instead of a private copy in this repository. No runtime change.

## 0.20.68

- Test only: the `test-support` feature also enables `subc-daemon/test-support`, which the `operator.confirm` real-daemon tests need to run the daemon with a scripted confirmation provider.

## 0.20.67

- Test only: the rollback-over-a-running-executable test ran a copy of `/bin/sleep` named `ck-aft` and never checked it stayed alive. macOS kills a copied platform binary run outside its system path, and a multicall coreutils refuses an unknown name, so the test passed with nothing running. It now runs a `ckdev-aft` copy of its own test binary, held open, and fails if that copy exits early.

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
