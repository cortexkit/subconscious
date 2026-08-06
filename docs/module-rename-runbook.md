# Module rename runbook

Covers renaming a supervised module's directory, module id, or both. Written
after the `ai-proxy` -> `thalamus` rename, which broke four things that a
reference sweep could not have found.

Two renames are queued against this: `subc-federation` -> `callosum` and
`ai-provider-quota` -> `insula`.

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
3. **Move, never copy.** The single-writer lock is per-location, so a copy leaves
   a second openable store and two live writers become possible.
4. Move the directory; apply the identity change; reconcile.
5. **Verify the minted identifier** before declaring anything.
6. **Restart the resident** so it re-binds to the real path.
7. **Remove the link, then have the resident act.** Do not verify by comparing
   path strings: a working directory is stored by the kernel as an inode, so a
   process reading it through the system call always sees the resolved path, while
   a shell hands back the logical path it remembered. Which string appears depends
   on how it was obtained, and that is not visible from inside — so the check can
   read *new* while running on the link, or *old* while correctly rebound.
   Ambiguous in both directions is not weak evidence; it is none.

   Remove the link first and have the resident run a command. Genuinely rebound,
   everything works. Still on the link, its tools fail immediately with a plain
   "no such file or directory" — the same failure as an unprepared rename, but
   triggered deliberately while someone is watching and can restore the link in
   one command, instead of surfacing days later when someone clears out stale
   links.

   Note the interaction with path canonicalization: while the old spelling is
   still recorded anywhere that matters, the link is what makes it resolve
   correctly. Remove it before that is true and a healed system goes back to
   broken.

Steps 6 and 7 are the ones that get dropped. A resident left running through a
rename works today and breaks whenever someone tidies up.

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
