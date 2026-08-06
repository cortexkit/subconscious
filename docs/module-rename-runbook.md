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
7. **Remove the link** — but only once the resident's own record names the new
   path. Under path canonicalization the link is what makes the old spelling
   resolve correctly, so removing it early takes a healed system back to broken.

Steps 6 and 7 are the ones that get dropped. A resident left running through a
rename works today and breaks whenever someone tidies up.

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
