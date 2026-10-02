#!/usr/bin/env bash
# Sweep placement snapshots left beside live binaries in the deploy bin.
#
# Four naming conventions from different eras coexist there
# (`ck-<m>.rollback-<UTCZ>`, `ck-<m>.bak-<stamp>`, `ck-<m>.pre-<version>`,
# `ck-<m>.<version>.pre-<thing>`), so the rule is deliberately prefix-blind:
# for each `ck-<binary>`, keep the TWO NEWEST `ck-<binary>.*` files by mtime
# and unlink the rest. place-module.sh writes its rollbacks into STAGING,
# not here, so nothing in bin/ is a current recovery path; the two kept per
# binary are for the placements other seats ran by hand.
#
# A manifest (sha256, size, mtime, name) is written before the first unlink.
# Reports the df delta, not the sum of unlinked sizes.
#
# On 2026-09-20 this directory held 275 snapshots / 3.6 GB, and every one was
# an executable `ck-*` on PATH that `ck` domain discovery probed once with
# `--ck-domain` (fixed in ck 0.20.7 by refusing dotted names as candidates).
#
# Usage: sweep-bin-backups.sh [--dry-run]
set -euo pipefail

BIN="${CK_BIN_DIR:-$HOME/.local/share/cortexkit/bin}"
RUN="${CK_RUN_DIR:-$HOME/.local/share/cortexkit/run}"
KEEP=2
DRY=0
[ "${1:-}" = "--dry-run" ] && DRY=1

cd "$BIN"
list=$(mktemp)
# Bases are the part before the first dot; the live binary has no dot and is
# never listed, which is what keeps the live binary structurally unreachable.
ls | grep -E '^ck[a-z-]*\.' | sed -E 's/^(ck[a-z-]*)\..*/\1/' | sort -u |
  while read -r base; do ls -t "$base".* 2>/dev/null | tail -n +$((KEEP + 1)); done > "$list"

count=$(wc -l < "$list" | tr -d ' ')
if [ "$count" -eq 0 ]; then
  echo "nothing to sweep: every ck-<binary> has at most $KEEP snapshots"
  rm -f "$list"; exit 0
fi

# Refuse if a candidate has no dot (would be a live binary) -- cannot happen
# by construction of the listing above, and this is the check that says so.
if grep -qvE '^ck[a-z-]*\.' "$list"; then
  echo "REFUSED: a candidate is not a dotted snapshot name:"; grep -vE '^ck[a-z-]*\.' "$list"; exit 2
fi

before=$(df -k / | awk 'NR==2 {print $4}')
manifest="$RUN/bin-backup-sweep-$(date -u +%Y%m%dT%H%M%SZ).manifest"
# GNU and BSD stat have incompatible formatting switches. Probe the utility,
# not the OS: a BSD host can also have GNU coreutils on PATH.
stat_style=bsd
if stat --version >/dev/null 2>&1; then stat_style=gnu; fi
{
  echo "# bin backup sweep $(date -u +%FT%TZ); keep $KEEP newest per ck-<binary>.* regardless of prefix"
  while read -r f; do
    digest=$(shasum -a 256 "$f" | cut -d' ' -f1)
    if [[ "$stat_style" == "gnu" ]]; then
      size=$(stat -c %s "$f")
      mtime=$(TZ=UTC stat -c %y "$f")
      mtime=${mtime/ /T}
      mtime="${mtime%%.*}Z"
    else
      size=$(stat -f %z "$f")
      mtime=$(TZ=UTC stat -f %Sm -t %FT%TZ "$f")
    fi
    # Standalone assignments above fail closed on a metadata error; command
    # substitutions inside echo would report success and permit deletion.
    printf '%s  %s  %s  %s\n' "$digest" "$size" "$mtime" "$f"
  done < "$list"
} > "$manifest"
echo "manifest: $manifest ($count rows)"

if [ "$DRY" -eq 1 ]; then
  echo "dry run: would unlink $count file(s); keeping $KEEP newest per binary"
  rm -f "$list"; exit 0
fi

n=0
while read -r f; do [ -f "$f" ] && rm -f -- "$f" && n=$((n + 1)); done < "$list"
residue=$(while read -r f; do [ -e "$f" ] && echo x; done < "$list" | wc -l | tr -d ' ')
after=$(df -k / | awk 'NR==2 {print $4}')
echo "unlinked $n; residue $residue; df delta $(( (after - before) / 1024 )) MiB; remaining snapshots $(ls | grep -cE '^ck[a-z-]*\.')"
rm -f "$list"
