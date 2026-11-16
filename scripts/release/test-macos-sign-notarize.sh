#!/usr/bin/env bash
set -euo pipefail
finished=

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIXTURE_DIR="$(mktemp -d "${SCRIPT_DIR}/.test-release-notarize.XXXXXX")"
finish() {
  local rc=$?
  rm -rf "$FIXTURE_DIR"
  if [ "$rc" -eq 0 ] && [ "${finished:-}" != 1 ]; then
    echo "test-macos-sign-notarize.sh: exited early without finishing" >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT

mkdir -p "$FIXTURE_DIR/bin" "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist"
for binary in ck ck-subc ck-subc-mcp ck-script-runner-bun ck-script-runner-python; do
  # Signing smoke-runs a copied binary with --version before checking identities.
  cat > "$FIXTURE_DIR/source/$binary" <<EOF
#!/bin/sh
[ "\${1:-}" = --version ] || exit 64
printf '%s\n' '$binary fixture'
EOF
  chmod +x "$FIXTURE_DIR/source/$binary"
done

cat > "$FIXTURE_DIR/bin/security" <<'EOF'
#!/usr/bin/env bash
printf '  1) ABCDEF0123456789 "Developer ID Application: CortexKit (ABCDE12345)"\n'
EOF

cat > "$FIXTURE_DIR/bin/codesign" <<'EOF'
#!/usr/bin/env bash
if [[ "$1" == "-dv" ]]; then
  binary="${@: -1}"
  if [[ "$binary" == *"/${CK_TEST_MIXED_BINARY:-ck-subc}" ]]; then
    printf 'Authority=Developer ID Application: Other Team (OTHER12345)\n' >&2
  else
    printf 'Authority=Developer ID Application: CortexKit (ABCDE12345)\n' >&2
  fi
fi
if [[ "$1" == -d && "${@: -1}" == */ck-script-runner-python && "${CK_TEST_PYTHON_ENTITLEMENTS:-}" == 1 ]]; then
  printf '<plist><dict><key>com.apple.security.cs.allow-jit</key><true/></dict></plist>\n'
fi
if [[ "$1" == -d && "${@: -1}" == */ck-script-runner-bun ]]; then
  printf '<plist><dict><key>com.apple.security.cs.allow-jit</key><true/></dict></plist>\n'
fi
EOF

cat > "$FIXTURE_DIR/bin/xcrun" <<EOF
#!/usr/bin/env bash
touch "$FIXTURE_DIR/notarytool-invoked"
EOF

chmod +x "$FIXTURE_DIR/bin/security" "$FIXTURE_DIR/bin/codesign" "$FIXTURE_DIR/bin/xcrun"

set +e
PATH="$FIXTURE_DIR/bin:$PATH" \
  "$SCRIPT_DIR/macos-sign-notarize.sh" "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist" \
  > "$FIXTURE_DIR/output" 2>&1
status=$?
set -e

if [[ "$status" -eq 0 ]]; then
  echo "mixed-identity fixture unexpectedly succeeded" >&2
  exit 1
fi
grep -Fq "refusing mixed signing identities before notarization" "$FIXTURE_DIR/output"
if [[ -e "$FIXTURE_DIR/notarytool-invoked" ]]; then
  echo "mixed-identity fixture reached notarization" >&2
  exit 1
fi

if PATH="$FIXTURE_DIR/bin:$PATH" CK_TEST_MIXED_BINARY=none CK_TEST_PYTHON_ENTITLEMENTS=1 \
  "$SCRIPT_DIR/macos-sign-notarize.sh" "$FIXTURE_DIR/source" "$FIXTURE_DIR/dist" \
  > "$FIXTURE_DIR/python-output" 2>&1; then
  echo 'Python runner with JIT entitlement unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'refusing Python runner: it carries entitlements:' "$FIXTURE_DIR/python-output"
[[ ! -e "$FIXTURE_DIR/notarytool-invoked" ]]
printf 'signing fixtures passed (2 checks)\n'
finished=1
