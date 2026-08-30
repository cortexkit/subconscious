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

# RESOLVE THE EXECUTIVE'S STORE, DO NOT NAME IT. The module id changes at the
# prefrontal rename, and a hardcoded name goes blind EXACTLY AT THE FLIP -- the one
# moment this report is being read closely. A name search inherits every renaming
# anyone has ever done; the first path that exists is a question about the resource.
# Ordered new-name-first so the window needs no edit here, with the old name as the
# fallback until it is gone.
STORE=""
for candidate in prefrontal-core prefrontal alfonso-core; do
  if [ -f "$HOME/.local/share/cortexkit/$candidate/store.db" ]; then
    STORE="$HOME/.local/share/cortexkit/$candidate/store.db"
    break
  fi
done
[ -n "$STORE" ] || STORE="$HOME/.local/share/cortexkit/alfonso-core/store.db"
# Peers that have gone quiet for longer than this are surfaced for a decision.
# Roughly two of my wake cycles: long enough that a normal work stretch does not
# trip it, short enough that a stalled seat is caught within an hour.
IDLE_ALERT_MIN=${IDLE_ALERT_MIN:-90}

bold() { printf '\033[1m%s\033[0m\n' "$1"; }
dim()  { printf '\033[2m%s\033[0m\n' "$1"; }

# Run a query and FAIL LOUDLY. Callers were `$(sqlite3 ... 2>/dev/null)`, which
# returns empty on a renamed column, a locked store or a missing table -- and
# empty is a legal answer everywhere here, so a failed probe printed a clean
# operational claim. The file-readability guards elsewhere in this script do not
# cover it: a store can be perfectly readable while the QUERY is wrong.
#
# The message goes to STDERR deliberately. This is called from inside command
# substitution, so a warning on stdout would be captured into the caller's
# variable and never seen -- which is how ENGRAM's equivalent fix swallowed its
# own failures ten minutes after being written to prevent swallowing. Returning
# nonzero is what lets a caller separate empty-result from did-not-run.
#
# DEFINED HERE, ABOVE EVERY CALLER. Placed further down it parsed fine and was
# simply not yet defined at first use, so every guarded query failed with
# "command not found" -- a fix that manufactured the failures it reports.
sq() {
  local db=$1 q=$2 out
  if ! out=$(sqlite3 "$db" "$q" 2>&1); then
    echo "  QUERY FAILED on $(basename "$db"): $out" >&2
    return 1
  fi
  printf '%s' "$out"
}

echo
bold "FLEET PULSE  $(date '+%Y-%m-%d %H:%M %Z')"
echo

# ---------------------------------------------------------------- module health
# First because a degraded module can be the CAUSE of peer silence, and reading
# the peer table first would send you chasing a symptom.
# NOT-OK LINES ARE REPORTED BY PERSISTENCE, NOT BY MAGNITUDE. Some gauges are
# structurally noisy: alfonso-core's delivery counter samples on a 30s cadence
# while intents legitimately live 29-59s in flight, so nearly every refresh
# freezes one to three mid-flight rows and renders "degraded". Reporting that each
# cycle trains the reader to wave through the one gauge that will eventually mean
# something -- an instrument whose complaints you have learned to dismiss is
# indistinguishable from one that has stopped working.
#
# So the detail line is remembered between cycles and surfaced only when it is
# UNCHANGED, which is the module owner's own triage trigger: the same condition
# frozen across two readings is a stall, a different one is traffic. A first
# sighting says so explicitly rather than staying silent, because silence on a
# genuine new failure is the expensive direction.
bold "MODULES"
if health=$(ck health 2>/dev/null); then
  ok_count=$(printf '%s\n' "$health" | grep -c '● ok')
  # COUNT WHAT SHOULD EXIST, NOT ONLY WHAT ANSWERED. This line reported what the
  # daemon RETURNED, with nothing to compare it against -- so a module removed from
  # the config, or never spawned, read as a smaller count and NO not-ok line. That
  # is QUIETER than a degraded module, which is the wrong direction: the more
  # completely a module is gone, the less this section says about it. The seats
  # section already learned this and prints MISSING against an expected roster; the
  # config is the equivalent authority here, and it is the same file the daemon
  # spawns from rather than a list maintained beside it.
  # NAME-DIFF, not count-diff: counts cancel (one configured module missing plus
  # one unconfigured registrant reads as equal counts and says nothing), and a
  # count cannot tell an operator WHICH module to look for. The names are the
  # signal. This absence gauge is load-bearing for the reserved-name residual:
  # boot-time spawn failure is non-fatal by design, so its only native evidence
  # is a log line nobody reads -- this line is the one that gets read.
  # FLEET_PULSE_SUBC_CONFIG exists so a control run can prove the ABSENT arm
  # fires (point it at a config naming a module that cannot be running).
  cfg_ids=$(python3 -c "import json,re,os
p=os.environ.get('FLEET_PULSE_SUBC_CONFIG') or os.path.expanduser('~/.config/cortexkit/subc.jsonc')
try:
    s=re.sub(r'//.*','',open(p).read())
    m=json.loads(s).get('modules',{})
    ids=list(m.keys()) if isinstance(m,dict) else [x.get('id','') for x in m]
    print('\n'.join(i for i in ids if i))
except Exception:
    pass" 2>/dev/null)
  # LAYOUT ANOMALY SCAN (astrocyte 2026-08): a 0-byte store.db at the
  # conventional path with a real store nested deeper reads as "empty store"
  # to every tool that opens it -- an empty database and a wrongly-located one
  # are different facts that look identical at the only place anyone checks.
  # Two signatures, both cheap: doubled cortexkit/ nesting inside a module
  # dir, and a 0-byte store.db beside a non-trivial sibling tree.
  layout_bad=""
  for md in "$HOME/.local/share/cortexkit"/*/; do
    case "$md" in */run/|*/backups/|*/staging/|*/dev-rig/|*/ckdev-rig/|*/u1-evidence-rig/|*/seam-drive-evidence/|*backup*/) continue;; esac
    [ -d "${md}cortexkit" ] && layout_bad="$layout_bad $(basename "$md")(nested)"
    if [ -f "${md}store.db" ] && [ ! -s "${md}store.db" ]; then
      layout_bad="$layout_bad $(basename "$md")(0-byte-store)"
    fi
  done
  [ -n "$layout_bad" ] && printf '  LAYOUT ANOMALY:%s -- see docs/module-rename-runbook.md data-layout section\n' "$layout_bad"
  # SYNAPSE CERTIFICATION (2026-08-30, SYNAPSE nomination after the Aug 25-29
  # four-day silent refusal): certification_stale=true means synapse FAIL-CLOSED
  # refuses the affected model class while process health reads ok -- the exact
  # healthy-but-refusing shape no process gauge can see. Alarm on the field, and
  # keep the three states distinct: false (fine, say nothing), true (alarm),
  # ABSENT (older binary or probe failure -- say so; absence must not read as
  # false, that is the absent-arm rule).
  cert_line=$(ck health synapse 2>/dev/null | grep -m1 'certification_stale')
  if [ -n "$cert_line" ]; then
    case "$cert_line" in
      *true*)
        since=$(ck health synapse 2>/dev/null | sed -n 's/.*stale_since_ms[": ]*\([0-9]*\).*/\1/p' | head -1)
        printf '  SYNAPSE CERTIFICATION STALE: fail-closed refusals active while process health reads ok%s\n' "${since:+ (since_ms $since)}"
        ck health synapse 2>/dev/null | grep 'certified=false' | head -4 | sed 's/^/    /'
        ;;
    esac
  else
    printf '  synapse certification field ABSENT (pre-gen-72 binary or probe failure -- cert outage detection is blind)\n'
  fi
  if [ -n "$cfg_ids" ]; then
    absent=$(printf '%s\n' "$cfg_ids" | while IFS= read -r mid; do
      printf '%s\n' "$health" | grep -q "[[:space:]●]$mid[[:space:]]\|^$mid[[:space:]]\|● $mid\b" || printf '%s ' "$mid"
    done)
    if [ -n "$absent" ]; then
      printf '  %s ok  -- CONFIGURED BUT ABSENT FROM HEALTH: %s(daemon never spawned it, or it died unregistered -- check ck module status <id>)\n' "$ok_count" "$absent"
    else
      printf '  %s ok  (all %s configured modules reporting)\n' "$ok_count" "$(printf '%s\n' "$cfg_ids" | grep -c .)"
    fi
  else
    printf '  %s ok  (config unreadable -- cannot say whether any module is ABSENT)\n' "$ok_count"
  fi
    # COMPARE THE MODULE AND ITS STATUS, NOT THE WHOLE LINE. Byte-identity of the
    # rendered text is a PROXY for "the same condition is still present", and it
    # stops accompanying the property the moment any volatile substring appears --
    # an age, a queue depth, a count. A module stuck degraded for hours whose
    # detail carries a rising number would read as (new or changed) EVERY cycle and
    # never once as PERSISTS, so the one signal worth acting on is the one a
    # counting detail suppresses. Detail still prints; only the comparison key is
    # narrowed.
    notok=$(printf '%s\n' "$health" | grep '●' | grep -v '● ok' || true)
    notok_key=$(printf '%s\n' "$notok" | awk '{print $1, $2, $3}')
  prev_file="${XDG_RUNTIME_DIR:-${TMPDIR:-/tmp}}/ck-fleet-pulse-health.prev"
    prev=$(cat "$prev_file" 2>/dev/null || true)
    printf '%s' "$notok_key" > "$prev_file" 2>/dev/null || true
    if [ -n "$notok" ]; then
      if [ "$notok_key" = "$prev" ]; then
      printf '%s\n' "$notok" | sed 's/^/  PERSISTS /'
      echo "  ^ unchanged since the previous cycle -- this is the shape worth acting on"
    else
      printf '%s\n' "$notok" | sed 's/^/  (new or changed) /'
      echo "  ^ first sighting; noisy gauges clear by the next cycle, a real one persists"
    fi
  fi
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
terminal=$(sq "$STORE" "
  SELECT substr(consult_id, -12), consult_kind, phase,
         COALESCE(terminal_reason,'-'),
         COALESCE((SELECT name FROM peers WHERE session_id = caller_session), caller_session),
         (strftime('%s','now') - updated_at/1000)/60
  FROM consult
  WHERE phase IN ('failed','done')
    AND consult_kind IN ('spec','campaign')
    AND caller_session IS NOT NULL AND caller_session <> ''
    AND updated_at > (strftime('%s','now') - 43200) * 1000
  ORDER BY updated_at DESC LIMIT 6;") || terminal="__FAILED__"
# The LIMIT above keeps a busy day from burying the rest of the report, but a
# truncated list that does not say it was truncated is a clean-looking result over
# a partial set -- the same shape as a scanner that will not print what it
# skipped. So count the window separately and say when rows were withheld.
terminal_total=$(sq "$STORE" "
  SELECT COUNT(*) FROM consult
  WHERE phase IN ('failed','done')
    AND consult_kind IN ('spec','campaign')
    AND caller_session IS NOT NULL AND caller_session <> ''
    AND updated_at > (strftime('%s','now') - 43200) * 1000;") || terminal_total=""
if [ "$terminal" = "__FAILED__" ]; then
  # A failed query used to land on the "none terminal" branch below: a clean
  # answer about campaigns, from a probe that never ran.
  echo "  campaign query FAILED (see error above) -- terminals UNCHECKED this cycle"
elif [ -n "$terminal" ]; then
  printf '%s\n' "$terminal" | while IFS='|' read -r id kind phase reason who age; do
    printf '  %-14s %-8s %-7s %-26s %s  (%sm ago)\n' "$id" "$kind" "$phase" "$reason" "$who" "$age"
  done
  if [ -n "$terminal_total" ] && [ "$terminal_total" -gt 6 ] 2>/dev/null; then
    echo "  ... and $((terminal_total - 6)) more in the window, not shown"
  fi
  dim "  quiet since BEFORE one of these terminals = produced nothing after it; check the transcript for why"
elif [ ! -r "$STORE" ]; then
  # AN EMPTY QUERY RESULT AND AN UNREADABLE STORE ARE DIFFERENT FACTS, and the
  # section reported both as "none terminal in the last 12h" -- an emptiness claim
  # about a source it could not read. Every other section here already announces a
  # missing input; this one asserted a clean answer instead, which is the worse
  # direction because it is reassuring.
  echo "  store unreadable ($STORE) -- campaign terminals UNCHECKED this cycle"
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

# ------------------------------------------------------------------ memory pressure
# Included because machine-wide memory exhaustion presents as unrelated symptoms
# in every other section: builds that hang, tests that time out, a language
# server deadlocked on a pipe whose buffer the kernel could not grow. Chasing
# those individually costs hours; one line here names the common cause.
#
# FREE RAM IS THE WRONG NUMBER AND IS DELIBERATELY NOT THE HEADLINE. It oscillates
# by ~1 GB on a 30-second timescale, so any two samples can show whatever you
# already believe. The two that answer the question are the COMPRESSOR SIZE and
# the SWAP FILE TOTAL: the kernel grows the swap file only under sustained
# pressure and shrinks it when pressure falls, so its direction is a fact about
# the machine rather than about the instant you sampled.
#
# Reported without a verdict on purpose. A threshold here would be calibrated on
# whatever this box was doing the day it was written, and a large working set on
# a 128 GB machine is ordinary rather than alarming.
#
# Both probes report ABSENCE rather than a number when they fail. An earlier
# version let awk's uninitialised variables render as "free 0.0 GB", which reads
# as catastrophic exhaustion rather than as a broken probe -- a false alarm in the
# one direction that would send the reader hunting a machine-wide emergency that
# does not exist.
bold "MEMORY"
if mem=$(vm_stat 2>/dev/null) && [ -n "$mem" ]; then
  printf '%s\n' "$mem" | awk '/Pages free/{f=$3}/Pages occupied by compressor/{c=$5}
    END {if (f == "" || c == "") print "  vm_stat output unrecognised -- memory UNCHECKED";
         else printf "  free %.1f GB (oscillates -- not the signal)   compressor %.1f GB\n", f*16384/1073741824, c*16384/1073741824}'
else
  echo "  vm_stat unavailable -- memory UNCHECKED this cycle"
fi
  # The swap FILE TOTAL is the quantity that separates a loaded box from a
  # degrading one, and it only says anything ACROSS TIME -- the kernel extends the
  # file under sustained pressure and shrinks it as pressure falls. An earlier
  # version printed one sample and told the reader that growing meant pressure,
  # which asks for a comparison the output does not contain: the same defect as
  # reading a level and calling it a trend. So the total is remembered between
  # cycles and the DELTA is what gets printed.
  #
  # The remembered value lives in the runtime dir beside the connection file. That
  # directory is the WRONG place for durable state -- broca's write-ahead log sits
  # there today and a reasonable cleanup would destroy it -- so the placement is
  # deliberate rather than incidental: this file is a CACHE OF ONE NUMBER,
  # regenerated on the next cycle, and losing it costs exactly one UNCHECKED line.
  # Disposable-by-construction is what the runtime dir is for. The distinction is
  # invisible from a directory listing, which is the whole hazard, so it is stated
  # here rather than left for a reader to infer.
  #
  # A first run, or an unreadable value, says UNCHECKED rather than implying
  # stability -- a fresh install must not get a reassuring answer it has not earned.
  if swap=$(sysctl -n vm.swapusage 2>/dev/null) && [ -n "$swap" ]; then
    echo "  swap file: $swap"
    now_total=$(printf '%s' "$swap" | sed -n 's/.*total = \([0-9.]*\)M.*/\1/p')
    prev_file="$HOME/.local/share/cortexkit/run/.fleet-pulse-swap-total"
    prev_total=$(cat "$prev_file" 2>/dev/null)
    if [ -n "$now_total" ] && [ -n "$prev_total" ]; then
      delta=$(awk -v a="$now_total" -v b="$prev_total" 'BEGIN{printf "%.0f", a-b}')
      case "$delta" in
        -*) dim "  swap file SHRANK ${delta#-}M since last cycle -- pressure falling" ;;
        0)  dim "  swap file unchanged since last cycle -- loaded, not degrading" ;;
        *)  echo "  swap file GREW ${delta}M since last cycle -- sustained pressure" ;;
      esac
    else
      dim "  no previous sample -- direction UNCHECKED this cycle (a level alone is not a trend)"
    fi
    [ -n "$now_total" ] && printf '%s\n' "$now_total" > "$prev_file" 2>/dev/null
  else
    echo "  swap usage unavailable -- pressure direction UNCHECKED"
  fi
echo

# ---------------------------------------------------------------------- DISK
# The 2026-08-11 incident: the volume reached 12-17 GB free of 3.7 TB while
# ~1 TB of regenerable Rust target/ caches sat across twelve seats' repos, and
# NOBODY COULD SEE THE TOTAL -- each seat can measure its own tree and none is
# responsible for the sum. At that margin a WAL append or SQLite checkpoint can
# fail, so this is a durability hazard wearing a housekeeping costume.
#
# Two numbers, both cheap: free space (df, instant) and the summed target/ dirs
# (du -d0 per repo, a few seconds against warm metadata). The floor is 150 GB:
# generous, because the failure mode below it is fleet-wide storage writes
# failing, and the remedy (purge a debug tree) is minutes. target/ sizes are
# listed only when the floor trips or a single tree exceeds 100 GB -- a quiet
# disk needs no inventory.
#
# HONEST LIMIT (BROCA): the floor catches the STATE; nothing here catches the
# RATE. A 6 GB tree is one `cargo test --workspace` away from 200 GB, so a green
# reading means "not currently at risk", never "will not be at risk tomorrow".
# The 150 GB margin IS the rate allowance -- it buys hours-to-days of even fast
# accumulation against a 30-minute cadence; a derivative would need history and
# earn complexity the margin already pays for.
bold "DISK"
free_gb=$(df -g / 2>/dev/null | awk 'NR==2{print $4}')
if [ -n "$free_gb" ]; then
  ck_root="$HOME/Work/Projects/CortexKit"
  tgt_total=0; tgt_lines=""; tgt_count=0
  for t in "$ck_root"/*/target; do
    [ -d "$t" ] || continue
    gb=$(du -sg "$t" 2>/dev/null | awk '{print $1}')
    [ -n "$gb" ] || continue
    tgt_total=$((tgt_total + gb)); tgt_count=$((tgt_count + 1))
    [ "$gb" -ge 100 ] && tgt_lines="${tgt_lines}  target OVER 100GB: ${t} (${gb} GB)\n"
  done
  echo "  free ${free_gb} GB · summed repo target/ caches ${tgt_total} GB across ${tgt_count} trees"
  if [ "$free_gb" -lt 150 ]; then
    echo "  DISK FLOOR TRIPPED (<150 GB free): WAL appends and checkpoints are at risk fleet-wide"
    for t in "$ck_root"/*/target; do
      [ -d "$t" ] && du -sg "$t" 2>/dev/null
    done | sort -rn | head -8 | awk '{printf "    %s GB  %s\n", $1, $2}'
  fi
  [ -n "$tgt_lines" ] && printf "$tgt_lines"
else
  echo "  df unavailable -- disk UNCHECKED this cycle"
fi
echo

# ------------------------------------------------------------------------ CI
# A failing default branch blocks everyone working in that repository, and NO
# OTHER SECTION HERE CAN SEE IT. Module health inspects the running process and
# the deploy section compares the deployed binary against git; neither reads
# remote CI, so both stayed correctly green through 22 hours in which this
# repository failed CI on every push and nothing reported it.
#
# Delegated to its own script because it is the only section that uses the
# network: about 24 API calls, issued in parallel, roughly three seconds. It runs
# on the cadence rather than on request because the failure it addresses was
# nobody thinking to check.
bold "CI"
if [ -x "$(dirname "$0")/ci-redness.sh" ]; then
  "$(dirname "$0")/ci-redness.sh" || true
else
  echo "  ci-redness.sh missing or not executable -- CI state UNCHECKED"
fi
echo

# ------------------------------------------------------------------- daemon log
# THE SHARED DAEMON LOG IS AN INCIDENT-RESPONSE SURFACE, AND ONE SUBSYSTEM CAN
# MAKE IT UNREADABLE FOR EVERY OTHER.
#
# The failure mode: a repeating bind-rejection line can reach tens of percent of
# the whole file at tens of lines per second. The cost is not disk. It is that
# EVERY OTHER SUBSYSTEM'S LINES STOP APPEARING IN THE TAIL -- so a routine "is my
# module alive" check reads the last few hundred lines, sees none of its own, and
# concludes silence. That reads as a dead module, i.e. a false incident report.
# The 200 MB flag and the 20k-line window below are both sized against that: big
# enough not to fire on ordinary volume, small enough to catch a flood early.
#
# WHY THE RATE IS NOT THE LINE COUNT ONCE A RATE LIMIT IS IN PLACE. The emitter's
# fix keeps the message byte-identical and appends "(repeated Nx in last 60s)" to
# the next emission, so THE LINE COUNT COLLAPSES WHILE THE UNDERLYING LOOP
# CONTINUES -- the loop itself only dies as hosts restart onto a client-side
# fix, which is a separate and slower event. Counting lines would report the
# flood solved on the day the counting changed, which is the same trap as a
# metric going to zero because something was renamed.
#
# So this sums suppressed counts and reports THREE distinguishable states,
# because absent and zero are not the same observation:
#   lines, no suffixes   pre-rate-limit; the count IS the rate
#   lines + suffixes     rate limit live; true rate is AT LEAST lines + suffix sum
#   no lines at all      loop dead, OR this check has gone blind -- which the
#                        control below is what separates
#
# THE SUM IS A FLOOR, NOT A TOTAL, and the reason is in the emitter's contract:
# a suppressed count rides on the NEXT emission after its window, so a flood that
# STOPS mid-window never flushes its final count. Suppression is keyed per
# distinct message, so the undercount is bounded by one window per distinct
# message -- with dozens of distinct roots that is not a rounding error. Reported
# as ">=" because a number labelled as a total, that is not one, is the specific
# way a correct measurement turns into a wrong claim.
bold "DAEMON LOG"
DLOG="$HOME/.local/share/cortexkit/run/subc.log"
if [ ! -f "$DLOG" ]; then
  echo "  $DLOG absent -- UNCHECKED (not clean)"
else
  dl_mb=$(( $(stat -f '%z' "$DLOG" 2>/dev/null || echo 0) / 1048576 ))
  # POSITIVE CONTROL FIRST. A zero below is only meaningful if this file is being
  # written at all; without it a rotated, renamed or empty log and a genuinely
  # quiet one produce the same reassuring zero.
  dl_total=$(wc -l < "$DLOG" 2>/dev/null | tr -d ' ')
  if [ "${dl_total:-0}" -eq 0 ]; then
    echo "  log is EMPTY -- every count below would read zero regardless; UNCHECKED"
  else
    dl_win=$(tail -20000 "$DLOG" 2>/dev/null)
    dl_hits=$(printf '%s\n' "$dl_win" | grep -c 'route bind rejected (config_divergence)' || true)
    dl_supp=$(printf '%s\n' "$dl_win" | grep -o 'repeated [0-9]*x' | grep -o '[0-9]*' | awk '{s+=$1} END {print s+0}')
    printf '  size %s MB, %s lines total\n' "$dl_mb" "$dl_total"
    if [ "$dl_hits" -eq 0 ] && [ "$dl_supp" -eq 0 ]; then
      printf '  bind-rejection flood: none in last 20k lines (control: %s lines exist)\n' "$dl_total"
      # THE FOURTH STATE, AND IT ARRIVES EXACTLY WHEN SOMEONE IS WATCHING.
      # A suppressed count rides the NEXT emission after its window, so for the
      # first window after any restart there is nothing to carry a count and BOTH
      # numbers read zero -- indistinguishable from the loop being dead. Measured
      # live: immediately after a bounce this printed 0 and 0 while the loop was
      # provably still running, because the client-side fix reaches hosts only on
      # THEIR restart, which had not happened. Zero here means "not yet
      # observable" if the emitter restarted within the last window.
      if [ -n "$(find "$DLOG" -newermt '-90 seconds' 2>/dev/null)" ]; then
        echo "  note: if a module restarted in the last ~60s, this zero is NOT YET"
        echo "        OBSERVABLE rather than clean -- re-read after a full window"
      fi
    elif [ "$dl_supp" -eq 0 ]; then
      printf '  bind-rejection flood: %s lines / 20k, NO suppression suffixes\n' "$dl_hits"
      echo   "    -> rate limit not yet deployed; the line count IS the rate"
    else
      printf '  bind-rejection flood: %s lines / 20k + %s suppressed = >=%s true\n' "$dl_hits" "$dl_supp" "$(( dl_hits + dl_supp ))"
      echo   "    -> rate limit live; loop still running until hosts roll onto dormancy"
      echo   "    -> total is a FLOOR: a flood ending mid-window never flushes its last count"
    fi
    [ "$dl_mb" -gt 200 ] && echo "  UNROTATED: >200 MB on a shared surface"
    # A skipped MCP provider is a partial tool surface serving as healthy, and
    # the skip line is its ONLY witness anywhere in the stack: the harness sees
    # nothing (the model treats absent tools as normal and works around them
    # silently -- a whole tool surface once vanished from Claude Code without a
    # single error), and the shim serves the remaining providers as designed.
    # Deliberate skips (plexus's first-party-only policy) look identical to a
    # misconfigured provider being dropped, so every distinct skip is printed
    # with its reason rather than counted.
    dl_skips=$(printf '%s\n' "$dl_win" | grep 'skipping provider' | sed 's/.*skipping provider/skipping provider/' | sort -u)
    if [ -n "$dl_skips" ]; then
      echo "  MCP providers skipped in recent sessions (partial tool surface):"
      printf '%s\n' "$dl_skips" | sed 's/^/    /'
    else
      printf '  MCP provider skips: none in last 20k lines (control: %s lines exist)\n' "$dl_total"
    fi
  fi
fi
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
    # SAY "UNPUBLISHED", NOT "UPLOADING". The counter proves only that a generation
    # is not published yet; whether anything is being sent is a separate fact,
    # established two lines below by the sidecar. Calling it "uploading" asserts an
    # activity this query cannot observe -- and when uploads are stopped entirely,
    # as during a credential outage, this line and the sidecar line contradict each
    # other two lines apart. The reader anchors on the first one.
    # "$pub/$total" IS A LIFETIME RATIO AND IMPROVES WITH AGE WHILE A FAULT PERSISTS:
    # three stuck generations read as 4% of 74 today and would read as 1.5% of 200
    # later, with the stuck count unchanged. Anti-correlation against uptime -- the
    # longer this has run, the healthier the ratio looks for the same defect. It is
    # labelled rather than dropped because the absolute counts on the HALTED line
    # below carry the real signal; an unlabelled ratio beside them invites reading
    # the reassuring one.
      echo "  gen $newest unpublished (last published $maxpub; $pub/$total published all-time)"
      # WHAT IS BACKED UP IS NOT WHAT WAS ASKED FOR, AND ONLY ONE OF THOSE IS
      # COUNTED HERE. entry_count is the number of entries CAPTURED. A module can
      # declare an entry that engram enrols, plans, and then classifies out of the
      # capture set -- an unimplemented mechanism, or a class engram excludes by
      # design -- and every counter in this section reads exactly the same as if
      # the module had declared nothing at all.
      #
      # TWO LIVE SPECIMENS, and they fail differently:
      #
      # DECLARED-AND-DECLINED: broca declares four entries, one a 1.2GB WAL it
      # cannot rebuild. That entry's capture mechanism is unimplemented, so it has
      # been declared and refused for four weeks while entry_count read 2 across
      # all 94 generations -- both of them another module's. The refusal IS
      # published (backup.status plannedEntries carries an `uncapturable` status)
      # and was read by nobody until a question about an unrelated module
      # surfaced it.
      #
      # DECLARED-AND-REJECTED, which is worse and has no surface at all: engram
      # rejects a descriptor whose declared module_id disagrees with its directory
      # name, and builds an `invalid` classification carrying both values. That
      # classification is put in the backup.run reply and NEVER PERSISTED -- the
      # autonomous scheduler prints two fields of that reply and drops the rest.
      # broca's descriptor sat in a directory named llm-runner-state until a
      # rename this morning, so the rejection was computed correctly ~94 times and
      # discarded microseconds later. A COMPUTED-AND-DISCARDED VALUE IS
      # INDISTINGUISHABLE FROM A CHECK NOBODY WROTE, except that the code reads as
      # though the check is working.
      #
      # NEITHER can be computed from the store: entry_count records what was
      # captured, and no local column holds what was declared-and-declined or
      # declared-and-rejected. Until backup.status carries both counts and this
      # polls it, the denominator is unavailable -- and saying so is the honest
      # report, because an uncounted refusal rendered as a clean number is the
      # failure this whole file exists to catch.
      # `|| echo '?'` would NOT cover the interesting failure: sqlite3 exits 0 and
      # prints NOTHING when the row is absent or the column moves, so the fallback
      # never fires and the line renders a blank where a number belongs. Test the
      # value, not the exit code.
      captured=$(sqlite3 "$ENGRAM_STORE" "SELECT entry_count FROM generations WHERE device_seq=$newest;" 2>/dev/null)
      [ -n "$captured" ] || captured="UNREADABLE"
      echo "  entries captured: $captured (DECLARED-BUT-UNCAPTURABLE ENTRIES ARE NOT COUNTED HERE -- read backup.status plannedEntries)"
    # The generation counter above cannot see WITHIN a generation: one that is
    # 60% uploaded and one that has not started read identically. That gap is
    # how a stalled publish hides -- it looks exactly like a slow one, and the
    # counter sits still either way for hours.
    #
    # The upload sidecar is appended as objects land, so its line count is real
    # progress. Sampling it twice is the only thing here that distinguishes
    # moving from stuck, and it costs the seconds between the two reads.
    # WATCH THE GENERATION THE DRAIN IS WORKING, WHICH IS THE OLDEST UNPUBLISHED --
    # NOT THE NEWEST. engram publishes oldest-first, so the newest staged generation
    # is the LAST one that will move and normally has no sidecar at all.
    #
    # This line has now been wrong twice, in opposite directions, and the second
    # error was introduced by the fix for the first. Originally it took the most
    # recently modified sidecar anywhere under staging, which could land on a
    # different generation than the header named. The fix bound the sidecar to the
    # reported generation so the two selections agreed -- and they agreed on the
    # newest, which is the one generation guaranteed not to be uploading. Observed
    # live: "gen 99 has no upload sidecar yet" printed for ten minutes while gen 97
    # was uploading at ~10 objects/min, so a recovering backup read as a dead one.
    # MAKING TWO SELECTION RULES CONSISTENT IS NOT THE SAME AS MAKING EITHER CORRECT.
    # KEEP THE EXIT STATUS. These ran with 2>/dev/null and no status check, so a
    # query that FAILED -- renamed column, locked store, missing table -- yielded
    # an empty string, which ${drain_seq:-0} then turned into a lookup for
    # device_seq 0. That returns nothing, sidecar stays empty, and the branch
    # below prints "publish not started": a confident operational claim, in the
    # right vocabulary, from an instrument that never ran.
    # A LOUD FAILURE IS ONLY LOUD WHERE SOMEONE HEARS IT. sqlite exits nonzero on
    # a bad column; 2>/dev/null plus a defaulted substitution is what converts
    # that fail-closed error into this script's fail-open silence.
    drain_fail=""
    if ! drain_seq=$(sqlite3 "$ENGRAM_STORE" \
      "SELECT MIN(device_seq) FROM generations WHERE published = 0;" 2>&1); then
      drain_fail="$drain_seq"
      drain_seq=""
      drain_pub=""
    elif ! drain_pub=$(sqlite3 "$ENGRAM_STORE" \
      "SELECT lower(hex(pub_id)) FROM generations WHERE device_seq = ${drain_seq:-0};" 2>&1); then
      drain_fail="$drain_pub"
      drain_pub=""
    fi
    if [ -n "$drain_fail" ]; then
      echo "  BACKUP PROBE FAILED (store unreadable or schema moved): $drain_fail"
    fi
    sidecar=""
    if [ -n "$drain_pub" ]; then
      sidecar=$(ls -t "$HOME/.local/share/cortexkit/engram/staging/$drain_pub"/uploaded-*.hex 2>/dev/null | head -1)
    fi
    if [ -n "$sidecar" ]; then
      before=$(wc -l < "$sidecar" 2>/dev/null | tr -d ' ')
      sleep 20
      after=$(wc -l < "$sidecar" 2>/dev/null | tr -d ' ')
      if [ "${after:-0}" -gt "${before:-0}" ] 2>/dev/null; then
        # NUMERATOR ONLY. A denominator is available and correct -- see
        # engram_upload_rate below, which filters the progress file and the two
        # commit markers out of the staging listing before counting. Getting
        # that filter right is the whole difficulty: an unfiltered listing sits
        # three short of complete FOREVER, and the residual stops shrinking at
        # exactly the moment the upload finishes, which is indistinguishable
        # from a stall. That is not hypothetical -- it cost an hour of treating
        # a finished upload as a stalled one, from an ad-hoc `ls | grep -c` at
        # a terminal rather than from this file.
        #
        # A PROGRESS RATIO IS TWO MEASUREMENTS AND ONLY THE NUMERATOR IS
        # SELF-EVIDENTLY RIGHT: the uploader writes that file. Any denominator
        # is an assumption about which directory entries are objects. Where one
        # is printed, the filter is the thing to check first.
        echo "  uploading gen $drain_seq: $after objects, +$((after - before)) in 20s"
      else
        # Twenty seconds at the observed ~10 objects/min is only ~3 objects, so a
        # zero delta here is weak evidence. Say what was measured rather than
        # declaring a stall.
        echo "  gen $drain_seq at $after objects, no change in 20s -- weak signal at this rate, re-read before calling it stalled"
      fi
    else
      # No sidecar on the DRAIN TARGET means uploading has not begun -- distinct
      # from begun-and-stalled, and the remedies differ.
      # Reachable now only when the queries SUCCEEDED, so an absent sidecar is a
      # fact about the generation rather than about the probe.
      if [ -n "$drain_fail" ]; then
        echo "  drain target unknown -- publish state NOT REPORTED (probe failed above)"
      else
        echo "  gen ${drain_seq:-?} (oldest unpublished) has no upload sidecar yet -- publish not started"
      fi
    fi
  else
    echo "  gen $maxpub published ($pub/$total)"
  fi
  # A backlog at the cap is not "a bit behind", it is STOPPED: engram's scheduler
  # refuses to start a new capture once three generations await publish
  # (engram-module/src/main.rs, backpressure gate), so at 3 the module has
  # stopped protecting new work and will not resume without an operator. Report
  # that in words -- the bare count reads the same at 2 (progressing) and at 3
  # (halted), and the difference is the whole point.
  #
  # The line carries BOTH the absolute timestamp and the elapsed age. The absolute
  # one makes the report reproducible by whoever reads it later; the age is what
  # decides urgency, and asking a reader to subtract two datetimes at a glance is
  # how a four-hour gap and a four-day gap get read the same way. Neither
  # substitutes for the other, and both are already in hand -- the timestamp is
  # rendered from the same epoch the arithmetic uses.
  unpub=$((total - pub))
  if [ "$unpub" -ge 3 ] 2>/dev/null; then
    last_epoch=$(sqlite3 "$ENGRAM_STORE" "SELECT MAX(created_at) FROM generations;" 2>/dev/null)
    if [ -n "$last_epoch" ] 2>/dev/null; then
      last=$(date -r "$last_epoch" '+%Y-%m-%d %H:%M:%S' 2>/dev/null)
      age_s=$(( $(date +%s) - last_epoch ))
      echo "  CAPTURE HALTED: $unpub staged await publish (cap 3); nothing new for $((age_s / 3600))h$(( (age_s % 3600) / 60 ))m (newest $last)"
    else
      echo "  CAPTURE HALTED: $unpub staged await publish (cap 3); newest snapshot time UNREADABLE"
    fi
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

# The module set is DERIVED from subc.jsonc, never listed here. An earlier
# version transcribed nine module:binary pairs, which agreed with the fleet only
# on the day it was typed: five supervised modules had since been added and were
# silently outside the check, and the section still reported "no binary behind
# its master" -- a clean result over an incomplete set, which is the failure this
# whole file exists to catch.
#
# Only the module_id -> repo-directory step is residual, because nothing on disk
# records it (prefrontal-core lives in prefrontal/, subc-mcp in subconscious/).
# A module whose directory this cannot resolve is REPORTED rather than skipped,
# so the mapping going stale is visible instead of silent -- which is how the
# entries below were caught after a rename.
#
# Every entry here exists because a module's id disagrees with its directory, so
# renaming a directory to match its module id DELETES an entry rather than
# editing one. That is the direction to prefer: the fallthrough is already
# correct for anything that agrees with itself.
module_repo() {
  case "$1" in
    prefrontal-core | prefrontal-routing) echo prefrontal ;;
    subc-mcp)     echo subconscious ;;
    *)            echo "$1" ;;
  esac
}

CFG="$HOME/.config/cortexkit/subc.jsonc"
modmap=$(python3 - "$CFG" <<'PY' 2>/dev/null
import json,re,sys,os
try:
    s=open(sys.argv[1]).read()
except OSError:
    sys.exit(1)
s=re.sub(r'//.*','',s); s=re.sub(r',(\s*[}\]])',r'\1',s)
for mid,cfg in sorted(json.loads(s).get('modules',{}).items()):
    print(f"{mid} {os.path.basename(str(cfg.get('program','')))}")
PY
)
# A config that cannot be read must not read as an empty fleet -- the same
# absent-means-nothing conversion that would have let a rescan retire every
# module. Refuse the section instead.
if [ -z "$modmap" ]; then
  echo "  cannot read $CFG -- deploy gaps UNCHECKED this cycle"
  modmap=""
fi

while read -r modid binname; do
  [ -n "$modid" ] || continue
  repo=$(module_repo "$modid")
  dir="$HOME/Work/Projects/CortexKit/$repo"
  if [ ! -d "$dir/.git" ]; then
    echo "  $modid: no repo at $repo/ -- cannot check for unshipped code"
    printed=1
    continue
  fi
  # Three ways a module can drop out of this check. All of them USED TO BE SILENT,
  # which meant a clean section could cover any subset of the fleet and read
  # exactly like a clean section covering all of it. Zero modules hit these today,
  # but that is a property of today's state rather than of the check -- and the
  # first time one does, the reader must not be told the fleet is current.
  if [ ! -f "$BIN/$binname" ]; then
    echo "  $modid: $binname is not in the deploy dir -- UNCHECKED, not current"
    printed=1
    continue
  fi
  if ! head_epoch=$(cd "$dir" && git log -1 --format='%ct' 2>/dev/null) || [ -z "$head_epoch" ]; then
    echo "  $modid: cannot read $repo HEAD -- UNCHECKED, not current"
    printed=1
    continue
  fi
  # -L deliberately: this asks when the BINARY was built, not when a path to it was
  # created. Without it a deploy path that is a symlink reports the link's own
  # timestamp, so a stale binary behind a fresh link reads as current.
  if ! bin_epoch=$(stat -Lf '%m' "$BIN/$binname" 2>/dev/null) || [ -z "$bin_epoch" ]; then
    echo "  $modid: cannot stat $binname -- UNCHECKED, not current"
    printed=1
    continue
  fi
  # Only hours-scale gaps are worth a line. A binary minutes older than its head
  # is the normal state right after a deploy, and flagging it trains the reader
  # to ignore this section.
  gap_h=$(( (head_epoch - bin_epoch) / 3600 ))

  # The shared-crate check runs BEFORE the small-gap floor below, not after.
  # A binary rebuilt an hour ago against a five-day-old commons has an own-repo
  # gap of zero and a shared-crate gap of five days -- and a fresh rebuild is the
  # normal state right after any deploy, so the floor would swallow the check
  # exactly when someone has just deployed and is most likely to be reading this.
  # A guard placed after a skip inherits that skip's blind spot, and here the
  # skip's condition is not incidental to the fault but a common companion of it.
  # A PATH DEPENDENCY CROSSING A REPOSITORY BOUNDARY IS INVISIBLE TO AN
  # OWN-REPO COMPARISON, AND IT FAILS TOWARD A FALSE ALL-CLEAR. A commit in
  # subconscious changes a module's binary with no commit in that module's
  # repo, so mtime-versus-its-own-history reports clean while the binary is
  # stale. Nine of the fourteen modules link subconscious crates this way and
  # only the commons half of the problem was checked here; the subconscious
  # half was found by a module owner auditing a clean verdict I had given them,
  # which is the only reason it was found at all.
  #
  # Both sources are checked against the same binary mtime and reported
  # separately, because they are different repositories with different owners
  # and "which upstream moved" is the first thing a reader needs.
  #
  # Only PUBLISHED consumption is safe from this: a crates.io dependency needs
  # a publish and a lock bump to reach a consumer, so a working-tree change
  # cannot. This check keys on the path-dependency spelling for that reason.
  for up in commons subconscious; do
    up_dir="$HOME/Work/Projects/CortexKit/$up"
    grep -rqs "path *= *\"[^\"]*$up/" "$dir/Cargo.toml" "$dir"/crates/*/Cargo.toml 2>/dev/null || continue
    # Measure the upstream's last RUNTIME commit, not its HEAD. HEAD moves on
    # docs and CI edits, and this file's own doc commits promptly produced
    # "predates subconscious master by 0h" against three binaries -- a check
    # that cries wolf on its author's own prose gets ignored within a day.
    # Restricted to crate sources, excluding tests and benches, mirroring what
    # the own-repo leg below already does with the binary's crate closure.
    up_head=$(cd "$up_dir" 2>/dev/null && git log -1 --format='%ct' \
      -- 'crates/*/src/*' 'crates/*/Cargo.toml' 2>/dev/null) || continue
    [ -n "$up_head" ] || continue
    if [ "$up_head" -gt "$bin_epoch" ] 2>/dev/null; then
      printf '  %-20s predates %s master by %sh -- shared-crate changes are not in this binary\n' \
        "$binname" "$up" "$(( (up_head - bin_epoch) / 3600 ))"
      printed=1
    fi
  done

  [ "$gap_h" -ge 6 ] || continue

  # A time gap alone cannot tell a stale deploy from a busy repo: a CI-workflow
  # or docs commit moves HEAD and reads exactly like unshipped runtime code. So
  # ask what the gap is MADE OF -- if nothing in it reaches the running binary,
  # there is nothing to deploy and the line would be a false alarm that costs the
  # owner an interruption and costs this section its credibility.
  #
  # "Reaches the binary" is asked of THIS binary, not of the repo. Repo-wide
  # attribution counts every tracked crate, including ones nothing deployed links:
  # standalone spikes, evaluation prototypes, and benches outside the workspace.
  # Those inflate the count on a line whose whole value is being believed.
  #
  # The crate set is cargo's own path-dependency closure rather than a list, so a
  # crate added later cannot silently fall outside it. Note this is CRATE
  # granularity, not target: a sibling binary's source inside a linked crate still
  # counts, which over-reports rather than under-reports and is the safe direction
  # for a deploy gap. A repo cargo cannot describe falls back to repo-wide.
  since=$(date -r "$bin_epoch" '+%Y-%m-%d %H:%M:%S' 2>/dev/null) || continue
  crate_filter=$(cd "$dir" && cargo metadata --no-deps --offline --format-version 1 2>/dev/null \
    | BINNAME="$binname" python3 -c '
import sys, json, os
try:
    meta = json.load(sys.stdin)
except Exception:
    sys.exit(1)
root = meta["workspace_root"]
pkgs = {p["name"]: p for p in meta["packages"]}
owner = next((p["name"] for p in meta["packages"]
              for t in p["targets"] if "bin" in t["kind"] and t["name"] == os.environ["BINNAME"]), None)
if owner is None:
    sys.exit(1)
# Walk path dependencies only: a workspace-local crate is the only kind whose
# source lives in this repo and therefore the only kind a git path can name.
seen = set()
stack = [owner]
while stack:
    name = stack.pop()
    if name in seen or name not in pkgs:
        continue
    seen.add(name)
    stack.extend(d["name"] for d in pkgs[name]["dependencies"] if d.get("path"))
dirs = sorted(os.path.relpath(os.path.dirname(pkgs[n]["manifest_path"]), root) for n in seen)
print("|".join(f"^{d}/" for d in dirs))
' 2>/dev/null)
  runtime=$(cd "$dir" && git log --since="$since" --format='' --name-only 2>/dev/null \
    | grep -E '\.(rs|toml)$' \
    | grep -v -E '(^|/)(tests?|benches|examples)/' \
    | grep -v -E '(^|/)bin/.*_cli\.rs$' \
    | { if [ -n "$crate_filter" ]; then grep -E "$crate_filter|^Cargo\.(toml|lock)$"; else cat; fi; } \
    | sort -u)
  if [ -z "$runtime" ]; then
    dim "  $repo: ${gap_h}h gap, but nothing in it reaches the binary (ci/docs/tests only)"
    continue
  fi
  n=$(echo "$runtime" | wc -l | tr -d ' ')
  # Name the BINARY, not the repo. They are the same thing in thirteen of the
  # fourteen fleet repos, which is exactly why the distinction is invisible until
  # it matters: subconscious ships three binaries, so a line reading "subconscious
  # is 48h behind" while the daemon was rebuilt an hour ago is not wrong so much
  # as unanswerable -- the reader cannot tell which artifact is meant, and the
  # obvious guess is the most important one.
  printf '  %-20s is %sh behind %s master, %s runtime file(s) unshipped -- ask the owner\n' \
    "$binname" "$gap_h" "$repo" "$n"
  # SAY WHEN THE LIST IS A SAMPLE. This printed three names with no indication
  # that more existed, and during a live incident I read those three as the
  # unshipped set and told the owner their fix was not among them -- it was, in
  # a file the truncation hid. A count in the line above plus an unmarked list
  # below reads as a count and its contents, not as a count and a sample of it.
  echo "$runtime" | head -3 | sed 's/^/      /'
  [ "$n" -gt 3 ] && printf '      ... and %s more (sample only -- do not read these 3 as the set)\n' "$((n - 3))"
  printed=1
done <<EOF
$modmap
EOF
# Version-tagged repos get two PRECISE legs the mtime heuristics above cannot
# provide, and the legs catch DIFFERENT failures (BROCA's correction, 2026-08-14,
# after I claimed coverage this section did not have):
#   manifest > newest tag   = CUT BUT NEVER TAGGED  -- the repo is self-consistent
#       (deployed==tag, CI green) and the only moved surface is Cargo.toml, so
#       tag-vs-deployed reads clean through the whole gap. Proof case: broca
#       0.3.45 sat cut-and-verified for hours while prod served 0.3.44.
#   newest tag > deployed   = TAGGED BUT NEVER PLACED -- the release finished and
#       the binary swap did not happen.
# Either leg alone reports clean on the other's failure. Repos without v-tags or
# without a --version self-report are SKIPPED AND SAY SO (a skipped scope that
# prints nothing reads as a clean scope).
# The header follows the file's comment-rule convention; there is no section()
# helper in this script (the first run of this leg proved that with a
# command-not-found that the loop survived -- fail-open on a missing header).
# -- claustrum audit-chain external anchor (standing, per CKCRED d0e8709) --
# verify-audit proves prefix validity only: a deleted TAIL is a shorter valid
# chain, so truncation is invisible from inside the store. This pulse is the
# external witness: it records the served tip (auditSeq + auditTipMac) and
# alarms on regression or mac-divergence-at-recorded-seq (monotonicity alone
# passes truncate-and-reappend; mac-at-seq stability is the binding check).
# Bound stated in the checker: catches truncation persisting across a pulse,
# blind to full repair between two pulses. Engram generations are the
# slow-window complement.
echo "-- claustrum audit-chain anchor"
_tip_out=$(ck health claustrum --json 2>/dev/null | python3 "$SUBCONSCIOUS_DIR/scripts/fleet/audit-tip-anchor.py" 2>&1)
_tip_rc=$?
if [ $_tip_rc -eq 3 ]; then
  echo "  *** $_tip_out"
else
  echo "  $_tip_out"
fi

# -- sibling committed-lock staleness (standing, per E2E nightly 2026-08-25) --
# A version bump in subconscious or commons re-stales every dependent sibling's
# committed Cargo.lock with no change in that sibling's repo and no signal to
# its owner; discovery previously happened a night later through E2E's floating
# gate. The check is read-only and owner-directed; this pulse line converts
# night-later discovery into next-pulse discovery. Runs the existing checker so
# there is exactly one implementation to trust.
echo "-- sibling committed locks (subconscious/commons bumps re-stale dependents)"
_lock_checker="$HOME/Work/Projects/CortexKit/subconscious/scripts/fleet/check-sibling-locks.sh"
if [ -x "$_lock_checker" ]; then
  _lock_out=$("$_lock_checker" 2>&1) || true
  # Three verdict states, all counted: OK, STALE (committed lock does not
  # resolve), DIRTY (working-tree lock differs from committed -- an unlocked
  # command already repaired it locally, which is the mask-that-gets-stronger
  # case and MORE urgent than STALE, not less). Missing DIRTY here undercounted
  # examined AND hid the worst rows; caught by driving this section against the
  # live fleet before committing it.
  _lock_examined=$(printf '%s\n' "$_lock_out" | grep -cE '^(OK|STALE|DIRTY)' || true)
  if [ "${_lock_examined:-0}" -lt 1 ]; then
    echo "  LOCK CHECK BROKEN: checker examined 0 repos (instrument, not cleanliness)"
  else
    printf '%s\n' "$_lock_out" | grep -E '^(STALE|DIRTY)' | sed 's/^/  /'
    _lock_bad=$(printf '%s\n' "$_lock_out" | grep -cE '^(STALE|DIRTY)' || true)
    echo "  locks: $_lock_bad stale-or-dirty of $_lock_examined examined"
  fi
else
  echo "  LOCK CHECK MISSING: check-sibling-locks.sh not found or not executable"
fi

# -- hand-rolled subc client census (standing, per ASTRO 2026-08-22) --
# SDK-level transport fixes (subc-client-rs) do NOT reach modules that consume
# subc-transport/protocol directly with hand-rolled frame loops; nothing in a
# green fleet check distinguishes them. This census keeps the population named
# so every future SDK fix has its non-inheriting list ready-made (docs:
# cortexkit-sdk-affordances.md \u00a710b).
# Transition-printing (ENGRAM's decay-shape note): a list that prints every
# pulse becomes background by the third cycle, and the rows most worth reading
# are the ones that only matter when they CHANGE. So membership transitions
# print loudly; an unchanged population prints one count line. Full list on
# demand: FLEET_PULSE_CENSUS_FULL=1. (Rung classification within the population
# -- enforcer / enforcer-by-documentation / accident, sdk-affordances \u00a710b --
# is audit knowledge a grep cannot derive; this census tracks membership only.)
echo "-- hand-rolled subc clients (SDK fixes do not reach these)"
hr_examined=0; hr_now=""; hr_examined_now=""
# FLEET_PULSE_ROOTS keeps the maintainer's single-root layout as the default,
# while allowing a fleet split across several project trees to remain one
# census. Names are the identity used by the old census, so duplicate roots do
# not double-count a repository. Keep this list-based: the fleet still includes
# macOS boxes whose stock /usr/bin/env bash is 3.2.
IFS=: read -r -a _hr_roots <<< "${FLEET_PULSE_ROOTS:-$HOME/Work/Projects/CortexKit}"
_hr_seen=""
for _root in "${_hr_roots[@]}"; do
  [ -n "$_root" ] || continue
  for _r in "$_root"/*/; do
    _repo="${_r%/}"
    [ -d "$_repo" ] || continue
    _repo_name=$(basename "$_repo")
    [ -e "$_repo/Cargo.toml" ] || ls "$_repo"/crates/*/Cargo.toml >/dev/null 2>&1 || continue
    printf '%b' "$_hr_seen" | grep -qxF "$_repo_name" && continue
    _hr_seen="$_hr_seen$_repo_name\n"
    hr_examined=$((hr_examined+1))
    hr_examined_now="$hr_examined_now$_repo_name\n"
    # Bounded manifest discovery: --include=Cargo.toml still WALKS the whole tree
    # (multi-GB target/ dirs made the census the pulse's slowest line); find with
    # a depth cap and target/ pruned reads only the manifests.
    _manifests=$(find "$_repo" -maxdepth 4 -name Cargo.toml -not -path "*/target/*" -not -path "*/.cortexkit/*" 2>/dev/null)
    [ -n "$_manifests" ] || continue
    if printf '%s\n' "$_manifests" | xargs grep -lq "subc-client-rs" 2>/dev/null; then
      :
    elif printf '%s\n' "$_manifests" | xargs grep -lqE "subc-transport|subc_transport" 2>/dev/null; then
      hr_now="$hr_now$_repo_name\n"
    fi
  done
done
hr_examined_now=$(printf '%b' "$hr_examined_now" | sort -u)
hr_now=$(printf '%b' "$hr_now" | sort)
hr_found=$(printf '%s\n' "$hr_now" | grep -c . || true)
hr_state_dir="${FLEET_PULSE_STATE_DIR:-$HOME/.local/share/cortexkit/run}"
hr_prev_file="$hr_state_dir/.fleet-pulse-handrolled-census"
hr_examined_prev_file="$hr_state_dir/.fleet-pulse-handrolled-census-examined"
mkdir -p "$hr_state_dir" 2>/dev/null || true
hr_prev=$(cat "$hr_prev_file" 2>/dev/null || true)
hr_prev_examined=$(cat "$hr_examined_prev_file" 2>/dev/null || true)
# A missing or malformed companion state cannot distinguish absence from
# migration. Treat it as a baseline rather than printing an invented transition.
if [ -z "$hr_prev_examined" ] \
  || printf '%s\n%s\n' "$hr_prev" "$hr_prev_examined" | grep -q '[^[:alnum:]_.-]'; then
  hr_prev=""; hr_prev_examined=""
fi
if [ "$hr_examined" -lt 8 ]; then
  echo "  CENSUS BROKEN: only $hr_examined rust repos examined -- expected 8+ (check FLEET_PULSE_ROOTS)"
elif [ -z "$hr_prev_examined" ]; then
  printf '%s\n' "$hr_now" | sed 's/^/  /'
  echo "  census baseline: $hr_found hand-rolled of $hr_examined rust repos (transitions print from next pulse)"
  printf '%s' "$hr_now" > "$hr_prev_file" 2>/dev/null || true
  printf '%s' "$hr_examined_now" > "$hr_examined_prev_file" 2>/dev/null || true
else
  hr_gaps=$(comm -23 <(printf '%s\n' "$hr_prev_examined") <(printf '%s\n' "$hr_examined_now"))
  hr_entered=$(comm -13 <(printf '%s\n' "$hr_prev") <(printf '%s\n' "$hr_now"))
  hr_left=$(comm -23 <(printf '%s\n' "$hr_prev") <(printf '%s\n' "$hr_now"))
  hr_migrated=$(comm -23 <(printf '%s\n' "$hr_left") <(printf '%s\n' "$hr_gaps"))
  if [ -n "$hr_gaps" ] || [ -n "$hr_entered" ] || [ -n "$hr_migrated" ] \
    || [ "$hr_now" != "$hr_prev" ] || [ "$hr_examined_now" != "$hr_prev_examined" ]; then
    while IFS= read -r _gap; do
      [ -n "$_gap" ] || continue
      if printf '%s\n' "$hr_prev" | grep -qxF "$_gap"; then
        echo "  CENSUS GAP: $_gap was in the hand-rolled population and is no longer FOUND by the scan (moved? renamed? check FLEET_PULSE_ROOTS) -- NOT evidence of SDK adoption"
      else
        echo "  CENSUS GAP: $_gap was examined by the census and is no longer FOUND by the scan (moved? renamed? check FLEET_PULSE_ROOTS)"
      fi
    done <<EOF
$hr_gaps
EOF
    printf '%s\n' "$hr_entered" | sed '/^$/d; s/^/  ENTERED hand-rolled population: /'
    printf '%s\n' "$hr_migrated" | sed '/^$/d; s/^/  LEFT hand-rolled population (adopted SDK): /'
    echo "  census CHANGED: $hr_found hand-rolled of $hr_examined rust repos examined"
    printf '%s' "$hr_now" > "$hr_prev_file" 2>/dev/null || true
    printf '%s' "$hr_examined_now" > "$hr_examined_prev_file" 2>/dev/null || true
  else
    echo "  census unchanged: $hr_found hand-rolled of $hr_examined rust repos examined"
    [ "${FLEET_PULSE_CENSUS_FULL:-0}" = "1" ] && printf '%s\n' "$hr_now" | sed 's/^/  /'
  fi
fi

echo "-- release ledger (manifest vs tag vs deployed)"
NOW_EPOCH=$(date +%s)
for spec in "broca:ck-broca" "engram:ck-engram" "fusiform:ck-fusiform"; do
  repo="${spec%%:*}"; bin="${spec##*:}"
  rdir="$HOME/Work/Projects/CortexKit/$repo"
  [ -d "$rdir/.git" ] && [ -x "$BIN/$bin" ] || { echo "  $repo: skipped (no repo or binary)"; continue; }
  tag=$(cd "$rdir" && git tag --list 'v[0-9]*' --sort=-v:refname 2>/dev/null | head -1)
  [ -n "$tag" ] || { echo "  $repo: skipped (no v-tags)"; continue; }
  tagv="${tag#v}"
  manifest=$(cd "$rdir" && grep -m1 '^version *= *"' "$(git ls-files '*Cargo.toml' | head -1)" 2>/dev/null | sed 's/.*"\(.*\)".*/\1/')
  deployed=$("$BIN/$bin" --version 2>/dev/null | grep -oE '[0-9]+\.[0-9]+\.[0-9]+' | head -1)
  line="  $bin: manifest ${manifest:-?} tag ${tagv} deployed ${deployed:-?}"
  if [ -n "$manifest" ] && [ "$manifest" != "$tagv" ]; then
    # GRACE FLOOR (BROCA's measurement): the NORMAL release spends 36-57 min with
    # manifest ahead of newest tag -- bump, push, wait out 3-platform CI, tag on
    # green. An instant alarm fires on every correct release and trains its
    # reader to dismiss it, which is how it gets dismissed the one time it
    # matters. 90 min clears a green CI run comfortably and catches the 341-min
    # hole this leg was built for. Age from the BUMP COMMIT of the manifest
    # version (the moment the ledger went ahead), not from tag or mtime.
    bump_epoch=$(cd "$rdir" && git log -1 --format='%ct' -S"version = \"$manifest\"" -- "$(git ls-files '*Cargo.toml' | head -1)" 2>/dev/null)
    age_min=$(( (NOW_EPOCH - ${bump_epoch:-NOW_EPOCH}) / 60 ))
    if [ "$age_min" -ge 90 ]; then
      echo "$line -- CUT BUT NEVER TAGGED for ${age_min}m (release finished nowhere)"
    else
      dim "$line -- mid-release (bump ${age_min}m old, grace 90m)"
    fi
  elif [ -n "$deployed" ] && [ "$deployed" != "$tagv" ]; then
    echo "$line -- TAGGED BUT NEVER PLACED (binary swap missing)"
  else
    dim "$line -- consistent"
  fi
done

# Commons crates publish by tag push (<crate>-v<version>); a merged-but-untagged
# crate is invisible from BOTH ends (QTA, twice on the same crate): the producer
# sees the merge and assumes the release followed, the consumer sees the old
# registry version and assumes batching, and the artifact that would surface the
# gap -- the tag -- is exactly the thing that is missing. Same grace floor as
# binaries: age from the bump commit, so a mid-release window stays dim.
COMMONS="$HOME/Work/Projects/CortexKit/commons"
if [ -d "$COMMONS/.git" ]; then
  crates_seen=0
  for ct in "$COMMONS"/crates/*/Cargo.toml; do
    [ -f "$ct" ] || continue
    crate=$(basename "$(dirname "$ct")")
    manifest=$(grep -m1 '^version *= *"' "$ct" | sed 's/.*"\(.*\)".*/\1/')
    [ -n "$manifest" ] || continue
    crates_seen=$((crates_seen + 1))
    tagv=$(cd "$COMMONS" && git tag --list "$crate-v[0-9]*" --sort=-v:refname 2>/dev/null | head -1)
    tagv="${tagv#"$crate-v"}"
    # Never-tagged crates are unpublished by omission (documented in README);
    # only a crate with at least one release tag has a ledger to keep.
    [ -n "$tagv" ] || continue
    if [ "$manifest" != "$tagv" ]; then
      bump_epoch=$(cd "$COMMONS" && git log -1 --format='%ct' -S"version = \"$manifest\"" -- "crates/$crate/Cargo.toml" 2>/dev/null)
      age_min=$(( (NOW_EPOCH - ${bump_epoch:-NOW_EPOCH}) / 60 ))
      if [ "$age_min" -ge 90 ]; then
        echo "  commons/$crate: manifest $manifest tag ${tagv} -- MERGED BUT NEVER TAGGED for ${age_min}m"
      else
        dim "  commons/$crate: manifest $manifest tag ${tagv} -- mid-release (bump ${age_min}m old, grace 90m)"
      fi
    fi
  done
  [ "$crates_seen" -ge 1 ] || echo "  commons: VACUOUS (0 crates examined at $COMMONS/crates)"
else
  echo "  commons: skipped (no repo at $COMMONS)"
fi

# The daemon is not a module, so a module-derived list structurally cannot reach
# it -- and it is the one binary whose staleness affects every other. Checked
# separately for that reason, against the subc-core crates only: a commit to the
# mcp shim or a client SDK moves subconscious HEAD without touching the daemon.
subc_dir="$HOME/Work/Projects/CortexKit/subconscious"
if [ -d "$subc_dir/.git" ] && [ -f "$BIN/ck-subc" ]; then
  d_bin=$(stat -Lf '%m' "$BIN/ck-subc" 2>/dev/null)
  d_head=$(cd "$subc_dir" && git log -1 --format='%ct' 2>/dev/null)
  if [ -n "$d_bin" ] && [ -n "$d_head" ]; then
    d_gap=$(( (d_head - d_bin) / 3600 ))
    if [ "$d_gap" -ge 6 ]; then
      d_since=$(date -r "$d_bin" '+%Y-%m-%d %H:%M:%S')
      d_rt=$(cd "$subc_dir" && git log --since="$d_since" --format='' --name-only 2>/dev/null \
        | grep -E '^crates/(subc-core|subc-protocol|subc-transport|subc-control)/.*\.rs$' \
        | grep -v -E '(^|/)tests?/' | sort -u)
      if [ -n "$d_rt" ]; then
        printf '  %-16s DAEMON is %sh behind master, %s runtime file(s) unshipped -- needs a bounce\n' \
          "ck-subc" "$d_gap" "$(printf '%s\n' "$d_rt" | grep -c .)"
        printf '%s\n' "$d_rt" | head -3 | sed 's/^/      /'
        printed=1
      fi
    fi
  fi
fi

# The deploy surface is WIDER THAN THE MODULE SET, and that gap is invisible from
# subc.jsonc alone. Operator CLIs, helper binaries, and worker executables spawned
# BY a module all live in the same bin/ directory and drift independently -- one
# such worker was stale for days while its parent module read current, because
# nothing counted it. So the live contents of bin/ are enumerated and anything the
# loops above did not examine is named.
#
# Ownership is derived by asking each repo which binaries it declares, rather than
# from a mapping here: a repo that adds a binary is covered without anyone
# remembering to update this. A binary no repo claims is still REPORTED, because
# an unattributable deployed artifact is a worse finding than an unchecked one.
checked=$(printf '%s\n' "$modmap" | awk '{print $2}'; echo ck-subc)
# WORKTREES DECLARE THE SAME BINARIES AS THE REPO THEY BRANCH FROM, and they sit
# in this same parent directory. Ownership resolves to whichever match comes
# first, and a worktree name sorts before its parent (`alfonso-wire-v2/` before
# `alfonso/`, because `-` precedes `/`), so the worktree WINS every collision.
#
# The consequence is not a wrong label, it is a check that cannot fail, and both
# observed cases fail SILENTLY in different ways. A worktree pinned to an old
# branch has a HEAD behind the deploy, so the gap computes NEGATIVE and the binary
# reads as current forever (ck-alfonso-core: -335h against a real gap of 0h). A
# worktree whose parent repository has since been RENAMED cannot resolve its git
# dir at all, so the gap is EMPTY and the arithmetic is skipped entirely
# (broca-deploy still points at llm-runner/, renamed to broca/ months ago).
# Neither prints anything. This is the silent half of the section -- the noisy
# half announces itself, this one never does.
#
# Identified structurally rather than by name: a worktree's `.git` is a FILE, a
# real repository's is a DIRECTORY.
bin_owner=$(for d in "$HOME/Work/Projects/CortexKit"/*/; do
  [ -f "$d/Cargo.toml" ] || continue
  [ -d "$d/.git" ] || continue
  (cd "$d" && cargo metadata --no-deps --offline --format-version 1 2>/dev/null \
    | REPO="$(basename "$d")" python3 -c '
import sys, json, os
try:
    m = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for p in m["packages"]:
    for t in p["targets"]:
        if "bin" in t["kind"]:
            print(t["name"], os.environ["REPO"])
' 2>/dev/null)
done)
uncovered=0
skipped=0
extra=0
for f in "$BIN"/*; do
  b=$(basename "$f")
  # Skip the backup copies the deploy ritual leaves behind: dated snapshots and
  # pre-/staged- prefixes are deliberate history, not deployed surface.
  # *.bak-<anything> and *.broken-* joined the list after the first full run
  # printed 27 backup copies as "NO REPO DECLARES IT" -- drowning the two real
  # husks the line exists to surface. The husk report only means something when
  # deliberate deploy-ritual residue cannot reach it.
  case "$b" in *.bak|*.bak-*|*.broken-*|*pre-*|*staged-*|*rollback*|*reclamation*|*.[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]T*) skipped=$((skipped + 1)); continue ;; esac
  printf '%s\n' "$checked" | grep -qx "$b" && continue
  extra=$((extra + 1))
  owner=$(printf '%s\n' "$bin_owner" | awk -v b="$b" '$1==b {print $2; exit}')
  if [ -z "$owner" ]; then
    echo "  $b: deployed but NO REPO DECLARES IT -- cannot check for unshipped code"
    uncovered=1; printed=1; continue
  fi
  d="$HOME/Work/Projects/CortexKit/$owner"
  h=$(cd "$d" && git log -1 --format='%ct' 2>/dev/null) || continue
  m=$(stat -Lf '%m' "$f" 2>/dev/null) || continue
  g=$(( (h - m) / 3600 ))
  [ "$g" -ge 24 ] || continue
  printf '  %-24s %sh behind %s master (not a supervised module -- check if still wanted)\n' "$b" "$g" "$owner"
  uncovered=1; printed=1
done

# A SWEEP REPORTS TWO NUMBERS: what it found, and what it LOOKED AT. The first is
# useless without the second, because a clean result over a silently truncated set
# is indistinguishable from a clean result over the whole one. The denominator
# here is built from what the loops actually examined -- modules from the config,
# the daemon, and the bin/ entries neither covered -- rather than from a count of
# the directory, so a future skip that nobody remembered to disclose shrinks this
# number instead of hiding inside it. Backup copies are counted separately: they
# are DELIBERATELY excluded, and folding a deliberate exclusion into the same
# number as an examined artifact is how a denominator stops meaning anything.
[ "$printed" -eq 0 ] && [ -n "$modmap" ] && \
  echo "  no binary more than 6h behind its master ($(printf '%s\n' "$modmap" | grep -c .) modules + daemon + ${extra:-0} other bin/ entries checked; ${skipped:-0} backup copies skipped)"
# The boundary belongs in the output, not in someone's memory of how this works.
# Without it the next reader takes a clean result as fleet-wide, which it is not:
# a stale Cloudflare Worker has no local mtime and cannot appear here at all --
# and when a Worker rejects first, a freshly deployed binary behind it still
# fails. mtime is also only a screen: proving WHICH build runs needs the artifact
# itself (inode for no-replace-in-place, symbol presence for which build).
dim "  covers local binaries only -- cloud-deployed code cannot appear here"
# THE UPPER BOUND HAS A MIRROR THAT FAILS THE OTHER WAY, and a module owner found
# it by auditing a clean verdict rather than a noisy one. The path filter excludes
# tests/ and benches/, which is right for a repo that keeps only tests there and
# WRONG for one that keeps runtime code under those paths -- and in the second
# case this section reports clean while real code is unshipped. It cannot be
# fixed from here: whether a file under tests/ ships is a fact about that repo's
# layout, not about its path. The owner who raised it was clean anyway, but only
# because they had deployed that morning -- correct by circumstance rather than
# by coverage, which is precisely what a clean line cannot distinguish.
dim "  a clean line means nothing under tests/ or benches/ was checked -- ask the owner whether runtime code lives there"
# The runtime-file count is an UPPER BOUND, and saying so here is cheaper than
# the alternative. The filter excludes tests/ and benches/ DIRECTORIES, but a
# Rust file commonly carries its tests inline under `#[cfg(test)] mod tests`, and
# a path-based filter structurally cannot see a region defined by an attribute.
# So a commit that only adds an in-file test still counts as an unshipped runtime
# file. Detecting that properly means deciding whether each changed hunk falls
# inside a conditionally-compiled module -- brace matching gets this wrong on
# `#[cfg(test)]` applied to individual items, which is how a filter of that shape
# reads a file as clean while it ships real code.
#
# The second cause is COMMENT-ONLY EDITS, and the reason this section does not try
# to filter them is worth stating rather than leaving as an omission. The obvious
# filter -- strip comment lines, hash the rest, compare -- MEASURES LAYOUT RATHER
# THAN CODE: a formatter re-wraps an expression when the comment above it changes
# length, so a purely documentary commit reads as a code change. Measured on this
# repo: two comment-only commits reported as real, and the tell was that their
# hashes SWAPPED, x -> y then y -> x, which is the signature of a re-wrap and its
# reversal rather than of behaviour. A whitespace-blind normalisation answers
# correctly, but it belongs at the deploy decision where one repo is being
# examined, not in a fleet sweep that would run it over every pending commit.
#
# Over-reporting is the deliberate direction for both causes: a missed stale deploy
# costs far more than a line someone checks and dismisses. But an unstated upper
# bound erodes the section's credibility the first time someone investigates a
# phantom, so the bound is printed rather than remembered.
#
# AND A COMMENT-ONLY COMMIT IS NOT RELIABLY A PHANTOM, which makes the
# over-reporting direction more right than it first appears. Panic sites embed
# their source line numbers into the binary, so inserting comment lines above one
# shifts every later line number and CHANGES THE COMPILED OUTPUT. Measured on a
# sibling repo: a pure-comment commit moved the release hash. So "comments cannot
# reach the binary" is false wherever a panic location or any line-number-bearing
# macro is compiled in -- true only for edits inside `cfg(test)`, which is
# compiled out entirely.
#
# The consequence for a deploy decision: do not dismiss a comment-only commit by
# reading the diff. Build both commits with any embedded build stamp pinned and
# compare artifact hashes. Only equality is conclusive.
dim "  count is an upper bound: in-file #[cfg(test)] and comment-only edits count as runtime"
echo

dim "idle is a proxy for attention, not for progress -- confirm before acting"

# --- engram backup progress -------------------------------------------------
# No durable ROW advances while a generation uploads: `generations` is written
# twice (insert at capture, flip at publish) and nothing in between. The
# per-object counter is the upload sidecar, one object id appended and fsynced
# per confirmed upload -- which is also the resume record, so it is by
# construction the true measure of completed work.
#
# Read it as a RATE, never as a ratio: the file is append-as-you-go, so the
# denominator is not final until published=1.
#
# The observation window must be sized against the SLOWEST rate that still counts
# as progress, not the fastest. An earlier version sampled 30s and declared a
# stall on a zero delta, which was sound at the ~20/min rate it was written
# against -- and cried wolf the first time a generation ran at ~3/min, where a
# 30s window expects fewer than two objects and a zero is ordinary. A false stall
# is expensive twice: it costs an investigation, and it teaches the reader to
# discount the one signal that would matter during a real one.
#
# So a zero window EXTENDS rather than concludes. Only sustained zero across the
# long window is a stall, and a restart is safe then -- it costs the in-flight
# object and resumes from the sidecar.
#
# Written after five store tables all read "unchanged" and all five turned out to
# have zero rows: they belong to a subsystem with no live session on this box.
engram_upload_rate() {
  local db=~/.local/share/cortexkit/engram/store.db
  [ -f "$db" ] || return 0
  local pub sidecar n1 n2 sealed
  pub=$(sqlite3 "$db" "SELECT lower(hex(pub_id)) FROM generations WHERE published=0 ORDER BY device_seq DESC LIMIT 1;" 2>/dev/null)
  [ -z "$pub" ] && { echo "  engram: nothing unpublished"; return 0; }
  sidecar=$(ls ~/.local/share/cortexkit/engram/staging/"$pub"/uploaded-*.hex 2>/dev/null | head -1)
  [ -z "$sidecar" ] && { echo "  engram: gen unpublished, no sidecar yet (sealing)"; return 0; }
  # ASSERT WHAT SHOULD BE PRESENT, NOT WHAT SHOULD BE ABSENT. This counts entries
  # by EXCLUDING the two sidecar kinds, so an unreadable or renamed staging
  # directory yields 0 -- and 0 is a legal value that prints as "1220/0 objects",
  # a broken probe wearing the shape of a real reading. A count phrased as an
  # exclusion cannot distinguish "nothing matched" from "nothing was looked at".
  local staging_dir=~/.local/share/cortexkit/engram/staging/"$pub"
  if [ -d "$staging_dir" ]; then
    sealed=$(ls "$staging_dir" 2>/dev/null | grep -vc 'uploaded-\|journal')
  else
    sealed="?"
  fi
  n1=$(wc -l < "$sidecar")
  sleep 30
  n2=$(wc -l < "$sidecar")
  if [ "$n2" -gt "$n1" ]; then
    echo "  engram: uploading $n2/$sealed objects at $(( (n2 - n1) * 2 ))/min"
    return 0
  fi
  # Zero in 30s: extend to 3 minutes before calling it, and report the rate over
  # the whole window so a slow generation reads as slow rather than as stopped.
  sleep 150
  local n3
  n3=$(wc -l < "$sidecar")
  if [ "$n3" -gt "$n1" ]; then
    echo "  engram: uploading slowly -- $n3/$sealed objects, $(( (n3 - n1) * 60 / 180 ))/min over 3m"
  else
    echo "  engram: STALLED -- $n3/$sealed objects, 0 in 3m (restart is safe, resumes from sidecar)"
  fi
}
