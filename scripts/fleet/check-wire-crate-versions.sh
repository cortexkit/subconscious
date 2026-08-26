#!/usr/bin/env bash
# Refuse a change to a cross-repo crate's code that does not move its version.
#
# Seven repos in this fleet consume these crates BY RELATIVE PATH -- broca,
# engram, claustrum, callosum, astrocyte, insula, synapse -- so they float to
# whatever this repo's HEAD is. A path dependency is recorded in a consumer's
# Cargo.lock as a bare version string with NO source and NO checksum, so
# `cargo build --locked` over there cannot see that the code moved. Two cases:
#
#   version moved     -> the consumer's --locked build FAILS until they take it
#   version unchanged -> the new code is silently compiled in, lock byte-identical
#
# The second is the common one and there is no signal anywhere. Measured on this
# repo when the hazard was found: subc-protocol had 5 source commits in 30 days
# and 0 version bumps. Nothing broke, because those changes happened to be
# additive -- which is luck, not a mechanism.
#
# DOC-ONLY CHANGES ARE EXEMPT DELIBERATELY. Three of those five commits were
# doc comments. A rule that fires on prose gets ignored on substance, so this
# asks whether the OUTPUT can move, not whether a file did -- the same
# discriminator a corpus regeneration check uses.
set -uo pipefail

BASE="${1:-}"
if [ -z "$BASE" ]; then
  echo "usage: $0 <base-ref>   (e.g. origin/master, HEAD~1)" >&2
  exit 2
fi

if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
  echo "  base ref '$BASE' does not resolve -- cannot compare, refusing rather than passing" >&2
  exit 2
fi

# Resolvability is not comparability (#72): a shallow checkout or a merge
# commit fetched without its history can hold a ref that RESOLVES while
# `$BASE...HEAD` has NO MERGE BASE -- every diff then fails, and a swallowed
# failure reads as "unchanged", which composes into a confident all-clear over
# zero comparisons. Prove the comparison is possible before counting anything.
if ! git merge-base "$BASE" HEAD >/dev/null 2>&1; then
  echo "  no merge base between '$BASE' and HEAD (shallow checkout?) -- cannot compare, refusing rather than passing" >&2
  exit 2
fi

# The crates other repos actually path-depend on. Derived by sweeping sibling
# manifests for `path = "../subconscious/crates/<name>"` rather than assumed;
# re-derive with that sweep rather than editing from memory.
CRATES=(
  subc-protocol
  subc-transport
  subc-core
  subc-control
  subc-jsonc
  subc-client-rs
)

examined=0
violations=0

# This check compares COMMITS ($BASE...HEAD). Uncommitted working-tree edits
# are invisible to it, so "clean" here answers a narrower question than "is
# the tree consistent". Say so instead of letting the narrow answer read as
# the broad one (a wire-crate edit sitting unstaged would pass silently).
if ! git diff --quiet -- crates/ 2>/dev/null; then
  echo "  note: uncommitted changes exist under crates/ -- this check reads" >&2
  echo "  committed history only ($BASE...HEAD) and does not see them." >&2
fi

for crate in "${CRATES[@]}"; do
  src="crates/$crate/src"
  manifest="crates/$crate/Cargo.toml"
  [ -d "$src" ] || continue

  # Every added/removed line under src/, minus the diff's own +++/--- headers.
  # A rename or a pure move shows up here too, which is correct: a consumer
  # compiles the result either way. The diff's OWN failure must stay separate
  # from its empty output (#72): capture the exit code before filtering, and
  # count the crate as examined only once its comparison actually ran.
  raw_diff=$(git diff "$BASE"...HEAD -- "$src" 2>&1)
  diff_rc=$?
  if [ "$diff_rc" -ne 0 ]; then
    echo "  $crate: git diff failed (rc=$diff_rc): $raw_diff" >&2
    echo "  a failed comparison is not an unchanged crate -- refusing" >&2
    exit 2
  fi
  examined=$((examined + 1))
  changed=$(printf '%s\n' "$raw_diff" \
    | grep -E '^[+-]' | grep -vE '^(\+\+\+|---)' || true)
  [ -z "$changed" ] && continue

  # Strip comment and blank lines. Doc comments (///, //!), line comments (//),
  # and block-comment bodies (/*, *, */) are all prose: they cannot change what
  # a consumer compiles.
  substantive=$(printf '%s\n' "$changed" \
    | sed -E 's/^[+-][[:space:]]*//' \
    | grep -vE '^(///|//!|//|/\*|\*|\*/)' \
    | grep -vE '^[[:space:]]*$' || true)
  [ -z "$substantive" ] && continue

  # The version line must have moved in the same range. Same separation: a
  # failed manifest diff must refuse, not read as "version did not move".
  manifest_diff=$(git diff "$BASE"...HEAD -- "$manifest" 2>&1)
  if [ $? -ne 0 ]; then
    echo "  $crate: manifest diff failed: $manifest_diff -- refusing" >&2
    exit 2
  fi
  if printf '%s\n' "$manifest_diff" | grep -qE '^\+version[[:space:]]*='; then
    continue
  fi

  cur=$(grep -m1 -E '^version[[:space:]]*=' "$manifest" | sed -E 's/.*"(.*)".*/\1/')
  echo "  $crate: code changed, version still $cur"
  echo "      seven repos path-depend on these crates and cannot see this."
  echo "      bump $manifest, or confirm the change is doc-only."
  violations=$((violations + 1))
done

# A run that examined nothing is not a pass. If the crate list stops matching
# the tree -- renamed, moved, restructured -- this would otherwise report clean
# over an empty set, which is the failure the whole file exists to prevent.
if [ "$examined" -eq 0 ]; then
  echo "  no crates matched under crates/ -- the crate list is stale, refusing" >&2
  exit 2
fi

if [ "$violations" -gt 0 ]; then
  echo "  $violations of $examined cross-repo crates changed without a version bump"
  exit 1
fi

echo "  $examined cross-repo crates examined against $BASE, none changed without a version bump"
