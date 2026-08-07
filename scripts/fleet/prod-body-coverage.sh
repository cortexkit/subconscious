#!/usr/bin/env bash
# Report how much of each Rust file a naive "cut at the first #[cfg(test)]"
# scan would actually read.
#
# Excluding test code by cutting at the first #[cfg(test)] is wrong: that
# attribute also guards test-only accessors and constructors on production
# types, which sit wherever the author put them. The cut position is therefore
# uncorrelated with how much test code a file has -- a file that is 90% tests
# can read fine, while two lines of scaffolding near the top can leave a scan
# reading 4% of a router.
#
# Truncation reports things ABSENT that are present, so its artefacts look like
# findings rather than failures, and a real gap in the same run is
# indistinguishable from them.
#
# Printing coverage is the load-bearing half. Fixing an anchor is silent, so the
# next wrong anchor produces the same confident output; a line saying a file
# would truncate at 4% cannot be mistaken for silence.
set -euo pipefail

root=${1:-crates}
threshold=${2:-50}

printf '%-52s %7s %7s %7s\n' FILE CUT TOTAL READS
found=0
while IFS= read -r f; do
  cut=$(grep -n '#\[cfg(test)\]' "$f" 2>/dev/null | head -1 | cut -d: -f1) || true
  [ -n "${cut:-}" ] || continue
  total=$(wc -l < "$f" | tr -d ' ')
  [ "$total" -gt 0 ] || continue
  pct=$(( cut * 100 / total ))
  if [ "$pct" -le "$threshold" ]; then
    printf '%-52s %7s %7s %6s%%\n' "$f" "$cut" "$total" "$pct"
    found=$((found + 1))
  fi
done < <(find "$root" -name '*.rs' | sort)

echo
if [ "$found" -eq 0 ]; then
  echo "No file would truncate below ${threshold}%."
  echo "If that seems too clean, check the search found any files at all:"
  echo "  find $root -name '*.rs' | wc -l"
else
  echo "$found file(s) would truncate at or below ${threshold}%."
  echo "Anchor on '#[cfg(test)]' immediately followed by a module instead."
fi

# The recommended anchor is better than the naive cut and still incomplete, in
# the opposite direction. A test-only ITEM -- a constructor, an accessor, an
# injection hook -- carries the attribute with no module after it, so anchoring
# on module-following occurrences leaves those items inside what a scan calls
# production code.
#
# That direction is the quieter one. Truncating reports things ABSENT that are
# present, which produces a finding somebody investigates. Over-including
# reports things PRESENT that exist only under cfg(test), so a sweep asking
# "does every file do X" can find X in a test-only helper and call the file
# fine -- a null nobody looks at twice.
#
# Reported rather than excised: removing them needs each item's span, and a
# brace-matched span is the same guessing game that produced the original
# defect. Naming them lets a reader check whether a result rests on one.
item_level=0
while IFS= read -r f; do
  while IFS=: read -r ln _; do
    case "$(sed -n "$((ln + 1))p" "$f")" in
      *"mod "*) ;;
      *) item_level=$((item_level + 1)) ;;
    esac
  done < <(grep -n '#\[cfg(test)\]' "$f" 2>/dev/null)
done < <(grep -rl '#\[cfg(test)\]' "$root" --include='*.rs' 2>/dev/null)

if [ "$item_level" -gt 0 ]; then
  echo
  echo "$item_level test-only item(s) carry the attribute with no module after it."
  echo "Those sit inside the production body under either anchor. A sweep that"
  echo "finds what it wants in one of them reports a false clean."
fi
