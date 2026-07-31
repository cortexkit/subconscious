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
for candidate in prefrontal alfonso-core; do
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
  cfg_count=$(python3 -c "import json,re,os,sys
p=os.path.expanduser('~/.config/cortexkit/subc.jsonc')
try:
    s=re.sub(r'//.*','',open(p).read())
    print(len(json.loads(s).get('modules',[])))
except Exception:
    pass" 2>/dev/null)
  reported=$(printf '%s\n' "$health" | grep -c '●')
  if [ -n "$cfg_count" ] && [ "$reported" -lt "$cfg_count" ] 2>/dev/null; then
    printf '  %s ok  (%s of %s configured modules reporting -- %s ABSENT from health)\n' \
      "$ok_count" "$reported" "$cfg_count" "$((cfg_count - reported))"
  elif [ -z "$cfg_count" ]; then
    printf '  %s ok  (config unreadable -- cannot say whether any module is ABSENT)\n' "$ok_count"
  else
    printf '  %s ok\n' "$ok_count"
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
# The LIMIT above keeps a busy day from burying the rest of the report, but a
# truncated list that does not say it was truncated is a clean-looking result over
# a partial set -- the same shape as a scanner that will not print what it
# skipped. So count the window separately and say when rows were withheld.
terminal_total=$(sqlite3 "$STORE" "
  SELECT COUNT(*) FROM consult
  WHERE phase IN ('failed','done')
    AND consult_kind IN ('spec','campaign')
    AND caller_session IS NOT NULL AND caller_session <> ''
    AND updated_at > (strftime('%s','now') - 43200) * 1000;" 2>/dev/null)
if [ -n "$terminal" ]; then
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
    # The generation counter above cannot see WITHIN a generation: one that is
    # 60% uploaded and one that has not started read identically. That gap is
    # how a stalled publish hides -- it looks exactly like a slow one, and the
    # counter sits still either way for hours.
    #
    # The upload sidecar is appended as objects land, so its line count is real
    # progress. Sampling it twice is the only thing here that distinguishes
    # moving from stuck, and it costs the seconds between the two reads.
    # THE SIDECAR MUST BELONG TO THE GENERATION BEING REPORTED. An earlier version
    # took the most recently modified sidecar anywhere under staging, which is a
    # different selection rule from the line above (newest by device_seq) and can
    # therefore pick a different generation. Observed live: the header said gen 99
    # while this sampled gen 96's sidecar -- a PUBLISHED generation whose staging
    # directory had not yet been swept -- and reported NOT MOVING, which was true
    # of that file and said nothing about gen 99. A finished upload looks exactly
    # like a stalled one once you are watching the wrong file.
    newest_pub=$(sqlite3 "$ENGRAM_STORE" \
      "SELECT lower(hex(pub_id)) FROM generations WHERE device_seq = $newest;" 2>/dev/null)
    sidecar=""
    if [ -n "$newest_pub" ]; then
      sidecar=$(ls -t "$HOME/.local/share/cortexkit/engram/staging/$newest_pub"/uploaded-*.hex 2>/dev/null | head -1)
    fi
    if [ -n "$sidecar" ]; then
      before=$(wc -l < "$sidecar" 2>/dev/null | tr -d ' ')
      sleep 20
      after=$(wc -l < "$sidecar" 2>/dev/null | tr -d ' ')
      if [ "${after:-0}" -gt "${before:-0}" ] 2>/dev/null; then
        echo "  uploading: $after objects, +$((after - before)) in 20s"
      else
        echo "  NOT MOVING: gen $newest at $after objects, unchanged over 20s -- check before assuming slow"
      fi
    else
      # No sidecar for the reported generation means uploading has not begun --
      # distinct from begun-and-stalled, and the remedies differ.
      echo "  gen $newest has no upload sidecar yet (sealing, or not started)"
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
# records it (alfonso-core lives in alfonso/, thalamus in ai-proxy/, subc-mcp in
# subconscious/). A module whose directory this cannot resolve is REPORTED rather
# than skipped, so the mapping going stale is visible instead of silent.
module_repo() {
  case "$1" in
    alfonso-core | prefrontal) echo alfonso ;;
    thalamus)     echo ai-proxy ;;
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
  if ! bin_epoch=$(stat -f '%m' "$BIN/$binname" 2>/dev/null) || [ -z "$bin_epoch" ]; then
    echo "  $modid: cannot stat $binname -- UNCHECKED, not current"
    printed=1
    continue
  fi
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
  # A binary can also be stale against a SHARED crate with no commit in this repo
  # at all. Modules depend on commons by absolute or ../ path, so those sources
  # never appear in this repo's git log and the comparison above cannot see them
  # -- a fleet-wide fix lands in commons and every module reads as current.
  # Proven by the case that prompted this: an owner-only store-permissions fix
  # merged to commons while all fourteen binaries still carried the old crate.
  commons_head_ms=""
  if grep -q 'cortexkit-\(store\|lease\|paths\|provider-usage\|model-catalog\)' "$dir/Cargo.toml" 2>/dev/null \
     || grep -rq 'cortexkit-\(store\|lease\)' "$dir"/crates/*/Cargo.toml 2>/dev/null; then
    commons_head_ms=$(cd "$HOME/Work/Projects/CortexKit/commons" 2>/dev/null \
      && git log -1 --format='%ct' 2>/dev/null)
  fi
  if [ -n "$commons_head_ms" ] && [ "$commons_head_ms" -gt "$bin_epoch" ] 2>/dev/null; then
    commons_gap_h=$(( (commons_head_ms - bin_epoch) / 3600 ))
    printf '  %-20s predates commons master by %sh -- shared-crate changes are not in this binary\n' \
      "$binname" "$commons_gap_h"
  fi

  # Reported AFTER the shared-crate check, deliberately. A module can have no
  # runtime change of its own while still carrying a stale shared crate, and that
  # is the most important case rather than an edge one: a fleet-wide fix lands in
  # commons and touches no module repo at all. Skipping here first would have
  # silenced exactly the modules a shared fix is waiting on.
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
  echo "$runtime" | head -3 | sed 's/^/      /'
  printed=1
done <<EOF
$modmap
EOF
# The daemon is not a module, so a module-derived list structurally cannot reach
# it -- and it is the one binary whose staleness affects every other. Checked
# separately for that reason, against the subc-core crates only: a commit to the
# mcp shim or a client SDK moves subconscious HEAD without touching the daemon.
subc_dir="$HOME/Work/Projects/CortexKit/subconscious"
if [ -d "$subc_dir/.git" ] && [ -f "$BIN/ck-subc" ]; then
  d_bin=$(stat -f '%m' "$BIN/ck-subc" 2>/dev/null)
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
  case "$b" in *.bak|*pre-*|*staged-*|*rollback*|*reclamation*|*.[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]T*) skipped=$((skipped + 1)); continue ;; esac
  printf '%s\n' "$checked" | grep -qx "$b" && continue
  extra=$((extra + 1))
  owner=$(printf '%s\n' "$bin_owner" | awk -v b="$b" '$1==b {print $2; exit}')
  if [ -z "$owner" ]; then
    echo "  $b: deployed but NO REPO DECLARES IT -- cannot check for unshipped code"
    uncovered=1; printed=1; continue
  fi
  d="$HOME/Work/Projects/CortexKit/$owner"
  h=$(cd "$d" && git log -1 --format='%ct' 2>/dev/null) || continue
  m=$(stat -f '%m' "$f" 2>/dev/null) || continue
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
  sealed=$(ls ~/.local/share/cortexkit/engram/staging/"$pub" 2>/dev/null | grep -vc 'uploaded-\|journal')
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
