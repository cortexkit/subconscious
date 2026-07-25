#!/usr/bin/env bash
# Fleet pulse: one screen answering "what needs me right now?"
#
# Written for the AFK window where SUBC drives the team unattended. The design
# rule is that everything here must be readable in about ten seconds, because a
# wake-up that needs interpretation gets skimmed and a skimmed signal is worse
# than none. Sections are ordered by how likely they are to demand action.
#
# Idle comes from alfonso-core's turn-boundary activity, not from peer chatter.
# The distinction is load-bearing and is documented at the seats section below.
# Either way the number tells you where to LOOK, never what is true: a seat
# heads-down on a long build is silent and healthy, a wedged seat is silent and
# stuck, and this cannot tell them apart. Confirm before acting on it.

set -uo pipefail

STORE="$HOME/.local/share/cortexkit/alfonso-core/store.db"
# Peers that have gone quiet for longer than this are surfaced for a decision.
# Roughly two of my wake cycles: long enough that a normal work stretch does not
# trip it, short enough that a stalled seat is caught within an hour.
IDLE_ALERT_MIN=${IDLE_ALERT_MIN:-90}

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
dim()  { printf '\033[2m%s\033[0m\n' "$1"; }

echo
bold "FLEET PULSE  $(date '+%Y-%m-%d %H:%M %Z')"
echo

# ---------------------------------------------------------------- module health
# First because a degraded module can be the CAUSE of peer silence, and reading
# the peer table first would send you chasing a symptom.
bold "MODULES"
if health=$(ck health 2>/dev/null); then
  ok_count=$(printf '%s\n' "$health" | grep -c '● ok')
  printf '  %s ok\n' "$ok_count"
  # Anything not ok is printed in full: these are the lines worth stopping for.
  printf '%s\n' "$health" | grep '●' | grep -v '● ok' | sed 's/^/  /'
else
  echo "  daemon unreachable -- this is the first thing to fix"
fi
echo

# ------------------------------------------------------------------ peer idleness
bold "SEATS (idle = since last turn boundary)"
# Sourced from alfonso-core's projects.overview rather than computed here. An
# earlier version measured minutes since a peer's last OUTBOUND message, which
# was materially wrong: it reported seats as hours-idle while they were actively
# working, because a busy seat talks to nobody. Turn-boundary activity is the
# real signal, the roster is keyed by session id so fleet renames cannot leave
# ghosts, and the attention counts come from the same authority.
bun run "$(dirname "$0")/fleet-idle.ts" 2>/dev/null || echo "  idle probe unavailable (alfonso-core down?)"
echo

# ------------------------------------------------------- stuck-vs-free discriminator
# The idle number above says where to look, never what is true, and its blindest
# case is a seat waiting on a campaign that already terminated: it reads exactly
# like a seat with nothing to do. A spec campaign is also the one construct a
# seat cannot cheaply poll, so if its completion is never delivered the seat
# simply waits, indefinitely, with nothing anywhere reporting a problem.
#
# The tell is an ordering: a seat whose last turn boundary falls BEFORE its
# campaign's terminal time has produced nothing since the campaign ended. Read
# it that way and not as "idle for about as long as the campaign has been
# terminal" -- a seat normally goes quiet when it FIRES a campaign and parks, so
# an idle window merely matching a campaign's total lifetime is the healthy
# case, and mistaking one for the other flags every seat that ever fired one.
#
# What this cannot tell you is WHICH of two causes produced the silence: a wake
# that was never delivered, or a wake that was delivered into a session that no
# longer generates. Both look identical from here, and they need opposite
# remedies -- resend versus restart the session. Only the host transcript
# separates them, by counting assistant turns after the wake landed.
bold "CAMPAIGNS (terminal in the last 12h -- compare against idle times above)"
terminal=$(sqlite3 "$STORE" "
  SELECT substr(consult_id, -12), consult_kind, phase,
         COALESCE(terminal_reason,'-'),
         COALESCE((SELECT name FROM peers WHERE session_id = caller_session), caller_session),
         (strftime('%s','now') - updated_at/1000)/60
  FROM consult
  WHERE phase IN ('failed','done')
    AND consult_kind IN ('spec','campaign')
    AND caller_session IS NOT NULL AND caller_session <> ''
    AND updated_at > (strftime('%s','now') - 43200) * 1000
  ORDER BY updated_at DESC LIMIT 6;" 2>/dev/null)
if [ -n "$terminal" ]; then
  printf '%s\n' "$terminal" | while IFS='|' read -r id kind phase reason who age; do
    printf '  %-14s %-8s %-7s %-26s %s  (%sm ago)\n' "$id" "$kind" "$phase" "$reason" "$who" "$age"
  done
  dim "  quiet since BEFORE one of these terminals = produced nothing after it; check the transcript for why"
else
  echo "  none terminal in the last 12h"
fi
echo

bold "INBOX"
unread=$(sqlite3 "$STORE" "
  SELECT COUNT(*) FROM peer_messages
  WHERE to_name='SUBC' AND read_at IS NULL AND discarded_at IS NULL;" 2>/dev/null || echo '?')
echo "  $unread unread"
echo

# ------------------------------------------------------------------ backup health
# Engram is the one subsystem whose failure is silent and expensive: backups stop
# and nothing else changes, so it needs an explicit line rather than a health dot.
bold "BACKUPS"
# Read the durable store, NOT `ck health engram`. Engram's health snapshot is
# recomputed only when a capture or publish RETURNS, so during the long upload
# you actually want to watch it silently reports pre-run values -- it read
# "lastPublished=75, unpublished=0" for 80 minutes while generation 78 was
# mid-flight. That is by design: the health probe answers from an atomic
# snapshot and deliberately never touches SQLite. It is a liveness probe, not a
# progress gauge, and reading it as progress makes a running backup look idle.
ENGRAM_STORE="$HOME/.local/share/cortexkit/engram/store.db"
if [ -f "$ENGRAM_STORE" ] && gens=$(sqlite3 "$ENGRAM_STORE" \
  "SELECT COUNT(*), SUM(published), MAX(CASE WHEN published=1 THEN device_seq END), MAX(device_seq) FROM generations;" 2>/dev/null) && [ -n "$gens" ]; then
  IFS='|' read -r total pub maxpub newest <<<"$gens"
  if [ "$newest" != "$maxpub" ]; then
    echo "  gen $newest uploading (last published $maxpub, $pub/$total published)"
    # The generation counter above cannot see WITHIN a generation: one that is
    # 60% uploaded and one that has not started read identically. That gap is
    # how a stalled publish hides -- it looks exactly like a slow one, and the
    # counter sits still either way for hours.
    #
    # The upload sidecar is appended as objects land, so its line count is real
    # progress. Sampling it twice is the only thing here that distinguishes
    # moving from stuck, and it costs the seconds between the two reads.
    sidecar=$(ls -t "$HOME/.local/share/cortexkit/engram/staging"/*/uploaded-*.hex 2>/dev/null | head -1)
    if [ -n "$sidecar" ]; then
      before=$(wc -l < "$sidecar" 2>/dev/null | tr -d ' ')
      sleep 20
      after=$(wc -l < "$sidecar" 2>/dev/null | tr -d ' ')
      if [ "${after:-0}" -gt "${before:-0}" ] 2>/dev/null; then
        echo "  uploading: $after objects, +$((after - before)) in 20s"
      else
        echo "  NOT MOVING: $after objects, unchanged over 20s -- check before assuming slow"
      fi
    fi
  else
    echo "  gen $maxpub published ($pub/$total)"
  fi
  # Liveness is still health's job: a module that stopped answering matters even
  # when the store looks fine.
  ck health engram 2>/dev/null | head -1 | sed 's/^/  /'
else
  # Engram now stamps its health snapshot with snapshotAgeMs, so the fallback
  # can say how old these numbers are rather than presenting them as current.
  # A large age here means a capture or publish is mid-flight and every counter
  # below is frozen at its pre-run value -- read backup.status instead.
  age=$(ck health engram 2>/dev/null | sed -n 's/.*snapshotAgeMs[": ]*\([0-9]*\).*/\1/p' | head -1)
  if [ -n "$age" ] && [ "$age" -gt 120000 ] 2>/dev/null; then
    echo "  engram store unreadable; health snapshot is $((age / 60000))m old (an operation is likely in flight)"
  else
    echo "  engram store unreadable; falling back to health"
  fi
  ck health engram 2>/dev/null | head -2 | tail -1 | sed 's/^/  /' || echo "  engram unreachable"
fi
echo

# ------------------------------------------------------------------- deploy gap
# A module can be healthy, current in git, green in CI, and still be RUNNING code
# from days ago -- found twice in one afternoon, once with ten runtime commits
# unshipped including the fix whose whole job was making a silent failure
# visible. Merging and deploying are separate steps by design, and "separate"
# becomes "forgotten" unless something counts it.
bold "DEPLOY"
BIN="$HOME/.local/share/cortexkit/bin"
printed=0
for repo_bin in "ai-proxy:ck-thalamus" "magic-context:ck-mc" "broca:ck-broca" \
                "ai-provider-quota:ck-quota" "cortexkit-credentials:ck-credentials" \
                "engram:ck-engram" "astrocyte:ck-astrocyte" "plexus:ck-plexus" \
                "subc-federation:ck-callosum"; do
  repo="${repo_bin%%:*}"; binname="${repo_bin##*:}"
  dir="$HOME/Work/Projects/CortexKit/$repo"
  [ -d "$dir" ] && [ -f "$BIN/$binname" ] || continue
  head_epoch=$(cd "$dir" && git log -1 --format='%ct' 2>/dev/null) || continue
  bin_epoch=$(stat -f '%m' "$BIN/$binname" 2>/dev/null) || continue
  # Only hours-scale gaps are worth a line. A binary minutes older than its head
  # is the normal state right after a deploy, and flagging it trains the reader
  # to ignore this section.
  gap_h=$(( (head_epoch - bin_epoch) / 3600 ))
  [ "$gap_h" -ge 6 ] || continue

  # A time gap alone cannot tell a stale deploy from a busy repo: a CI-workflow
  # or docs commit moves HEAD and reads exactly like unshipped runtime code. So
  # ask what the gap is MADE OF -- if nothing in it reaches the running binary,
  # there is nothing to deploy and the line would be a false alarm that costs the
  # owner an interruption and costs this section its credibility.
  #
  # Deliberately generous about what counts as reaching the binary: a path this
  # does not recognise is reported rather than dismissed, because a missed stale
  # deploy is far more expensive than an extra line to check.
  since=$(date -r "$bin_epoch" '+%Y-%m-%d %H:%M:%S' 2>/dev/null) || continue
  runtime=$(cd "$dir" && git log --since="$since" --format='' --name-only 2>/dev/null \
    | grep -E '\.(rs|toml)$' \
    | grep -v -E '(^|/)(tests?|benches|examples)/' \
    | grep -v -E '(^|/)bin/.*_cli\.rs$' \
    | sort -u)
  if [ -z "$runtime" ]; then
    dim "  $repo: ${gap_h}h gap, but nothing in it reaches the binary (ci/docs/tests only)"
    continue
  fi
  n=$(echo "$runtime" | wc -l | tr -d ' ')
  printf '  %-16s binary is %sh behind master, %s runtime file(s) unshipped -- ask the owner\n' \
    "$repo" "$gap_h" "$n"
  echo "$runtime" | head -3 | sed 's/^/      /'
  printed=1
done
[ "$printed" -eq 0 ] && echo "  no binary more than 6h behind its master"
# The boundary belongs in the output, not in someone's memory of how this works.
# Without it the next reader takes a clean result as fleet-wide, which it is not:
# a stale Cloudflare Worker has no local mtime and cannot appear here at all --
# and when a Worker rejects first, a freshly deployed binary behind it still
# fails. mtime is also only a screen: proving WHICH build runs needs the artifact
# itself (inode for no-replace-in-place, symbol presence for which build).
dim "  covers local binaries only -- cloud-deployed code cannot appear here"
echo

dim "idle is a proxy for attention, not for progress -- confirm before acting"
