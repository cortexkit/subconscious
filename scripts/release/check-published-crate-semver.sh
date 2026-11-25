#!/usr/bin/env bash
set -euo pipefail
finished=
tool_dir=

finish() {
  local rc=$?
  if [[ -n "$tool_dir" ]]; then
    rm -rf "$tool_dir"
  fi
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "check-published-crate-semver.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

cd "$(dirname "${BASH_SOURCE[0]}")/../.."
candidates=$(bash scripts/release/unpublished-wire-crates.sh --semver-candidates "$@")
if [[ -z "$candidates" ]]; then
  echo "No bumped crates with a crates.io baseline to check"
  finished=1
  exit 0
fi

# Pin the Linux x86-64 release binary used by both CI and the publisher.
tool_version=0.51.0
tool_sha256=cacadafaa5cd27ae6d05a74a103266917ebab4c5e43c898360e177d4fbf06a65
tool_dir=$(mktemp -d)
archive="$tool_dir/cargo-semver-checks.tar.gz"
curl --fail --silent --show-error --location --retry 2 --connect-timeout 10 --max-time 120 \
  --output "$archive" \
  "https://github.com/obi1kenobi/cargo-semver-checks/releases/download/v${tool_version}/cargo-semver-checks-x86_64-unknown-linux-gnu.tar.gz"
printf '%s  %s\n' "$tool_sha256" "$archive" | sha256sum --check
tar -xzf "$archive" -C "$tool_dir" cargo-semver-checks
checker="$tool_dir/cargo-semver-checks"
"$checker" semver-checks --version

failed=0
while read -r crate version baseline; do
  echo "Checking $crate $baseline -> $version for public API compatibility"
  # Let the tool classify the bump from both versions. A 0.x patch (x > 0)
  # is a minor change to the tool, so breaking API lints still run.
  if ! "$checker" semver-checks check-release --manifest-path "crates/$crate/Cargo.toml" \
    --package "$crate" --baseline-version "$baseline"; then
    echo "Semver check failed for $crate $baseline -> $version" >&2
    failed=1
  fi
done <<< "$candidates"
finished=1
if ((failed)); then
  exit 1
fi
