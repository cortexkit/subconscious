# Task: `ck` — the CortexKit operator CLI (daemon/module domain first)

Repo: this worktree (subconscious). Build a new binary target `ck` in the
`subc-core` crate: `crates/subc-core/src/bin/ck.rs`.

`ck` is the founding piece of the CortexKit umbrella operator CLI. The
daemon/module control domain ships first; other domains (`ck vault ...`,
`ck quota ...`, `ck account ...`) join later — so structure arg parsing as a
`<domain> <verb>` dispatch that is trivially extensible, and say so in the
bin's top doc comment.

## Why

Operators today restart supervised modules with either `subc-probe
--supervisor-restart <id> --subc <connection-file>` (needs the path, lives in a
build tree, diagnostic-flavored UX) or raw `pkill` (skips the daemon's drain
path entirely). The CLI is the human incident tool: it must work from any
terminal with zero setup, exactly when the module fleet is on fire.

## Reference implementation

`crates/subc-core/src/bin/subc-probe.rs` already implements everything needed:
connection-file read, authenticate_client, channel-0 control requests
(supervisor.restart / supervisor.list / supervisor.health / server.describe /
catalog.list), response decode. Reuse its patterns (or extract small shared
helpers if clean — do NOT change subc-probe behavior).

## Command surface (v1, exactly this)

```
ck module list                  # supervisor.list — table: id, state, enabled, live, pid?, restarts
ck module status <id>           # one module's supervisor.list row + last health report (supervisor.health)
ck module restart <id>          # supervisor.restart — prints applied + new state
ck module stop <id>             # supervisor.set_enabled false
ck module start <id>            # supervisor.set_enabled true
ck health                       # supervisor.health — per-module status table (like subc-probe --supervisor-health but human-tabular)
ck daemon                       # server.describe — daemon version, uptime-ish, connected clients
```

Global flag: `--subc <path>` to override connection-file discovery (same
semantics as subc-probe). `--json` on every subcommand for raw JSON output
(agent/script consumption).

## Connection-file discovery (the core UX win — zero args)

Resolution order, first file that exists AND parses:
1. `--subc <path>` if given.
2. `$XDG_RUNTIME_DIR/subc-connection.json`
3. `$HOME/.local/share/cortexkit/run/subc-connection.json` (the launchd-pinned
   prod location on macOS — see the plist convention)
4. Platform tmp fallback: the same default path logic bootstrap uses when
   XDG_RUNTIME_DIR is unset (look at how bootstrap.rs / connection_file.rs
   compute the default publish path — mirror READ side of that exactly; do not
   invent a new path).

If none found: exit 2 with a one-line error listing the paths tried. If found
but the daemon doesn't answer (connect/auth failure): exit 3 with the path it
used and the error. Success exit 0; module-not-found or op-rejected exit 1
with the daemon's error string. Errors go to stderr, data to stdout.

## Output style

Human-first: aligned plain-text tables, no color, no emoji, terse. This is an
incident tool read over ssh at 3am. `--json` bypasses all formatting.

## Constraints

- No new deps beyond what subc-core already has (serde_json formatting is
  fine; NO clap — hand-roll arg parsing like subc-probe does, keep it simple).
- `#![forbid(unsafe_code)]` like the other bins.
- Do not modify daemon/server code, subc-probe behavior, or any protocol type.
  This is a pure client binary.
- Windows must compile: run `cargo clippy -p subc-core --target x86_64-pc-windows-gnu --all-targets`
  and fix any cfg/unused warnings (CI runs -D warnings on 3 OSes).

## Tests

Integration test (crates/subc-core/tests/): spawn a TestServer daemon (see
existing tests for the helper) with a stub module, then drive the compiled
`ck` binary via std::process::Command against it:
- `ck module list --json` shows the stub,
- `ck module restart <stub>` returns applied and the module respawns
  (poll supervisor.list for the new state),
- `ck module stop` then `start` flips enabled state,
- discovery failure path: run with XDG_RUNTIME_DIR pointed at an empty temp
  dir, no --subc, assert exit 2 and the tried-paths error on stderr,
- `--subc` override works (this is how the test drives it deterministically).
Use CARGO_BIN_EXE_ck for the binary path. Follow the existing test
conventions: level-triggered polling, no absolute-latency assertions, 10s
setup timeout helper.

## Release lane

Add `ck` to the binary set in .github/workflows/release.yml (the
subc-core-v* tag lane builds subc-core, subc-probe, fake-aft-stub, subc-mcp —
add ck alongside) and to scripts/release-darwin-binaries.sh if it lists
binaries explicitly.

## Gate

cargo fmt --check, clippy clean native + x86_64-pc-windows-gnu (all targets),
full `cargo test -p subc-core` green, plus the new integration tests. Commit
with a clear message; do not push.
