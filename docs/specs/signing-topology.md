# CortexKit release signing topology

Revision: signing-topology/v1
Status: Built (this document is the citable form of operational doctrine that
has governed fleet deployments since 2026-08-06; nothing here is aspirational).
Owner: subconscious (SUBC seat). Consumers cite the revision string above.

## The topology in one paragraph

Every deployed CortexKit binary on macOS is ad-hoc signed AT BUILD/STAGE TIME
with an explicitly pinned signing identifier equal to the name the binary is
DEPLOYED as, then placed with a plain copy (temp name, atomic rename). Signing
never happens at the deployment path, and never in place under a running
process. Each deployment environment (production, dev-rig) uses a distinct
identifier so macOS TCC grants cannot alias across environments.

## Rules

1. SIGN AT STAGE, WITH A PINNED IDENTIFIER.
   `codesign --force --sign - --identifier <deployed-name>` runs on the staged
   artifact. A bare `codesign --force --sign -` DERIVES the identifier from
   LC_UUID (prefix `55554944`), which changes on every rebuild; macOS binds
   TCC privacy grants (Screen Recording, Accessibility) to the identifier and
   attributes supervised modules to the daemon, so a derived identifier
   silently revokes fleet-wide privacy grants on the next binary swap.

2. THE IDENTIFIER IS THE DEPLOYED NAME, NOT THE BUILT NAME.
   Read the name from the launch configuration (`subc.jsonc` `program`), not
   from the build target. A rig binary carrying production's identifier
   inherits or disturbs production's TCC grant row invisibly.

3. PLACE WITH PLAIN COPY; NEVER RE-SIGN AT PLACEMENT.
   A pin is not sticky: any later bare `--force --sign -` re-derives it. The
   robust procedure has the dangerous command ABSENT, not present with the
   right flag. Copying does not invalidate a signature. Placement is: copy to
   a temp name beside the destination, then atomic rename over it (rm-first
   where a live process holds the old inode — macOS permits in-place text
   page rewrites, so never write through a running binary's path).

4. ONE IDENTIFIER PER PRINCIPAL PER ENVIRONMENT.
   `ck-thalamus` (prod) vs `ckdev-thalamus` (rig). The TCC settings pane
   shows one row per identifier; sharing one across environments aliases
   their grants.

5. STAGED AND PLACED ARE BYTE-IDENTICAL.
   Because signing happens at stage and placement is a plain copy, whole-file
   SHA-256 is a valid placement check. An artifact hash published for
   verification is valid ONLY paired with the exact signing command that
   produced it; any re-sign invalidates previously published hashes and
   requires republication to every verifier.

6. AD-HOC IS THE CURRENT TRUST LEVEL, STATED HONESTLY.
   Ad-hoc signatures authenticate nothing about the author; they exist to
   satisfy exec policy and to carry the pinned identifier for TCC identity.
   Ad-hoc signing IS deterministic (measured: two independent signings of one
   binary produce identical bytes), which is what makes rule 5 usable.
   Developer-ID / notarization is a future revision of this topology; nothing
   in the fleet currently assumes it.

7. VERIFICATION INSTRUMENTS.
   LC_UUID (`dwarfdump --uuid`) is invariant under signing: it proves two
   artifacts are the same BUILD across a re-sign. Whole-file SHA-256 proves
   the same BYTES (changes on re-sign). Running-image identity is proven by
   inode comparison (`lsof -p PID -a -d txt` vs `stat -Lf '%i'`), never by
   path-derived hashing.

## What a consumer binds

A release gate citing this topology binds the revision string
`signing-topology/v1`. The revision advances only when a rule above changes
meaning — wording and clarification changes do not advance it.
