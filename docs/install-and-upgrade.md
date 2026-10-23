# Installing and upgrading

This guide installs the daemon and the `ck` CLI, adds modules, connects an
agent, upgrades, and removes everything again. The full set of rules the
installer follows is in [`setup-and-upgrade-contract.md`](setup-and-upgrade-contract.md).

Nothing here needs administrator rights. Everything installs into your own
account.

## 1. Install `ck`

macOS and Linux (needs `bash` and `curl`):

```sh
curl -fsSL https://cortexkit.io/install | bash
```

Windows (PowerShell):

```powershell
irm https://cortexkit.io/install/win | iex
```

The script downloads the `ck` archive for your platform from the release index
(`https://cortexkit.io/releases/v1/index.json`), checks its SHA-256 against the
index, and places only `ck` (`scripts/install/install.sh`,
`scripts/install/install.ps1`):

| Platform | `ck` goes to | PATH change |
| --- | --- | --- |
| macOS, Linux | `<data home>/cortexkit/cmd/ck` | A block between `# cortexkit-managed PATH begin` and `# cortexkit-managed PATH end` in `~/.zshrc`, `~/.bashrc` or Fish `config.fish`. |
| Windows | `%LOCALAPPDATA%\cortexkit\cmd\ck.exe` | The user `PATH` in `HKCU\Environment`. |

`<data home>` is `$XDG_DATA_HOME`, else `~/.local/share`. Only the `cmd/`
directory goes on PATH; the daemon and modules live in `<data home>/cortexkit/bin/`
and are reached through `ck`. Open a new shell so the PATH change applies.

The install script relies on the HTTPS connection to `cortexkit.io` to know
the index is genuine; it does not check the index's signature itself. The `ck`
it places checks that signature on its first `ck setup`, and refuses an index
that is unsigned or wrongly signed.

Verify:

```sh
ck --version
```

## 2. Set up the daemon

```sh
ck setup --dry-run   # print the plan; changes nothing
ck setup
```

`ck setup` places the daemon (`ck-subc`) and the MCP gateway (`ck-subc-mcp`),
writes the daemon configuration (`<config home>/cortexkit/subc.jsonc`), registers
the daemon with your login session's service manager, starts it, and then
offers optional modules. Configuration writes are additive: setup prints the
change first and refuses to overwrite a value you set yourself.

| Platform | Service registration |
| --- | --- |
| macOS | launchd agent `cortexkit.subc` at `~/Library/LaunchAgents/cortexkit.subc.plist` |
| Linux | `systemd --user` unit `cortexkit-subc.service` (needs a reachable user session bus) |
| Windows | Task Scheduler task `\CortexKit\subc-daemon`, started at logon |

Verify:

```sh
ck              # dashboard: daemon, modules, health
ck daemon       # version, uptime, connection info
ck health       # one line per supervised module
```

## 3. Add modules

```sh
ck setup aft
ck setup --with aft,insula --dry-run
```

The components `ck setup` knows are `aft`, `mc`, `insula`, `claustrum` and
`synapse` (`SETUP_HELP` in `crates/subc-core/src/bin/ck.rs`). A module that
declares a minimum daemon version is refused before anything is downloaded
when the installed daemon is older; run `ck upgrade` first.

`ck setup aft --convert` and `ck setup mc --convert` move an existing
standalone installation under the daemon. They print the plan and change
nothing until you add `--confirm`.

## 4. Connect an agent

The modules the daemon runs are presented to an agent as one MCP server,
`ck subc-mcp shim`.
Name the harness so the tool surface can be shaped for it.

Claude Code:

```sh
claude mcp add ck -- ck subc-mcp shim --harness claude-code
```

OpenCode (`opencode.json`):

```json
{ "mcp": { "ck": { "type": "local", "command": ["ck", "subc-mcp", "shim", "--harness", "opencode"] } } }
```

Codex (`~/.codex/config.toml`):

```toml
[mcp_servers.ck]
command = "ck"
args = ["subc-mcp", "shim", "--harness", "codex"]
```

Any other MCP host: run `ck subc-mcp shim --harness <name>` as a stdio server.

## 5. Upgrade

```sh
ck upgrade --check     # what is newer; changes nothing
ck upgrade --dry-run   # the full plan; changes nothing
ck upgrade
```

`ck upgrade` upgrades the daemon first, then modules, then `ck` itself. For
each target it verifies the archive's SHA-256, keeps a rollback copy, places
the new binary, restarts the target, and checks that it came back healthy at
the new version. If that check fails, it offers to roll back to the copy it
kept.

- The daemon restarts through the service manager, with a 30-second drain.
  Windows and macOS wait for the old daemon PID (checked against the daemon
  executable and process start time) to exit before starting the replacement.
  If it remains after 40 seconds, the restart refuses with `daemon-stop-timeout`
  and the PID. Linux's `systemctl --user restart` already waits for the stop.
  While checking health, an ended task or job with no daemon process triggers
  one start retry. If it ends again, upgrade reports "the daemon did not stay
  running", including process presence, task/job state and last result, rather
  than spending the 180-second health budget on a start that has already ended.
- Modules restart through `supervisor.restart`; the upgrade waits up to 180
  seconds for each to report healthy.
- `ck` replaces itself last. On Windows the old binary is left as
  `ck.exe.old` and removed by the next successful `ck` run.

The daemon never checks for updates itself. Bare `ck` refreshes a 24-hour
update cache in the background and never waits on the network for more than a
moment.

Verify:

```sh
ck daemon
ck module list
```

## 6. Uninstall

```sh
ck setup --uninstall --dry-run
ck setup --uninstall
```

Uninstall removes only what the installer inventory
(`<data home>/cortexkit/installer-manifest.json`) records: placed binaries,
the PATH change and the service registration. Your configuration and every
module's store stay on disk; the command lists what it kept. Delete
`<config home>/cortexkit/` and `<data home>/cortexkit/` yourself to remove
them too.

## 7. Existing installations

Installs made before `ck` moved to its own `cmd/` directory are not migrated.
Their PATH entry stays as it was; edit your shell profile by hand if you want
only `ck` on PATH.
