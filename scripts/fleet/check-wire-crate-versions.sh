#!/usr/bin/env bash
# Refuse a change to a cross-repo crate's code that does not move its version.
#
# Seven repos in this fleet consume these crates BY RELATIVE PATH -- broca,
# engram, claustrum, callosum, astrocyte, insula, synapse -- so they float to
# whatever this repo's HEAD is. A path dependency is recorded in a consumer's
# Cargo.lock as a bare version string with NO source and NO checksum, so
# `cargo build --locked` over there cannot see that the code moved. Two cases:
#
#   version moved     -> the consumer's --locked build FAILS until they take it
#   version unchanged -> the new code is silently compiled in, lock byte-identical
#
# The second is the common one and there is no signal anywhere. Measured on this
# repo when the hazard was found: subc-protocol had 5 source commits in 30 days
# and 0 version bumps. Nothing broke, because those changes happened to be
# additive -- which is luck, not a mechanism.
#
# DOC-ONLY CHANGES ARE EXEMPT DELIBERATELY. Three of those five commits were
# doc comments. A rule that fires on prose gets ignored on substance, so this
# asks whether the OUTPUT can move, not whether a file did -- the same
# discriminator a corpus regeneration check uses.
set -uo pipefail

BASE="${1:-}"
if [ -z "$BASE" ]; then
  echo "usage: $0 <base-ref>   (e.g. origin/master, HEAD~1)" >&2
  exit 2
fi

if ! git rev-parse --verify --quiet "$BASE" >/dev/null; then
  echo "  base ref '$BASE' does not resolve -- cannot compare, refusing rather than passing" >&2
  exit 2
fi

# Resolvability is not comparability (#72): a shallow checkout or a merge
# commit fetched without its history can hold a ref that RESOLVES while
# `$BASE...HEAD` has NO MERGE BASE -- every diff then fails, and a swallowed
# failure reads as "unchanged", which composes into a confident all-clear over
# zero comparisons. Prove the comparison is possible before counting anything.
if ! git merge-base "$BASE" HEAD >/dev/null 2>&1; then
  echo "  no merge base between '$BASE' and HEAD (shallow checkout?) -- cannot compare, refusing rather than passing" >&2
  exit 2
fi

# Every crate in the workspace, derived from the tree at run time. This used to
# be a hand-kept list of "the crates other repos path-depend on", with a comment
# asking to re-derive it by sweep. It drifted anyway: when subc-daemon was split
# out (2026-09-19) it never joined the list, although six sibling repos
# path-depend on it, so a subc-daemon version regression passed this check
# twice over (2026-09-23). CI cannot see the siblings to derive the consumed set,
# and checking a crate nobody consumes costs one version bump, while missing a
# consumed one costs a silent lock rewrite in another repo. So: all of them.
CRATES=()
for dir in crates/*/; do
  [ -f "$dir/Cargo.toml" ] && [ -d "$dir/src" ] && CRATES+=("$(basename "$dir")")
done

examined=0
violations=0
bumped_crates=()

# This check compares COMMITS ($BASE...HEAD). Uncommitted working-tree edits
# are invisible to it, so "clean" here answers a narrower question than "is
# the tree consistent". Say so instead of letting the narrow answer read as
# the broad one.
#
# `git diff HEAD`, not `git diff`: the bare form compares the working tree to
# the INDEX, so a change that has been `git add`ed reads as clean -- which is
# the natural state right before a commit, i.e. exactly when a human runs this
# by hand. CI never sees it because CI checkouts are clean, which is how it
# survived (FUSI found it in their copy, 2026-09-19; reproduced here on a
# clean tree: staged plant -> bare form CLEAN, HEAD form DIRTY).
if ! git diff --quiet HEAD -- crates/ 2>/dev/null; then
  echo "  note: uncommitted changes exist under crates/ -- this check reads" >&2
  echo "  committed history only ($BASE...HEAD) and does not see them." >&2
fi

# Versions are compared as values against the merge base, not detected as "a
# version line was added". The line test read a DECREASE as a bump: on
# 2026-09-23 a cherry-picked commit reset subc-daemon from 0.20.8 to 0.20.7
# (its worker restored the manifest to its own older base), the diff carried
# `+version = "0.20.7"`, this check passed, and master shipped a version that
# already named different code. Path consumers' locks rewrote silently.
merge_base=$(git merge-base "$BASE" HEAD 2>&1)
if [ $? -ne 0 ]; then
  echo "  merge-base $BASE HEAD failed: $merge_base -- refusing" >&2
  exit 2
fi

# version_of <rev> <manifest>: the package version at that revision, or empty
# when the manifest did not exist there (a new crate).
version_of() {
  git show "$1:$2" 2>/dev/null | grep -m1 -E '^version[[:space:]]*=' | sed -E 's/.*"(.*)".*/\1/'
}

# 0 when $2 is strictly greater than $1 (semver-ordered via sort -V).
version_gt() {
  [ "$1" != "$2" ] && [ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | tail -1)" = "$2" ]
}

for crate in "${CRATES[@]}"; do
  src="crates/$crate/src"
  manifest="crates/$crate/Cargo.toml"
  [ -d "$src" ] || continue

  # A version must never go backwards, whether or not the crate's code changed
  # in this range: a lower number re-names code a consumer may already have
  # locked under the higher one.
  base_version=$(version_of "$merge_base" "$manifest")
  head_version=$(version_of HEAD "$manifest")
  if [ -n "$base_version" ] && version_gt "$base_version" "$head_version"; then
    bumped_crates+=("$crate" "$head_version")
  fi
  if [ -n "$base_version" ] && [ -n "$head_version" ] \
    && [ "$base_version" != "$head_version" ] \
    && ! version_gt "$base_version" "$head_version"; then
    echo "  $crate: version went BACKWARDS, $base_version -> $head_version"
    echo "      a version number already published to path consumers now names different code."
    echo "      set $manifest above $base_version."
    violations=$((violations + 1))
    examined=$((examined + 1))
    continue
  fi

  # Every added/removed line under src/, minus the diff's own +++/--- headers.
  # A rename or a pure move shows up here too, which is correct: a consumer
  # compiles the result either way. The diff's OWN failure must stay separate
  # from its empty output (#72): capture the exit code before filtering, and
  # count the crate as examined only once its comparison actually ran.
  raw_diff=$(git diff "$BASE"...HEAD -- "$src" 2>&1)
  diff_rc=$?
  if [ "$diff_rc" -ne 0 ]; then
    echo "  $crate: git diff failed (rc=$diff_rc): $raw_diff" >&2
    echo "  a failed comparison is not an unchanged crate -- refusing" >&2
    exit 2
  fi
  examined=$((examined + 1))
  changed=$(printf '%s\n' "$raw_diff" \
    | grep -E '^[+-]' | grep -vE '^(\+\+\+|---)' || true)

  # Strip comment and blank lines. Doc comments (///, //!), line comments (//),
  # and block-comment bodies (/*, *, */) are all prose: they cannot change what
  # a consumer compiles.
  substantive=$(printf '%s\n' "$changed" \
    | sed -E 's/^[+-][[:space:]]*//' \
    | grep -vE '^(///|//!|//|/\*|\*|\*/)' \
    | grep -vE '^[[:space:]]*$' || true)

  # The manifest is read either way: a failed diff must refuse, not read as
  # "nothing changed".
  manifest_diff=$(git diff "$BASE"...HEAD -- "$manifest" 2>&1)
  if [ $? -ne 0 ]; then
    echo "  $crate: manifest diff failed: $manifest_diff -- refusing" >&2
    exit 2
  fi

  # A dependency edit in the manifest changes what a consumer compiles as
  # surely as a source edit does, and for a registry consumer it is the whole
  # difference: crates.io keeps a version's dependency requirements forever.
  # On 2026-09-23 subc-control's subc-protocol requirement moved ^0.22 -> ^0.23
  # under an unchanged version, so the publisher skipped it (0.16.1 already
  # existed), and every crate needing the ^0.23 control failed package
  # verification against the published ^0.22 one. Any changed manifest line
  # other than the package's own version line, a comment or a blank counts.
  manifest_substantive=$(printf '%s\n' "$manifest_diff" \
    | grep -E '^[+-]' | grep -vE '^(\+\+\+|---)' \
    | sed -E 's/^[+-][[:space:]]*//' \
    | grep -vE '^version[[:space:]]*=' \
    | grep -vE '^#' \
    | grep -vE '^[[:space:]]*$' || true)
  [ -z "$substantive" ] && [ -z "$manifest_substantive" ] && continue
  # Moved means strictly INCREASED against the merge base; a decrease was
  # refused above. The diff is still read so a failed comparison refuses.
  if [ -z "$base_version" ] || version_gt "$base_version" "$head_version"; then
    continue
  fi

  cur=$(grep -m1 -E '^version[[:space:]]*=' "$manifest" | sed -E 's/.*"(.*)".*/\1/')
  if [ -n "$substantive" ]; then
    echo "  $crate: code changed, version still $cur"
  else
    echo "  $crate: dependencies changed in its manifest, version still $cur"
  fi
  echo "      seven repos path-depend on these crates and cannot see this."
  echo "      bump $manifest, or confirm the change is doc-only."
  violations=$((violations + 1))
done

# A run that examined nothing is not a pass. If the crate list stops matching
# the tree -- renamed, moved, restructured -- this would otherwise report clean
# over an empty set, which is the failure the whole file exists to prevent.
if [ "$examined" -eq 0 ]; then
  echo "  no crates matched under crates/ -- the crate list is stale, refusing" >&2
  exit 2
fi

# Path dependencies compile against the workspace's newest code even when their
# registry requirement still permits an older release. Only dependents with
# substantive changes under src/ need to raise that floor when a sibling is
# bumped.
if [ "${#bumped_crates[@]}" -gt 0 ]; then
  # Cargo handles renamed, inherited and target-specific dependencies. Read a
  # committed snapshot so uncommitted manifest edits cannot hide a stale floor.
  mkdir -p target/scratch
  snapshot=$(mktemp -d "$(pwd)/target/scratch/wire-versions.XXXXXX") || exit 2
  trap 'rm -rf "$snapshot"' EXIT
  if ! git archive HEAD | tar -x -C "$snapshot"; then
    echo "  could not read committed workspace -- refusing" >&2
    exit 2
  fi
  if ! (cd "$snapshot" && cargo metadata --format-version 1 --no-deps --offline) > "$snapshot/metadata.json"; then
    echo "  cargo metadata failed for committed workspace -- refusing" >&2
    exit 2
  fi
  python3 - "$snapshot" "$merge_base" "${bumped_crates[@]}" <<'PY'
import json
import pathlib
import re
import subprocess
import sys

snapshot = pathlib.Path(sys.argv[1])
base = sys.argv[2]
bumps = {f"crates/{crate}/Cargo.toml": version
         for crate, version in zip(sys.argv[3::2], sys.argv[4::2])}
metadata = json.loads((snapshot / "metadata.json").read_text())
packages = [p for p in metadata["packages"] if p["id"] in metadata["workspace_members"]]
bumped = {
    p["name"]: bumps[str(pathlib.Path(p["manifest_path"]).relative_to(snapshot))]
    for p in packages
    if str(pathlib.Path(p["manifest_path"]).relative_to(snapshot)) in bumps
}


def version_key(version):
    release, _, prerelease = version.split("+", 1)[0].partition("-")
    numbers = tuple(map(int, release.split(".")))
    identifiers = tuple((0, int(s)) if s.isdigit() else (1, s) for s in prerelease.split("."))
    return numbers, not prerelease, identifiers


def requirement_at_least(requirement, version):
    # Cargo joins comparators with AND, so the strongest lower bound wins.
    # Upper bounds alone (and '*') do not prevent selecting an older release.
    for comparator in requirement.split(","):
        match = re.fullmatch(r"\s*(>=|<=|>|<|=|\^|~)?\s*(\d+(?:\.\d+){0,2})(?:\.[*xX])?(-[0-9A-Za-z.-]+)?\s*", comparator)
        if not match:
            if comparator.strip() == "*":
                continue
            print(f"  unsupported Cargo requirement: {requirement} -- refusing", file=sys.stderr)
            sys.exit(2)
        operator, release, prerelease = match.groups()
        if operator in ("<", "<="):
            continue
        numbers = list(map(int, release.split(".")))
        width = len(numbers)
        numbers += [0] * (3 - width)
        if operator == ">":
            if prerelease:
                prerelease += ".0"
            else:
                numbers[width - 1] += 1
                # An explicit prerelease comparator opts that release into
                # Cargo's candidate set, including releases between patches.
                next_release = ".".join(map(str, numbers))
                if re.search(r"(?<![\d.])" + re.escape(next_release) + "-", requirement):
                    prerelease = "-0"
        floor = ".".join(map(str, numbers)) + (prerelease or "")
        if version_key(floor) >= version_key(version):
            return True
    return False


violations = 0
for package in packages:
    manifest = pathlib.Path(package["manifest_path"]).relative_to(snapshot)
    result = subprocess.run(
        ["git", "diff", "--no-ext-diff", "--no-color", base, "HEAD", "--", str(manifest.parent / "src")],
        capture_output=True, text=True,
    )
    if result.returncode:
        print(f"  source comparison failed: {result.stderr} -- refusing", file=sys.stderr)
        sys.exit(2)
    diff = result.stdout
    # Match the version-bump check's exemption for prose and blank lines.
    changed = [line[1:].lstrip() for line in diff.splitlines()
               if line.startswith(("+", "-")) and not line.startswith(("+++", "---"))]
    if not any(line and not line.startswith(("//", "/*", "*")) for line in changed):
        continue
    for dependency in package["dependencies"]:
        name = dependency["name"]
        if name == package["name"] or name not in bumped:
            continue
        requirement = dependency["req"]
        if not requirement_at_least(requirement, bumped[name]):
            print(f"  {package['name']}: dependency {name} requires {requirement}, below new version {bumped[name]}")
            print("      raise the sibling requirement when changing dependent source alongside a sibling bump.")
            violations += 1
sys.exit(1 if violations else 0)
PY
  sibling_rc=$?
  if [ "$sibling_rc" -gt 1 ]; then
    echo "  sibling requirement check failed -- refusing" >&2
    exit 2
  fi
  violations=$((violations + sibling_rc))
fi

if [ "$violations" -gt 0 ]; then
  echo "  version checks failed: missing bumps, backwards versions, or stale sibling requirements ($examined cross-repo crates examined)"
  exit 1
fi

echo "  $examined cross-repo crates examined against $BASE, version bumps and sibling requirements are consistent"
