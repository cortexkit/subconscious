#!/usr/bin/env bash
# Report which repos under a projects directory have no working off-machine copy.
#
# PROBES the remote rather than reading `git remote -v`. A configured remote
# pointing at a repository that does not exist renders identically to a working
# one -- the config records an intention, only the probe establishes the fact.
# Found live 2026-07-26: a 1,465-commit repo classified as protected by a
# config-only survey, whose remote returned "Repository not found" and whose
# every push had been failing silently.
#
# Reports three states, and DEAD is deliberately louder than ABSENT because it
# is the one that masquerades as safe:
#   DEAD REMOTE  configured, unreachable  -- believed protected, is not
#   NO REMOTE    nothing configured       -- visibly unprotected
#   UNPUSHED     reachable, commits ahead -- protected but stale
#
# Usage: check-repo-protection.sh [projects-dir]   (default ~/Work/Projects/CortexKit)

set -uo pipefail

# The two probes every verdict rests on, named once and used everywhere below.
# The premise line prints these rather than describing them: a sentence beside
# the code it describes can disagree with it, and a premise that disagrees is
# worse than none, because a reader who checks it is checking a claim rather
# than the rule.
REACHABLE_PROBE="ls-remote --heads origin"
AHEAD_PROBE="rev-list --count @{u}..HEAD"

ROOT="${1:-$HOME/Work/Projects/CortexKit}"
[ -d "$ROOT" ] || { echo "no such directory: $ROOT"; exit 1; }
cd "$ROOT" || exit 1

# POSITIVE CONTROL: the probe must succeed somewhere. If every ls-remote fails,
# the cause is this machine's network or credentials, and every "DEAD REMOTE"
# below would be an artifact rather than a finding -- the failure mode where an
# error blamed on your own environment stops being investigated, inverted.
control=""
for d in */; do
  [ -d "$d/.git" ] || continue
  git -C "$d" remote get-url origin >/dev/null 2>&1 || continue
  # shellcheck disable=SC2086 # the probe is a fixed word list, not user input
  if timeout 25 git -C "$d" $REACHABLE_PROBE >/dev/null 2>&1; then control="${d%/}"; break; fi
done
if [ -z "$control" ]; then
  echo "REFUSING: no remote anywhere was reachable."
  echo "The probe cannot produce a success, so every unreachable result below would be"
  echo "a statement about this machine rather than about any repository."
  exit 2
fi
echo "probe control: ${control} reachable"
# A repo is judged by whether its origin answers right now -- not by what its
# config says, and not by any other remote it may have. The findings look
# identical under a different rule, so a reader who would disagree cannot tell
# from them that a choice was made.
echo "premise: protected means 'git $REACHABLE_PROBE' succeeds and 'git $AHEAD_PROBE' is 0"
echo

# The examined count is reported alongside the findings, not just the findings.
# This tool hunts an absence, and every way it can break -- a wrong root, a
# skipped directory, a probe that silently fails -- removes repositories from
# consideration rather than adding them. So its bugs and its findings both
# render as "fewer problems here", and a clean result is indistinguishable from
# a scan that examined nothing. The denominator is what separates them.
examined=0
dead=0; none=0; stale=0
for d in */; do
  [ -d "$d/.git" ] || continue
  examined=$((examined+1))
  name="${d%/}"
  url=$(git -C "$d" remote get-url origin 2>/dev/null)
  commits=$(git -C "$d" rev-list --count HEAD 2>/dev/null || echo 0)

  if [ -z "$url" ]; then
    printf 'NO REMOTE     %-24s %6s commits\n' "$name" "$commits"
    none=$((none+1)); continue
  fi
  # shellcheck disable=SC2086 # the probe is a fixed word list, not user input
  if ! timeout 25 git -C "$d" $REACHABLE_PROBE >/dev/null 2>&1; then
    printf 'DEAD REMOTE   %-24s %6s commits  -> %s\n' "$name" "$commits" "$url"
    dead=$((dead+1)); continue
  fi
  # Reachable. Ahead of upstream is a weaker problem but still unprotected work.
  # shellcheck disable=SC2086 # the probe is a fixed word list, not user input
  ahead=$(git -C "$d" $AHEAD_PROBE 2>/dev/null || echo 0)
  if [ "${ahead:-0}" -gt 0 ]; then
    printf 'UNPUSHED      %-24s %6s ahead\n' "$name" "$ahead"
    stale=$((stale+1))
  fi
done

  echo
  echo "examined: $examined repos   dead remotes: $dead   no remote: $none   unpushed: $stale"
  [ $((dead+none+stale)) -eq 0 ] && echo "every repo has a reachable remote and nothing local-only"
