# Fleet checks

`fleet-pulse.sh` runs on a cadence; everything else here runs when something
prompts it.

| script | question |
| --- | --- |
| `fleet-pulse.sh` | which seats are idle, which modules are unhealthy |
| `ci-redness.sh` | how long each repo's default branch has been failing CI |
| `check-repo-protection.sh` | which repos have no working off-machine copy |
| `reap-orphan-lsp.sh` | which language servers outlived their project root |
| `verify-running-image.sh` | is each module running the binary on disk |
| `spec-shrink-census.ts` | which spec sections lost content between rounds |
| `prod-body-coverage.sh` | how much of each file a test-excluding scan reads |
| `fleet-idle.ts` | per-seat idleness from the authoritative op |

These refuse to report rather than returning a clean result when their own
instrument cannot produce a positive answer. That property is the point of them:
each was written after a survey returned a confident, wrong, reassuring number.

`fleet-pulse.sh` answers one question on a fixed cadence: which seats are idle,
which modules are unhealthy, and is anything stranded. Run it bare; it takes no
arguments.

```
./scripts/fleet/fleet-pulse.sh
```

## Where the numbers come from

Per-seat idleness comes from alfonso-core's `projects.overview`, not from peer
message traffic. That distinction is the whole reason `fleet-idle.ts` exists.

An earlier version measured minutes since a seat's last outbound message, which
tracks *talking* rather than *working*. A seat heads-down on a long build talks
to nobody, so it reported as hours-idle while it was the busiest thing running —
and dispatching on that reading sends work to seats that are already loaded
while genuinely free ones sit untouched. Measured against the authoritative op,
the proxy was wrong by hours: one seat read 5h53m idle against a real 6m.

`projects.overview` reports turn-boundary activity, and its roster is keyed by
session id, so fleet renames cannot leave ghost rows behind the way a
sender-name table does.

## What it cannot tell you

It distinguishes idle from busy. It does **not** distinguish idle-and-fine from
idle-and-stuck: a wedged seat with no live work reads exactly like a free one.
Treat a high idle number as "look here", never as "this seat is broken".

Before handing an idle seat new work, read its `awaiting settle` count. A seat is
often idle *precisely because* it delivered something nobody merged, and new work
buries the old — that column is review debt owed to the seat, not spare capacity.

A seat blocked on a genuine dependency chain also reads as idle, and is the one
case where dispatching around it is actively wrong: if the next slice consumes
the property the blocking slice is repairing, starting it early builds on the
thing under repair. Ask the seat before treating a long idle number as capacity.

Per-seat expected cadence is also unsolved; baselines drift with the kind of work
a seat is doing, so there is no threshold here to tune.

## Campaign rows undercount completed work

The campaign lines come from terminal status, and a campaign whose slices were
recovered by standalone re-dispatch stays `cancelled` while its work sits merged
on main. The row is not wrong — it is accurate about its own lifecycle and
silent about work that completed outside it.

So a terminal campaign is not evidence that work was lost, and a rollup
undercounts exactly the epics that had trouble and recovered, which are the ones
most worth auditing. Read completion slice by slice, and check for a superseding
re-fire before treating any terminal as stranded work.

## check-repo-protection.sh: probe the remote, do not read the config

`git remote -v` renders identically for a working remote and a tombstone. The
config records an intention; only `ls-remote` establishes the fact.

A config-only survey run here classified a 1,465-commit repo as protected. Its
remote was configured to a GitHub repository that does not exist, so every push
anyone believed was happening had been failing silently.

Three states, and the first is deliberately louder than the second because it is
the one that masquerades as safe:

- **DEAD REMOTE** — configured, unreachable. Believed protected, is not.
- **NO REMOTE** — nothing configured. Visibly unprotected.
- **UNPUSHED** — reachable, commits ahead. Protected but stale.

The third is the quietest and was invisible to both earlier passes: a repo with a
working remote passes a presence check and a liveness check while holding 83
unpushed commits. Only the ahead-count sees it.

The script refuses to report if *no* remote anywhere is reachable, because in
that case every unreachable result is a statement about this machine rather than
about any repository — the failure where an error blamed on your own environment
stops being investigated.

A repo with no remote is not automatically unprotected: two here are superseded
husks whose history was grafted into another workspace and verified as ancestors
of its HEAD. Pushing those would create a second lineage. Ask the owner before
treating an unprotected reading as work to rescue.

## reap-orphan-lsp.sh: language servers that outlived their root

Worktrees get reclaimed while a language server is still indexing them. The
server is never told, so it holds its whole index in RAM forever. Two sweeps on
one night each recovered roughly 40 GB.

It classifies by whether a process's working directory still exists, and runs a
positive control first: if the probe cannot return a live path for any process,
the probe is broken and every "orphan" is a null rather than a finding. It also
matches on the executable rather than a command-line substring — `pgrep -f
<name>` matches the script itself, and in a reaper that is a self-kill.

This is a stopgap. The durable fix is the owning tool reclaiming a root when its
directory disappears.

## Consume the op, not the tables

`fleet-idle.ts` calls `projects.overview` rather than querying alfonso-core's
database. The schema belongs to alfonso-core and is theirs to change; a direct
query would rot silently the first time it did.
