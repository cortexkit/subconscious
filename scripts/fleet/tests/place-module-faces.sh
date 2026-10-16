#!/usr/bin/env bash
set -euo pipefail

ROOT=$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)
PLACE_SCRIPT="$ROOT/scripts/fleet/place-module.sh"
TEST_ROOT=$(mktemp -d "${TMPDIR:-/tmp}/place-module-faces.XXXXXX")
trap 'rm -rf "$TEST_ROOT"' EXIT

CASE_NAME=""
CASE_DIR=""
BIN_DIR=""
STAGING=""
STUBS=""
DEST=""
STAGED=""
CONTROL="CLI_FACE_TEST_CONTROL"

fail() {
  printf 'FAIL: %s: %s\n' "$CASE_NAME" "$*" >&2
  exit 1
}

assert() {
  "$@" || fail "assertion failed: $*"
}

assert_contains() {
  [[ "$1" == *"$2"* ]] || fail "expected output to contain: $2"
}

new_sandbox() {
  CASE_NAME="$1"
  CASE_DIR="$TEST_ROOT/$CASE_NAME"
  BIN_DIR="$CASE_DIR/bin"
  STAGING="$CASE_DIR/staging"
  STUBS="$CASE_DIR/stubs"
  DEST="$BIN_DIR/ck-demo"
  STAGED="$STAGING/ck-demo"
  mkdir -p "$BIN_DIR" "$STAGING" "$STUBS" "$CASE_DIR/data"

  cat >"$STUBS/file" <<'EOF'
#!/usr/bin/env bash
printf 'ELF 64-bit LSB executable\n'
EOF
  cat >"$STUBS/nm" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  cat >"$STUBS/codesign" <<'EOF'
#!/usr/bin/env bash
exit 0
EOF
  cat >"$STUBS/strings" <<'EOF'
#!/usr/bin/env bash
cat "$1"
EOF
  cat >"$STUBS/ck" <<'EOF'
#!/usr/bin/env bash
[ "${1:-}" = "--ck-cli-face-reserved-names" ] || exit 64
printf '%s\n' activity catalog daemon health help machine module provenance quota routes setup upgrade
EOF
  chmod +x "$STUBS/file" "$STUBS/nm" "$STUBS/codesign" "$STUBS/strings" "$STUBS/ck"

  write_image "$DEST" '{"cli_faces":[]}' "CLI_FACE_OLD_IMAGE"
}

write_image() {
  local image="$1" manifest="$2" marker="$3"
  cat >"$image" <<EOF
#!/usr/bin/env bash
if [ "\${1:-}" = "--manifest" ]; then
  if [ '$manifest' = '__PROBE_FAILURE__' ]; then exit 1; fi
  printf '%s\\n' '$manifest'
elif [ "\${1:-}" = "--version" ]; then
  printf 'ck-demo test image\\n'
fi
# $marker
# $CONTROL
EOF
  chmod +x "$image"
}

run_script() {
  local manifest="$1" marker="$2"
  shift 2
  write_image "$STAGED" "$manifest" "$marker"
  (cd "$STAGING" && shasum -a 256 "$(basename "$STAGED")" >"$(basename "$STAGED").sha256")
  CK_BIN_DIR="$BIN_DIR" \
  CK_STAGING="$STAGING" \
  XDG_DATA_HOME="$CASE_DIR/data" \
  PATH="$STUBS:$PATH" \
    bash "$PLACE_SCRIPT" --module demo --staged "$STAGED" --dest "$DEST" \
      --marker "$marker" --control "$CONTROL" --no-restart "$@"
}

run_place() {
  local output
  if ! output=$(run_script "$1" "$2" --place 2>&1); then
    printf '%s\n' "$output" >&2
    return 1
  fi
  printf '%s\n' "$output"
}

run_refused() {
  local output
  if output=$(run_script "$1" "$2" --place 2>&1); then
    fail "placement unexpectedly succeeded"
  fi
  printf '%s\n' "$output"
}

case_created_two_faces() {
  new_sandbox created-two-faces
  local output
  output=$(run_place '{"cli_faces":[{"name":"agents","binary":"ck-demo"},{"name":"projects","binary":"ck-demo"}]}' CLI_FACE_CREATE_MARKER) \
    || fail "placement failed"
  assert test -L "$BIN_DIR/ck-agents"
  assert test -L "$BIN_DIR/ck-projects"
  assert test "$(readlink "$BIN_DIR/ck-agents")" = ck-demo
  assert test "$(readlink "$BIN_DIR/ck-projects")" = ck-demo
  assert_contains "$output" "cli face: created ck-agents"
  assert_contains "$output" "cli face: created ck-projects"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_rename_removes_old_face() {
  new_sandbox rename-removes-old-face
  run_place '{"cli_faces":[{"name":"agents","binary":"ck-demo"},{"name":"projects","binary":"ck-demo"}]}' CLI_FACE_RENAME_FIRST >/dev/null \
    || fail "initial placement failed"
  local output
  output=$(run_place '{"cli_faces":[{"name":"agents","binary":"ck-demo"},{"name":"roles","binary":"ck-demo"}]}' CLI_FACE_RENAME_SECOND) \
    || fail "renamed placement failed"
  assert test -L "$BIN_DIR/ck-agents"
  assert test -L "$BIN_DIR/ck-roles"
  assert test ! -e "$BIN_DIR/ck-projects"
  assert test ! -L "$BIN_DIR/ck-projects"
  assert_contains "$output" "cli face: removed ck-projects"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_empty_faces_removes_owned_links() {
  new_sandbox empty-faces-removes-owned-links
  ln -s ck-demo "$BIN_DIR/ck-agents"
  ln -s ck-demo "$BIN_DIR/ck-projects"
  local output
  output=$(run_place '{"cli_faces":[]}' CLI_FACE_EMPTY_MARKER) || fail "empty declaration placement failed"
  assert test ! -e "$BIN_DIR/ck-agents"
  assert test ! -L "$BIN_DIR/ck-agents"
  assert test ! -e "$BIN_DIR/ck-projects"
  assert test ! -L "$BIN_DIR/ck-projects"
  assert_contains "$output" "cli face: removed ck-agents"
  assert_contains "$output" "cli face: removed ck-projects"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_reserved_catalog_refused() {
  new_sandbox reserved-catalog-refused
  ln -s ck-demo "$BIN_DIR/ck-agents"
  local output
  output=$(run_refused '{"cli_faces":[{"name":"catalog","binary":"ck-demo"}]}' CLI_FACE_RESERVED_MARKER) \
    || fail "could not capture reserved-name refusal"
  assert test "$(readlink "$BIN_DIR/ck-agents")" = ck-demo
  assert test ! -e "$BIN_DIR/ck-catalog"
  assert_contains "$output" "CLI face 'catalog' is a built-in ck command"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_foreign_link_is_preserved() {
  new_sandbox foreign-link-is-preserved
  ln -s other-module "$BIN_DIR/ck-agents"
  ln -s ck-demo "$BIN_DIR/ck-projects"
  local output
  output=$(run_refused '{"cli_faces":[{"name":"agents","binary":"ck-demo"}]}' CLI_FACE_FOREIGN_MARKER) \
    || fail "could not capture foreign-link refusal"
  assert test "$(readlink "$BIN_DIR/ck-agents")" = other-module
  assert test "$(readlink "$BIN_DIR/ck-projects")" = ck-demo
  assert_contains "$output" "symlink not owned by $DEST"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_regular_file_is_preserved() {
  new_sandbox regular-file-is-preserved
  printf 'user data\n' >"$BIN_DIR/ck-agents"
  ln -s ck-demo "$BIN_DIR/ck-projects"
  local output
  output=$(run_refused '{"cli_faces":[{"name":"agents","binary":"ck-demo"}]}' CLI_FACE_FILE_MARKER) \
    || fail "could not capture regular-file refusal"
  assert test "$(<"$BIN_DIR/ck-agents")" = "user data"
  assert test "$(readlink "$BIN_DIR/ck-projects")" = ck-demo
  assert_contains "$output" "not a symlink owned by $DEST"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_absent_faces_leave_handmade_links() {
  new_sandbox absent-faces-leave-handmade-links
  ln -s hand-made-target "$BIN_DIR/ck-agents"
  ln -s ck-demo "$BIN_DIR/ck-projects"
  local output
  output=$(run_place '{"module_id":"demo"}' CLI_FACE_ABSENT_MARKER) || fail "legacy manifest placement failed"
  assert test "$(readlink "$BIN_DIR/ck-agents")" = hand-made-target
  assert test "$(readlink "$BIN_DIR/ck-projects")" = ck-demo
  assert_contains "$output" "cli faces: not declared; existing links left unchanged"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_failed_probe_leaves_links() {
  new_sandbox failed-probe-leaves-links
  ln -s hand-made-target "$BIN_DIR/ck-agents"
  local output
  output=$(run_place '__PROBE_FAILURE__' CLI_FACE_FAILED_PROBE_MARKER) || fail "unchecked probe placement failed"
  assert test "$(readlink "$BIN_DIR/ck-agents")" = hand-made-target
  assert_contains "$output" "cli faces: not checked; links unchanged"
  assert_contains "$output" "provenance: not checked (the binary did not answer --manifest)"
  printf 'PASS: %s\n' "$CASE_NAME"
}

case_invalid_face_declaration_refused() {
  new_sandbox invalid-face-declaration-refused
  ln -s hand-made-target "$BIN_DIR/ck-agents"
  local output marker index=0 manifest
  for manifest in \
    '{"cli_faces":null}' \
    '{"cli_faces":[{"name":"agents","binary":"ck-demo","extra":true}]}' \
    '{"cli_faces":[{"name":"ck-agents","binary":"ck-demo"}]}' \
    '{"cli_faces":[{"name":"agents","binary":"ck-demo"},{"name":"agents","binary":"ck-demo"}]}' \
    '{"cli_faces":[{"name":"agents","name":"roles","binary":"ck-demo"}]}' \
    '{"cli_faces":[{"name":"agents","binary":"other"}]}'; do
    marker="CLI_FACE_INVALID_MARKER_$index"
    output=$(run_refused "$manifest" "$marker") \
      || fail "could not capture invalid declaration refusal"
    assert test "$(readlink "$BIN_DIR/ck-agents")" = hand-made-target
    assert_contains "$output" "invalid cli_faces in --manifest"
    index=$((index + 1))
  done
  printf 'PASS: %s (%s malformed declarations)\n' "$CASE_NAME" "$index"
}

case_dry_run_reports_faces_without_changes() {
  new_sandbox dry-run-reports-faces-without-changes
  local output
  output=$(run_script '{"cli_faces":[{"name":"agents","binary":"ck-demo"}]}' CLI_FACE_DRY_RUN_MARKER) \
    || fail "dry run failed"
  assert test ! -e "$BIN_DIR/ck-agents"
  assert test ! -L "$BIN_DIR/ck-agents"
  assert_contains "$output" "cli face: would create ck-agents -> $DEST"
  printf 'PASS: %s\n' "$CASE_NAME"
}

run_case() {
  case "$1" in
    created-two-faces) case_created_two_faces ;;
    rename-removes-old-face) case_rename_removes_old_face ;;
    empty-faces-removes-owned-links) case_empty_faces_removes_owned_links ;;
    reserved-catalog-refused) case_reserved_catalog_refused ;;
    foreign-link-is-preserved) case_foreign_link_is_preserved ;;
    regular-file-is-preserved) case_regular_file_is_preserved ;;
    absent-faces-leave-handmade-links) case_absent_faces_leave_handmade_links ;;
    failed-probe-leaves-links) case_failed_probe_leaves_links ;;
    invalid-face-declaration-refused) case_invalid_face_declaration_refused ;;
    dry-run-reports-faces-without-changes) case_dry_run_reports_faces_without_changes ;;
    *) printf 'unknown test case: %s\n' "$1" >&2; exit 2 ;;
  esac
}

if [ "$#" -eq 0 ] || [ "$1" = all ]; then
  for test_case in \
    created-two-faces \
    rename-removes-old-face \
    empty-faces-removes-owned-links \
    reserved-catalog-refused \
    foreign-link-is-preserved \
    regular-file-is-preserved \
    absent-faces-leave-handmade-links \
    failed-probe-leaves-links \
    invalid-face-declaration-refused \
    dry-run-reports-faces-without-changes; do
    run_case "$test_case"
  done
else
  run_case "$1"
fi
