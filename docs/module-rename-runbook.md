# Module rename runbook

## Renaming a GitHub repository burns the old name permanently

GitHub redirects the old repository path to the new one, and that redirect is
what every existing clone, bookmark and API reference relies on. It has one
fatal property: **creating a new repository at the old path silently replaces
the redirect, and deleting that repository does not bring the redirect back.**
The loss is permanent.

So every name we rename away from becomes a burned namespace. The realistic
way to trigger it is somebody months later creating a small helper repo under
the old familiar name, with no idea it was ever taken. Low probability, zero
recovery.

Burned so far: `cortexkit-credentials`, `subc-federation`, `ck-projects`,
`ai-provider-quota`, `alfonso`.

## `uses:` is the one reference that does not redirect

A workflow step that consumes an action from a renamed repository fails with
`repository not found`. There is no compatibility window for that one case,
which makes renaming a repository that HOSTS an action a hard breaking change
for every consumer.

Everything else follows the redirect, verified per layer rather than assumed:
git fetch, the REST API, and the tarball endpoint `actions/checkout` uses. Each
layer can fail independently, so one working does not license the others.

Before renaming, check both keys across every workflow in the fleet, and prove
the search can return something before trusting an empty result:

```sh
find . -maxdepth 4 -path '*/.github/workflows/*.y*ml' | wc -l   # must be non-zero
find . -maxdepth 4 -path '*/.github/workflows/*.y*ml' -print0 |
  xargs -0 grep -nE '^\s*(uses|repository):' | grep '<old-name>'
```

An unbounded `find` over these trees can stall and return empty rather than
erroring, which reads exactly like a clean fleet.

Covers renaming a supervised module's directory, module id, or both. Written
after the `ai-proxy` -> `thalamus` rename, which broke four things that a
reference sweep could not have found.

Renames queued against this: `cortexkit-credentials` -> `claustrum` (ratified;
the Latin means "a bolt, an enclosure", which is what a vault is) and
`ai-provider-quota` -> `insula`. `subc-federation` -> `callosum` is done.

All three have since landed. The quota rename was held for a week on a belief
that its name-keyed crash journal had to migrate first, or an empty one would
double-spend reset credits. Both halves of that were wrong, and both were
checked only when someone finally read the source: the journal path is a
hardcoded literal that a module-id rename cannot move, and losing the file
exposes at most a thirty-minute window against a server that is already
idempotent on the redemption id. See `rename-window-prep.md` for the corrected
sizing — the blocking belief failed safe, which is exactly why nothing pressed
anyone to test it.

## Why a reference sweep is not enough

A sweep asks *what refers to this name*. Every casualty of the last two renames
was **keyed by** it — a lookup, not a reference — so all of them sat outside that
question however carefully it was asked.

They also all failed as **absence** rather than error: an empty screen, an empty
contact list, a job that stopped running, an inbox that reports nothing while
hundreds of messages wait under the old spelling. Nothing errored on either side
in any of the four cases.

So ask both questions before starting: **what is keyed on this name, and what is
keyed on this path.**

**And a third, which the first two cannot reach: what is DERIVED from the path.**
A derived identity is invisible to a reference sweep *and* to a path sweep, because
the path itself never appears anywhere — only its digest, computed at runtime.

The vault rename found two: a keychain service named
`<prefix>:<first 8 bytes of SHA-256(canonical data_dir)>`, and an anti-splice vault
id that is a full SHA-256 over the same bytes, each derived independently by the
daemon and the CLI from their own view of the directory. Neither is greppable.

**Ask this question of each owner rather than trying to answer it centrally.** It is
a genuine limit rather than a weak sweep: grep cannot find a hash of a string that is
never written down, so the only way to find a derived identity is to read the code
that computes it — which the component's owner is uniquely positioned to do.

This produces a failure ordering worth internalising: **moving the store alone was
worse than moving nothing.** Nothing moved leaves a vault that reads as empty;
store-only leaves a vault that is *locked*, because a real store opens and its key
is looked up under a scope that has never existed — and it presents as a key problem
rather than a rename problem, so the symptom points away from the change that caused
it. A partial migration can also self-repair into a plausible wrong state: the CLI's
default data dir follows the module id, so it would bootstrap a **new empty vault**
at the new path rather than erroring.

## Before the move

**Classify the state as cache or fence.** A cache keyed on the old name fails as
slowness and announces itself. A fence — anything that reads its own history to
avoid repeating an action — fails as a *clean start*, which is indistinguishable
from having nothing to do. Fences move first, and are verified by inspecting what
is pending, never by the file existing.

**Ask the resident agent separately from the component owner.** These are
different subjects with different failure modes, and asking an owner "is your
state safe" reliably gets the component answer, because component state is what an
owner thinks of as theirs. A working session binds a project root at startup and
gates every operation on it; that binding is invisible to a component audit.

**Verify an identity only the original could carry.** A row count that might
legitimately be zero cannot distinguish a migrated store from a fresh one. Prefer
an identifier minted once at creation: a recreated store produces a *different*
one rather than a missing one.

Where you do record counts, **record them as floors, never as equalities.** A
count taken from a live system keeps moving while you prepare the operation, so a
verifier comparing for equality reports a false failure — and it does so
mid-migration, which is the worst possible moment to be told something is wrong
that is not. One count on this system grew from 52 to 55 between writing the check
and running it, purely from ordinary traffic.

**Find the point of no easy return, and say which side of it you are on.** "The old
copy stays until the new one is verified" sounds like free rollback and usually is
— but only until the new instance *writes*. The vault's window closes at its first
served credential refresh: after that the two stores have diverged, and rolling back
resurrects a superseded token the provider has already invalidated. So order the
verification to put the non-writing checks first (status, chain integrity, a
round-trip that touches no external token), and treat the first real write as the
boundary. The point is not to hold writes off; it is that both parties should know
which side of the line they are on when they call it good.

**Existence is not identity.** Verify a key by fingerprint against an anchor the data
itself carries, never by observing that an item exists at the new location — and the
same instrument as verifying a deployed binary by the running image's inode rather
than by a file with the right name sitting at the path. Both are cases where **the
cheap observation is satisfied by the exact failure it is meant to catch**: a freshly
bootstrapped keychain item and a stale binary each pass an existence test and fail an
identity test.

**Move state through the shipped code path, not a bespoke one.** Two properties fall
out of it that a hand-written copy cannot claim: the derivation runs in the same code
the daemon will use, so a scope cannot be typed into existence; and the write keeps
its real semantics, so it cannot silently clobber something a half-applied attempt
already put at the destination. Check whether the destination is occupied before
writing rather than assuming the copy is into emptiness.

**Carry the pending slot as well as the current one.** A two-phase handover leaves an
unpromoted successor behind after a crash — an empty one costs a check, a dropped one
converts a recoverable pending rotation into a lost key.

**Record what every observable should read, before and after.** Include changes
you expect from *unrelated* work landing in the same window. A confound that hides
a failure eventually announces itself; a confound that decorates a success is
never examined, because nobody re-derives a number that agrees with them.

## The move

1. **Create the compatibility link first**, pointing the old path at the new one.
   Ahead of the move this converts a hard break into a soft one at no cost.
   Afterwards it is a rescue, and the resident spends the gap unable to run the
   commands needed to diagnose it.
2. **Stop the module** if its storage location derives from its identity.
3. **Move, never copy — and move the whole directory, never individual files.**
   The single-writer lock is per-location, so a copy leaves a second openable
   store and two live writers become possible. A database in write-ahead mode is
   also several files: recently committed data lives in a sidecar until it is
   folded into the main one, so copying the main file alone silently drops the
   most recent writes. Any pre-move backup must be the whole directory for the
   same reason — a partial-file backup is worse than none, because it would be
   trusted.

   Measured here: 520 audit rows lived only in the sidecar, and three of four
   verification checks could not see the difference, because the values they read
   were old enough to have been folded in already. **Include one continuously
   growing table in the checks** — it is the only kind sensitive to a lost tail.

   **Open verification copies read-only, and treat that as load-bearing rather
   than good manners.** A write-ahead database missing its sidecar *refuses to
   open* read-only, because a read-only connection cannot create the shared-memory
   file it needs.

   **That refusal is ambiguous, and the two readings are opposites.** It means
   either *this copy is missing its most recent writes* or *this store was shut
   down cleanly* — a clean shutdown folds the sidecar into the main file and
   removes it, leaving nothing for a read-only connection to attach to. Measured
   during one migration: the directory went from four files to two and the main
   file grew by almost exactly the sidecar's size.

   So **the flag is the right default for reading a live store, and proves nothing
   about a stopped one.** At the verification step, the counts do the work: the
   minted identifier proves it is the same store, and a continuously growing table
   at or above its recorded floor proves it kept its tail — because a partial copy
   comes back *short* while every other check passes.

   The flaw in the earlier wording is worth keeping as a caution: it was derived
   against a running store, where the sidecar always exists, and applied at a step
   that only ever runs against a stopped one. **A rule validated in one state and
   applied in another was never true where it was written to be used.** Followed
   literally it would have called a successful migration a data loss, at exactly
   the moment when the natural response is to undo correct work. Opened read-write it opens happily and answers every query —
   silently short. The flag converts a silent wrong answer into a hard error.

   So **the error is the finding.** `unable to open database file` on a copy you
   just made is not an obstacle to work around by dropping the flag; it is the
   result. Retrying without it converts a correct alarm into a wrong number, which
   is the likeliest way this bites in practice. Take the copy again, as the whole
   directory.

   The incomplete copy is missing its tail from the instant `cp` finishes, and no
   flag on the reader changes that — a read-write open creates the sidecars but
   leaves the main file byte-identical. A complete copy reads the same either way;
   only an incomplete one diverges.
4. **Pre-seed the single-writer lease before the module's first open under the new
   name.** The counter that guards the store lives in a *file* named by a hash of
   the module id; the value it must beat lives in a *row inside the database*,
   keyed on nothing. So a new name mints a fresh counter starting at zero against
   a store still demanding the epoch the old name accumulated over its lifetime.

   The store then serves reads and refuses every write, permanently. Restarting
   adds one per attempt, so it cannot catch up -- measured here at 4 against a
   required 174, and that gap only grows with the store's age.

   With the module stopped and the lease file unheld: read the epoch from the
   fence row, then write that number into the new name's lease file. First open
   claims epoch+1, which beats every writer the database has ever had.

   Seed the lease *up*; do not lower the row. Lowering it leaves any pre-rename
   copy -- including the rollback target, still holding the old epoch -- able to
   fence the live store from the other direction later.

   **This is the step most likely to be skipped, because its failure arrives
   minutes later and does not look like a rename problem.** Reads keep working,
   so health is green and callers succeed; only writes fail, so credentials serve
   until individual tokens need refreshing and then fail one at a time with the
   cause several steps upstream. It cost a live outage here, and it had been
   written down from two earlier occurrences without being carried into this
   runbook -- a note that has to be remembered is not a procedure.
5. Move the directory; apply the identity change; reconcile.

   **Changing a `program` path needs a rescan AND a restart, in that order.**
   The rescan re-reads the config from disk and reports the module under
   `changed-pending-reload`; the restart is what respawns from the reloaded
   config. A restart alone respawns from the daemon's in-memory config and
   silently starts the OLD executable again, while every config-derived check
   agrees with the new config.

   That is why the inode comparison is load-bearing rather than ceremonial: it
   is the only step that reads the RUNNING image. Measured — restart-only left
   the previous binary serving with the config reading new, and only the inode
   check disagreed.

   Read the rescan's whole output. `added` and `removed` are both empty for
   this case; the informative row is a third one, and truncating the output
   after two lines makes a successful reload look like no change at all.
6. **Verify the minted identifier** before declaring anything, and **verify a
   write commits** rather than only that the module reports healthy. Reads are
   unfenced, so a store that has lost write authority answers every read
   normally. Mint and revoke something, then confirm the fence row advanced.
7. **Restart the resident** so it re-binds to the real path. This step is not
   optional and not reorderable: a session's project root is captured at start and
   is *not* re-resolved per call, so a session that has not restarted is still
   bound to the old path however the new one reads. Confirmed the hard way — a
   resident acted before restarting, and every command was refused at a
   precondition on the bound root, including one using only absolute paths with no
   working-directory reference.
8. **Remove the link only after the resident confirms it has restarted, and
   confirm it by asking rather than by measuring.** There is no filesystem
   check for this. A session's bound project root is not its working directory,
   so a seat can be entirely dependent on a path that `lsof` shows nobody
   standing in — the measurement is accurate and answers a different question.

   Removing a link on that evidence wedges the seat completely: every tool call
   is refused at a precondition, including calls using only absolute paths, and
   the session can neither work around it nor restart itself. It can still send
   messages, which is the only reason you find out.

   Links must not linger either. A symlinked path and its target can register
   as two different directories in the peer registry, which splits a seat's
   message routing from its message visibility.
   The confirmation is a claim, so verify it the same way: **remove the link,
   then have the resident act, while you are watching.** Do not verify by
   comparing path strings. A working directory is stored by the kernel as an
   inode, so a process reading it through the system call always sees the
   resolved path, while a shell hands back the logical path it remembered. Which
   string appears depends on how it was obtained, and that is not visible from
   inside — so the check can read *new* while running on the link, or *old*
   while correctly rebound. Ambiguous in both directions is not weak evidence;
   it is none.

   Genuinely rebound, everything works. Still on the link, its tools fail
   immediately — the same failure as an unprepared rename, but triggered
   deliberately while someone can restore the link in one command, instead of
   surfacing days later when someone clears out stale links.

   Note the interaction with path canonicalization: while the old spelling is
   still recorded anywhere that matters, the link is what makes it resolve
   correctly. Remove it before that is true and a healed system goes back to
   broken.

Steps 7 and 8 are the ones that get dropped. A resident left running through a
rename works today and breaks whenever someone tidies up.

Reading a store during any of this: use `mode=ro` on a **live** database and
`immutable=1` only on a **stopped** one. The immutable flag cannot write, which
is why it is right for a rollback target -- but it also ignores the write-ahead
sidecar, so against a running store it returns a pre-write snapshot confidently
and without erroring. Measured here on one file at one instant: it said 174
while a read-only open said 175, with 189 KB of committed data in the sidecar
that the first could not see. The property that makes it safe on a stopped store
is the same property that makes it wrong on a live one.

## Placing a signed binary

Sign at **stage** time with an explicit identifier, then place with a plain copy.

An ad-hoc signature with no identifier given derives one from the build, so it
changes on every rebuild of any source change. The operating system binds privacy
grants to that identifier and attributes them to the supervising process, which
means replacing a supervisor binary can silently revoke a permission for
everything it supervises: no prompt, no error, and no self-healing, because a new
process carries the new identifier while the grant still names the old one.

So the dangerous command must be **absent** from the placement procedure rather
than present with the right flag. A pin is not sticky: any later signature without
an explicit identifier silently reverts it.

**The identifier must match the filename the binary is deployed as**, read from
the consumer's configuration — not the name it was built as. Requiring stability
across rebuilds is not enough on its own: someone will fill in the only name in
front of them, which is the artifact's build-time filename, and a test build then
carries production's identity and inherits its grants. Nothing is denied and
nothing is logged, which is what makes it a problem rather than an error.

Stated this way it is checkable in one line per binary rather than something to
remember: read the deployed names from the configuration, read each binary's
identifier, and compare.

Expect three distinct forms when you sweep. An identifier matching the deployed
name is correct. One ending in a long hex string was derived by the signing tool
because no identifier was given. One carrying an *underscore* and a short hash is
the build tool's own linker-applied identity — never signed by anyone.

The middle form is decidable by inspection, which makes the sweep cheap. Strip the
fixed prefix and the remainder is the binary's own build identifier with its
separators removed — verified across every instance on this machine. So a one-line
read proves the identity moves with every build, without rebuilding anything.

The third form needs a real experiment, because a hash suffix is not always
build-derived: one such identity on this fleet proved **stable** across two
genuinely different builds. Build twice at different commits and compare.

**Which identity-bound resources matter is per-module.** Screen capture and
accessibility are the visible ones, but keychain access is identity-bound too, so
a module that reads stored credentials is exposed by a moving identity even
though it touches neither. Ask which of these the module actually uses rather than
checking a fixed list.

Watch for one combination in particular: **a test binary carrying production's
name in the derived form.** It is not colliding today only because it is
unsigned — the derived suffix keeps it distinct by accident. The moment someone
pins it, correctly following the stability rule, using the name it already
carries, it becomes production's principal. A latent defect waiting for someone
to do the right thing by halves, and invisible until the rule is stated as
*match the deployed name* rather than *be stable*.

**Apply that rule at stage time, never at placement.** If a test artifact arrives
carrying the production identifier and others have already verified its hash,
re-signing it changes the bytes they accepted. Substituting a different artifact
mid-operation is worse than the risk being corrected, even when the substitution
is an improvement — a hash is only a valid acceptance value when paired with the
signing command that produced it. Raise it with whoever builds the artifact so the
published hash is of a correctly identified binary from the start.

Two consequences worth having:

- **Whole-file hashing works again as a placement check**, because staged and
  placed are byte-identical. That check is unusable when signing happens after
  copying.
- **Re-signing invalidates every published hash.** The build identifier does not
  change, so it remains the way to prove two artifacts are the same build across
  a re-sign. A hash is only a valid acceptance value when paired with the exact
  signing command that produced it.

Copy to a temporary name and rename over the destination — writing in place
rewrites the pages of a process currently executing from that file.

Afterwards, compare the **running process's** file identity against the
destination's. They correctly disagree between placement and restart, so reading
the destination alone reports a restart as done when it has not happened.

## Verification

**Sub-checkouts must resolve to the new root**, not merely appear in a listing.
Listings include dangling entries without complaint. Repair from *inside* each
one; repairing from the parent reports them broken and fixes neither. And ask for
absolute paths — a relative answer looks like a pass and says nothing about where
it points.

**Count the resident's contacts before and after.** A shrunken list is visible
immediately. A silent inbox is invisible until someone sends something worth
reading — and the resident can send confident status the entire time they cannot
read a word back.

**Exercise a write.** A read-only check proves the service is serving and nothing
about the write path.

## What survives untouched

Content-addressed state. Search indexes keyed on repository identity rather than
location survive a directory rename for the cost of one probe. Historical records
of past events should keep their original spelling — they are records of where
something went, not live routes.
