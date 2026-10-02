# @cortexkit/log

The TypeScript fleet logger for harness-hosted CortexKit plugins and clients. It
writes the canonical line format and the module-owned day segments specified in
`docs/specs/fleet-logging.md` (r2), and it is the twin of the Rust
`cortexkit-log` crate: both render every case of
`crates/subc-core/tests/fixtures/log_format_golden.json` byte for byte.

```ts
import { createLogger, forPlugin } from "@cortexkit/log";

const log = createLogger(forPlugin("magic-context", "opencode"));

log.info("plugin started");

const session = log.child("historian").withSession("pi", "01a0b7fc-bba1-7f60-aa2b-27c2df4481ab");
session.info("trigger fired", { reason: "force_band", usage: "93.9%" });
```

## Installing and developing

The published package contains compiled ESM and declarations for Node 18+ and
Bun. For local development, build `../store` after installing its dependencies,
then run `npm install --no-save --package-lock=false ../store` here when testing
an unpublished store version. Consumers of the tarball resolve the versioned
npm dependency.

## The line

```
<timestamp> <LEVEL> <logger>: [<bound fields>] <message> <event fields>
```

```
2026-09-19T04:46:29.432Z INFO  magic-context.historian: [harness=pi session=pi:01a0b7fc-bba1-7f60-aa2b-27c2df4481ab] trigger fired reason=force_band usage=93.9%
2026-09-19T07:59:01.882Z ERROR engram.scheduler: capture halted after 3 consecutive failures gen=192 class=dedup_map_unreadable
2026-09-19T07:58:59.004Z WARN  aft.index: [root="/Users/x/My Project"] build fell back to full projection reason=journal_gap ms=4288
```

- RFC3339 UTC with milliseconds and a `Z`; `LEVEL` padded to five columns.
- The logger name is dotted and rooted at the module id, and it always ends in
  a colon. `child("perf")` appends a component; each segment matches
  `[a-z][a-z0-9-]*`.
- Bound context renders in a bracket before the message: process-level fields
  first in config order, then scoped fields, an inner scope overriding the same
  key in place. The bracket is **absent** when nothing is bound, never `[]`.
- `session=` carries the whole `<issuer>:<id>`, never truncated.
- A value containing a space, `"`, `]`, or a line break is double-quoted with
  `\\`, `\"`, `\n`, `\r` escapes; an empty value renders as `""`. An unquoted
  value is verbatim.
- One line per event, always: the message and every value escape their line
  breaks, and no raw control character survives, including through a caller's
  redactor.
- Control characters are handled per field, never across the line: a complete
  CSI or OSC sequence inside one message or value is removed (and counted in
  `stats().ansiStripped`); every control character left over is written as
  `\uXXXX` and forces quoting. A stray `ESC ]` therefore stays visible in its
  own value instead of swallowing the fields after it. The guard after a
  caller's redactor escapes the same way and never strips.

## Levels

`CK_LOG` uses `RUST_LOG`'s grammar over the logger hierarchy, so a directive
names a dotted prefix on segment boundaries: `aft=debug` covers `aft` and
`aft.index` but not `aftershock`, and the most specific directive wins.

```
CK_LOG=error                                errors only
CK_LOG=error,magic-context.perf=info        errors, plus one component's metrics
CK_LOG=warn,aft=debug                       everything aft, warnings elsewhere
CK_LOG=off                                  nothing
```

An empty or unset value is `info`. A malformed value falls back to `info` and
is reported once on stderr.

## Files

```
<module data dir>/logs/<module_id>.<YYYY-MM-DD>.log
```

One segment per module per UTC day, named by the day of each write and **never
renamed**. Every process that logs for a module — the module and each harness
lane — opens today's segment with `O_APPEND` and issues one synchronous write
per line, so they share one timeline and `harness=` is what tells the lanes
apart. The writer reopens when the UTC day rolls.

At open and at each day roll the writer unlinks segments whose **filename**
date is strictly older than `today - maxAgeDays` (default 14; the boundary day
is kept), never deciding by stat, and leaves any name that is not exactly
`<module_id>.<YYYY-MM-DD>.log` alone. It announces a prune on stderr so that
"never ran" and "ran and found nothing" stay distinguishable. There is no size
cap within a day: a segment past `alarmSegmentMb` (default 256) is reported
once on stderr and never truncated.

## Config

```ts
interface LogConfig {
  moduleId: string;
  logsDir?: string;          // default: <module data dir>/logs
  bound?: Array<[string, string]>; // process-level, rendered first, in order
  spec?: string;             // default: process.env.CK_LOG
  maxAgeDays?: number;       // default 14, or CK_LOG_MAX_AGE_DAYS
  alarmSegmentMb?: number;   // default 256, or CK_LOG_ALARM_SEGMENT_MB
  redact?: (line: string) => string;
  clock?: () => Date;
}
```

`forPlugin(moduleId, harness)` is that config with `harness=` bound. A module
MUST NOT log prompt text, message bodies, or credential payloads at any level;
the built-in credential redactor is the backstop, not the policy. It covers
`Authorization:` values, `Bearer` tokens, JWTs, `ckh_` handles, `sk-` keys,
GitHub tokens (`ghp_`, `gho_`, `ghs_`, `ghu_`, `ghr_`, `github_pat_`), URL
userinfo (`https://user:pass@host`), and credential query parameters
(`access_token`, `token`, `api_key`, `apikey`, `password`, `secret`,
`client_secret`).

Also exported for consumers that read the files: `formatLine`, `parseLine`,
`segmentName`, `segmentDay`, and `pruneCandidates`.
