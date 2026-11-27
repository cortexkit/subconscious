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
index, and lets the verified copy of `ck` place itself. The script then runs
the installed `ck setup`, which completes the handover and configures the
installation (`scripts/install/install.sh`, `scripts/install/install.ps1`):

| Platform | `ck` goes to | PATH change |
| --- | --- | --- |
| macOS, Linux | `<data home>/cortexkit/cmd/ck` | A block between `# cortexkit-managed PATH begin` and `# cortexkit-managed PATH end` in `~/.zshrc` (or `.zshrc` in the startup-file directory selected by zsh's `ZDOTDIR`), `~/.bashrc` or Fish `config.fish`. |
| Windows | `%LOCALAPPDATA%\cortexkit\cmd\ck.exe` | The user `PATH` in `HKCU\Environment`. |

`<data home>` is `$XDG_DATA_HOME`, else `~/.local/share`. Only the `cmd/`
directory is added to PATH; the daemon and modules live in `<data home>/cortexkit/bin/`
and are reached through `ck`. Existing `bin` PATH entries are left intact.
The installer retires only the old layout's `<data home>/cortexkit/bin/ck`
(`%LOCALAPPDATA%\cortexkit\bin\ck.exe` on Windows) by
moving it to a unique `ck[.exe].retired-*` name beside the original. Setup
records the original path, backup path and digest in `installer-manifest.json`.
An operator's or package manager's `ck` anywhere else is left untouched.
The installer puts `cmd` first in the user
PATH on Windows and in the persisted shell-profile block on Unix, then checks
resolution in a fresh environment. Windows uses registry machine/user PATH and
PATHEXT, not the installer's inherited environment; Unix reads the updated
profile in a new shell. If another `ck` still wins, installation refuses as
`cli-shadowed`, naming the file and its PATH entry without moving it (a Windows
machine PATH entry may need an administrator). Open a new shell so the
persisted PATH change applies to your session.

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

On Linux the unit file goes where the user manager looks for it:
`$XDG_CONFIG_HOME/systemd/user/` when `XDG_CONFIG_HOME` is set to an absolute
path, otherwise `~/.config/systemd/user/`. After reloading the manager, setup
checks that `systemctl --user show -p FragmentPath cortexkit-subc.service`
names that file, and refuses with both paths if the manager loads the unit from
anywhere else (for example when the manager's `XDG_CONFIG_HOME` differs from
your shell's). The same check runs before setup accepts the unit, before `ck`
starts or restarts it by name (including after the reload a definition update
makes), and before uninstall disables it, so a different unit file that systemd
loads under the same name is never started, restarted or disabled; if systemd
cannot be asked, ck refuses rather than guess. A unit setup recorded at the
other location is copied, not moved: setup writes the new file, reloads, and
only when systemd then loads the new file does it remove the old file, its
enablement links and its records. If systemd still loads the old file, setup
removes what it wrote, leaves the old unit running and enabled, and says why.
Setup moves only its own unit (a regular file whose contents match what it
recorded), and a move that stops part way is finished by the next `ck setup`.

On Windows the setup-owned XML remains at `%USERPROFILE%\cortexkit-subc-daemon.xml`.
Setup verifies that file against its inventory before comparing the registered
task's behavioral fields: command, arguments, working directory, principal,
logon type, run level, triggers, restart policy and explicit non-default settings.
Windows' registration metadata and implicit defaults are not user edits; a SID
and its resolved account name identify the same principal. A verified behavioral
edit refuses with the differing field and is not overwritten. The previous
owned XML is evidence of what setup registered, not the desired configuration:
an older owned task is upgraded to the current release's definition.

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

The components `ck setup` knows are `aft`, `mc`, `insula`, `claustrum`,
`synapse`, `fusiform`, `cingulate`, `entorhinal`, `astrocyte`, `striatum`,
`broca`, `cerebellum`, and `plexus`. A module that declares a minimum daemon
version is refused before anything is downloaded when the installed daemon is
older; run `ck upgrade` first.

Bare `ck setup` selects all components with a complete release for this host and
explains any it leaves out. It does not prompt to select modules.
Insula's file/API providers do not read browser stores; cookies come only from
vault deposits on every OS.

`ck setup --with plexus` also selects its hard dependencies, Claustrum and
Entorhinal. It installs `ck-plexus-admin` in the managed `bin` directory; run it
by path, not as a `ck` subcommand. Setup offers to enable signed GitHub agent
policies through operator confirmation once Plexus reports that it may act as
the connector host. Declining does not stop setup; setup prints the
command to enable policies later. `ck upgrade` refreshes the programs and
catalog together without asking to enable policies.

`ck setup aft --convert` and `ck setup mc --convert` move an existing
standalone installation under the daemon. They print the plan and change
nothing until you add `--confirm`.

## 4. Connect an agent

The modules the daemon runs are presented to an agent as one MCP server,
`ck subc-mcp shim`.
Name the harness so the tool surface can be shaped for it.

Core setup installs and configures the supervised `subc-mcp` gateway, including
its `module` argument and `reserved: true` admission. The shim connects to that
gateway's runtime connection file; it does not start a second gateway. For an
older install missing the gateway entry, run `ck setup` again. Setup adds the
entry and rescans the daemon, but refuses a conflicting user-owned entry at
`modules.subc-mcp` rather than replacing it.

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

### Session folder and project attribution

Session attribution works when the daemon supervises `subc-mcp`: the module
registers records using its current supervised launch nonce. Its module entry
uses `reserved: true` for admission as `reserved:subc-mcp`; the supervised
launch nonce, not the reserved flag alone, supplies attribution authority.
Core setup writes this entry on fresh installs and repairs its absence on a
rerun. Attribution requires
daemon 0.36.35 or newer advertising `scope-workdir/v1` through
`server.describe`, as well as scope sync/apply support. Older daemons keep
serving tools without attribution.

Without a valid `CK_INSTANCE_TOKEN`, subc-mcp hosts the MCP session itself;
Claude Code connected directly to AFT over MCP does not need Thalamus. After
MCP initialization, subc-mcp requests `roots/list` if the client advertises
roots. It uses the first root, normalized to the top of the enclosing git
checkout or linked worktree. With no roots capability or an empty list, it
uses the shim's resolved project root and logs the fallback. A non-local or
invalid root, a failed roots request, or a roots timeout also uses the shim's
project root, with a warning naming the cause.

Before registering the session, subc-mcp asks Entorhinal to resolve that
folder. Only an explicit project match supplies `project_id`; an implicit
match, refusal or unavailable Entorhinal leaves it unset. Folder and project
are fixed for that session's scope epoch, and the record ends when the MCP
connection closes. Providers declaring `scope-workdir/v1` receive these
fields on their bind stamp. Other providers keep their existing bind body,
and the legacy bind identity's `project_root` is unchanged for all providers.

Attribution never gates tool availability. A scope-authority refusal, record
refusal or attribution timeout logs a warning and falls back to unscoped
provider routes with the existing bind body; catalog refresh and tools remain
available. These fallbacks apply to otherwise attested sessions. Startup
attestation rules are unchanged: a truly hand-run module without a module ID
and launch nonce is refused, never bound as a trusted direct caller. Running
the stdio shim by hand is supported as shown above.

A valid `CK_INSTANCE_TOKEN` instead identifies a gateway-hosted session.
Subc-mcp never creates its own record for it, even if the gateway is down.
Conversation-bound calls use the gateway's optional `scope` selector from
`session.resolve`; absent a selector, their routes keep the existing bind
behavior. The module logs which owner it selected once at session attach.

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
- Supervised modules, including `subc-mcp` configured by core setup, restart
  through `supervisor.restart`; the upgrade waits up to 180 seconds for each to
  report healthy. If a module is not supervised on this host, upgrade says its
  restart was omitted rather than starting an independent process.
- Before each change, the installer syncs its expected old and new states to
  `installer.journal` beside `installer-manifest.json`. A release becomes
  current only after activation is verified and the inventory is published.
  If the command stops, the next applying command verifies and finishes a
  complete transaction or restores the previous state. It refuses to
  overwrite an unexpected change and names the path.
- macOS exchanges the daemon bundles atomically. On Windows a running
  executable must move aside before the new one can take its name. If `ck`
  is missing after an interrupted Windows upgrade, run `install.ps1` again;
  it restores the previous version, after which you can rerun `ck upgrade`.
  If a Windows process still holds a backup, the completed journal moves to
  `installer.retired/` so it does not block the next operation. Applying
  commands report the retained file and clean it up after the process exits.
- The first applying command reconciles a verified `installer-manifest.bootstrap.json`
  (the older install script's separate executable ownership record) and
  recorded backup files left by released installers. It leaves a backup in
  place if it is still in use or does not match the recorded ownership.
  If a development build left an unfinished activation, bundle or unit move,
  finish it with that build, which understands its recovery records, before
  retrying. Do not delete recovery files.

The daemon never checks for updates itself. Bare `ck` refreshes a 24-hour
update cache in the background and never waits on the network for more than a
moment.

Only one of `ck setup`, `ck upgrade`, `ck setup --uninstall`, and bootstrap changes an
installation at a time. Each uses the exclusive `installer.lock` beside the
inventory. A second command waits up to 30 seconds, then refuses and names the
holder's process ID and command when known. Applying commands recover before
planning; previews report an unfinished journal or retained cleanup files
without locking, starting services or writing anything.

Setup confirmation and the app's upgrade rollback prompt release the install
lock while waiting for your answer. If another installer changes the plan or
recovers the journal in that time, the answer is discarded. It is safe to
rerun the command to review the current state.

The vault owns its signing-key grants and ck-bus owns its NATS listener file.
Setup does not put either external operation in the installer journal. After
committing installation, it checks and reapplies these idempotent operations
when needed on each applying run. If setup stops before the external operation,
run it again; it does not need to unwind the committed installation. Setup's
own optional health entry is a separate journaled config change.
Updating a shell profile preserves its permission bits and any symlink to it,
but replaces the target file. Other hard links keep the old bytes; ACLs,
extended attributes, and group ownership are not copied.
Tightening access to installer-owned files and directories is also outside
rollback. It checks ownership and only narrows access; retrying can finish the
repair, and undo never restores a looser mode.

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
