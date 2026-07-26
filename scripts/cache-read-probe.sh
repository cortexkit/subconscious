#!/usr/bin/env bash
# cache-read-probe: drive an N-turn session through a subc rig's llm-runner
# module and report the per-step provider cache economics from the WAL.
#
# The measurement half of the cache_tiers work: run it BEFORE the renderer
# emits cache_control (baseline: cache_read stays 0) and AFTER (retention:
# cache_read > 0 from turn 2, write premium visible on turn 1), same command.
#
# Usage:
#   scripts/cache-read-probe.sh <connection-file> <state-root> [turns] [model]
# Example (the dev rig):
#   scripts/cache-read-probe.sh /tmp/llmr-swift-rig/runtime/subc-connection.json \
#     /tmp/llmr-swift-rig/runtime/llm-runner-state 6 anthropic/claude-haiku-4-5
set -euo pipefail

CONN="${1:?connection file}"
STATE_ROOT="${2:?llm-runner state root (contains wal/)}"
TURNS="${3:-6}"
MODEL="${4:-anthropic/claude-haiku-4-5}"
SESSION="cache-probe-$(date +%s)"
ROOT="${CACHE_PROBE_ROOT:-/tmp/cache-probe-root}"
LLMR_SESSION="${LLMR_SESSION_BIN:-$HOME/Work/Projects/CortexKit/llm-runner/target/release/llmr-session}"

mkdir -p "$ROOT"
echo "session=$SESSION model=$MODEL turns=$TURNS"

# Turn 1 establishes the prefix; later turns append. Prompts grow the tail
# without touching earlier turns so the cached prefix stays byte-stable.
# --cache-role primary opts into the cache policy (an absent cache field means
# no policy at all, the byte-identity default); primary resolves to the 1h tier
# for Anthropic per the shipped defaults.
CACHE_ROLE="${CACHE_PROBE_ROLE:-primary}"
"$LLMR_SESSION" --subc "$CONN" --project-root "$ROOT" --session "$SESSION" \
  --json-events run --prompt "Turn 1: reply with exactly CACHE-PROBE-T1" \
  --cache-role "$CACHE_ROLE" --model "$MODEL" >/dev/null

for i in $(seq 2 "$TURNS"); do
  "$LLMR_SESSION" --subc "$CONN" --project-root "$ROOT" --session "$SESSION" \
    --json-events send \
    --prompt "Turn $i: what is $i+$i? Reply with just the number." \
    --cache-role "$CACHE_ROLE" --model "$MODEL" >/dev/null
  # level-triggered: wait for the run to leave Active before the next send
  for _ in $(seq 1 60); do
    st=$("$LLMR_SESSION" --subc "$CONN" --project-root "$ROOT" --session "$SESSION" \
      status 2>/dev/null | tail -1 || true)
    [[ "$st" == *Idle* ]] && break
    sleep 1
  done
done

echo
echo "step | input | cache_read | cache_write | output | read_ratio"
echo "-----+-------+------------+-------------+--------+-----------"
found=""
  # `strings` here reads a WAL, which is length-prefixed frames wrapping JSON --
  # a DATA file, so the text it looks for is genuinely present as bytes. That is
  # the safe use. The unsafe one is `strings` over a compiled BINARY looking for a
  # source literal: match-arm literals are compared by length-and-bytes and never
  # emitted as a contiguous constant, so they read zero however present they are.
  # Noted here because the two invocations look identical at a glance and only one
  # of them can be trusted.
  for f in "$STATE_ROOT"/wal/*.wal; do
    strings "$f" | grep -q "\"session\":\"$SESSION\"" || continue
  found=1
  strings "$f" | python3 -c '
import sys, json
step = 0
tot_in = tot_read = tot_write = 0
for line in sys.stdin:
    # strings glues WAL frame-header bytes onto the JSON payload; parse from
    # the first brace.
    brace = line.find("{")
    if brace < 0:
        continue
    try:
        r = json.loads(line[brace:].strip())
    except json.JSONDecodeError:
        continue
    if r.get("type") != "model_step_finished":
        continue
    u = r.get("usage", {})
    step += 1
    i, cr, cw, o = (u.get("input_tokens", 0), u.get("cached_input_tokens", 0),
                    u.get("cache_write_tokens", 0), u.get("output_tokens", 0))
    tot_in, tot_read, tot_write = tot_in + i, tot_read + cr, tot_write + cw
    denom = i + cr
    ratio = f"{cr/denom:.0%}" if denom else "-"
    print(f"{step:4} | {i:5} | {cr:10} | {cw:11} | {o:6} | {ratio}")
denom = tot_in + tot_read
print("-----+-------+------------+-------------+--------+-----------")
ratio = f"{tot_read/denom:.0%}" if denom else "-"
print(f" sum | {tot_in:5} | {tot_read:10} | {tot_write:11} |        | {ratio}")
if tot_read == 0:
    print("\nNOTE: zero cache reads. Expected BEFORE the renderer emits cache")
    print("markers/hints; a regression AFTER cache_tiers ships.")
'
done
[[ -n "$found" ]] || { echo "no WAL found for $SESSION under $STATE_ROOT/wal"; exit 1; }
