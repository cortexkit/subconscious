#!/usr/bin/env bash
#
# Keep the newest N placement rollbacks per name in the staging directory and
# unlink the rest, with their .sha256 and .format-versions.json siblings.
#
# A rollback is `<name>.rollback-<YYYYMMDD>T<HHMMSS>[Z]` (older placements wrote
# the stamp without the Z). Stamps sort lexically in time order, so "newest" is
# decided by name, never by mtime, which a copy or touch can change.
#
# Legacy store snapshots named only `store.db.rollback-*` are REFUSED as a name:
# several modules' stores share that basename, so "keep the newest N" would
# delete one module's only store backup because another module's is newer.
# place-module.sh now writes `<module>.<store basename>.rollback-*`.
#
# A manifest (sha256, size, name) is written before the first unlink.
#
# Usage: prune-rollbacks.sh [--keep N] [--apply] [name ...]
#   No names: every name found in staging, except the refused legacy one.
#   Default is a dry run; --apply unlinks.
set -euo pipefail

STAGING="${CK_STAGING:-$HOME/.local/share/cortexkit/staging}"
KEEP=3
APPLY=0
names=()
while (($# > 0)); do
  case "$1" in
    --keep) KEEP="$2"; shift 2 ;;
    --apply) APPLY=1; shift ;;
    -*) echo "REFUSED: unknown argument '$1'" >&2; exit 2 ;;
    *) names+=("$1"); shift ;;
  esac
done
case "$KEEP" in ''|*[!0-9]*) echo "REFUSED: --keep must be a whole number" >&2; exit 2 ;; esac
[ "$KEEP" -ge 1 ] || { echo "REFUSED: --keep must be at least 1; zero would delete every rollback" >&2; exit 2; }

stamp_re='\.rollback-[0-9]{8}T[0-9]{6}Z?$'
cd "$STAGING"
if [ "${#names[@]}" -eq 0 ]; then
  while IFS= read -r n; do names+=("$n"); done < <(ls | grep -E "$stamp_re" | sed -E "s/$stamp_re//" | sort -u)
fi

list=$(mktemp)
trap 'rm -f "$list"' EXIT
for name in "${names[@]}"; do
  if [ "$name" = "store.db" ]; then
    echo "skipped: store.db (legacy store snapshots of several modules share this name; prune by hand after identifying each)"
    continue
  fi
  ls | grep -E "^$(printf '%s' "$name" | sed 's/[.[\*^$]/\\&/g')$stamp_re" | sort \
    | awk -v keep="$KEEP" '{ line[NR] = $0 } END { for (i = 1; i <= NR - keep; i++) print line[i] }' >> "$list" || true
done

count=$(wc -l < "$list" | tr -d ' ')
if [ "$count" -eq 0 ]; then
  echo "nothing to prune: every name has at most $KEEP rollbacks"
  exit 0
fi

manifest="$STAGING/rollback-prune-$(date -u +%Y%m%dT%H%M%SZ).manifest"
{
  echo "# rollback prune $(date -u +%FT%TZ); keep $KEEP newest per name; apply=$APPLY"
  while read -r f; do
    for g in "$f" "$f.sha256" "$f.format-versions.json"; do
      [ -e "$g" ] && echo "$(shasum -a 256 "$g" | cut -d' ' -f1)  $(stat -f %z "$g" 2>/dev/null || stat -c %s "$g")  $g"
    done
  done < "$list"
} > "$manifest"
bytes=$(awk '!/^#/ {s+=$2} END {print s+0}' "$manifest")
echo "manifest: $manifest ($count rollbacks, $((bytes / 1048576)) MiB)"

if [ "$APPLY" -ne 1 ]; then
  echo "dry run: would unlink $count rollback(s) and their sidecars; pass --apply"
  exit 0
fi
while read -r f; do rm -f "$f" "$f.sha256" "$f.format-versions.json"; done < "$list"
echo "unlinked $count rollback(s) and their sidecars"
