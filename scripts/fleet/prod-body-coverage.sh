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
  echo "Anchor on '#[cfg(test)]' immediately followed by 'mod tests' instead."
fi
