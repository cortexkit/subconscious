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
scanned=0
while IFS= read -r f; do
  scanned=$((scanned + 1))
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
# THE DENOMINATOR GATES THE CLAIM, and it must be checked BEFORE the findings.
# Over an empty tree every finding count is zero -- including any count that
# exists to detect a broken scan -- so a guard placed inside the "we found
# something" branch is unreachable in exactly the case it was written for. A
# vacuity guard that depends on the scan having worked is not a guard.
#
# This previously printed the clean line plus a suggestion that the reader check
# the file count by hand. Advice is not a guard either: it is discharged only if
# someone reads it and acts, and a clean result is the least likely thing anyone
# re-examines.
if [ "$scanned" -eq 0 ]; then
  echo "NO FILES EXAMINED under $root -- this run proves nothing."
  echo "Check the root is right: find $root -name '*.rs' | wc -l"
  exit 2
elif [ "$found" -eq 0 ]; then
  echo "No file would truncate below ${threshold}% (${scanned} file(s) examined)."
else
  echo "$found of ${scanned} file(s) would truncate at or below ${threshold}%."
  # The anchor advice must not swap one marker-offset defect for its mirror:
  # cutting AT the tests module assumes the module is terminal, and top-level
  # code appended after it (where the least careful edit lands) would read as
  # test code. The safe shape skips the module's brace-balanced extent.
  # `mod tests` by name is its own blind spot: 6 of 65 test modules in one
  # sibling crate are named otherwise (`stage_tests`, `wallet_tests`), and an
  # anchor that never fires counts the whole test module AS production. The
  # predicate is the attribute followed by `mod <any identifier> {`.
  echo "Anchor on '#[cfg(test)] mod <name>' (any name, not only 'tests'), skip"
  echo "each module's brace-balanced extent, and keep the code after it: that is"
  echo "production. Then assert the stripped body has no #[test] or"
  echo "#[tokio::test] left; truncation and a missed module both fail that."
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

# Printed every run, zero included. This count DESCRIBES the corpus rather than
# selecting from it, so if the detector above breaks, every other number stays
# identical and only this line changes -- and a line that prints solely when
# non-zero simply disappears. A reader cannot notice a line that is not there.
echo
echo "$item_level test-only item(s) carry the attribute with no module after it."
if [ "$item_level" -gt 0 ]; then
  echo "Those sit inside the production body under either anchor. A sweep that"
  echo "finds what it wants in one of them reports a false clean."
fi
