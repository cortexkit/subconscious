# Fleet logging — one crate, one format, module-owned files

Status: normative. Approved by the operator 2026-09-05 after a fourteen-module
census (thirteen answered; ENGRAM and WERNI fold in when they land). Supersedes
the "Daemon slice" of `ck-module-logs.md` (daemon-owned per-module files); the
verb section of that document is re-based here.

## Why one crate

The census found two modules with a logging framework and twelve with bare
`eprintln!`: three tags carried a dead or mixed name (`[ck-quota]` ×27 after the
rename; `thalamus:` ×4 vs `thalamus ` ×11; `[alfonso-core]` ×13 beside
`[prefrontal-core]`), two owners called their own prefix unstable, one tagged
by Rust crate path, and the exact tags collided with the same string used as a
repository path by other modules. Twelve of fourteen have no level control.
Two plugin lanes log to `$TMPDIR`, one of them uncapped. Consistency across
fourteen implementations does not happen by asking; it happens when the file,
format, rotation, retention, levels, tags, session field and redaction all come
from one place a module initialises with its module id and nothing else — the
same lesson the data-path resolver taught.

- Rust: `cortexkit-log` in `commons/crates/cortexkit-log` (published; path-dep
  consumers get the usual lock-wave notice).
- TypeScript: `@cortexkit/log` in `subconscious/clients/log` (beside
  `@cortexkit/store`), for harness-hosted plugins.
- Both are conformance-tested against one golden fixture set that lives in
  `subconscious/crates/subc-core/tests/fixtures/log_format_golden.json` and is
  vendored into commons (authority side owns the fixture; same rule as the
  store-path golden).

## Files

```text
<module data dir>/logs/<module_id>.log                 the supervised process
<module data dir>/logs/<module_id>.<harness>.log       one per harness-hosted plugin process
<module data dir>/logs/<name>.log.1 … .log.<keep>      rotated generations
~/.local/share/cortexkit/run/logs/subc.log             the daemon's own log (subconscious only)
~/.local/share/cortexkit/run/logs/<module_id>.stderr.log   daemon capture of a module's stray stderr/stdout
```

- **One file per process.** A plugin runs inside the harness process and cannot
  share the module's handle; `<module>.<harness>.log` is the natural unit
  (`magic-context.opencode.log`, `magic-context.pi.log`, `magic-context.omp.log`,
  `aft.opencode.log`, `prefrontal.opencode.log`). A module never needs lane
  files for its own threads — that is what `session=` and `tag=` are for.
- The data dir is `module_data_dir(module_id)` from `cortexkit-store-types` /
  `@cortexkit/store` — the crate takes the module id and resolves the path;
  callers never assemble it (the doubled-path incident).
- Files `0600`, directory `0700` on Unix; per-user ACL inherited on Windows.
- **Nothing a module writes lands in `subc.log` anymore.** The daemon's log
  carries supervisor events, health transitions, route drops — all already
  stamped `module_id=<id>`. Two daemon-side residues stay on purpose: the
  stderr crash ring (`supervisor.stderr_tail`; a panicking module cannot write
  its own log, and the backtrace is the line worth having) and
  `run/logs/<id>.stderr.log`, a daemon-owned capture of anything a module still
  emits on stderr/stdout. A module that logs through the crate writes nothing
  there, so that file being non-empty is itself a finding.

## Line format

One line per event, UTC with millisecond precision, level padded to five,
module id, then optional fields, then the message, then `key=value` fields.
No ANSI in files, ever (the daemon's file layer sets `with_ansi(false)`).

```
2026-09-05T10:41:03.123Z INFO  broca session=broca:8f3a1c02 run=r_12 dispatch admitted model=anthropic/claude-sonnet-4-5
2026-09-05T10:41:03.130Z WARN  magic-context session=opencode:ses_00fc88222ffe tag=perf transform stage folded ms=412 retry=2
2026-09-05T10:41:04.002Z ERROR insula provider=codex account=ufuk3 refresh failed class=auth_invalid
2026-09-05T10:41:05.500Z INFO  fusiform poll changed version=1788526509641 eras=22 facts_changed=0 arrived=2
```

- `session=<issuer>:<id>` — issuer is the namespace that minted the id
  (`opencode`, `pi`, `omp`, `claude-code`, `broca`, `wernicke:<platform>`…). A
  line may carry more than one session-shaped field when two lineages meet
  (`session=opencode:… broca_session=…`). **Absent when there is none; never a
  placeholder.** MC's synthetic `"global"` ids become absence.
- `tag=<t>` — the module's own tag vocabulary (`perf`, `wire`, `trace`,
  `hist`, `store`…), declared in the module manifest (`self_signals`-style: a
  list with a one-line description each) so `ck module logs <id> --tags` can
  list what exists. Tags are tracing targets on Rust; the TS twin mirrors them.
- Message text is free; fields after it are `key=value`, values with spaces
  double-quoted. The crate escapes newlines inside values so one event is
  always one line.
- Multi-line payloads (backtraces) are emitted as one line per source line,
  each carrying the same timestamp and `tag=panic`; the crate installs a panic
  hook that does this so a module's last words go into its own file, not only
  the daemon's ring.

## Levels and tags: one knob

`CK_LOG=<spec>` in the process environment, `RUST_LOG` grammar:
`info`, `info,perf=debug`, `warn,wire=trace,mc-store=debug`. Both crates read
it; nothing else configures verbosity.

- The daemon injects it at spawn from `subc.jsonc`:
  ```jsonc
  "modules": { "broca": { "program": "…", "log": { "level": "info", "tags": { "perf": "debug" },
                                                    "max_file_mb": 32, "keep": 2, "max_age_days": 14 } } }
  ```
  Defaults when the key is absent: `info`, no tag overrides, 32 / 2 / 14.
  `ck module restart <id>` applies a change; a live-reload op is a later
  addition and not part of this contract.
- Plugins read the same env from the harness process. Harnesses that give a
  plugin no environment path get the defaults; a plugin may also read
  `.cortexkit/<module>.jsonc log.*` for the same keys and must document which
  wins (env wins, as it does for every other knob).
- **No fleet tag vocabulary.** A module declares its tags; the crate refuses
  an undeclared tag at compile time on Rust (a `const` list) and at init on TS.

## Retention

Size cap per file (`max_file_mb`, checked on every write), `keep` rotated
generations, and `max_age_days` applied to rotated generations at every
rotation and at init. Rotation renames `.log → .log.1 → … → .log.<keep>` and
reopens; a follower detects the inode change. No time-based rotation of the
active file: a quiet module's single line must not roll on a clock.

## Redaction

The sink takes a `Redactor` (a `fn(&str) -> Cow<str>` on Rust, a
`(line: string) => string` on TS) applied to every complete line before the
write. Fleet default redacts credential shapes (bearer/JWT-looking tokens, `ckh_`
handles, `sk-`/`ghp_`-style keys, `Authorization:` values); a module composes
its own on top (MC's sanitizer; claustrum's hand-written redacting `Debug`
impls remain the first line of defence). A module MUST NOT log prompt text,
message bodies, or credential payloads at any level; the redactor is the
backstop, not the policy.

## Observability of the logger itself

- `swallowed_writes` counter (write failed, line dropped) readable by the module
  for its health report; the crate reports the first failure per process to
  stderr once, never per line (a full disk must not generate a second flood).
- Rust: when the file sink cannot be opened, the crate falls back to stderr so
  the daemon's capture still exists; the fallback is announced in the first
  line.

## Adoption

- **Live-user modules (aft, magic-context) move only with their doctors.**
  Both have CLI doctors that parse today's paths and formats to extract
  errors into GitHub issues. For them: the doctor learns the new path and
  format first and reads BOTH for at least one release; adoption ships in the
  same release as the doctor; the old `$TMPDIR` plugin log and pid-suffixed
  file are read-only compatibility inputs, never written again after the cut.
  Nothing else in the fleet waits on them.
- Every other module: replace `eprintln!` with the crate; the census items
  each owner took (broca session ids on every line, insula heartbeat only on
  change, thalamus/astrocyte/synapse prefixes, synapse worker rings forwarded,
  callosum crate-path targets, prefrontal's uncapped plugin log) are done in
  that same change because the crate makes them the default shape.
- The daemon: `subc.log` moves to `run/logs/subc.log` through the same crate
  (no ANSI, rotation); stdout of children becomes a pipe alongside stderr into
  `run/logs/<id>.stderr.log`; `CK_LOG` and the retention knobs injected at
  spawn from `subc.jsonc`.

## `ck module logs`

```text
ck module logs [<id>] [-n <lines>] [-f] [--since <dur>] [--tag <t>] [--level <l>] [--lane <harness>|module|stderr|daemon] [--json]
```

- No id: a table of every module's log files (path, size, last line's
  timestamp) — the census as a verb.
- Default: last 200 lines of the merged view for `<id>` — the module's file,
  its plugin lane files, `run/logs/<id>.stderr.log`, and `run/logs/subc.log`
  lines with `module_id=<id>` — merged by timestamp, newest last, each source
  marked in a fixed column (`mod`, `opencode`, `pi`, `stderr`, `daemon`).
- `-f` follows all sources across rotation. `--since` bounds by the line's own
  timestamp (all sources share the format). `--tag`/`--level` filter on the
  fields the format guarantees. `--lane` selects one source.
- Every filtered view ends with one stderr line stating what it removed
  (`showing 200 of 3,412 lines (2,900 below INFO, 312 daemon lines hidden)`).
- Plain text on stdout, one line per line; colour only on a tty.
- Empty states: `no log yet for <id>`; `no module named '<id>'. Run ck module list.`

## Out of scope

Per-request joins across a module's data sinks (thalamus `exchange_id`);
shipping logs anywhere; live level reload.
