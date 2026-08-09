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

for crate in "${CRATES[@]}"; do
  src="crates/$crate/src"
  manifest="crates/$crate/Cargo.toml"
  [ -d "$src" ] || continue
  examined=$((examined + 1))

  # Every added/removed line under src/, minus the diff's own +++/--- headers.
  # A rename or a pure move shows up here too, which is correct: a consumer
  # compiles the result either way.
  changed=$(git diff "$BASE"...HEAD -- "$src" \
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

  # The version line must have moved in the same range.
  if git diff "$BASE"...HEAD -- "$manifest" | grep -qE '^\+version[[:space:]]*='; then
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
