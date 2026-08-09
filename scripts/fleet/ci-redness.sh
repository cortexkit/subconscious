#!/usr/bin/env bash
# How long has each fleet repo's default branch been failing CI?
#
# Written after this repository failed CI on every push for 22 hours and about 30
# runs without anyone noticing, including me. The first failure was my own commit,
# and I found it only because I opened CI for an unrelated reason. Nothing else
# reports this: module health says a module is serving and the deploy section says
# which binary is stale, and both were correctly green throughout.
#
# THE MEASURE IS AGE, NOT COLOUR. A red run is normal and usually means someone
# is mid-fix; a branch red for a day means nobody is looking. Reporting every red
# would train the reader to skim exactly the section that matters, so a fresh
# failure is DIM and an old one is loud.
#
# Cost: one API call per repo, ~840ms, run in parallel. Serial would be ~20s,
# which is past the point where a wake-up report gets skipped.

set -uo pipefail

ROOT="$HOME/Work/Projects/CortexKit"
# Older than this and the redness is not a fix in progress. Deliberately shorter
# than a working day: the case this exists for ran 22 hours.
STALE_H=${CI_STALE_H:-4}

# DISCOVER the repos, never list them. A transcribed list agrees with the fleet
# on the day it is typed and silently omits every repo added afterwards -- the
# same failure that had the deploy check covering nine of fourteen modules while
# reporting the fleet current.
#
# `.git` must be a DIRECTORY. In a worktree it is a FILE pointing at the parent,
# and a worktree is a checkout of a repo already covered here -- sitting on a task
# branch, so its "default branch" is a branch nobody merges to. Including them
# added four entries that reported UNCHECKED forever: noise that trains the reader
# to skim the section, which is the failure this file exists to prevent.
# --porcelain emits one TAB-SEPARATED row per repo INCLUDING GREEN ONES, for
# other seats to wire into their own sweeps:
#   repo <TAB> state(green|red|unchecked) <TAB> red_hours <TAB> tip_sha <TAB> tip_is_failing
# red_hours is -1 when unknown and -2 when no success exists in the fetched page
# (a LOWER BOUND, not a duration). tip_is_failing is yes/no_run_yet/unknown and
# answers a different question from red_hours: whether the CURRENT tip is broken,
# which is what a merge gate cares about, versus how long the branch has been
# broken, which is what neglect-hunting cares about.
PORCELAIN=""
[ "${1:-}" = "--porcelain" ] && PORCELAIN=1

repos=()
for dir in "$ROOT"/*/; do
  name=$(basename "$dir")
  [ -d "$dir/.git" ] || continue
  [ -d "$dir/.github/workflows" ] || continue
  repos+=("$name")
done

if [ ${#repos[@]} -eq 0 ]; then
  echo "  no repos with workflows found under $ROOT -- CI state UNCHECKED"
  exit 1
fi

# One repo's line, emitted to a temp file so the calls can run concurrently.
probe() {
  local name=$1 out=$2 dir="$ROOT/$name"

  # DERIVE the default branch. The fleet is split between master and main, so a
  # hardcoded name returns ZERO RUNS for half of it -- and zero runs is
  # indistinguishable from zero failures unless something checks. Ask the repo.
  local branch
  branch=$(git -C "$dir" symbolic-ref --short HEAD 2>/dev/null)
  if [ -z "$branch" ]; then
    printf '  %-22s UNCHECKED: cannot resolve default branch\n' "$name" >"$out"
    return
  fi

  local runs
  if ! runs=$(gh run list --repo "cortexkit/$name" --branch "$branch" --limit 20 \
      --json conclusion,status,createdAt,displayTitle 2>&1); then
    printf '  %-22s UNCHECKED: %s\n' "$name" "$(printf '%s' "$runs" | head -1)" >"$out"
    return
  fi

  # Completed runs only: a run still executing has no verdict, and counting it
  # either way invents one. Cancelled is also excluded -- a superseded run says
  # nothing about the commit, and treating it as a failure would flag every repo
  # where someone pushes twice quickly.
  local latest
  latest=$(printf '%s' "$runs" | jq -r '[.[] | select(.status=="completed" and .conclusion!="cancelled")][0].conclusion // "none"')

  case "$latest" in
    none)
      # No verdict at all is NOT clean. A repo whose workflows never run looks
      # exactly like one that always passes, and the difference is the whole
      # question.
      if [ -n "$PORCELAIN" ]; then
        printf '%s\tunchecked\t-1\t%s\t-\n' "$name" "$(git -C "$dir" rev-parse --short "origin/$branch" 2>/dev/null || echo -)" >"$out"
      else
        printf '  %-22s no completed runs on %s -- UNCHECKED\n' "$name" "$branch" >"$out"
      fi
      ;;
    success)
      # PORCELAIN EMITS GREEN ROWS; the human report does not. A consumer that
      # only ever sees problem rows cannot tell a clean fleet from a scan that
      # did not run, which is the failure this whole file exists to prevent --
      # and it is a failure the human reader is protected from by the examined
      # count at the end.
      if [ -n "$PORCELAIN" ]; then
        printf '%s\tgreen\t0\t%s\t-\n' "$name" "$(git -C "$dir" rev-parse --short "origin/$branch" 2>/dev/null || echo -)" >"$out"
      else
        : >"$out"
      fi
      ;;
    *)
      # Red. Age it from the last SUCCESS, which is what separates "someone is
      # fixing this" from "nobody is looking".
      local last_ok since=0 hours title last_run last_run_epoch
      last_ok=$(printf '%s' "$runs" | jq -r '[.[] | select(.conclusion=="success")][0].createdAt // ""')
      last_run=$(printf '%s' "$runs" | jq -r '[.[] | select(.status=="completed" and .conclusion!="cancelled")][0].createdAt // ""')
      title=$(printf '%s' "$runs" | jq -r '[.[] | select(.status=="completed" and .conclusion!="cancelled")][0].displayTitle' | cut -c1-40)
      if [ -n "$last_ok" ]; then
        # -u IS LOAD-BEARING. `date -j -f` MATCHES the trailing Z in the format
        # without INTERPRETING it, so the timestamp is parsed as local time and
        # every epoch comes out shifted by this machine's UTC offset. In the age
        # below that is a couple of hours on a scale of tens and reads as
        # plausible; in the comparison against the commit time it FLIPS A BOOLEAN,
        # and it did: astrocyte reported "pushed to since" on a branch that had not
        # moved in 18 days, because its last commit landed 120 minutes -- exactly
        # the offset -- "after" a run that actually followed it.
        since=$(date -j -u -f '%Y-%m-%dT%H:%M:%SZ' "$last_ok" '+%s' 2>/dev/null || echo 0)
        if [ "$since" -gt 0 ]; then
          hours=$(( ( $(date -u +%s) - since ) / 3600 ))
        else
          hours=-1
        fi
      else
        # No success in the window at all: older than this page can see, so the
        # honest answer is a lower bound rather than a number.
        hours=-2
      fi

      # DORMANT OR ACTIVELY BREAKING? Both are red and they need opposite
      # responses: a branch nobody has pushed to since the failure is a repo to
      # revisit, while a branch taking commits ON TOP of a failure means people
      # are building on a broken base right now. Without this the two render
      # identically and the loud number belongs to whichever failed longest ago,
      # which is usually the least urgent one.
      #
      # The reference point is the LAST RUN, not the last green. Comparing
      # against the last green called astrocyte "still being pushed to" when its
      # master had not moved in 18 days: every commit after a green is trivially
      # newer than that green, so the comparison answers "has anything happened
      # since things worked", which is true of every red repo by construction and
      # therefore discriminates nothing.
      local pushed_since="" head_epoch
      last_run_epoch=$(date -j -u -f '%Y-%m-%dT%H:%M:%SZ' "${last_run:-}" '+%s' 2>/dev/null || echo 0)
      head_epoch=$(git -C "$dir" log -1 --format=%ct "origin/$branch" 2>/dev/null || echo 0)
      if [ "$head_epoch" -gt 0 ] && [ "$last_run_epoch" -gt 0 ]; then
        if [ "$head_epoch" -gt "$last_run_epoch" ]; then
          pushed_since=" -- PUSHED TO SINCE, CI not re-run"
        else
          pushed_since=" -- dormant, no push since"
        fi
      fi

      if [ -n "$PORCELAIN" ]; then
        # tip_is_failing answers "is the CURRENT tip broken" -- distinct from
        # red_hours, which is about the branch. A repo can be red for a day with
        # a tip nobody has run yet, and a consumer gating a merge cares about the
        # tip while a consumer chasing neglect cares about the duration.
        local tip_failing="unknown"
        [ "$last_run_epoch" -gt 0 ] && [ "$head_epoch" -gt 0 ] && {
          if [ "$head_epoch" -gt "$last_run_epoch" ]; then tip_failing=no_run_yet; else tip_failing=yes; fi
        }
        printf '%s\tred\t%s\t%s\t%s\n' "$name" "$hours" \
          "$(git -C "$dir" rev-parse --short "origin/$branch" 2>/dev/null || echo -)" "$tip_failing" >"$out"
      elif [ "$hours" -ge "$STALE_H" ]; then
        printf '  %-22s RED for ~%sh (last green %s)%s  %s\n' "$name" "$hours" "${last_ok:0:16}" "$pushed_since" "$title" >"$out"
      elif [ "$hours" -eq -2 ]; then
        printf '  %-22s RED, no green in the last 20 runs  %s\n' "$name" "$title" >"$out"
      elif [ "$hours" -lt 0 ]; then
        printf '  %-22s RED, age unknown (unparseable timestamp)  %s\n' "$name" "$title" >"$out"
      else
        printf '  \033[2m%-22s red %sh -- likely a fix in progress\033[0m\n' "$name" "$hours" >"$out"
      fi
      ;;
  esac
}

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
for name in "${repos[@]}"; do
  probe "$name" "$tmp/$name" &
done
wait

printed=0
for name in "${repos[@]}"; do
  if [ -s "$tmp/$name" ]; then
    cat "$tmp/$name"
    printed=1
  fi
done

# ALWAYS state the denominator. A silent section is ambiguous between "all green"
# and "the loop ran zero times", and this file exists because a clean-looking
# absence hid a real failure for a day.
# The denominator goes to STDERR in porcelain mode, never stdout. A consumer
# splitting stdout on tabs would read this line as a malformed repo row -- but it
# must still be EMITTED, because it is the only thing distinguishing "scanned 24,
# all green" from "scanned nothing". Dropping it to keep the stream clean would
# trade a parse error for a silent false clean, which is the worse failure.
if [ -n "$PORCELAIN" ]; then
  printf 'examined %s repos\n' "${#repos[@]}" >&2
elif [ "$printed" -eq 0 ]; then
  printf '  all %s repos green on their default branch\n' "${#repos[@]}"
else
  printf '  (%s repos examined)\n' "${#repos[@]}"
fi
