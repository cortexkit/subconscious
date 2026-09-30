#!/bin/bash
# Daily sweep of cargo debug build output across the CortexKit trees.
#
# What goes: every `debug` directory inside a cargo target directory
# (target/debug, target/<triple>/debug, target/<custom-profile-dir>/debug), at
# any depth under the repos, including worktree-local and nested target dirs.
# Debug output rebuilds from source (and from the sccache cache), so deleting it
# costs a rebuild and nothing else. What stays: `release` directories, because
# staging and placement build from them, and everything that is not cargo
# output. A target directory is recognised by the CACHEDIR.TAG cargo writes
# into it; SwiftPM writes the same tag into `.build`, so the tag's text must
# name cargo.
#
# A debug directory that any running process has open (a build holding
# .cargo-lock, rustc writing an artifact, a test binary running from deps/, a
# process whose working directory is inside it) is skipped, from one lsof
# snapshot taken before the sweep. A build that starts between the snapshot and
# the delete can still fail; that is the accepted cost of running unattended.
#
# Sizes are KiB from `du -sk` (BSD du reports 512-byte blocks by default).
# Freed space is the df delta, never a sum of du figures, because APFS clones
# make the sum an upper bound. Every directory is listed in a manifest before
# it is removed.
#
# Usage: sweep-build-debris.sh [--root DIR] [--dry-run]
# Scheduled daily by the cortexkit.build-sweep launch agent.
set -u
# Paths are split on newlines only, so a directory name with a space stays one path.
IFS=$'\n'
ROOT=~/Work/Projects/CortexKit
DRY=0
while [ $# -gt 0 ]; do
  case "$1" in
    --root) ROOT="$2"; shift 2 ;;
    --dry-run) DRY=1; shift ;;
    *) echo "usage: $0 [--root DIR] [--dry-run]" >&2; exit 64 ;;
  esac
done
# lsof reports resolved paths (/private/var/..., not /var/...), so the root is
# resolved the same way or an in-use directory would not match its open files.
ROOT=$(cd "$ROOT" 2>/dev/null && pwd -P) || { echo "no such root" >&2; exit 66; }
free_gib() { df -k /System/Volumes/Data | tail -1 | awk '{print int($4/1048576)}'; }
stamp() { date -u +%Y-%m-%dT%H:%M:%SZ; }
B=$(free_gib)
RUN=~/.local/share/cortexkit/run
mkdir -p "$RUN"
MANIFEST="$RUN/build-sweep-$(date -u +%Y%m%dT%H%M%SZ).manifest"
OPEN=$(mktemp -t build-sweep-open)
trap 'rm -f "$OPEN"' EXIT
: > "$MANIFEST"
echo "$(stamp) sweep start: root $ROOT, free ${B} GiB$([ "$DRY" = 1 ] && echo ', dry run')"

# Every path any process has open, plus every working directory, one per line.
lsof -Fn 2>/dev/null | sed -n 's/^n//p' > "$OPEN"
# Programs the daemon is configured to start. A module launched straight from a
# repo's target/debug would fail at its next restart if that directory went, and
# while it is stopped nothing has the file open, so it is protected by name.
# A program inside a repo (a launcher script that runs the repo's own debug
# build, for instance) protects that whole repo, because which binary the
# launcher runs cannot be read from the config.
CONFIG=${XDG_CONFIG_HOME:-$HOME/.config}/cortexkit/subc.jsonc
PROTECTED=$(sed -n 's/.*"program"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$CONFIG" 2>/dev/null \
  | while read -r prog; do
      case "$prog" in
        ("$ROOT"/*) rel=${prog#"$ROOT"/}; echo "$ROOT/${rel%%/*}/" ;;
        (*) echo "$prog" ;;
      esac
    done | sort -u)

# Cargo target roots: directories holding a CACHEDIR.TAG that names cargo.
# node_modules is skipped: it never holds cargo output and is slow to walk.
roots=$(find "$ROOT" -name node_modules -prune -o -name CACHEDIR.TAG -print 2>/dev/null \
  | while read -r tag; do
      grep -q "created by cargo" "$tag" 2>/dev/null && dirname "$tag"
    done)

swept=0; skipped=0
# `debug` at depth 1 to 3 below a root covers the plain profile, a target
# triple and a custom target dir with a triple beneath it. Nested roots are
# roots of their own, so their debug dirs are found again; sort -u dedupes.
dirs=$(for r in $roots; do find "$r" -mindepth 1 -maxdepth 3 -type d -name debug -prune 2>/dev/null; done | sort -u)
for d in $dirs; do
  [ -d "$d" ] || continue
  if grep -qF -- "$d/" "$OPEN" || grep -qxF -- "$d" "$OPEN"; then
    echo "  SKIP (in use) $d"; skipped=$((skipped+1)); continue
  fi
  guarded=""
  for prefix in $PROTECTED; do
    case "$d/" in "$prefix"*) guarded=$prefix ;; esac
    case "$prefix" in "$d/"*) guarded=$prefix ;; esac
  done
  if [ -n "$guarded" ]; then
    echo "  SKIP (a configured module runs from $guarded) $d"; skipped=$((skipped+1)); continue
  fi
  kib=$(du -sk "$d" 2>/dev/null | cut -f1)
  echo "$kib KiB  $d" >> "$MANIFEST"
  [ "$DRY" = 1 ] || rm -rf "$d"
  swept=$((swept+1))
done
A=$(free_gib)
echo "$(stamp) manifest: $MANIFEST ($swept removed, $skipped skipped as in use)"
if [ "$DRY" = 1 ]; then
  echo "dry run: would free up to $(awk '{s+=$1} END {printf "%.0f", s/1048576}' "$MANIFEST") GiB (du sum, upper bound)"
else
  echo "$(stamp) freed $((A-B)) GiB by df delta; free now ${A} GiB"
fi
