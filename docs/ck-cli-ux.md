# `ck` operator CLI — UX treatment

Status: S1–S3 landed and driven on the macOS alpha VM; the after transcript is `docs/evidence/ck-cli-walk-2026-09-04-after.txt`. Normative for the alpha CLI. Written from a full walk of every verb on a
fresh macOS alpha VM (released 0.17.5, all five modules installed) on
2026-09-04, after the operator reported the mechanical voice and the
module-binaries-as-domains defect. The captured "before" outputs live in the
walk transcript referenced by the commits that implement this note.

## Who reads `ck`

A person at a terminal who installed CortexKit ten minutes ago, or who runs it
daily and is checking whether something is wrong. Not the daemon's author.
Every line `ck` prints is read by that person; the machine-readable form is
`--json` and nothing else. The planner, the validator, the linter and the
triage forensics are instruments `ck` uses; their vocabulary does not reach
the terminal unless the person asked for it (`--verbose`, `--dry-run`,
`--json`, `ck daemon triage`).

## Principles

1. **Say what happened, in the order the person cares about.** Verdict first,
   detail after. `Nothing to change.` before the list of things that were
   already right; the failing line before the twenty that passed.
2. **Steps are for plans, not for results.** A numbered plan renders in
   `--dry-run`, or when a run has something to apply. A run that changed
   nothing prints one line.
3. **No instrument vocabulary.** "observe alpha platform support",
   "outcome: no-op", "mutation(s) planned", "validate with ck daemon triage",
   "vacuity floor", "manifest_unparsable", "help[2]:", `Some(36)`,
   `{"age_seconds":…}` — none of these are sentences a person would say.
4. **A number is a unit and a scale.** `1788487030621` is never printed; it is
   `4h ago` or `2026-09-04 01:57`. Bytes are `11.4 MiB`. Milliseconds under a
   second are `62 ms`.
5. **Absence has one word.** `-` in a table cell, `none` in a sentence, never
   `null`, `-` and `{}` in the same screen.
6. **Only real commands are commands.** A `ck <name>` that execs a module
   binary is a defect. Domains opt in.
7. **Errors name the fix.** One sentence about what is wrong, one about what
   to run. No usage dump after an error unless the error was a usage error.
8. **Booleans are words the person uses.** `enabled`/`disabled`,
   `running`/`stopped` — never `true`/`false` in a table.
9. **Detail is behind `--verbose`, never removed.** Every metric that exists
   today stays reachable; it stops being the default.
10. **The operator's file is the operator's.** `subc.jsonc` is edited by
    textual insertion; comments and formatting the person wrote survive
    byte-for-byte. A "diff" shows the inserted lines, not two copies of the
    file.

## Defects found in the walk (fix before voice)

| # | Where | What | Fix |
|---|---|---|---|
| D1 | bare `ck`, `ck --help` | `aft`, `claustrum`, `insula`, `subc`, `subc-mcp` listed as domains; `ck aft` execs the module binary (it started AFT in stdio mode and shut it down on stdin close) | Domains opt in: a `ck-<name>` binary is a domain only if `ck-<name> --ck-domain` prints one headline line and exits 0 within 2 s. Non-domain `ck-*` binaries are never listed or dispatched; `ck aft` → `'aft' is a module, not a command. Try: ck module status aft` (exit 64). Domain probe results are cached beside the update cache, keyed by (path, mtime, size) |
| D2 | `ck setup <module>` | `subc.jsonc` re-serialised through serde: every comment in the file is dropped (the claustrum `reserved` rationale included) | Textual insertion into the existing JSONC; parsed-value comparison stays for conflicts; test: a file with comments on every line gains one module entry and is otherwise byte-identical |
| D3 | `ck setup <module>` | the "diff" prints the whole file twice under `-`/`+` | Real unified diff of the change (only the inserted/changed lines with 3 lines of context), rendered only in `--dry-run` or before a prompt; on apply, one line: `configured magic-context in ~/.config/cortexkit/subc.jsonc` |
| D4 | `ck setup`, `ck setup <module>` | `ck daemon triage` and `ck health` full output spliced into the run | Validation is silent on success; on failure it prints the failing check's verdict line and `ck daemon triage` for the forensics |
| D5 | `ck fleet lint` | operator-visible, examines 0 of 3 (modules do not emit `--manifest`), wording is the linter's | Leaves the operator help. Reachable as `ck daemon lint` for CI; output rewritten: `checked 0 of 3 configured modules — aft, claustrum, insula do not expose a manifest` |
| D6 | `ck setup <module>` on a network failure | `could not download <url>: curl: (35) LibreSSL SSL_connect …` reads as a broken release | `could not reach GitHub to download ck-mc (network error); nothing was installed. Retry: ck setup mc` — the curl text goes to `--verbose` |
| D7 | bare `ck` | `alerts: aft` on a module in its first probe window (fixed in 0.17.6, ships with 0.17.8) | done |

## Target output, per verb

Bare `ck`:

```
ck 0.17.8 · daemon running (pid 6257, up 4h, 4 clients)
modules: aft ok · claustrum ok · insula ok
updates: none

commands: setup · upgrade · module · health · routes · quota · daemon · auth · projects · workspaces
run `ck <command>` for its verbs, `ck --help` for everything
```

- `bin:` moves to `--verbose` (it exists to catch a stale PATH `ck`; print it
  only when the running daemon's version differs from `ck`'s, as the skew
  warning already does).
- `updates:` reads `none`, `ck-aft 0.55.1 → 0.55.2 (run ck upgrade)`, or
  `unknown (could not reach cortexkit.io)`. Cache age is `--verbose`.
- Domain descriptions in `--help` come from the same `--ck-domain` line.
- The `help[N]:` footer becomes `next:` with the same lines; drop the count.

`ck setup` (nothing to do):

```
CortexKit is set up: daemon running, aft · claustrum · insula ok.
Optional modules not installed: mc, synapse — run `ck setup mc` or `ck setup synapse`.
```

`ck setup mc` (apply):

```
Installing magic-context (ck-mc-alpha.22464bf2, darwin-arm64)
  downloaded and verified ck-mc (12.1 MiB)
  placed ~/.local/share/cortexkit/bin/ck-mc
  configured magic-context in ~/.config/cortexkit/subc.jsonc
  registered with the daemon; magic-context ok
Done.
```

`ck setup mc --dry-run`: the same lines with `would` and the unified diff of
the config insertion.

`ck setup synapse` when the release lacks the platform:

```
synapse has no darwin-arm64 release yet; nothing was installed.
```

`ck upgrade` (nothing to do): `Everything is up to date (ck 0.17.8 · ck-subc 0.17.8 · ck-subc-mcp · ck-aft 0.55.1).`
`ck upgrade --check` with an update: `ck-aft 0.55.1 → 0.55.2. Run ck upgrade.`
`ck upgrade` (apply): one line per binary as it is replaced (`upgraded ck-aft 0.55.1 → 0.55.2, restarted`), `ck` last.

`ck module list`:

```
module     status   health
aft        running  ok
claustrum  running  ok
insula     running  ok
```

- `enabled`/`live` fold into `status`: `running`, `stopped` (disabled),
  `starting`, `failed (exit 1, 3 restarts)`, `restarting`.

`ck module status aft`: a key/value block, not a one-row table:

```
aft — running, healthy
  pid 6291 · started 4h ago · restarts 0 of 3
  last exit: none
  binary: ~/.local/share/cortexkit/bin/ck-aft (running image matches)
metrics: run `ck health aft`
```

`ck module terminals aft`: `no exits recorded since the daemon started (4h ago)`; with records, a table `when · exit · disposition`.

`ck module rescan --dry-run`: `no changes: 3 modules match subc.jsonc`; with changes, a list by kind (`would add: mc` / `would remove: …` / `needs daemon restart: storage`).

`ck health`: unchanged shape, but the insula detail line becomes what a person
reads: `1 provider degraded (antigravity); 36 not configured`.

`ck health <id>`: the status line plus at most the headline metrics the module
marks as such (`snapshot_age`, `open_routes`, `roots`); the full tree is
`--verbose`. Timestamps rendered as ages.

`ck quota`: drop the `Usage` header; `Antigravity — not running locally`;
the 36-provider list stays under `--verbose`.

`ck daemon`: `daemon 0.17.8 · pid 6257 · up 4h · 4 clients · no frame drops in the last 10 minutes`; the counter table is `--verbose`.

`ck daemon triage`: verdict first (`daemon appears live: connection file present, pid 6257 alive`), then the evidence in the same order as today with ages instead of raw metadata JSON.

`ck provenance aft`: sentence case headers; `started 4h ago`; `running image matches the file it was spawned from`.

`ck routes` with none: `no live routes` (no footer).

Help text rewrites (jargon out): `ck setup --help` "without calling an installation mutator" → "without changing anything"; `ck upgrade --help` "MC is wiring-only in alpha" → drop; `ck module release <id>` "retire a removed module's retained reserved-id gate" → "forget a removed module's reserved id so another module may use it".

Errors: `ck module status nosuch` → `no module named 'nosuch'. Run ck module list.` (one line, exit 1). `ck health nosuch` → same. `ck nosuchverb` → `unknown command 'nosuchverb'. Run ck --help.` (no usage dump).

## Out of scope for this treatment

`ck auth` (claustrum's CLI; findings relayed to its owner: `Some(36)` Debug
leak, keychain-locked-over-SSH remedy already landed), `ck projects` /
`ck workspaces` (entorhinal; not an alpha component), `ck release`.

## Slices

- S1 — defects D1–D6 (domain opt-in, comment-preserving config edit + real diff, silent validation, network refusal text, `fleet` → `daemon lint`).
- S2 — setup/upgrade voice per the targets above.
- S3 — dashboard, module, health, daemon, provenance, routes, quota rendering and help text.

Each slice is driven on the VM against the walk transcript before it merges;
the transcript is re-captured after S3 and attached to the closing commit.
