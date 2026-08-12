# CortexKit release signing topology

Revision: signing-topology/v2
Status: Built (v1 governed fleet deployments since 2026-08-06; v2 adds a real
signing identity after TCC cdhash-pinning was measured orphaning grants on
every rebuild. Rollout is per-binary at its next natural placement).
Owner: subconscious (SUBC seat). Consumers cite the revision string above.

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
   runs on the staged artifact. The fleet identity on this host is
   `Apple Development: ISMET UFUK ALTINOK (7UX762GU88)` (from the existing
   Apple Developer account; the same account CKIOS publishes to TestFlight
   with). A bare `codesign --force --sign -` DERIVES the identifier from
   LC_UUID (prefix `55554944`), which changes on every rebuild — never use it.

2. WHY A REAL IDENTITY (the v2 change): TCC pins an ad-hoc-signed binary's
   grants to its CDHASH, because an ad-hoc signature has no stable designated
   requirement — so every rebuild silently orphaned Screen Recording and
   Accessibility grants while System Settings kept showing the toggle ON (the
   pane displays the record, not whether it applies). A real identity gives
   the binary a designated requirement of identifier + certificate chain
   (verified on this host: `identifier "ck-subc" and anchor apple generic and
   certificate leaf[subject.CN] = "Apple Development: ..."`), which SURVIVES
   REBUILDS. Grants die once at the ad-hoc-to-identity transition, then never
   again from a rebuild. Certificate renewal preserves the subject CN, so the
   requirement survives renewal too.

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
   SHA-256 is a valid placement check. Identity signing is deterministic on
   this host (measured under v2 adoption: two independent signings of one
   binary produce identical bytes), preserving the property v1 measured for
   ad-hoc. An artifact hash published for verification is valid ONLY paired
   with the exact signing command that produced it; any re-sign invalidates
   previously published hashes and requires republication to every verifier.

7. FIRST-EXEC ASSESSMENT IS PER FILE, NOT PER BUILD (measured at v2
   adoption): Gatekeeper assesses a newly-signed binary on ITS first exec —
   staged ck-models cost 22.5s then 3ms; the byte-identical PLACED copy of
   ck-fusiform cost 15.4s on its own first exec even though the staged copy
   had been warmed. A copy at a new inode pays the toll again. Therefore the
   warm-exec ladder step runs ON THE DEPLOY PATH after placement — warming
   the staged artifact buys the placed file nothing — and a multi-second
   first exec inside a supervisor health window reads as a failed start (a
   deployment failure that never recurs, the worst kind to debug).

8. ROLLOUT AND THE TRANSITION COST, STATED HONESTLY.
   Each binary moves to the identity at its NEXT NATURAL PLACEMENT — no
   fleet-wide re-sign wave (a re-sign wave would orphan every TCC grant at
   once and invalidate every published hash for no operational win). The
   FIRST placement of a TCC-dependent binary under the identity orphans its
   grants one final time; that re-grant is a named ladder step for that
   placement. Ad-hoc remains valid for binaries not yet transitioned and for
   throwaway/test artifacts; a gate consumer that requires the identity binds
   v2 and checks the certificate authority in `codesign -dv` output, not just
   the identifier.

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
