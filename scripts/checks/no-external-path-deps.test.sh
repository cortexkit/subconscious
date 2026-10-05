#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(cd -- "$script_dir/../.." && pwd -P)
check="$script_dir/no-external-path-deps.sh"
test_root=$(mktemp -d)
trap 'rm -rf "$test_root"' EXIT

fail() {
  printf 'test failure: %s\n' "$1" >&2
  exit 1
}

run_check() {
  local output_path="$1"
  local manifest_path="$2"
  local status=0

  CARGO_NET_OFFLINE=true bash "$check" "$manifest_path" >"$output_path" 2>&1 || status=$?
  printf '%s\n' "$status"
}

expect_violation() {
  local status="$1"
  local output_path="$2"
  local dependency_name="$3"
  local label="$4"

  if [[ "$status" != 3 ]]; then
    cat "$output_path" >&2
    if [[ "$status" == 0 ]]; then
      fail "$label unexpectedly passed instead of exiting 3 for a violation"
    fi
    fail "$label: the check itself failed with exit code $status instead of reporting a violation with exit code 3"
  fi
  if ! grep -F "external path dependency: $dependency_name " "$output_path" >/dev/null; then
    cat "$output_path" >&2
    fail "$label exited 3 without naming $dependency_name"
  fi
  assert_fixture_counts "$output_path" "$label"
}

write_crate() {
  local crate_dir="$1"
  local name="$2"
  local dependency_name="${3:-}"
  local dependency_path="${4:-}"

  mkdir -p "$crate_dir/src"
  {
    printf '[package]\nname = "%s"\nversion = "0.1.0"\nedition = "2021"\n' "$name"
    if [[ -n "$dependency_name" ]]; then
      printf '\n[dependencies]\n%s = { path = "%s" }\n' "$dependency_name" "$dependency_path"
    fi
  } >"$crate_dir/Cargo.toml"
  printf 'pub fn fixture() {}\n' >"$crate_dir/src/lib.rs"
}

assert_fixture_counts() {
  local output_path="$1"
  local label="$2"

  if ! grep -F 'cargo metadata examined 2 packages (2 path packages)' "$output_path" >/dev/null; then
    cat "$output_path" >&2
    fail "$label did not report both packages and both path packages"
  fi
}

inside_root="$test_root/inside/root"
inside_dependency="$inside_root/local-dependency"
write_crate "$inside_dependency" 'local-dependency'
write_crate "$inside_root" 'inside-root' 'local-dependency' './local-dependency'
cargo generate-lockfile --offline --manifest-path "$inside_root/Cargo.toml"

inside_output="$test_root/inside-output"
inside_status=$(run_check "$inside_output" "$inside_root/Cargo.toml")
if [[ "$inside_status" != 0 ]]; then
  cat "$inside_output" >&2
  fail "a path dependency inside the workspace did not pass; check itself failed with exit code $inside_status"
fi
assert_fixture_counts "$inside_output" 'in-workspace fixture'
printf 'PASS: in-workspace path dependency\n'

outside_root="$test_root/outside/root"
outside_dependency="$test_root/outside/sibling-dependency"
write_crate "$outside_dependency" 'sibling-dependency'
write_crate "$outside_root" 'outside-root' 'sibling-dependency' '../sibling-dependency'
cargo generate-lockfile --offline --manifest-path "$outside_root/Cargo.toml"

outside_output="$test_root/outside-output"
outside_status=$(run_check "$outside_output" "$outside_root/Cargo.toml")
expect_violation "$outside_status" "$outside_output" 'sibling-dependency' 'external-path fixture'
printf 'PASS: external path dependency rejected and named\n'

replace_root="$test_root/replace/root"
replace_registry="$test_root/replace/registry"
replace_dependency='replace-dependency'
replace_target="$test_root/replace/sibling-replacement"
write_crate "$replace_target" "$replace_dependency"
mkdir -p "$replace_root/src" "$replace_root/.cargo" "$replace_registry/re/pl"
printf 'fn main() {}\n' >"$replace_root/src/main.rs"
printf '[package]\nname = "replace-root"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\n%s = "=0.1.0"\n\n[replace]\n"%s:0.1.0" = { path = "../sibling-replacement" }\n' \
  "$replace_dependency" "$replace_dependency" >"$replace_root/Cargo.toml"

# This local-only index lets Cargo resolve the original registry package before
# applying [replace]. The metadata check below runs offline and sees the path
# replacement, without consulting crates.io or downloading a crate archive.
printf '{"dl":"file://%s/crates/{crate}/{version}/download","api":"file://%s"}\n' \
  "$replace_registry" "$replace_registry" >"$replace_registry/config.json"
printf '{"name":"%s","vers":"0.1.0","deps":[],"cksum":"0000000000000000000000000000000000000000000000000000000000000000","features":{},"yanked":false}\n' \
  "$replace_dependency" >"$replace_registry/re/pl/$replace_dependency"
git -C "$replace_registry" init -q
git -C "$replace_registry" -c user.name='Path dependency test' \
  -c user.email='path-dependency-test@example.invalid' add .
git -C "$replace_registry" -c user.name='Path dependency test' \
  -c user.email='path-dependency-test@example.invalid' commit -qm 'fixture registry index'
printf '[source.crates-io]\nreplace-with = "fixture"\n\n[source.fixture]\nregistry = "file://%s"\n' \
  "$replace_registry" >"$replace_root/.cargo/config.toml"

# Lock generation contacts only the local file registry above. Cargo then has
# its local index cache, so all metadata checks, including this one, run offline.
(cd "$replace_root" && cargo generate-lockfile --manifest-path Cargo.toml)
replace_output="$test_root/replace-output"
replace_status=0
(cd "$replace_root" && CARGO_NET_OFFLINE=true bash "$check" Cargo.toml) >"$replace_output" 2>&1 || replace_status=$?
expect_violation "$replace_status" "$replace_output" "$replace_dependency" '[replace] fixture'
printf 'PASS: external [replace] path rejected and named\n'

repo_output="$test_root/repo-output"
repo_status=$(run_check "$repo_output" "$repo_root/Cargo.toml")
if [[ "$repo_status" != 0 ]]; then
  cat "$repo_output" >&2
  fail "the repository workspace did not pass; check itself failed with exit code $repo_status"
fi
if ! grep -E 'cargo metadata examined [1-9][0-9]* packages \([0-9]+ path packages\)' "$repo_output" >/dev/null; then
  cat "$repo_output" >&2
  fail 'the repository check did not report nonzero package counts'
fi
printf 'PASS: subconscious workspace\n'

missing_manifest="$test_root/missing/Cargo.toml"
parser_output="$test_root/parser-output"
parser_status=$(run_check "$parser_output" "$missing_manifest")
if [[ "$parser_status" == 0 || "$parser_status" == 3 ]]; then
  cat "$parser_output" >&2
  fail "an unreadable manifest must fail as a check error, not exit $parser_status"
fi
if ! grep -F 'check itself failed' "$parser_output" >/dev/null; then
  cat "$parser_output" >&2
  fail 'an unreadable manifest did not report that the check itself failed'
fi
printf 'PASS: parser/input error is distinct from violation (exit %s)\n' "$parser_status"
