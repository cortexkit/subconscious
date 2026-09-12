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
# Upstream arm: a version bump WRITTEN to a path-dep crate's Cargo.toml is
# fleet-visible the moment it is on disk — every consumer's cargo call records
# the working-tree version, which resolves locally and fails their CI (the
# committed ref does not have it). So an uncommitted bump in an upstream tree
# is itself a fleet exposure, reported here so the bump's author sees the
# window they are holding open. Absent at the committed ref means: commit and
# push it now, or revert it.
#
# Exit: 0 all clean locks resolve and no uncommitted upstream bumps; 1 stale,
# dirty, or uncommitted bump found; 2 vacuity floor.

set -uo pipefail

ROOT="${CK_PROJECTS_ROOT:-$HOME/Work/Projects/CortexKit}"
REPOS=(engram synapse plexus claustrum astrocyte fusiform entorhinal wernicke cerebellum insula broca prefrontal thalamus callosum aft magic-context)
UPSTREAMS=(subconscious commons)

examined=0
bad=0

for name in "${UPSTREAMS[@]}"; do
  repo="$ROOT/$name"
  [ -d "$repo/.git" ] || continue
  for manifest in "$repo"/crates/*/Cargo.toml "$repo"/cortexkit-release/Cargo.toml; do
    [ -f "$manifest" ] || continue
    rel="${manifest#"$repo"/}"
    tree=$(sed -nE 's/^version *= *"([^"]+)".*/\1/p' "$manifest" | head -1)
    head=$(git -C "$repo" show "HEAD:$rel" 2>/dev/null | sed -nE 's/^version *= *"([^"]+)".*/\1/p' | head -1)
    if [ -n "$tree" ] && [ -n "$head" ] && [ "$tree" != "$head" ]; then
      echo "UNCOMMITTED-BUMP $name/$rel — working tree $tree, HEAD $head; every path consumer's next cargo call records $tree and its CI cannot resolve it (author: commit and push now, or revert)"
      bad=$((bad + 1))
    fi
  done
done
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
