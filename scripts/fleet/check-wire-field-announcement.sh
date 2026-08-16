#!/usr/bin/env bash
# Wire-field change interrupt (CALLO's design, adopted 2026-08-16).
#
# THE RULE THIS ENFORCES: a change to subc-protocol's public struct/enum
# surface must carry its consumer-impact statement IN THE COMMIT MESSAGE,
# because the announcement rule fires AT COMMIT TIME and has now been missed
# twice on intention alone (scheduled_tasks removal, ManagementSurface
# concurrency addition). A rule that fires at a moment needs something that
# interrupts at that moment; this is the interrupt.
#
# Usage: check-wire-field-announcement.sh <base-ref> <head-ref>
# Fails when any commit in the range touches field definitions in
# crates/subc-protocol/src and its message lacks a "CONSUMER-IMPACT:" line.
# The line is free-form prose after the marker; the check demands presence,
# not eloquence. Doc/comment-only edits to the same files pass: the filter
# keys on added/removed lines that look like field or variant definitions.
set -euo pipefail

BASE="${1:?usage: check-wire-field-announcement.sh <base-ref> <head-ref>}"
HEAD="${2:?usage: check-wire-field-announcement.sh <base-ref> <head-ref>}"

# Guard: both refs must resolve, and the range must be walkable.
git rev-parse --verify --quiet "$BASE" >/dev/null || { echo "base ref '$BASE' does not resolve"; exit 2; }
git rev-parse --verify --quiet "$HEAD" >/dev/null || { echo "head ref '$HEAD' does not resolve"; exit 2; }

commits=$(git rev-list "$BASE..$HEAD")
[ -z "$commits" ] && { echo "no commits in range $BASE..$HEAD; nothing to check"; exit 0; }

checked=0
failed=0
for c in $commits; do
  # -m --first-parent blindness fixed by diffing the commit against its own
  # first parent explicitly; merges inherit their branch commits' checks.
  files=$(git diff --name-only "$c^" "$c" -- 'crates/subc-protocol/src' 2>/dev/null || true)
  [ -z "$files" ] && continue
  # Field-shaped added/removed lines: "pub name: Type" / enum variant fields.
  fieldish=$(git diff "$c^" "$c" -- 'crates/subc-protocol/src' \
    | grep -E '^[+-]\s*(pub\s+\w+\s*:|\w+\s*:\s*[A-Z][A-Za-z0-9_<>:]*\s*,?\s*$)' \
    | grep -vcE '^[+-]\s*//' || true)
  [ "$fieldish" -eq 0 ] && continue
  checked=$((checked+1))
  if ! git log -1 --format=%B "$c" | grep -q "CONSUMER-IMPACT:"; then
    echo "FAIL: commit $c changes subc-protocol field definitions without a CONSUMER-IMPACT: line"
    git log -1 --format='  %h %s' "$c"
    failed=$((failed+1))
  fi
done

echo "wire-field announcement check: $checked field-touching commit(s) examined, $failed missing the line"
[ "$failed" -eq 0 ]
