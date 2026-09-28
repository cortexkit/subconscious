# Fleet logging — one crate, one format, module-owned files

Status: normative, **r2**. Approved by the operator 2026-09-19 after the fleet
log census (`docs/audits/fleet-log-census-2026-09-19.md`) found two of nineteen
producers on the r1 format two weeks after r1 was approved. r2 supersedes r1 on
the line format, the file layout, rotation, and the tag mechanism; it keeps
r1's crate architecture, levels, `CK_LOG` knob, redaction, session semantics,
and adoption rules for the two live-user modules.

What changed and why, in one paragraph: r1 had a `tag=` field orthogonal to
level, a positional module id, per-harness files for plugins, and rename-based
rotation. The census showed a fleet with six line formats, and the design
conversation that followed found that every mainstream logging framework
solves "errors only, but also perf metrics" the same way — a **hierarchical
logger name with per-logger levels**, never a separate tag dimension. r2 adopts
that, which also settles where the module id lives (it is the root of the
logger name), and moves rotation from rename to **date-stamped segments**,
which removes the multi-writer race that per-harness files existed to avoid,
so plugins and the module can share one file.

## Why one crate

The census found two modules with a logging framework and twelve with bare
`eprintln!`: three tags carried a dead or mixed name (`[ck-quota]` ×27 after the
rename; `thalamus:` ×4 vs `thalamus ` ×11; `[alfonso-core]` ×13 beside
`[prefrontal-core]`), two owners called their own prefix unstable, one tagged
by Rust crate path, and the exact tags collided with the same string used as a
repository path by other modules. Twelve of fourteen have no level control.
Two plugin lanes log to `$TMPDIR`, one of them uncapped. Consistency across
fourteen implementations does not happen by asking; it happens when the file,
format, rotation, retention, levels, logger names, session field and redaction
all come from one place a module initialises with its module id and nothing
else — the same lesson the data-path resolver taught.

The 2026-09-19 census measured the cost of the gap directly: eleven seats
**linked** the crate and one **routed** through it. Linking is a Cargo edit;
routing is the per-seat change nobody is blocked on, and the daemon's stderr
capture (a well-behaved fallback with rotation and a renderer) removed the
visible cost of not doing it. Adoption is therefore **mandatory** under r2, and
the capture lane's non-emptiness becomes a reported finding rather than a
sentence in a spec.

- Rust: `cortexkit-log` in `commons/crates/cortexkit-log` (landed at commons
  `01da197`; publish to crates.io follows the first fleet adopter; path-dep
  consumers get the usual lock-wave notice).
- TypeScript: `@cortexkit/log` in `subconscious/clients/log` (beside
  `@cortexkit/store`), for harness-hosted plugins; landed at `a4575374`.
- Both are conformance-tested against one golden fixture set that lives in
  `subconscious/crates/subc-core/tests/fixtures/log_format_golden.json` and is
  vendored into commons (authority side owns the fixture; same rule as the
  store-path golden). **The r2 fixture is a breaking rewrite of the r1 one**;
  both twins re-pin in the same change.

## Line format

```
<timestamp> <LEVEL> <logger>: [<bound fields>] <message> <event fields>
```

```
2026-09-19T04:46:29.432Z INFO  magic-context.historian: [harness=pi session=pi:01a0b7fc-bba1-7f60-aa2b-27c2df4481ab] trigger fired reason=force_band usage=93.9%
2026-09-19T04:46:29.449Z INFO  magic-context.transform: [harness=opencode session=opencode:ses_0758f6ce7ffeJ0A9sV8Qvema7d] transform completed ms=24.3 messages=25 targets=23
2026-09-19T04:46:29.209Z DEBUG magic-context.perf: [harness=pi session=pi:01a0b7fc-bba1-7f60-aa2b-27c2df4481ab] transform stage stage=stickyReplayDecisions ms=0.0
2026-09-19T07:59:01.882Z ERROR engram.scheduler: capture halted after 3 consecutive failures gen=192 class=dedup_map_unreadable
2026-09-19T07:59:03.501Z DEBUG synapse.perf: job done model=qwen3-0.6b lane=decode tokens=12 ms=118
2026-09-19T07:58:59.004Z WARN  aft.index: [root=/Work/CortexKit/prefrontal] build fell back to full projection reason=journal_gap ms=4288
2026-09-19T07:59:02.113Z INFO  prefrontal-core.wake: [agent=agent_ab64ea0873bbee0c session=opencode:ses_0758f6ce7ffeJ0A9sV8Qvema7d] delivered lane=notice ms=22
2026-09-19T07:58:58.970Z INFO  subc.supervise: module reported non-ok health module_id=engram status=Degraded
```

Field by field, each with the reason it is where it is:

- **`<timestamp>`** — RFC3339, UTC, millisecond precision, `Z` suffix. Fixed
  width, lexically monotonic (MC's dashboard sorts on it). Never local time.
- **`<LEVEL>`** — one of `TRACE DEBUG INFO WARN ERROR`, padded to five. Fixed
  column so the eye can run down it.
- **`<logger>:`** — the **hierarchical logger name**, dotted, rooted at the
  module id: `engram`, `engram.scheduler`, `magic-context.perf`. This is
  Logback's `%logger`, Python's `getLogger(__name__)`, tracing's target. The
  trailing colon is the separator every framework uses; without it the name
  reads as the first word of the message. The module id is here **because it
  is the root of the hierarchy that `CK_LOG` filters on**, not as decoration —
  which is why it survived the argument that the file path already names it.
- **`[<bound fields>]`** — context bound to a scope rather than to an event:
  process-level (`harness=pi`) or scoped (`session=…`, `agent=…`, `root=…`).
  Bracketed because bound fields are **constant across a run of lines**, so the
  bracket has a stable width within any one scope and the message column stays
  aligned in the file you are reading. `]` is the unambiguous end of context; a
  parser lifts these without guessing. **Absent entirely when nothing is
  bound** — never `[]`.
- **`<message>`** — free text, positional. Positional over `msg="…"` because
  every line in this fleet is read by a human before it is read by a parser,
  and the machine consumers we have (two doctors, `ck logs`) key on the fixed
  columns and on message words, never on extracting the message as a field.
  The event is structured underneath (`tracing` / the TS twin's record), so a
  JSON renderer is a formatter swap if one is ever needed, not a call-site
  change.
- **`<event fields>`** — `key=value`, after the message, values with spaces or
  `"` double-quoted with `\"` escaping, newlines escaped as `\n`. Last because
  they vary from zero to eighty characters per line and a terminal wrap should
  cut them, not the message.

**One line per event, always.** The crate escapes `\n` and `\r` inside the
message and inside every value, so a record can never span two lines. This is
not a formatting preference: `tail`, `grep`, a merge-by-time and every doctor
read the file **per line**, and a record that is correct and complete across
two lines is two lines to all of them. BROCA measured it in their own capture
the day r2 was approved — a prefix rule satisfied per record left 8 of 29 lines
unattributable, and the fleet census read the continuation as a producer with
no id. Multi-line payloads (backtraces) are emitted as one line per source
line, each carrying the full prefix and a `logger` of `<module>.panic`; the
crate installs a panic hook that does this so a module's last words land in
its own file, not only the daemon's ring.

**Control characters are escaped per field, never stripped across the line.**
Each message and each value is handled on its own, before it is placed in the
line:

1. A complete terminal escape sequence inside that one field is removed: CSI
   (`ESC [` … final byte `0x40`–`0x7e`, or C1 `U+009B` …), and OSC (`ESC ]` or C1
   `U+009D` … ended by `BEL`, `ESC \` or C1 `U+009C`). A sequence that does not
   end inside the field is not a sequence and is not removed.
2. Every control character still left — `U+0000`–`U+001F` other than `\n` and
   `\r` (which keep those escapes), `U+007F`, and `U+0080`–`U+009F` — is written
   as `\u` plus four lowercase hex digits (`\u001b`, `\u0009`, `\u0085`).
   Backslash is escaped first, so a literal `\u0041` in the text renders as
   `\\u0041` and cannot be read back as an escape.
3. A value that contains any escape is quoted, like one containing a space.

The guard after a module's redactor escapes any raw control character the
redactor introduces the same way; it never strips. Stripping on the whole line,
as both twins did before `cortexkit-log` 0.3.3 and `@cortexkit/log` 0.2.1, lost
data: a stray `ESC ]` in one value began an
OSC that consumed the rest of that value, its closing quote and every field
after it, up to a terminator that never came. A rendered line therefore never
contains a raw control character, which is what the `ansi-in-line` parse
reject checks.

Nothing marks where the message ends and event fields begin. A message that
contains a literal `key=value` is ambiguous to a field parser. This is the same
trade `tracing`'s fmt layer and zerolog's console writer make, and it is
accepted for the reason above: the text line is the human lane.

### Bound fields

Two scopes, both required by the operator's ask (session ids for AFT and MC,
agent ids for prefrontal):

- **Process-level** — bound once at `init` and present on every line the
  process writes. For a plugin, `harness=<pi|opencode|omp|claude-code|…>` is
  mandatory: after r2 all of a module's lanes share one file, so the harness
  field is the only thing that separates them.
- **Scoped** — bound for a span of work and present on every line inside it:
  `session=<issuer>:<id>`, `agent=<agent_id>`, `root=<path>`. Rust: a
  `tracing::Span` with the fields as span fields (`session_span` already does
  this for session). TS: a child logger carrying the bindings.

**`session=` carries the whole id, always.** It is a match key against a
session store, not a display string, and its shape differs by issuer
(`01a0b7fc-bba1-7f60-aa2b-27c2df4481ab` for pi, `ses_0758f6ce7ffeJ0A9sV8Qvema7d`
for opencode). A truncated id is greppable against nothing. Issuer prefix as in
r1: `session=<issuer>:<id>`, absent when there is none, never a placeholder.

### Logger names replace tags

r1 declared a `tag=` vocabulary per module, orthogonal to level, so an operator
could raise `perf` without raising everything. r2 gets the same capability from
the logger hierarchy, which is how every framework does it:

```
CK_LOG=error                                errors only, fleet-wide
CK_LOG=error,magic-context.perf=info        errors, plus MC's perf lines
CK_LOG=warn,aft=debug                       everything aft, warnings elsewhere
CK_LOG=info,engram.gc=trace                 one component at trace
```

RUST_LOG grammar, unchanged; the TS twin implements the same grammar. A logger
name is `<module_id>` optionally followed by `.<component>` segments matching
`[a-z][a-z0-9-]*`. A level set on a name applies to it and every name beneath
it; the most specific match wins. **The bare module id is a valid logger** — a
module that names no components still gets `engram:` on every line and working
level control; it forgoes only sub-component granularity.

**Manifest declaration of logger names is NOT REQUIRED and cannot be expressed
today.** `ModuleManifest` carries no logger field and `subc-protocol` has no
builder method for one, so this paragraph previously stated a requirement no
adopter could satisfy (ASTRO, 2026-09-19, reading the spec and the protocol
source together rather than either alone). A normative line nobody can meet has
one end state: each adopter works out privately that it is optional and skips
it, until the spec says one thing and twenty repos do another and the next
reader cannot tell a requirement from a residue.

The intent stands as a FUTURE affordance: if `subc-protocol` grows a logger
field, a module may declare its names so `ck logs <id> --loggers` can list them
and `ck daemon lint` can check them. Until then, adopt without declaring.
Runtime never depended on it either way — the crate does not refuse an
undeclared name (a refusal inside a logging call is a worse failure than an
undeclared name), which is one of the three doors the operator confirmed.

## Files

```
<module data dir>/logs/<module_id>.<YYYY-MM-DD>.log     one segment per UTC day, all lanes
~/.local/share/cortexkit/run/logs/subc.<YYYY-MM-DD>.log  the daemon's own log (subconscious only)
~/.local/share/cortexkit/run/logs/<module_id>.stderr.log daemon capture of stray stderr/stdout
```

- **Capture lines carry the daemon's capture time.** Each line of
  `<module_id>.stderr.log` is `2026-09-19T07:04:00.685Z ` (RFC 3339 UTC,
  milliseconds, `Z`, one space; 25 bytes) followed by the module's bytes
  verbatim. The stamp is taken when the daemon frames the line off the pipe,
  the same instant `supervisor.stderr_tail` reports as `at_ms`, and has the
  form the daemon's own log lines begin with. A line the module wrote in
  several pieces is reassembled first and carries one stamp; a line longer
  than the reader's 1 MiB reassembly ceiling is split into file lines, and
  each of those starts with its own stamp. Lines written by a daemon from
  before this carry no stamp, and a reader must leave them undated rather than
  assign one: `ck module logs` shows them after every dated line.

- **One file per module per day.** r1 split plugin lanes into
  `<module>.<harness>.log` because a plugin runs inside the harness process and
  cannot share a file handle across a rename-based rotation. r2 has no rename
  (below), so the plugins and the module all append to today's segment and
  `harness=` on every plugin line is what tells them apart. A user debugging MC
  reads **one timeline**, not four files to reconstruct one.
- **Date-stamped, never renamed.** Every writer computes today's segment name
  from the UTC clock and opens it `O_APPEND | O_CREAT`, one `write(2)` per
  framed line (the crate already does this; the comment at the site says why).
  Concurrent appends from any number of processes land at line boundaries. A
  writer re-derives the name on every write and reopens when the day rolls;
  the clock is the only coordinator and it needs none. Yesterday's segment
  becomes immutable at midnight by construction.
- **No size cap within a day.** This is the cost of never renaming and it is
  accepted: a segment that grows past a threshold (default 256 MiB) is an
  **alarm** — the crate reports it once through the same first-failure-to-
  stderr channel it uses for swallowed writes, and `fleet-pulse` reads
  oversized segments as a finding — because a module writing that much in a
  day has a defect worth surfacing, and truncating it would hide the defect.
- The data dir is `module_data_dir(module_id)` from `cortexkit-store-types` /
  `@cortexkit/store` — the crate takes the module id and resolves the path;
  callers never assemble it (the doubled-path incident).
- Files `0600`, directory `0700` on Unix; per-user ACL inherited on Windows.
- **Nothing a module writes lands in `subc.*.log`.** The daemon's log carries
  supervisor events, health transitions, route drops — all stamped
  `module_id=<id>` as an *event field*, because there the module is the subject
  of the line, not its emitter. Two daemon-side residues stay on purpose: the
  stderr crash ring (`supervisor.stderr_tail`; a panicking module cannot write
  its own log, and the backtrace is the line worth having) and
  `run/logs/<id>.stderr.log`, the daemon-owned capture of anything a module
  still emits on stderr/stdout. **A module on the crate writes nothing there,
  so an adopted module's capture containing a line THAT BUILD COULD EMIT is a
  defect, and `fleet-pulse` reports it.**

  NOT "non-empty" — that check flags every adopted module forever, because
  NOTHING TRUNCATES THESE FILES: the pre-adoption history stays on disk and the
  file keeps its last-minutes-before-the-bounce growth across the placement
  itself. PLEX measured exactly that (2026-09-19): their capture GREW 28,243 ->
  28,478 across adoption and contained zero lines the new build was capable of
  producing. A FILE THAT GREW IS NOT A FILE BEING WRITTEN.

  The discriminating question is what SHAPE the lines are. After r2 the cheap
  form is "capture contains an r2-shaped line" (timestamp, level, `module[.component]:`),
  which reads 0 for an adopted module whose history is r1. Where a seat retired
  a hand-rolled prefix, the absence of that literal from the new image makes the
  old lines structurally unproducible, which is a stronger discriminator still —
  and a grep for the retired literal is the control that proves the search works
  rather than that the file is empty.

  The capture keeps its r1 rename-based rotation because it has exactly one
  writer.

## Retention

Per-module policy, writer-executed, no central authority — with the reasons.

**Segments are not backup-class.** They are regenerable, bounded by the
retention window, and can grow to the alarm size in a day. A module whose
engram backup descriptor names its store FILE (`whole-db`, `path: "store.db"`)
already captures nothing under `logs/`, by construction rather than by
exclusion: naming one file cannot sweep its parent. A DIRECTORY-shaped
descriptor would capture segments, so a module writing one must exclude
`logs/` explicitly. Stated here because the seat writing a descriptor in six
months will not have read the thread this came from (THALAMUS, ASTRO, ENGRAM,
2026-09-19).

**Wherever two components must agree on a path, one resolves it and the other
is told.** A module's segment directory derives from the data directory the
daemon hands it at HELLO (or the state directory a rig points it at), never
from a second resolution of the same environment: a re-derived path reads as
equivalent and is the mechanism behind every isolated instance that appended
to the operator's segment this week. ENGRAM's descriptor path holds the same
invariant from the other end.

- **Policy is per-module** because volumes differ by orders of magnitude
  (engram's GC walk at `progress` verbosity versus claustrum's audit-adjacent
  events versus synapse perf). It lives in the one place the daemon already
  reads at spawn:
  ```jsonc
  "modules": { "broca": { "program": "…", "log": { "level": "info",
                                                    "loggers": { "broca.perf": "debug" },
                                                    "max_age_days": 14,
                                                    "alarm_segment_mb": 256 } } }
  ```
  Defaults when the key is absent: `info`, no per-logger overrides, 14 days,
  256 MiB. Standalone MC/AFT read the same keys from
  `.cortexkit/<module>.jsonc log.*`; env wins, as for every other knob.
- **Execution is by the writer, at process start and at each day roll.** With
  date-stamped names, pruning is a **string comparison on the filename** — no
  `stat`, no mtime, no generation renaming: `magic-context.2026-08-20.log` is
  older than the window, unlink it. A lost unlink race returns `ENOENT`, which
  is the correct outcome. It is cheap enough to run unconditionally and it is
  the only shape that works when the daemon is down or absent.
- **No daemon sweeper.** Module data dirs are module-owned; the daemon writes
  only under `run/`. That boundary is what keeps a daemon defect from deleting
  a module's data, and a background task unlinking inside `engram/logs/` would
  cross it for a job the writer does correctly. The one case the writer cannot
  cover — a retired module that never starts again — is an **operator verb**
  (`ck logs prune`, later), where the operator owns everything and no boundary
  is crossed. Measured 2026-09-19, orphaned log directories on this host are
  negligible.
- `max_age_days` bounds the set independently of rate; the segment alarm
  bounds the rate's visibility. Rate-driven loggers (upload progress, walk
  slices) ship at `debug` or below in the module's own baseline; a module must
  not put a per-object line at `info`.

## Redaction

The sink takes a `Redactor` (a `fn(&str) -> Cow<str>` on Rust, a
`(line: string) => string` on TS) applied to every complete line before the
write. The fleet default redacts, and both twins pin it with the fixture's
`redaction` section:

- `Authorization:` values, `Bearer …`, JWT-shaped tokens, `ckh_` handles and
  `sk-` keys;
- GitHub tokens: `ghp_`, `gho_`, `ghs_`, `ghu_`, `ghr_` and fine-grained
  `github_pat_`;
- URL userinfo, `scheme://user:pass@host` or `scheme://token@host`, to
  `scheme://[REDACTED]@host`;
- credential query parameters (`access_token`, `token`, `api_key`, `apikey`,
  `password`, `secret`, `client_secret`) after `?` or `&`, value only. A generic
  `key=value` rule misses `?access_token=` inside a URL because `https:` matches
  as the key, which is why the query rule is its own pattern.

A module composes its own on top (MC's sanitizer; claustrum's hand-written redacting
`Debug` impls remain the first line of defence). A module MUST NOT log prompt
text, message bodies, or credential payloads at any level; the redactor is the
backstop, not the policy.

## Observability of the logger itself

- `swallowed_writes` counter (write failed, line dropped) readable by the module
  for its health report; the crate reports the first failure per process to
  stderr once, never per line (a full disk must not generate a second flood).
- `oversized_segment` reported the same way, once, when today's segment crosses
  `alarm_segment_mb`.
- Rust: when the file sink cannot be opened, the crate falls back to stderr so
  the daemon's capture still exists; the fallback is announced in the first
  line.
- **A feature that fires later must log that it armed** (retention prune at
  start: `logger=<module> retention pruned=N kept=M window_days=14`), so
  "never ran" and "ran and found nothing" are distinguishable from outside.

## Why one line per record is a hard requirement

Attribution is satisfied per-record and the log is read per-line. Any format
where those two can diverge — a continuation line carrying part of a record
without its timestamp, level, or module — produces a census that is right about
lines and wrong about producers. Measured on 2026-09-19: a `tail -1` sample of
the shared stderr capture landed on a continuation line and recorded a module as
emitting no identifier at all, when every one of its records carried one. Under
r2 the logger writes module and component on each line by construction, so a
record that wraps still carries them; the property is structural rather than
maintained.

The cheap test for any log format: ask what a continuation line carries, not
what the record carries. A format where a record can span more than one line
needs either a reader that reassembles or a writer that cannot wrap. Nobody
builds the first, so the second is the only real option.

## Adoption

### Readers, not just producers

**Any change to this format carries a READERS line beside its producer roster,
or it is not ready to post.** The r2 adoption call ([#520]) named producers and
no readers, because an adoption call names who must ACT — and the consumers of a
format do nothing until the day they break, which makes them invisible in
exactly the notice that would have warned them (MC's framing, 2026-09-19, after
their drift detector caught a re-vendor that would have shipped a broken
parser).

Known readers of this format, to be named in any future change:

- `aft` — CLI doctor, extracts errors into GitHub issues
- `magic-context` — CLI doctor (`packages/cli/src/lib/log-lines.ts`) AND the
  desktop dashboard's Rust twin
- `ck module logs` — reads r1 and r2 both, deliberately
- anyone grepping, tailing or regexing `<module>.<date>.log`

**r1 and r2 lines coexist on disk in the same directory for the whole adoption
window, so a reader needs both arms rather than a cutover.** The cheapest
discriminator is the colon: r2 renders `fusiform: poll changed`, r1 renders
`fusiform poll changed`. A reader taking both arms must be tested against a
MIXED file, since a per-grammar test passes while one arm quietly consumes the
other grammar's lines.

**Mandatory, fleet-wide.** Every module and every plugin lane, as each seat has
time; a seat that cannot adopt names the blocker on the record. The census
question posted 2026-09-19 ([#512] in `#fleet-notices`) collects each seat's
reason before the wave, so a crate gap is fixed in commons rather than
discovered ten times.

- **Live-user modules (aft, magic-context) move only with their doctors.**
  Both have CLI doctors that parse today's paths and formats to extract errors
  into GitHub issues. For them: the doctor learns the new path and format
  first and reads BOTH for at least one release; adoption ships in the same
  release as the doctor; the old `$TMPDIR` plugin log and pid-suffixed file are
  read-only compatibility inputs, never written again after the cut. Nothing
  else in the fleet waits on them.

  Measured doctor contracts (2026-09-05), which the r2 format satisfies without
  special cases: both doctors are line-regex over free text, keyed on message
  words (`failed:`, `Error:`, `EMERGENCY`, `exception`, `crashed:`,
  `panicked at`, `timed out after <n>ms`) that the module's message text keeps;
  neither parses timestamp, level, or fields. Session filters widen to accept
  `session=<issuer>:<id>` inside the bracket beside the bracketed form for one
  release, with the raw `ses_…` / uuid preserved as the id. AFT's telemetry
  parsers anchor on the message (`index_event kind=… root=…`, `perf tick:`,
  `slow tool_call …`) and require the `key=value` tail unquoted — satisfied
  because AFT sanitises those values (no spaces, no `=`) and the format quotes
  only values that contain a space, `"` or newline. MC's dashboard sorts
  lexically on the timestamp capture; RFC3339 with `Z` and fixed millisecond
  width is lexically monotonic. Owners re-key error extraction on `<LEVEL>`
  once levels exist (their migration, not a format requirement).

  Consumers pin their parsers against the authority fixture rather than
  against sample lines.
- **A module whose diagnostics are not on stderr** (thalamus: durable
  failure-state JSON plus a per-turn decision log keyed by `exchange_id`) still
  adopts for the lines it does emit, and says so in its census row. The crate
  does not replace a structured instrument; it replaces `eprintln!`.
- `logs/` under a module data dir is never captured by engram, as an
  engram-wide rule independent of descriptors (rotation churn would otherwise
  be a fresh upload every capture); the status surface says it was excluded.
- Every other module: replace `eprintln!` with the crate; the census items
  each owner took (broca session ids on every line, insula heartbeat only on
  change, thalamus/astrocyte/synapse prefixes, synapse worker rings forwarded,
  callosum crate-path targets, prefrontal's uncapped plugin log) are done in
  that same change because the crate makes them the default shape.
- The daemon: `subc.log` becomes `subc.<date>.log` through the same crate;
  `CK_LOG`, `loggers`, `max_age_days` and `alarm_segment_mb` injected at spawn
  from `subc.jsonc`; the `module_id` prefix leaves the daemon's own lines and
  becomes the logger root `subc.<component>`.

### Ergonomics, because they decide adoption

Today `eprintln!` needs nothing and the crate needs a `Config`, an `init`, and a
kept `Handle`. That gradient is backwards and it is most of why ten seats linked
without routing. r2 requires the crate to offer a **zero-argument path** for a
supervised module: the daemon already injects `CK_LOG` at spawn and already
injects `SUBC_MODULE_ID` (`supervise.rs:3936`, for launch attestation), so
`cortexkit_log::init_from_env()` resolves module id, data dir, level and
retention with no arguments. Nothing new is injected for this; the crate reads
what is already there. The retention and per-logger keys ride in `CK_LOG`'s
siblings (`CK_LOG_MAX_AGE_DAYS`, `CK_LOG_ALARM_SEGMENT_MB`), which the daemon
derives from the `log` block — r1 named the `Retention` struct but no env
contract, which is what stopped the daemon injecting it. A plugin, which has no daemon, passes its module
id and harness and nothing else.

## `ck logs` — deferred

Built once the fleet is on one format, not before; a renderer over six formats
is a renderer with six special cases. Shape when it comes:

```text
ck logs [<id>] [-n <lines>] [-f] [--since <dur>] [--logger <name>] [--level <l>] [--lane module|<harness>|stderr|daemon] [--json]
```

Globs `<module>.*.log` and merges by timestamp; `-f` notices a new segment at
midnight; `--lane` filters on `harness=` rather than on file. The r1 `ck module
logs` verb keeps working over the capture and daemon files until then.

## Out of scope

Per-request joins across a module's data sinks (thalamus `exchange_id`);
shipping logs anywhere; live level reload; a JSON render lane (a formatter swap
if ever needed, not a format change).
