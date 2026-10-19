# Operating the daemon

Day-to-day commands for looking at the daemon and its modules, reading their
logs, and restarting, stopping and reconfiguring them. Every command accepts
`--json` for machine-readable output and `--subc <file>` to talk to a daemon
other than the one the connection file names. Run `ck <domain>` with no verb
for that domain's help.

Exit codes for `ck module` verbs: `1` means the operation was refused or
failed; `4` means a mutating operation timed out and its outcome is unknown,
so check state before retrying (`MODULE_HELP` in `crates/subc-core/src/bin/ck.rs`).

## 1. Is everything running?

```sh
ck                    # dashboard
ck daemon             # daemon version, uptime, connection info
ck module list        # every supervised module with state and health
ck module status aft  # one module in detail
ck health             # cached one-line health per module
ck health aft         # a fresh health.check probe, with headline metrics
ck health aft --verbose
```

`ck module list` shows the supervised roster: what the daemon spawned. `ck
catalog` shows the registry: everything registered on the wire, including
modules the daemon did not start. A harness waiting for a module to come up
should poll `ck catalog <id>`, which exits `1` until the module is registered.

A health of `unknown` means the module does not serve `health.check`, not that
it is unhealthy.

If the daemon does not answer at all:

```sh
ck daemon triage      # reads only the run directory; never contacts the daemon
```

## 2. Logs, stderr and exits

```sh
ck module logs aft                 # merged module, plugin, stderr and daemon logs
ck module logs aft -n 500 --since 30m --level warn
ck module logs aft -f              # follow across rotation
ck module stderr aft -n 50         # retained stderr lines
ck module terminals aft            # retained process exits, across daemon restarts
```

Log files are under `<data home>/cortexkit/run/logs/` (the daemon's own
`subc.<YYYY-MM-DD>.log` and each module's captured `<id>.stderr.log`) and in
each module's own data directory. See [`logging.md`](logging.md) and
[`data-model.md`](data-model.md).

Log file names and timestamps use UTC (the `Z` suffix), so a file's date can differ from your local date.

## 3. Restart, stop and start a module

```sh
ck module restart aft                  # drain in-flight requests, then restart
ck module restart aft --drain-ms 5000  # wait at most 5 s for in-flight requests
ck module restart aft --now            # do not wait; for a wedged module
ck module restart aft --swap           # blue/green; needs overlap: "safe"
ck module stop aft                     # disable and stop until start or a daemon restart
ck module start aft                    # enable and start
```

A restart closes every route to the module. Clients see `route.closing` and
`route.closed` and reopen; a request that was in flight when the route closed
has an unknown outcome. If you restart the module that carries your own
session (for example the module that runs your shell), the command itself
loses its reply even though the restart succeeds; see
[`restarting-the-module-that-serves-you.md`](restarting-the-module-that-serves-you.md).

Verify with `ck module status <id>` and `ck health <id>`, which open a fresh
connection.

To reverse `stop`, run `start`. A stop is held in the daemon's memory, not
written to `subc.jsonc`: after a daemon restart the module's configured
`enabled` value applies again. To keep a module off, set `"enabled": false`
in its configuration. A module that hit its crash budget stays down
until `ck module start` or `ck module restart`.

## 4. Change the configuration

Edit `<config home>/cortexkit/subc.jsonc`, then:

```sh
ck daemon lint                 # check the config and manifests offline
ck module rescan --dry-run     # show what would change
ck module rescan               # apply it
```

A rescan adds new modules, retires modules removed from the file (stopping
them), and applies changed launch settings at each module's next spawn and
health settings at once. Some settings apply only when the daemon restarts,
and rescan reports them as `restart_required`: `port`, `storage`,
`admission_facts_carrier_module_id`, `admission_facts_targets`,
`scope_authority_owners` and `privileged_operator_prompt`
(`RestartRequiredSection` in `crates/subc-daemon/src/daemon_config.rs`). An
edit to a running module's `restart` block or `drain_timeout_ms` also waits for
the next daemon start.

A module removed from the file keeps its reserved id, so no other process can
register under it. Release it explicitly:

```sh
ck module release old-module
```

The keys and their limits are in [`daemon-config.md`](daemon-config.md).

## 5. Restart the daemon

The daemon runs under the service manager that `ck setup` registered:

```sh
# macOS
launchctl kickstart -k "gui/$(id -u)/cortexkit.subc"
# Linux
systemctl --user restart cortexkit-subc.service
# Windows
schtasks /End /TN "\CortexKit\subc-daemon"
schtasks /Run /TN "\CortexKit\subc-daemon"
```

A daemon restart closes every route, re-reads the configuration, writes a new
connection key, and ends any module process the previous daemon left behind
before it starts new ones. Clients reconnect by re-reading the connection file.

New macOS LaunchAgents restart after a failed exit or fatal signal, but not a
clean exit (including the daemon's SIGTERM shutdown), with a 10-second launch
throttle. New Windows task definitions retry failures once a minute, up to 255
times, without a runtime limit or battery restrictions. They do not wake a
sleeping computer or add a resume trigger. Linux keeps `Restart=on-failure`.

With `ck` 0.20.108 or later, existing installs get these settings too. Run
`ck setup --dry-run` to inspect the service-definition update. Interactive
`ck setup` shows the same step before `Continue? [y/N]`; `--yes` accepts the
plan without asking (non-terminal callers also proceed without a question).
On macOS the step explicitly reloads the LaunchAgent with bootout/bootstrap:
**the daemon and every module restart**, so choose a time for that interruption.
An ordinary registration/start still refuses an unplanned live LaunchAgent
replacement. On Windows, setup updates the registered task with
`Set-ScheduledTask`, without stopping its current instance. New settings govern
future instances. Linux reloads changed systemd units without stopping the
running daemon; its existing `Restart=on-failure` policy is unchanged.

When `ck upgrade` activates a daemon upgrade, it regenerates an outdated owned
definition inside that same restart (between stop and start on macOS/Windows,
before systemd's reload/restart on Linux), not with a second restart. An upgrade
with no daemon activation does not restart it just to refresh a definition;
use setup for that case.

Only setup-owned definitions are reconciled. Missing inventory ownership,
unrecognized legacy contents, or semantic user edits produce a refusal, not an
overwrite. Formatting alone does not trigger an update: plist keys and typed
values, task XML elements/attributes, and systemd section/directive values are
compared after parsing. Windows also reads the registered task rather than
trusting the XML file alone. See the [setup contract](setup-and-upgrade-contract.md#service-definition-reconciliation)
for the legacy ownership and comparison rules. Back up and resolve a refused
definition deliberately; deleting inventory entries does not authorize adoption.

## 6. Who is connected

```sh
ck routes             # live consumers of every module
ck routes aft         # live consumers of one module
ck provenance aft     # what the daemon observed about the process beside what the module declared
```

## 7. Activity

```sh
ck activity                         # newest 100 events
ck activity --module aft --since-ms 1767225600000
ck activity --kind subc.module_restarted -f
```

Filters are exact matches and combine: `--agent`, `--project`, `--module`,
`--kind`, `--since-ms`, `--until-ms`, `--limit`. Times are shown in UTC. See
[`activity-ledger.md`](activity-ledger.md) for what is recorded.

## 8. This machine's id

```sh
ck machine show
ck machine adopt <32-hex-id>    # restore a previous id; applies at next daemon start
```

`adopt` refuses while the daemon is running unless you pass `--force`, and even
then the running daemon keeps serving its current id until it restarts.
