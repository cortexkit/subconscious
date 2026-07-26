# Prefrontal rename — subc's side

The alfonso module is being renamed to prefrontal. This file covers only what
subconscious owns: the consumer strings in this repo, and the daemon operations
that carry the change. ALF owns the module itself, its stores, and the
announcement.

Written before the window rather than during it, because the useful half of a
runbook is the part that records what must be true BEFORE each step, and that is
exactly the part nobody writes while the daemon is stopped.

## What makes this a flag-day

The daemon's registry keys modules by id in a flat map and REJECTS a duplicate
active id rather than replacing it — a deliberate choice so a reconnecting module
cannot hijack in-flight routes. So one process cannot advertise both names, and
there is no window where both resolve. Verified at source in `registry.rs`.

Consequence for ordering: every consumer must ship the new string BEFORE the
module changes identity. A consumer on the new string during the transition
retries `unknown_module` in place and recovers; a consumer left on the old string
fails permanently the moment the flip lands.

## The apps are the gate, not the slowest consumer

Four of this repo's live call sites are in the Swift and gpui apps, where the
module id is a literal compiled into the binary. There is no config path for it.

So "consumers first" does not mean a merged commit — it means a REBUILT AND
INSTALLED app on every device that runs one. A phone on the old build cannot be
repointed without reinstalling. That sets the window, and it is the reason the
apps gate rather than follow.

## Before the window

1. Ufuk present, daylight, box not under heavy build load.
2. Apps rebuilt and installed from the rename branch. Confirm the BINARY, not the
   commit — a merged commit is not an installed one.
3. `git merge-tree --write-tree master rename/prefrontal-consumer-strings` exits
   0. Checked 2026-07-27 and clean, but master moves.

## Establishing which config the daemon will read

The daemon does not publish its config path on `server.describe` (checked: the
fields are capabilities, connected_clients, counters, op, protocol_ver,
subc_ops). So it must be derived from the RUNNING PROCESS, never from the
operator's shell — those are two rules selecting one subject, and they agree
until someone runs a daemon with a non-default config, which is exactly the
ckdev-rig case.

    pid=$(launchctl print gui/$(id -u)/cortexkit.subc | sed -n 's/.*pid = \([0-9]*\).*/\1/p' | head -1)
    ps -p "$pid" -o comm=                 # validate the pid before using it
    ps -o command= -p "$pid"              # a --config flag would override the default
    ps -Eww -p "$pid" -o command= | tr ' ' '\n' | grep -E '^XDG_CONFIG_HOME=|^HOME='

`default_config_path()` reads `XDG_CONFIG_HOME` and falls back to `$HOME/.config`.
On this box the daemon has HOME set and XDG_CONFIG_HOME unset, so it resolves to
`~/.config/cortexkit/subc.jsonc`. That happens to match what one would guess,
which is why guessing has been harmless and why the agreement was never
established.

Do NOT resolve the pid with `pgrep -x ck-subc` — the process reports its full
path, so the pattern misses, and an empty pid then turns `lsof -p ""` into an
unfiltered listing that returns a plausible unrelated file.

## Registering the prediction

Rescan retires any supervised module absent from the config, which stops live
processes. Its premise is inspectable in advance (unlike a sealed one), but
nothing reconstructs the diff for the operator — the result table is read AFTER
the retires. Until `supervisor.rescan` gains a preview mode, register the
prediction by hand:

1. Edit the config.
2. Record `shasum -a 256` of the config. NOT mtime+size: a rename is a
   substitution so size is preserved, and mtime granularity is one second so a
   scripted edit landing in the read's own second is invisible. Measured — both
   reported IDENTICAL across a real rename edit while the hash moved.
3. `ck module list` over the same connection rescan will use. This is the running
   set from the executing process, not from disk.
4. Write down the expected added / removed / unchanged sets, from the config diff
   as the REASON and the daemon's own state as the evidence.
5. Re-hash the config immediately before rescan. A mismatch means something
   changed under you.
6. Run rescan. Compare its result table against the prediction.

## Verifying the flip

Do not substitute a cheap check for the one that matters. An inode match proves
the running image is the file on disk; it proves nothing about whether the flip
worked. The functional check is a route that reaches the renamed module under its
new id and returns.

Health `ok` is also not sufficient on its own: it answers a narrower question than
an operator reads it as, and a module can report healthy while a consumer cannot
address it.

## Rollback

The config edit is reversible and a rescan restores the previous module set. The
apps are NOT — a device on a new build cannot be repointed without another
install. So the rollback is asymmetric: the daemon side is cheap to undo, the
consumer side is not, which is another reason the apps go first and the flip goes
last.
