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
UPSTREAMS=(subconscious commons)

# THE CONSUMER SET IS DISCOVERED, NEVER WRITTEN DOWN.
#
# It was a hardcoded 16-name array until 2026-09-18, and it was wrong in both
# directions: it MISSED alfonso-tui, which path-deps subc-client-rs and
# subc-protocol from its root manifest and had therefore never had a lock
# checked; and it LISTED aft and thalamus, which consume no path deps at all
# (thalamus pins subc by git rev, so a bump here cannot reach it until it
# repins). Neither error was visible from the script's output: it reported
# "examined 16 path-dependent repos" whether or not the seventeenth existed.
#
# A hardcoded list is A CLAIM ABOUT ANOTHER FILE, and this guard cannot tell you
# when that claim expires -- which is the same defect one layer up from the one
# it exists to catch (FUSI, who hit it building the mirror of this check).
#
# Discovery reads the fleet convention: root manifest plus crates/*/Cargo.toml,
# looking for a path dep naming an upstream. Local clones are excluded by their
# origin pointing inside $ROOT, so a scratch copy of a seat is not counted as a
# second seat.
discover_consumers() {
  local repo name origin
  for repo in "$ROOT"/*/; do
    name=$(basename "$repo")
    [ -d "$repo/.git" ] || continue
    case " ${UPSTREAMS[*]} " in *" $name "*) continue ;; esac # nonempty-array: two literal upstreams
    origin=$(git -C "$repo" remote get-url origin 2>/dev/null || true)
    case "$origin" in "$ROOT"/*) continue ;; esac
    if [ "$(cat "$repo"/Cargo.toml "$repo"/crates/*/Cargo.toml 2>/dev/null \
         | grep -cE '^[a-z0-9_-]+ *= *\{[^}]*path *= *"[^"]*(subconscious|commons)/')" -gt 0 ]; then
      printf '%s\n' "$name"
    fi
  done
}

REPOS=()
while IFS= read -r line; do REPOS+=("$line"); done < <(discover_consumers)

examined=0
bad=0

for name in "${UPSTREAMS[@]}"; do # nonempty-array: two literal upstreams
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
for name in ${REPOS[@]+"${REPOS[@]}"}; do
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
