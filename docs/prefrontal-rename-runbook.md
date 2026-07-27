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

## A clean merge is not a complete rename

`git merge-tree` proves there is no textual conflict between the branch and master.
It CANNOT prove the rename is complete, and on a long-lived branch it usually is
not: master keeps acquiring new occurrences of the old id after the branch point,
and those survive the merge UNRENAMED with nothing to report. The green tick is
about conflicts, and completeness is a different question that looks answered.

GREP THE MERGED TREE, NOT EITHER SIDE. Grepping the branch understates (it cannot
see what master added); grepping master overstates (it counts what the branch
already fixed). Only the merge result is the thing that ships:

    T=$(git merge-tree --write-tree master <branch>)
    git grep -l '<old-id>' "$T" -- '*.rs' '*.ts' '*.swift'

THEN PARTITION BY MEANING BEFORE ACTING. The raw count mixes route targets, doc
comments, prose and historical records, and only the first kind breaks anything.
Measured here: 94 occurrences, 58 of them quoted bare ids, and ALL 58 in tests
with zero in sources -- which is simultaneously the proof that every production
call site was covered and the sizing of the remaining work.

RENAME THE TEST OCCURRENCES ANYWAY. They construct an id value rather than dialling
a route, so nothing breaks either way. That is precisely the reason to do it: a
test whose id disagrees with production stops exercising the shape production uses,
and the suite stays green while covering a string that exists nowhere in the fleet.

AND EXPECT A BLIND SWEEP TO HALF-RENAME A PAIR. Ours hit an assertion's EXPECTED
value but not the INPUT literal it was escaped inside, leaving a test demanding
that decoding the old id yield the new one. Wherever a value is transformed, the
two sides can be written differently enough that one pattern reaches only one of
them -- so run the suite after the sweep and read what fails, rather than widening
the pattern until nothing matches.

Leave provenance and gate records alone. A document recording what CAPTURED BYTES
contain must keep saying what they contain, or it starts lying about the fixture
beside it.

### The class that lives in other people's repos

The two branches for this rename each grep THEIR OWN tree, and there is a class
that cannot appear in either: A TRUST ALLOWLIST KEYED ON MODULE ID. A module that
names yours in a security decision has no reason to appear in your repo, and you
have no reason to look in theirs.

Found by grepping all fourteen repos rather than the two we own. Three modules gate
first-party capability on a hardcoded list of caller ids, and the new name is on
none of them: aft (bash), cerebellum (browser and computer use), plexus (connector
invocation). Each else-branch was READ rather than inferred: all three fail CLOSED,
so this is capability loss and not a security hole. That is also what makes it
expensive -- the executive quietly stops being permitted to do things, and "the
agent cannot run bash any more" gets debugged in the wrong repo.

THE ORDERING IS THE MIRROR OF THE ROUTE-TARGET RULE, and getting it backwards
leaves a window with no capability at all. Route targets: CONSUMERS ship the new
string first, because they DIAL. Trust allowlists: THE ALLOWLIST HOLDER ships
first, because it is DIALLED. Both collapse into one sentence worth carrying:

  EVERY PLACE THAT NAMES THE MODULE BY STRING MUST ACCEPT THE NEW NAME BEFORE THE
  MODULE STARTS USING IT.

AND THE GATE IS A RUNNING ARTIFACT, NEVER A MERGED COMMIT. A trust allowlist on
main grants nothing; the supervised process is still executing the binary it was
started with. AFT made this correction against their own earlier framing after
landing the fix, and it is the same condition as the apps gate one paragraph up --
there it is an INSTALLED build on a device, here it is a STAGED AND BOUNCED binary
on this box. Both collapse into: THE PRECONDITION IS SATISFIED BY WHAT IS RUNNING,
NOT BY WHAT IS ON MAIN.

So the pre-window checklist has a deploy list, not a merge list: the three
allowlist holders bounced onto binaries carrying their fix, and every app rebuilt
and installed. Verify each by the running image rather than by a commit -- inode
against the deploy path, or a symbol differential -- because "merged" and "green"
are both true of a binary nobody is executing.

So three more repos join the pre-window list, one line each -- add the new name
beside the old, keep both across the window, drop the old one afterwards.

THE PROOF THIS IS NOT THEORETICAL sits in aft's list, which carries BOTH the old
AND new names from the PREVIOUS rename. Someone hit this exact class before and
handled it by keeping both. It is the only place in the fleet where a prior
rename's transitional state is still visible, and it is inside a trust decision --
which means it also needs a comment saying why both are there, or the next person
tidying duplicates removes the wrong one.

RUN THE FLEET-WIDE GREP BEFORE WRITING THE RUNBOOK, not after. It is one command
over every repo and it finds the classes that a per-repo sweep cannot see by
construction.

### The classes to rename, and the two that must not be

After partitioning, only two classes are load-bearing and both must reach zero:
ROUTE TARGETS, which break the wire, and USER-VISIBLE STRINGS, which after the flip
name something that exists nowhere in the fleet -- a placeholder telling the user
to wait for a module that was renamed.

Tests count as a third, weaker class: they break nothing, which is exactly why they
are worth doing. A test whose id disagrees with production silently stops
exercising the shape production uses while staying green.

TWO CLASSES MUST KEEP THE OLD NAME, and a blanket comment sweep gets both wrong:

PINNED WIRE FIXTURES. Where a generator's output is compared byte-wise against
committed captures, renaming the generator breaks parity and renaming both makes
synthesised bytes claim to be captured. Check for a parity test BEFORE touching a
generator; the id is usually incidental payload there and nothing is gained.

HISTORICAL REFERENCES TO OLD BUILDS. A comment reading "older alfonso-core builds
lack this op" is a statement about builds that genuinely carried that name.
Renaming it makes the comment claim those builds were called something they were
not -- the same defect as rewriting a provenance record, wearing ordinary prose.
Distinguish it from a CURRENT-DESCRIPTIVE comment ("the alfonso-core ops are ..."),
which is merely stale and safe to update.

AND WATCH THE SUBSTITUTION ITSELF. A whole-file regex for "inside a string literal"
spans line boundaries: a quote early in the file pairs with one much later and
swallows everything between. Ours changed 13 lines where 8 were intended. The
extras happened to be correct; read every changed line rather than trusting the
count, because the mechanism does not know which.

## The daemon artifact for this window

The window needs a daemon bounce regardless (module ids change, so the config
changes and a rescan follows), which makes it the natural moment to close the
daemon's own deploy gap. Build the release binary BEFORE the window so the step is
copy, sign, warm-exec, restart -- not a build.

WHAT THE PENDING DAEMON BINARY CARRIES that matters here: `supervisor.rescan
--dry-run`, which shows the reconciliation without applying it. The running daemon
predates the flag and IGNORES it, executing a real rescan -- the CLI refuses loudly
rather than reporting success, but the preview is unavailable until the bounce. So
the order is BOUNCE FIRST, THEN USE THE PREVIEW during the rename, not the reverse.

MARKER FOR VERIFYING THE SWAP, measured rather than assumed:

    strings <binary> | grep -c preview      new: 3    old: 0

CONTROL, proving the probe reads the binary at all -- a marker that reads zero on
both is indistinguishable from a failed deploy and invites redeploying correct
bytes:

    strings <binary> | grep -c changed_pending_reload    new: 3    old: 3

TWO TRAPS I HIT PICKING THAT MARKER, both worth avoiding under window pressure.
First, my initial choice was a CLI string and read ZERO on the daemon -- the marker
must come from the artifact being swapped, so VERIFY IT ON THE NEW BINARY BEFORE
comparing against the old one. Second, match-arm string literals never reach the
binary at all: the compiler compares them by length and bytes without emitting a
contiguous constant, so a perfectly reasonable-looking marker can be structurally
unfindable. If a string is not in a binary built from source containing it, the
marker is unusable -- stop there rather than concluding the deploy failed.

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
processes. Its premise is inspectable in advance (unlike a sealed one), and the
daemon now reconstructs the diff on request.

RUN `ck module rescan --dry-run` FIRST. It reports the added / removed / unchanged
sets WITHOUT applying them, computed daemon-side by the same function as the
executing path with a single early return before any mutation — so the preview
cannot describe a different config than the operation reads.

AND THE OPERATOR'S `ck` MUST CARRY IT TOO. The flag was committed to master before
the binary on PATH was refreshed, so for a while this document prescribed a flag the
operator's own binary did not have. Check with `ck module --help`: if `rescan
--dry-run` is absent from the listing, install the current build first. A runbook
line is a claim about the INSTALLED tool, not about master.

TWO FURTHER CONDITIONS, BOTH LOAD-BEARING:

· THE DAEMON MUST CARRY THE PREVIEW BUILD. A daemon predating it DROPS THE UNKNOWN
  FIELD AND EXECUTES A REAL RECONCILIATION. The CLI refuses loudly rather than
  reporting success — exit 2, with an explicit warning that a reconciliation may
  have applied — but the ordering still matters: BOUNCE FIRST, THEN PREVIEW, never
  the reverse. This is the whole reason the pending daemon binary is staged.
· THE PREVIEW IS NOT A SUBSTITUTE FOR THE HASH. It answers what rescan WOULD do
  against the config as it stands now; it says nothing about the config changing
  between the preview and the run. Keep step 5.

With the preview available the hand prediction is a cross-check rather than the
only instrument, and it is still worth writing down — a prediction registered in
advance turns a surprise into a finding instead of something rationalised after the
fact:

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
