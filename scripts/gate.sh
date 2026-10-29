#!/usr/bin/env bash
# Keep check commands here so local runs and CI use the same verdicts.
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$ROOT"
VECTOR_PYTHON="$ROOT/target/gate-python/bin/python3"

usage() {
  cat <<'HELP'
Usage: scripts/gate.sh {fast|lint|test|swift|ts|all} [check]

  fast   Formatting; committed crate versions; offline locked Cargo metadata;
         path-dependency policy and its tests; CortexKit layout (including
         git ls-files -ci --exclude-standard -- .cortexkit); Markdown links and
         test citations; every wire-vector generator and SHA256SUMS; installer
         ShellCheck and Unix installer contract. Fails at the first bad check.
  lint   CI's workspace all-targets and release clippy, subc-core all-targets
         clippy, then both workspace invocations for x86_64-pc-windows-gnu.
         The untargeted checks use the host: run on Linux for CI's Linux verdict.
         Install the Windows Rust target beforehand; cross-checking is not a
         substitute for CI's native Windows/MSVC lint and execution.
  test   SLOW. Linux only; best on a machine with many cores.
         Build subc-core helper binaries first, then ONE whole-workspace
         --all-targets --locked --no-fail-fast test invocation, workspace doc
         tests, and CK alpha's setup, CLI and Unix inode selections with their
         nonzero-test guards. Independent checks still run after a failure.
         Requires CI's nats-server v2.15.0 on PATH (not a stand-in).
  swift  macOS + Xcode only: swift package resolve and swift test, plus generic
         iOS and macOS 13.0 xcodebuild builds with signing disabled.
  ts     CI's subc-client, store, log and POSIX private-files typechecks/tests;
         build the sibling packages first; prebuild ck-subc and run the live
         client suite with its nonzero-test guard. Use CI's Bun versions (latest
         for the main clients, 1.4.2 for private-files) for matching runtimes.
  all    Run fast, lint, test (Linux only), swift (macOS only), and ts, reporting
         a one-line PASS/FAIL/SKIP verdict for every stage; failure is nonzero.

Prerequisites / network:
  No tools are installed by this script. Provide Rust stable + clippy/rustfmt,
  Python 3, the pinned vector environment below, ShellCheck, and
  Bun with each client's frozen-lockfile dependencies already installed.
  Tool, pip, Bun dependency and pinned nats-server downloads remain CI setup:
  they require network. The gate itself does not do those downloads. Cargo
  lint/build/test and the path-policy checker can fetch crates; fast's lockfile
  metadata is locked AND offline.
  Swift's package has no external dependencies; resolving it is local.
  Vector regeneration uses ONLY target/gate-python/bin/python3, never system
  Python, so cryptographic dependency versions match CI. One-time setup:
    python3 -m venv target/gate-python && for requirements in vectors/*/requirements.txt; do target/gate-python/bin/pip install -r "$requirements"; done

Version comparisons:
  GATE_BASE=<ref> scripts/gate.sh fast
  Defaults to origin/master if available, otherwise HEAD^ (printed explicitly).
  Like CI, the version checker compares COMMITTED history through HEAD, not
  uncommitted source/manifests. Commit your candidate before trusting that check.
  No base is fetched by the gate; CI retains its event-base fetch/deepening.

CI adapters (optional [check], for the existing job boundaries and conditions):
  fast: fmt, versions, metadata, metadata-online, paths, layout, links,
        citations, vectors, vector <vectors/name/>, shellcheck, installer
  lint: workspace, release, alpha, windows, windows-release
  test: workspace, docs, alpha-setup, alpha-cli, alpha-inode
  swift: test, ios, macos
  ts: subc-client, store, log, private-files, typecheck <pkg>, unit <pkg>, live
  metadata-online is CI's original --locked metadata (may fetch crates).

CI-only (not claimed by a local PASS):
  * Native Windows Rust tests/doctests, Windows self-update and PowerShell
    installer analysis/Pester/PATH/legacy tests: need Windows and its modules.
  * private-files MSVC x64/ARM64 prebuilds, PE/import/long-path checks and negative
    controls, ACL/second-user/module-path tests, Node/Bun runtime-version and
    emulated-x64 legs: need real Windows hosts, users and distribution artifacts.
  * private-files foreign-owner POSIX fixture and kernel cgroup tests: require
    root (and writable Linux cgroup v2 for the latter); no sudo in this gate.
  * CI's before/after build tree snapshot remains around the CI job; the local
    test stage also compares snapshots around its builds/tests.
  * Design approval: GitHub issue labels, PR/check writes and trusted no-checkout
    execution; never run contributor code in pull_request_target.
  * Tag/release-only work: tag/version assertions, registry publication/state,
    native release archives, glibc-2.28/architecture and Debian-10 Docker probes,
    signing/notarization, asset inventories/uploads and OIDC need release tags,
    release toolchains, services or credentials. release.yml reuses ci.yml;
    release-npm.yml's client typecheck/test commands are covered by ts.

Pins, golden conformance and manifest-count assertions implemented as Rust or
client tests stay in those suites; CI has no separate fast commands for them.
HELP
}

STAGE=${1:---help}
CHECK=${2:-}
CURRENT_STEP=initialization
CHECKS=0
SCRATCH=

finish() {
  local rc=$?
  if [ -n "$SCRATCH" ] && [ -f "$SCRATCH/tree-before.txt" ]; then
    if tree_check; then :; else
      echo '[gate] FAIL: build changed the tree snapshot' >&2
      rc=1
    fi
  fi
  if [ -n "$SCRATCH" ]; then rm -rf "$SCRATCH"; fi
  if [ "$rc" -eq 0 ]; then
    printf '[gate] PASS %s%s (%s checks)\n' "$STAGE" "${CHECK:+/$CHECK}" "$CHECKS"
  else
    printf '[gate] FAIL %s%s: %s (exit %s)\n' "$STAGE" "${CHECK:+/$CHECK}" "$CURRENT_STEP" "$rc" >&2
  fi
  exit "$rc"
}

step() {
  CURRENT_STEP=$1
  shift
  printf '\n[gate] %s: %s\n' "$STAGE" "$CURRENT_STEP"
  printf ' +'; printf ' %q' "$@"; printf '\n'
  "$@"
  CHECKS=$((CHECKS + 1))
}

scratch() {
  if [ -z "$SCRATCH" ]; then
    mkdir -p "$ROOT/target/scratch"
    SCRATCH=$(mktemp -d "$ROOT/target/scratch/gate.XXXXXX")
  fi
}

invalid_check() { echo "Unknown $STAGE check: $CHECK (see --help)" >&2; exit 2; }

versions() {
  local base=${GATE_BASE:-}
  if [ -z "$base" ]; then
    if git rev-parse --verify --quiet origin/master >/dev/null; then
      base=origin/master
    else
      base=HEAD^
    fi
  fi
  echo "Version comparison: $base...HEAD (committed changes only)"
  bash scripts/fleet/check-wire-crate-versions.sh "$base"
}

metadata() { cargo metadata --format-version 1 --locked "$@" >/dev/null; }

require_vector_python() {
  if [ ! -x "$VECTOR_PYTHON" ]; then
    echo 'Missing pinned vector Python: target/gate-python/bin/python3' >&2
    echo 'One-time setup (downloads the committed requirements; the gate never installs them):' >&2
    echo 'python3 -m venv target/gate-python && for requirements in vectors/*/requirements.txt; do target/gate-python/bin/pip install -r "$requirements"; done' >&2
    return 1
  fi
}

vectors() {
  local directory
  for directory in vectors/*/; do
    vector "$directory"
  done
}

vector() {
  local directory=$1
  if [ -f "${directory}generate.py" ]; then
    step 'Pinned vector Python prerequisite' require_vector_python
    step "Regenerate ${directory}" "$VECTOR_PYTHON" "${directory}generate.py" --check
  fi
  step "Checksums ${directory}" vector_sums "$directory"
}

vector_sums() (
  cd "$1"
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum --check --strict SHA256SUMS
  else
    shasum -a 256 --check --strict SHA256SUMS
  fi
)

fast_check() {
  case "$1" in
    fmt) step 'Formatting (cargo fmt)' cargo fmt --all -- --check ;;
    versions) step 'Crate versions and sibling requirements' versions ;;
    metadata) step 'Locked offline Cargo metadata' metadata --offline ;;
    metadata-online) step 'Locked Cargo metadata' metadata ;;
    paths)
      step 'Path dependencies' bash scripts/checks/no-external-path-deps.sh
      step 'Path dependency checker tests' bash scripts/checks/no-external-path-deps.test.sh ;;
    layout) step 'CortexKit layout and tracked ignored files' bash scripts/check-cortexkit-layout.sh ;;
    links) step 'Markdown links' python3 scripts/checks/check-md-links.py ;;
    citations) step 'Documentation test citations' python3 scripts/checks/check-doc-test-citations.py ;;
    vectors) vectors ;;
    vector) vector "${2:?vector directory required}" ;;
    shellcheck) step 'Installer ShellCheck' shellcheck --shell=bash scripts/install/install.sh scripts/install/tests/install.sh.test ;;
    installer) step 'Unix installer contract' bash scripts/install/tests/install.sh.test ;;
    *) invalid_check ;;
  esac
}

lint_check() {
  case "$1" in
    workspace) step 'Workspace clippy (all targets)' cargo clippy --workspace --all-targets --locked -- -D warnings ;;
    release) step 'Workspace clippy (release)' cargo clippy --workspace --release --locked -- -D warnings ;;
    alpha) step 'Alpha clippy' cargo clippy -p subc-core --all-targets --locked -- -D warnings ;;
    windows) step 'Windows workspace clippy (all targets)' cargo clippy --workspace --all-targets --locked --target x86_64-pc-windows-gnu -- -D warnings ;;
    windows-release) step 'Windows workspace clippy (release)' cargo clippy --workspace --release --locked --target x86_64-pc-windows-gnu -- -D warnings ;;
    *) invalid_check ;;
  esac
}

# A separate process preserves errexit even when the caller collects failures.
# Otherwise Bash disables errexit throughout a function used in an if condition.
collect() {
  local check rc=0
  for check in "$@"; do
    if bash "$ROOT/scripts/gate.sh" "$STAGE" "$check"; then
      CHECKS=$((CHECKS + 1))
    else
      rc=1
    fi
  done
  CURRENT_STEP='independent checks (see named failures above)'
  return "$rc"
}

alpha_logged() {
  local pattern=$1
  shift
  scratch
  "$@" 2>&1 | tee "$SCRATCH/alpha.log"
  grep -Eq "$pattern" "$SCRATCH/alpha.log"
}

test_check() {
  case "$1" in
    workspace)
      scratch
      git status --porcelain | sort > "$SCRATCH/tree-before.txt"
      # Daemon tests launch feature-gated fixtures from both packages. Cargo's
      # test scheduling does not guarantee those binaries exist before a test.
      step 'Build helper binaries (test-support)' cargo build --locked -p subc-core -p subc-daemon --bins --features subc-core/test-support,subc-daemon/test-support
      step 'Whole-workspace tests' cargo test --workspace --all-targets --locked --no-fail-fast ;;
    docs) step 'Workspace doctests' cargo test --workspace --doc --locked ;;
    alpha-setup) step 'Alpha setup selection (nonzero tests)' alpha_logged 'test result: ok\. [1-9][0-9]* passed;' cargo test -p subc-core --lib --locked setup:: ;;
    alpha-cli) step 'Alpha CLI update-check contract' cargo test -p subc-core --test ck_cli --locked -v ;;
    alpha-inode)
      step 'Unix self-update inode contract (exactly one test)' alpha_logged 'test result: ok\. 1 passed;' cargo test -p subc-core --lib --locked setup::self_update_unix::tests::unix_self_update_keeps_running_process_on_original_inode -- --exact ;;
    *) invalid_check ;;
  esac
}

tree_check() {
  git status --porcelain | sort > "$SCRATCH/tree-after.txt"
  if ! diff -q "$SCRATCH/tree-before.txt" "$SCRATCH/tree-after.txt" >/dev/null; then
    echo 'the build rewrote tracked files:' >&2
    diff "$SCRATCH/tree-before.txt" "$SCRATCH/tree-after.txt" >&2 || true
    git diff >&2
    return 1
  fi
}

swift_command() (
  cd clients/subc-client-swift
  nice -n 10 "$@"
)

swift_check() {
  step 'Resolve Swift package dependencies' swift_command swift package resolve
  case "$1" in
    test) step 'Swift package tests' swift_command swift test ;;
    ios)
      step 'Generic iOS build' swift_command xcodebuild -scheme SubcFed -destination 'generic/platform=iOS' -derivedDataPath .build/xcode-derived CODE_SIGNING_ALLOWED=NO build ;;
    macos)
      step 'Generic macOS 13.0 build' swift_command xcodebuild -scheme SubcFed -destination 'generic/platform=macOS' -derivedDataPath .build/xcode-derived-macos MACOSX_DEPLOYMENT_TARGET=13.0 CODE_SIGNING_ALLOWED=NO build ;;
    *) invalid_check ;;
  esac
}

client_command() (
  case "$1" in subc-client|store|log|private-files) ;; *) invalid_check ;; esac
  cd "clients/$1"
  shift
  "$@"
)

live_suite() {
  scratch
  (
    cd clients/subc-client
    RUN_SUBC_LIVE=1 bun test tests/live-handshake.test.ts tests/live-provider.test.ts tests/live-streaming.test.ts 2>&1 | tee "$SCRATCH/live.log"
  )
  local passed
  # A skipped describe also exits zero; an absent pass line must count as zero.
  passed=$(sed -nE 's/^[[:space:]]*([0-9]+) pass.*/\1/p' "$SCRATCH/live.log" | head -1)
  if [ "${passed:-0}" -eq 0 ]; then
    echo 'live suite executed no tests -- RUN_SUBC_LIVE did not open the gate' >&2
    return 1
  fi
  echo "live suite executed ${passed} tests against a real daemon"
}

ts_check() {
  local pkg=$1 rc=0
  case "$pkg" in
    typecheck) step "Typecheck ${2:?package required}" client_command "$2" bun run typecheck ;;
    unit)
      if [ "${2:?package required}" = private-files ]; then
        step 'POSIX private-files tests' client_command private-files bun run test
      else
        step "Unit tests $2" client_command "$2" bun test
      fi ;;
    live) step 'Live daemon suite (nonzero tests)' live_suite ;;
    subc-client|store|log|private-files)
      case "$pkg" in
        store|log) step 'Build sibling private-files' client_command private-files bun run build ;;
      esac
      if [ "$pkg" = log ]; then step 'Build sibling store' client_command store bun run build; fi
      if [ "$pkg" = private-files ]; then step 'Bun version' bun --version; fi
      if bash "$ROOT/scripts/gate.sh" ts typecheck "$pkg"; then :; else rc=1; fi
      if bash "$ROOT/scripts/gate.sh" ts unit "$pkg"; then :; else rc=1; fi
      if [ "$pkg" = subc-client ] && [ "$(uname -s)" = Linux ]; then
        step 'Prebuild live daemon' cargo build --locked -p subc-core --bin ck-subc
        if bash "$ROOT/scripts/gate.sh" ts live; then :; else rc=1; fi
      fi
      CURRENT_STEP="${pkg} checks (see named failures above)"
      return "$rc" ;;
    *) invalid_check ;;
  esac
}

case "$STAGE" in
  -h|--help|help) usage; exit 0 ;;
  fast|lint|test|swift|ts|all) ;;
  *) usage >&2; exit 2 ;;
esac

if [ "$STAGE" = all ]; then
  result=0
  for stage in fast lint test swift ts; do
    case "$stage:$(uname -s)" in
      test:Linux|swift:Darwin|fast:*|lint:*|ts:*) ;;
      *) printf '[gate] SKIP %s (see --help for required host)\n' "$stage"; continue ;;
    esac
    if bash "$ROOT/scripts/gate.sh" "$stage"; then :; else result=1; fi
  done
  exit "$result"
fi

trap finish EXIT
case "$STAGE" in
  fast)
    if [ -n "$CHECK" ]; then fast_check "$CHECK" "${3:-}"; else
      step 'Pinned vector Python prerequisite' require_vector_python
      for check in fmt versions metadata paths layout links citations vectors shellcheck installer; do fast_check "$check"; done
    fi ;;
  lint)
    if [ -n "$CHECK" ]; then lint_check "$CHECK"; else collect workspace release alpha windows windows-release; fi ;;
  test)
    if [ -n "$CHECK" ]; then test_check "$CHECK"; else
      CURRENT_STEP='Linux build-server prerequisite'
      if [ "$(uname -s)" != Linux ]; then echo 'The test stage runs on Linux only; use a Linux machine with many cores.' >&2; exit 2; fi
      scratch
      git status --porcelain | sort > "$SCRATCH/tree-before.txt"
      collect workspace docs alpha-setup alpha-cli alpha-inode
    fi ;;
  swift)
    CURRENT_STEP='macOS/Xcode prerequisite'
    if [ "$(uname -s)" != Darwin ]; then echo 'The swift stage requires macOS and Xcode.' >&2; exit 2; fi
    if [ -n "$CHECK" ]; then swift_check "$CHECK"; else collect test ios macos; fi ;;
  ts)
    if [ -n "$CHECK" ]; then ts_check "$CHECK" "${3:-}"; else collect subc-client store log private-files; fi ;;
esac
