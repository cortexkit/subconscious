# CortexKit release signing topology

Revision: signing-topology/v2
Status: built signing contract; consumers cite the revision string above.

## The topology in one paragraph

Every deployed CortexKit binary on macOS is signed AT BUILD/STAGE TIME with
the fleet signing identity and an explicitly pinned signing identifier equal
to the name the binary is DEPLOYED as, then placed with a plain copy (temp
name, atomic rename). Signing never happens at the deployment path, and never
in place under a running process. Each deployment environment (production,
dev-rig) uses a distinct identifier so macOS TCC grants cannot alias across
environments.

## Rules

1. SIGN AT STAGE, WITH THE FLEET IDENTITY AND A PINNED IDENTIFIER.
   `codesign --force --sign "$CK_SIGNING_IDENTITY" --identifier <deployed-name>`
   runs on the staged artifact. A bare `codesign --force --sign -` derives the identifier from
   LC_UUID (prefix `55554944`), which changes on every rebuild — never use it.

2. USE A REAL IDENTITY FOR REBUILD-STABLE TCC GRANTS.
   Ad-hoc signing pins TCC grants to a cdhash; identity signing instead supplies a stable designated requirement based on the identifier and certificate chain, which survives rebuilds and certificate renewal preserving the subject CN.

3. THE IDENTIFIER IS THE DEPLOYED NAME, NOT THE BUILT NAME.
   Read the name from the launch configuration (`subc.jsonc` `program`), not
   from the build target. A rig binary carrying production's identifier
   inherits or disturbs production's TCC grant row invisibly.

4. PLACE WITH PLAIN COPY; NEVER RE-SIGN AT PLACEMENT.
   A pin is not sticky: any later re-sign replaces it. The robust procedure
   has the dangerous command ABSENT, not present with the right flag. Copying
   does not invalidate a signature. Placement is: copy to a temp name beside
   the destination, then atomic rename over it (rm-first where a live process
   holds the old inode — macOS permits in-place text page rewrites, so never
   write through a running binary's path).

5. ONE IDENTIFIER PER PRINCIPAL PER ENVIRONMENT.
   `ck-thalamus` (prod) vs `ckdev-thalamus` (rig). The TCC settings pane
   shows one row per identifier; sharing one across environments aliases
   their grants.

6. STAGED AND PLACED ARE BYTE-IDENTICAL.
   Because signing happens at stage and placement is a plain copy, whole-file
   SHA-256 is a valid placement check. An artifact hash published for verification is valid only paired
   with the exact signing command that produced it; any re-sign invalidates
   previously published hashes and requires republication to every verifier.

7. FIRST-EXEC ASSESSMENT IS PER FILE, NOT PER BUILD.
   A newly signed copy at a new inode may pay Gatekeeper's first-exec cost even when its byte-identical staged source was warmed; warm execution therefore runs on the deployed path after placement, outside the supervisor health window.

8. THE TRANSITION REQUIRES AN OPERATOR RE-GRANT.
   The first identity-signed placement of a TCC-dependent binary orphans its prior ad-hoc grants, so re-grant is a named placement step; ad-hoc signing remains valid for untransitioned and throwaway artifacts, while a v2 consumer checks the certificate authority as well as the identifier.

9. VERIFICATION INSTRUMENTS.
   LC_UUID (`dwarfdump --uuid`) identifies a (commit, path, toolchain) triple
   — invariant under signing, so it proves two artifacts are the same BUILD
   across a re-sign; it can NOT name a commit from a binary alone. Whole-file
   SHA-256 proves the same BYTES (changes on re-sign). Running-image identity
   is proven by inode comparison (`lsof -p PID -a -d txt` vs `stat -Lf '%i'`),
   never by path-derived hashing. `codesign -d -r-` prints the designated
   requirement — the v2 acceptance check for a transitioned binary is that
   requirement naming identifier + certificate rather than cdhash.

## What a consumer binds

A release gate citing this topology binds the revision string
`signing-topology/v2`. The revision advances only when a rule above changes
meaning — wording and clarification changes do not advance it. v1 remains a
valid citation for binaries not yet transitioned; a consumer requiring
rebuild-stable TCC grants binds v2 specifically.
