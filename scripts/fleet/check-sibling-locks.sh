#!/usr/bin/env bash
# Check COMMITTED Cargo.lock validity for fleet repos that path-depend on
# commons or subconscious.
#
# Mechanism this surfaces: path dependencies record the version read from the
# path, so a version bump in an upstream repo invalidates the committed lock of
# every sibling — with zero changes in the sibling's own tree and no signal to
# its owner. Local builds keep passing because any unlocked cargo command
# quietly repairs the WORKING-TREE lock; only a clean checkout (CI) fails.
#
# Instrument note, learned by running the first version: a git-archive-to-temp
# probe CANNOT judge these repos (the archive lacks sibling path-dep targets
# and, for some repos, workspace members — the probe's own failure then reads
# as a stale lock; 13 false positives out of 16 on first run). The honest
# read-only form is two-armed, in place:
#   lock CLEAN in tree  -> in-place `cargo metadata --locked` judges the
#                          committed lock exactly (same bytes).
#   lock DIRTY in tree  -> cannot judge the committed lock without mutating
#                          the owner's tree; reported as its own state, which
#                          is itself the owner signal (a dirty lock means an
#                          unlocked command already repaired the working tree
#                          — the committed lock is almost certainly stale).
#
# Exit: 0 all clean locks resolve; 1 stale or dirty found; 2 vacuity floor.

set -uo pipefail

ROOT="${CK_PROJECTS_ROOT:-$HOME/Work/Projects/CortexKit}"
REPOS=(engram synapse plexus claustrum astrocyte fusiform entorhinal wernicke cerebellum insula broca prefrontal thalamus callosum aft magic-context)

examined=0
bad=0
for name in "${REPOS[@]}"; do
  repo="$ROOT/$name"
  [ -f "$repo/Cargo.lock" ] || continue
  grep -qE 'path *= *"(\.\./|/Users/)' "$repo"/Cargo.toml "$repo"/crates/*/Cargo.toml 2>/dev/null || continue
  examined=$((examined + 1))
  if ! git -C "$repo" diff --quiet HEAD -- Cargo.lock 2>/dev/null; then
    echo "DIRTY $name — working-tree lock differs from committed; an unlocked command already repaired it locally, so the COMMITTED lock is likely stale (owner: commit the refreshed lock)"
    bad=$((bad + 1))
  elif (cd "$repo" && cargo metadata --locked --format-version 1 >/dev/null 2>&1); then
    echo "OK    $name (committed lock resolves)"
  else
    echo "STALE $name — committed Cargo.lock does not resolve against current upstream (owner: refresh and COMMIT the lock)"
    bad=$((bad + 1))
  fi
done

if [ "$examined" -lt 1 ]; then
  echo "VACUOUS: zero repos examined — roster or root wrong" >&2
  exit 2
fi
echo "examined $examined path-dependent repos, $bad stale-or-dirty"
[ "$bad" -eq 0 ]
