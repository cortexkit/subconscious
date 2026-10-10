# Only `ck` on PATH

Status: proposed. The project owner chose the direction on 2026-10-10: the installers put only the
`ck` command on PATH, not the whole CortexKit bin folder, so every other CortexKit program is
reached through `ck`. This document is how to get there, including existing installs.

## Why

The installers add `<data home>/cortexkit/bin` to PATH (`scripts/install/install.sh` writes a
marked profile block; `scripts/install/install.ps1` appends to the user `Path` in
`HKCU\Environment`). That folder holds every CortexKit program, so the daemon, every module and
every tool are on PATH as `ck-<name>`. Users and agents can then start a module binary by hand,
beside the copy the daemon runs. That copy didn't get the daemon's launch secret, health checks or
restart handling, and it can contend with the real one for its store. Commands like `ck-account`
also end up with two spellings.

Since `ck` 0.20.72, `ck <name>` looks for `ck-<name>` in the bin folder before PATH, so a domain
command no longer needs the bin folder on PATH.

## Layout

| Folder | Holds | On PATH |
|---|---|---|
| `<data home>/cortexkit/cmd/` | only `ck` (`ck.exe` on Windows), the real binary | yes |
| `<data home>/cortexkit/bin/` | the daemon, modules and `ck-<name>` domains | no |

The real `ck` binary lives in `cmd/` on every platform, not a link to `bin/ck`. Windows has no
link that survives `ck`'s rename-based self-update, and one rule on every platform keeps `ck
upgrade` simple. Nothing else uses `bin/ck`: domain discovery skips `ck` itself, and the daemon
never runs the CLI.

## New installs

- `install.sh` writes the same marked block (`# cortexkit-managed PATH begin` … `end`) with
  `cmd/` instead of `bin/`. Its existing rewrite rules stay: a re-run replaces the block, and
  malformed markers refuse.
- `install.ps1` writes `cmd\` to the user `Path` instead of `bin\`, records it in the installer
  manifest as today, updates the running session and broadcasts the change.
- Both place `ck` in `cmd/` and record that destination in the inventory.

## Existing installs

The next `ck upgrade` (or `ck setup`) moves an existing install, in this order:

1. Place the new `ck` in `cmd/` and move its inventory row from `bin/ck` to `cmd/ck`.
2. Point PATH at `cmd/`: rewrite the marked profile block (macOS, Linux), or swap the `bin\`
   entry for `cmd\` in the user `Path` (Windows). It uses the installer's exact marker rules,
   ported to Rust: no block, an unpaired marker or a duplicate block refuses with the file and
   line, and nothing is changed.
3. Keep `bin/ck` until it is no longer needed. Shells that are already open still have the old
   PATH, and `ck` must not vanish from them. A later `ck` run whose own PATH no longer contains
   `bin/` removes `bin/ck` if the inventory still owns it.

`ck upgrade` prints each change it made, and says that shells opened from then on find `ck` in
`cmd/`.

## The MCP gateway

Harness configs run `ck-subc-mcp shim --harness <name>` today (README, and the `ck setup` next
step pinned in `tests/ck_cli.rs`). Once `bin/` leaves PATH, that bare name stops resolving.

- `ck-subc-mcp` answers the `--ck-domain` handshake, so `ck subc-mcp shim --harness <name>` works.
  `ck`'s dispatch already passes stdin and stdout through and returns the child's exit code, so
  an MCP stdio server works behind it.
- The README and `ck setup`'s printed next step change to `ck subc-mcp shim --harness <name>`.
- Existing harness configs: during the migration, `ck upgrade` looks in the known config files
  (Claude Code, OpenCode, Codex) for a bare `ck-subc-mcp` and prints each file, the line, and the
  replacement. `ck upgrade` does not edit those files itself, because they belong to other
  programs.

## Other user-facing commands

Every doc or message that tells a user to run a `ck-<name>` binary by name changes to the `ck`
form, or to an absolute path where no `ck` form exists. Known instances: README MCP lines,
`docs/restarting-the-module-that-serves-you.md` (`ck-auth import`),
`docs/designs/nats-install-trust-chain.md` (`ck-bus install-apply`),
`docs/specs/ck-install-setup-upgrade.md` (`ck-subc --version`).

## Tests

- A new install puts only `cmd/` on PATH (profile block and Windows registry, as each installer's
  tests do today), and `cmd/` holds only `ck`.
- Migration: the inventory row moves; the profile block is rewritten; malformed markers refuse
  without changes; `bin/ck` survives while `bin/` is on the process PATH and is removed after.
- `ck subc-mcp shim` passes stdio through end to end, with a real `ck-subc-mcp`.
- Every test above fails, by name, when the change it covers is reverted.

## Open questions

1. The folder name `cmd/`. Alternatives: `path/`, `shims/`.
2. Harness configs: print the exact edits (proposed), or rewrite them automatically.
3. The development Mac doesn't use the installer layout: its `ck` is in `~/.local/bin` and the bin
   folder. Should its `ck` move to `cmd/` once this ships, so that Mac runs what users run?
