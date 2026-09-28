#!/usr/bin/env bash
#
# Refuses to place an image that cannot read what is already on disk.
#
# A module that rolls its on-disk formats forward records, per data tree, the
# lowest version of each format a build must read to open that tree safely:
#
#     <data home>/cortexkit/<module>/**/format-floor.json
#     {"version":1,"floors":{"<format>":<int>}}
#
# Floors are raised before the first record of a newer format is written and
# never lowered. Each staged card declares what it reads, beside the card:
#
#     <card>.format-versions.json   {"<format>": <max version read>}
#
# This check compares the two. It exists for the case staging cannot see:
# re-placing an OLDER image (a retained rollback) after a newer build has
# already written data only newer builds can read. A binary rollback there
# starts a process that misreads or refuses its own store.
#
# Usage: check-format-floors.sh <module-id> <image-path>
# Exit 0: no floors recorded, or every floor is met. Exit 2: refused.
set -euo pipefail

module="${1:?module id required}"
image="${2:?image path required}"
data_home="${XDG_DATA_HOME:-$HOME/.local/share}"
tree="$data_home/cortexkit/$module"

refuse() { printf 'REFUSED: %s\n' "$*" >&2; exit 2; }

floors=()
if [ -d "$tree" ]; then
  while IFS= read -r f; do floors+=("$f"); done < <(find "$tree" -maxdepth 4 -name format-floor.json -type f 2>/dev/null | sort)
fi
if [ "${#floors[@]}" -eq 0 ]; then
  echo "format floors: none recorded under $tree"
  exit 0
fi

# The map is looked up beside the image, and beside the image's name without a
# SIGNED. prefix, because some modules stage their signed copy under that prefix.
versions=""
for candidate in "$image.format-versions.json" \
                 "$(dirname "$image")/$(basename "$image" | sed 's/^SIGNED\.//').format-versions.json"; do
  if [ -f "$candidate" ]; then versions="$candidate"; break; fi
done
[ -n "$versions" ] || refuse "$module has format floors (${floors[*]}) but $image carries no format-versions map, so nothing proves it can read the stored data. If you have checked what this image reads, write $image.format-versions.json by hand and re-run."

python3 - "$versions" "${floors[@]}" <<'PY'
import json, sys
versions_path, floor_paths = sys.argv[1], sys.argv[2:]
try:
    versions = json.load(open(versions_path))
except Exception as e:
    print(f"REFUSED: cannot read format-versions map {versions_path}: {e}", file=sys.stderr)
    sys.exit(2)
if not isinstance(versions, dict):
    print(f"REFUSED: {versions_path} is not a JSON object", file=sys.stderr)
    sys.exit(2)
short = []
for path in floor_paths:
    try:
        doc = json.load(open(path))
        floors = doc["floors"]
        assert isinstance(floors, dict)
    except Exception as e:
        # A floor file we cannot read is not a floor we can check against, and
        # guessing "no floor" is the unsafe direction.
        print(f"REFUSED: cannot read floor file {path}: {e}", file=sys.stderr)
        sys.exit(2)
    for fmt, floor in sorted(floors.items()):
        have = versions.get(fmt)
        if not isinstance(have, int) or have < floor:
            short.append(f"{fmt}: floor {floor} in {path}, image reads {have if have is not None else 'nothing'}")
        else:
            print(f"format floor: {fmt} floor {floor}, image reads {have} ({path})")
if short:
    print("REFUSED: the image cannot read data already on disk:\n  " + "\n  ".join(short), file=sys.stderr)
    sys.exit(2)
PY
