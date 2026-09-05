# `ck module logs` — daemon-owned per-module log files

Status: SUPERSEDED by `fleet-logging.md` (operator ruling 2026-09-05: modules own their log files through one shared crate; the daemon owns only its own log and a stray-stderr capture). Kept for the census record below; the verb is re-specified in `fleet-logging.md`. Written 2026-09-05 from a
fourteen-module census (twelve answered; MC, ENGRAM, WERNI outstanding and folded
in when they land) and a source read of `crates/subc-core/src/stderr_tail.rs`
and `supervise.rs`.

## What the census settled

1. **A module's self-tag cannot be the attribution key.** Of twelve modules:
   three carry a dead or mixed name (`[ck-quota]` ×27 vs `[insula]` ×2;
   `thalamus:` ×4 vs `thalamus ` ×11; `[alfonso-core]` ×13 beside
   `[prefrontal-core]`), two are unstable by their owner's own account
   (astrocyte, synapse), two emit nothing or almost nothing (cerebellum,
   claustrum), one tags by Rust crate path rather than module id (callosum:
   `fed_module::*`), and the exact ones collide with the same string used as a
   repository path by other modules (`grep fusiform` on the shared file is 93%
   AFT lines carrying `root=/…/fusiform`). Panic backtraces are multi-line and
   untagged everywhere.
2. **The daemon already knows who wrote each line.** The supervisor pipes every
   child's stderr through `pump_stderr` (one write per reassembled line, bytes
   verbatim) into a per-module ring and forwards to its own stderr. Attribution
   by pipe is exact; attribution by string match is not. The two lanes disagree
   about what "a module's output" means today (QTA), and only one may remain.
3. **Fresh installs log to nothing.** `subc.log` exists on the development host
   only because a hand-written LaunchAgent redirects the daemon's stdio. The
   `ck setup`-generated definitions carry no redirect: macOS → `/dev/null`,
   Linux → journald, Windows → nothing. The supervisor's own lines about a
   module (restart, health transition — 339 `status=Degraded` lines for fusiform
   that fusiform never wrote) are recoverable nowhere on a fresh install.
4. **Two subjects, one question.** "What happened to X" needs both the lines X
   wrote and the daemon's lines about X, visually distinct (ASTRO, FUSI, PLEX).
5. **Levels are not a fleet concept.** Two of twelve modules have level control
   (`RUST_LOG` on aft, callosum). A `--level` flag would silently no-op for ten;
   it is refused, not accepted (BROCA's form).
6. **A filtered view must say what it removed.** 40 lines shown of 40 and 40 of
   40,000 look identical and are not (BROCA).

## Daemon slice

Files live under the run directory the daemon already owns
(`~/.local/share/cortexkit/run/` on Unix, the matching per-user directory on
Windows), created `0700`/`0600`:

```text
run/logs/<module_id>.log        that module's stdout AND stderr, by pipe
run/logs/<module_id>.log.1      one rotated generation
run/logs/daemon.log             the daemon's own tracing output
run/logs/daemon.log.1
```

- **Both streams are piped.** Today stdout is inherited; it becomes a pipe
  through the same reassembling pump as stderr, into the same per-module file.
  A module that prints to stdout under supervision (none do in production per
  the census; callosum's env_logger writes stdout) loses nothing.
- **Line format is the module's bytes, prefixed by the daemon:**
  `<RFC3339 ms Z> <e|o> <bytes>` — the pump's capture instant and the stream
  letter, then the line verbatim. The module's own text is never rewritten;
  the prefix is the only addition and it is the same on every platform. A line
  that exceeds `MAX_PENDING_LINE_BYTES` is written as today (split, marked in
  the ring), not dropped.
- **Rotation is size-based: 32 MiB active, one backup, checked on every write**
  (aft's numbers, which have a month of production behind them). Rotation
  renames `.log` → `.log.1` and reopens; a follower detects the inode change
  and reopens. No time-based rotation: a quiet module's file must not roll on
  a clock and lose the only line it wrote.
- **The shared-fd forward stays for now.** The pump still writes each line to
  the daemon's stderr so `subc.log` readers (`fleet-pulse.sh`, the rotate
  script, operators' greps) keep working while they migrate. Removal
  condition: `fleet-pulse.sh` reads `run/logs/` and no script under
  `scripts/fleet/` opens `subc.log` — then the forward is deleted in one
  commit, not left as a second lane.
- **The ring stays as the crash surface** (`supervisor.stderr_tail`, survives
  respawn); it now records stdout lines too. It answers "what did it say
  before it died"; the file answers everything else.
- **The daemon writes its own log to `run/logs/daemon.log`** with
  `with_ansi(false)`, same rotation, in addition to stderr. Every supervisor
  line about a module carries `module_id=<id>` as a structured field already;
  this makes that field the join key for the verb and closes the fresh-install
  hole for the daemon's own lines.
- **Startup marker.** On each spawn the daemon writes one line of its own into
  the module's file: `<ts> d process started pid=<pid> image=<sha8>` — the
  `d` stream letter marks it as the daemon's, and it gives a reader the
  restart boundary the ring already carries.
- **Windows:** identical layout under the per-user data directory; the pipe is
  the only sink there, which is exactly why the layout is the daemon's and not
  the service manager's.

Tests: a fake module that writes to stdout and stderr yields a file whose lines
carry the right stream letters and byte-identical payloads; a line spanning two
reads is written once; rotation renames at the cap and the follower reopens;
the forward to daemon stderr still receives every line (until removed); the
daemon's `module_id=` field appears on every supervisor line about the module.

## The verb

```text
ck module logs <id> [-n <lines>] [-f] [--since <dur>] [--module-only] [--daemon-only] [--json]
```

- **Default: the last 200 lines of the merged view** — the module's file and
  the daemon's `module_id=<id>` lines, merged by timestamp, newest last. The
  daemon's lines render distinctly (a `daemon:` marker in the same column where
  the module's stream letter sits). Ages are not used here: a log is the one
  surface where absolute timestamps are the right rendering.
- **`-f`** follows both files across rotation (reopen on inode change). Ctrl-C
  exits 0.
- **`--since 15m`** bounds by the daemon's capture timestamp, not by any
  timestamp inside the module's bytes.
- **Grep passes through**: output is plain text on stdout, one line per line,
  no colour unless stdout is a tty (`--no-color` forces off). When the caller
  filters with `--module-only`/`--daemon-only`, the last line on stderr states
  what was excluded: `showing 200 of 3,412 lines (3,212 daemon lines hidden)`.
- **No `--level`.** Modules that have level control expose it through their own
  environment; the verb does not pretend the others do.
- **`ck module logs` (no id)** lists the modules with a file, its size, and the
  timestamp of its last line — the log census as a table.
- **aft** is not special-cased: its own `aft-<pid>.log` is a duplicate of what
  the pipe carries (it tees to stderr), and the verb reads the daemon's file.
  If a module's own file ever diverges from its stderr, that is the module's
  defect, not a reason to read two sources.
- Empty states: `no log yet for <id> (module has not been spawned since the
  daemon started)`; `no module named '<id>'. Run ck module list.`

## Out of scope, recorded

- Per-request views (thalamus's `exchange_id` join across its JSONL and dump
  directories) — a later `--request` mode on thalamus's own CLI, not this verb.
- Harness plugin logs (`$TMPDIR/alfonso.log`, `…/aft/logs/aft-plugin.log`):
  written by the harness process, not by a supervised module. The verb does not
  tail them; `ck daemon triage` may list their paths.
- Module-side hygiene the census surfaced and each owner took: broca session
  ids on lines; insula's heartbeat emitted only on change; thalamus, astrocyte,
  synapse prefix normalization; synapse forwarding its worker rings; ALF's
  uncapped plugin log; callosum's crate-path targets. None gates the verb.
