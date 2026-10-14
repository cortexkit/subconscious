# Durable state

The daemon is a router and supervisor; it keeps no database of its own. What
it does keep across restarts is a small set of files, listed here with their
paths, formats, writers and what happens when one is absent or damaged. Paths
use these roots:

- `<data home>`, as the daemon resolves it: `$XDG_DATA_HOME`; else, on
  Windows, `%APPDATA%` (or `%USERPROFILE%\AppData\Roaming`); else
  `$HOME/.local/share` (`default_data_home` in
  `crates/subc-daemon/src/daemon_config.rs`; test
  `default_data_home_matches_golden_fixture` (`crates/subc-daemon/src/daemon_config.rs`)).
- `<install home>`, as `ck setup` resolves it for binaries and its inventory:
  `%LOCALAPPDATA%\cortexkit` on Windows; otherwise `<data home>/cortexkit`
  with `$XDG_DATA_HOME` or `$HOME/.local/share` (`data_directory` in
  `crates/subc-core/src/setup/apply.rs`).
- `<config home>`: `$XDG_CONFIG_HOME`; else, on Windows, `%APPDATA%` (or
  `%USERPROFILE%\AppData\Roaming`); else `$HOME/.config`
  (`default_config_home` in `crates/subc-daemon/src/daemon_config.rs`).

A relative data home (no `HOME` and no absolute `XDG_DATA_HOME`) is refused
instead of being resolved against the working directory
(`daemon_run_dir` in `crates/subc-daemon/src/daemon_config.rs`).

## 1. Overview

| Path | What | Writer | Survives daemon restart |
| --- | --- | --- | --- |
| `<config home>/cortexkit/subc.jsonc` | Daemon and module configuration. | The operator and `ck setup`. | Yes. |
| `<data home>/cortexkit/run/` | The run directory, created `0700`. | The daemon. | Yes. |
| `$XDG_RUNTIME_DIR/subc-connection.json`, or `<temp dir>/subc-<user>.connection.json` when `XDG_RUNTIME_DIR` is unset | Endpoint, key and daemon identity (section 2). | The daemon at start. | Replaced at each start. |
| `run/daemon.lock` | Ownership of the run directory (section 3). | The daemon. | The file stays; the lock dies with the process. |
| `run/live-children.json` | Supervised processes, for the orphan sweep (section 3). | The daemon. | Yes, read once at the next start. |
| `run/terminals.jsonl` | Module exit history (section 4). | The daemon. | Yes. |
| `run/activity.jsonl` | The activity ledger (section 5). | The daemon. | Yes. |
| `run/logs/subc.<YYYY-MM-DD>.log` | The daemon's own log, one segment per UTC day. | The daemon. | Yes, pruned by age. |
| `run/logs/<module>.stderr.log` | Captured stray stderr and stdout of each module. | The daemon. | Yes, rotated by size. |
| `<data home>/cortexkit/machine-id` | This machine's id (section 6). | The daemon, once; `ck machine adopt`. | Yes. |
| `<data home>/cortexkit/<module>/store.db` | A module's store, when central storage is configured. | The module. | Yes. The daemon only names the path. |
| `<install home>/installer-manifest.json` | What setup installed (section 7). | The installer, `ck setup`, `ck upgrade`. | Yes. |

Module logs (`<module data dir>/logs/<module>.<YYYY-MM-DD>.log`) are written by
each module, not by the daemon; see [`logging.md`](logging.md).

## 2. Connection file

The connection file holds the loopback endpoints, a 32-byte random key, the
daemon id, its pid and its version (`ConnectionInfo` in
`crates/subc-transport/src/connection_file.rs`). The daemon writes it to
`$XDG_RUNTIME_DIR/subc-connection.json`, or, when `XDG_RUNTIME_DIR` is unset
(as under the launch agent `ck setup` installs on macOS), to `subc-<user>.connection.json` in the system
temporary directory, where `<user>` is the numeric uid on Unix (`connection_file_path` in
`crates/subc-daemon/src/bootstrap.rs`). It writes through a temporary file and
a rename, mode `0600` on Unix and a private DACL on Windows.

- Readers refuse a file that is not owned by them, is readable by group or
  others, does not parse, or has an unsupported schema, no endpoints or a short
  key. Tests:
  `owner_only_file_validation_refuses_a_different_effective_uid` (`crates/subc-transport/src/connection_file.rs`),
  `read_rejects_group_or_world_readable_file` (`crates/subc-transport/src/connection_file.rs`),
  `validate_rejects_unsupported_schema_empty_endpoints_and_short_key` (`crates/subc-transport/src/connection_file.rs`).
- The writer refuses to publish beneath a directory another user can write.
  Test: `a_group_writable_ancestor_refuses_and_the_same_tree_without_the_bit_publishes` (`crates/subc-transport/src/connection_file.rs`).
- A client finds the file this way (`discovery_candidates` in
  `crates/subc-transport/src/connection_file.rs`): an explicit `--subc` path is
  the only candidate; otherwise `SUBC_CONNECTION_FILE`, when set, is the only
  candidate; otherwise it tries `$XDG_RUNTIME_DIR/subc-connection.json`,
  `$HOME/.local/share/cortexkit/run/subc-connection.json` and the
  temporary-directory name above, in that order. Tests:
  `set_and_wrong_environment_path_fails_without_fallback` (`crates/subc-transport/src/connection_file.rs`) and
  `discovery_without_overrides_keeps_three_rung_order_and_deduplicates` (`crates/subc-transport/src/connection_file.rs`).
- A new daemon writes a new key. A client holding the old one fails the
  handshake and must re-read the file. Test:
  `key_rotation_rejects_old_connection_file_then_accepts_reread_file` (`crates/subc-transport/tests/handshake.rs`).

## 3. Run-directory lock and the live-children record

Before touching the run directory, the daemon takes an exclusive advisory lock
on `daemon.lock` and holds it until it exits. A second daemon over the same
data home refuses to start and leaves the first daemon's modules alone. The
kernel releases the lock however the process dies, so a crashed daemon never
blocks the next one (`crates/subc-daemon/src/run_dir_lock.rs`).

`live-children.json` lists every supervised child (versioned JSON, rewritten
whole through a temporary file on each spawn and exit). At start, before
spawning anything, the daemon reads the previous record and ends each listed
process that is still the process recorded: same pid, same kernel start time
and same executable image. A process that fails any check is never signalled.
A record with an unknown version is not swept. On Windows the job object is the
primary cleanup and the sweep is a fallback (`crates/subc-daemon/src/live_children.rs`).

Tests:
`a_second_daemon_over_the_same_data_home_refuses_and_leaves_the_owners_modules_alone` (`crates/subc-core/tests/run_dir_ownership.rs`),
`a_killed_daemons_lock_does_not_block_the_next_daemon_and_its_orphan_is_swept` (`crates/subc-core/tests/run_dir_ownership.rs`);
and `orphan_of_a_killed_daemon_is_ended_by_the_next_daemon_before_it_respawns` (`crates/subc-core/tests/daemon_shutdown.rs`).

## 4. Terminal journal

`terminals.jsonl` appends one JSON line per module exit: the module id, the
daemon incarnation, and the exit record (time, exit code or signal,
disposition). A `daemon_shutdown` line marks where a daemon began an announced
shutdown. Retention: 1 MiB per file, three rotated files, 30 days
(`crates/subc-daemon/src/terminal_journal.rs`).

The journal is best-effort. If it cannot be opened the daemon keeps an
in-memory ring and continues; a torn final line is skipped on read and the line
boundary is repaired on the next append. `supervisor.terminals` and
`ck module terminals` merge the journal with the in-memory ring, so exits from
before a daemon restart remain visible. Tests:
`daemon_restart_recovers_terminal_when_the_ring_is_empty` (`crates/subc-core/tests/durable_terminals.rs`),
`daemon_boots_with_missing_terminal_journal` (`crates/subc-core/tests/durable_terminals.rs`),
`daemon_boots_with_empty_terminal_journal` (`crates/subc-core/tests/durable_terminals.rs`).

## 5. Activity ledger

`activity.jsonl` records consequential events from the daemon and from modules,
each with a sequence number that continues across restarts and the daemon
incarnation that wrote it. Retention defaults to 30 days and 64 MiB in total
(`activity.max_age_days`, `activity.max_total_mb`). The ledger has its own
writer thread and a bounded queue; it never blocks routing, supervision or
shutdown. The full contract is in [`activity-ledger.md`](activity-ledger.md).

## 6. Machine id

`<data home>/cortexkit/machine-id` is one line of 32 lowercase hexadecimal
characters. The daemon reads it before accepting connections and mints it only
when the file is absent. A file that does not parse stops the daemon by name
and is left untouched; the daemon never replaces a present file. Only
`ck machine adopt <id>` replaces it, and the change takes effect at the next
daemon start (`crates/subc-daemon/src/machine_id.rs`).

The id is a name for the machine, never an authority: nothing may grant trust
because two messages carry the same id. Tests:
`a_second_boot_with_the_same_data_home_serves_the_same_machine_id` (`crates/subc-core/tests/machine_id.rs`),
`a_corrupt_machine_id_file_stops_boot_by_name_and_is_left_untouched` (`crates/subc-core/tests/machine_id.rs`),
`ck_machine_adopt_refuses_a_running_daemon_and_force_never_changes_what_it_serves` (`crates/subc-core/tests/machine_id.rs`).

## 7. Installer inventory

`installer-manifest.json` records every change the installer, `ck setup`,
`ck upgrade` and self-update made: placed binaries, PATH edits and service
registration. `ck setup --uninstall` removes only what this inventory names,
and leaves configuration and module stores in place
(`crates/subc-core/src/setup/inventory.rs`, `crates/subc-core/src/setup/uninstall.rs`;
test `uninstall_removes_only_inventory_paths_and_retains_configuration_and_stores` (`crates/subc-core/src/setup/uninstall.rs`)).

## 8. What is not durable

- Routes, scopes and their epochs. A daemon restart closes every route, and
  scope owners re-register with `scope.sync`. Until an owner does, a scoped
  `route.open` is refused with the retryable `scope_not_synced`.
- Health reports, stderr tails and spawn events. They start empty in each
  daemon incarnation.
- The catalog. Modules re-register with `HELLO` after a restart.
