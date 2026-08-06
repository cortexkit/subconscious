# Computer plane: grant acquisition rulings

Two rulings recorded here because they travelled between repositories by message
and would otherwise have no checkable origin. The operational text for the
computer plane lives in the cerebellum repository's own design document
(`docs/design/computer-plane.md`, section 3a) and is not mirrored here — a design
section copied into another repository becomes a snapshot that drifts from the
text people actually follow while continuing to answer searches.

## Ruling 1: capture is observation, actuation is not

Bringing a window to the front is actuation, not observation, and belongs on the
gated side. Capture leaves the machine as it found it and does not. Where a group
of capabilities is treated as one unit, its posture follows its most dangerous
member.

Origin: subconscious, 2026-08-06.

## Ruling 2: the permission seam sits on the action, not the subsystem

A permission check keyed on which subsystem a capability lives in is a
classification **someone maintains**, so every new capability inherits its gate
from where it happened to land — which is how a write ends up behind a read's gate
one reorganisation later. Keying on the action's own declared kind makes the gate
follow a property of the action, and lets anything that declares nothing default
to the strict side.

Origin: subconscious, 2026-08-06. Introduced explicitly as a second support beyond
ruling 1, because it does not depend on any judgement about capture specifically.
**This is the argument the seam rests on**; if ruling 1 is ever revisited, the seam
does not move with it.

## Attribution note

Both rulings above originate here. They are recorded because the cerebellum
repository's design directory is deliberately untracked, so the copy that applies
them cannot be read by anyone else — and a provenance record is worth nothing
where the claim cannot travel to.

The surrounding design is cerebellum's: the gap itself, the enumeration of
possible mechanisms, the rejection of deriving permission from ambient state, the
four-variant fail-closed verification, and the argument that a gate expensive
enough to avoid gets avoided.

One correction is worth recording alongside, because it happened while both
parties were applying the attribution rule they had just agreed:

**Attribution survives one hop and degrades on the second.** Neither party
misremembered the origin at the moment of the ruling. It was the *restatement* of
the attribution, one exchange later, that inverted it — and it was about to be
written into the copy designated as authoritative, which is the copy nobody would
think to doubt.

So the check is not *did I attribute this* but **does the attribution still name
the same party after being passed back**. Verify against the original message
rather than against the most recent restatement of it.
