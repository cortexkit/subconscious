#!/usr/bin/env bash
# One-shot sweep of the EXISTING test-temp-dir orphan population.
#
# Before the RAII `TestTempDir` guard (crates/subc-core/src/test_support.rs),
# every test helper minted a uniquely-named directory directly under the OS
# temp dir and never removed it on failure. The name IS the lifecycle and it
# dies with the process, so no cleanup pass can be written after the fact --
# which is why this is a one-shot sweep of a bounded, measured population, not
# a recurring janitor. Future orphans live under `subc-tests/` (one recognizable
# parent) and are attributable by directory listing; this script does NOT touch
# that parent.
#
# The measured census (issue #85, 2026-08-30): 11,345 entries / 14 GB over ten
# days, dominated by `subc-control-*`, `subc-core-*`, `subc-client-rs-*` and
# `fake-aft-stub-copy*`. The prefix set below is bounded to the patterns the
# pre-guard test tree actually minted (enumerated from its temp_dir call sites);
# nothing else is eligible.
#
# Safety contract:
#   - age > 48h (mtime), so a live test run's fresh dirs are never touched.
#   - manifest-before-first-unlink: the full file list is written to a manifest
#     path printed on stdout BEFORE anything is deleted.
#   - --dry-run is the default; --execute is required to delete.
#   - no bare `&&` chains: every step is an explicit, checked statement.
#   - `subc-tests/` (the guard's parent) is refused outright, even if a prefix
#     would match it.
#
# Usage: sweep-test-temp-orphans.sh [--execute]

set -uo pipefail

EXECUTE=0
[ "${1:-}" = "--execute" ] && EXECUTE=1

# The OS temp dir, resolved to its PHYSICAL path. `mktemp -d` would create a
# NEW dir; we need the real one. Resolving matters because on macOS `/tmp` is a
# symlink to `/private/tmp` and `find` does not follow a symlink root, so a
# literal `/tmp` would silently match nothing.
TMP_ROOT="$(cd "${TMPDIR:-/tmp}" && pwd -P)"
# Age threshold: only delete entries whose mtime is older than this.
AGE_MINUTES=$((48 * 60))

# Bounded prefix set, from the pre-guard test-tree enumeration. Each entry is a
# glob fragment matched against the top-level temp dir. `subc-tests/` (the
# guard's parent) is deliberately NOT here and is refused below.
PREFIXES=(
  # Measured census (issue #85).
  'subc-control-*'
  'subc-core-*'
  'subc-client-rs-*'
  'fake-aft-stub-copy*'
  # Integration-test helpers (tests/common, watchdog, forwarding, reverse_request).
  'sc-*'
  'sc-closure-*'
  'subc-auth-handshake-*'
  'subc-route-open-*'
  'subc-missing-enable-program-*'
  'subc-fleet-lint-*'
  'subc-provenance-*'
  'subc-core-identity-*'
  # src unit-test fixtures (setup/*, bootstrap, bin/ck).
  'ck-upgrade-*'
  'ck-runtime-*'
  'ck-self-update-*'
  'ck-self-update-windows-*'
  'ck-self-update-unix-*'
  'ck-setup-*'
  'ck-update-cache-*'
  'ck-update-check-*'
  'ck-components-*'
  'ck-uninstall-*'
  'ck-mc-detection-*'
  'ck-triage-*'
)

# Collect candidates. `find` with a single top-level dir and a bounded set of
# -name predicates; `-maxdepth 1` keeps us from descending into anything. We
# match directories only (the orphans are dirs; the .json cache files are files
# and are swept by their parent dir's removal).
CANDIDATES=()
for prefix in "${PREFIXES[@]}"; do
  while IFS= read -r entry; do
    [ -n "$entry" ] && CANDIDATES+=("$entry")
  done < <(find "$TMP_ROOT" -maxdepth 1 -type d -name "$prefix" 2>/dev/null)
done

# De-duplicate (a path can match more than one prefix) while preserving order.
# bash 3.2 (macOS default) has no associative arrays; a linear scan is fine for
# a bounded population.
DEDUP=()
for entry in "${CANDIDATES[@]:-}"; do
  [ -z "$entry" ] && continue
  dup=0
  for prior in "${DEDUP[@]:-}"; do
    if [ "$prior" = "$entry" ]; then dup=1; break; fi
  done
  [ "$dup" -eq 0 ] && DEDUP+=("$entry")
done

# Explicit refusal: never touch the guard's parent or anything under it. A
# future population there is attributable and out of scope for this one-shot
# sweep; deleting it would erase the evidence the guard deliberately preserves.
GUARD_PARENT="$TMP_ROOT/subc-tests"
REFUSED=0
ORPHANS=()
for entry in "${DEDUP[@]:-}"; do
  [ -z "$entry" ] && continue
  case "$entry" in
    "$GUARD_PARENT"|"$GUARD_PARENT"/*)
      echo "REFUSING (guard parent, out of scope): $entry"
      REFUSED=$((REFUSED + 1))
      continue
      ;;
  esac
  # Age filter: keep only entries older than AGE_MINUTES.
  if [ -n "$(find "$entry" -maxdepth 0 -mmin +"$AGE_MINUTES" 2>/dev/null)" ]; then
    ORPHANS+=("$entry")
  fi
done

echo "temp root: $TMP_ROOT"
echo "candidates (all prefixes): ${#DEDUP[@]}"
echo "refused (guard parent): $REFUSED"
echo "orphans (age > 48h): ${#ORPHANS[@]}"

if [ "${#ORPHANS[@]}" -eq 0 ]; then
  echo "nothing to sweep"
  exit 0
fi

# Manifest-before-first-unlink: write the full file list to a manifest path and
# print it BEFORE deleting anything. The manifest is the audit record of what
# this run removed.
MANIFEST="$(mktemp "${TMP_ROOT}/subc-test-temp-sweep-manifest.XXXXXX")"
{
  echo "# subc test-temp orphan sweep manifest"
  echo "# generated: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "# age threshold: ${AGE_MINUTES}m"
  for entry in "${ORPHANS[@]}"; do
    echo "$entry"
  done
} >"$MANIFEST"
echo "manifest: $MANIFEST"

if [ "$EXECUTE" -eq 0 ]; then
  echo "(dry run; pass --execute to delete)"
  exit 0
fi

# Explicit, checked deletion loop -- no bare `&&` chains. Each removal is
# attempted and its failure is reported, not swallowed.
removed=0
failed=0
for entry in "${ORPHANS[@]}"; do
  if rm -rf -- "$entry"; then
    removed=$((removed + 1))
  else
    echo "FAILED to remove: $entry"
    failed=$((failed + 1))
  fi
done

echo "removed: $removed"
echo "failed: $failed"
if [ "$failed" -gt 0 ]; then
  echo "some entries could not be removed; see manifest: $MANIFEST"
  exit 1
fi
