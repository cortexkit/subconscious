#!/usr/bin/env bash
# Give only the Python runner an ELF import newer than the release ceiling.
set -euo pipefail
finished=
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
mkdir -p "$ROOT/target/scratch"
fixture="$(mktemp -d "$ROOT/target/scratch/glibc.XXXXXX")"
finish() {
  local rc=$?
  rm -rf "$fixture"
  if [[ "$rc" -eq 0 && "${finished:-}" != 1 ]]; then
    echo 'test-check-glibc.sh: exited early without finishing' >&2
    rc=1
  fi
  exit "$rc"
}
trap finish EXIT
mkdir -p "$fixture/bin"

# A versioned fixture library makes the imports independent of the host's libc.
printf 'int fixture(void) { return 0; }\n' > "$fixture/provider.c"
printf 'extern int fixture(void); int probe(void) { return fixture(); }\n' > "$fixture/consumer.c"
for version in 2.28 2.29; do
  printf 'GLIBC_%s { global: fixture; };\n' "$version" > "$fixture/versions.map"
  cc -shared -nostdlib "$fixture/provider.c" -Wl,--version-script="$fixture/versions.map" -o "$fixture/libfixture.so"
  cc -shared -nostdlib "$fixture/consumer.c" -L"$fixture" -lfixture -o "$fixture/import-$version"
done
for binary in ck ck-subc ck-subc-mcp ck-script-runner-bun ck-script-runner-python ck-nats-server; do
  cp "$fixture/import-2.28" "$fixture/bin/$binary"
done
cc -static -nostdlib "$fixture/provider.c" -Wl,-e,fixture -o "$fixture/bin/ck-nats-engine"
bash "$SCRIPT_DIR/check-glibc.sh" "$fixture/bin" > "$fixture/output" 2>&1
grep -Fq 'ck-nats-engine: static ELF (no interpreter or shared libraries)' "$fixture/output"
grep -Fq 'ck-script-runner-python: highest required GLIBC_2.28 <= GLIBC_2.28' "$fixture/output"

cp "$fixture/import-2.29" "$fixture/bin/ck-script-runner-python"
if bash "$SCRIPT_DIR/check-glibc.sh" "$fixture/bin" > "$fixture/output" 2>&1; then
  echo 'glibc_python_newer_import: newer Python runner import unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'ck-script-runner-python: requires GLIBC_2.29, newer than allowed GLIBC_2.28' "$fixture/output"
printf 'glibc_python_newer_import: rejected GLIBC_2.29\n'

rm "$fixture/bin/ck-script-runner-python"
if bash "$SCRIPT_DIR/check-glibc.sh" "$fixture/bin" > "$fixture/output" 2>&1; then
  echo 'missing Python runner unexpectedly passed' >&2
  exit 1
fi
grep -Fq 'ck-script-runner-python: cannot read ELF version requirements' "$fixture/output"
cp "$fixture/import-2.28" "$fixture/bin/ck-script-runner-python"
cp "$fixture/import-2.28" "$fixture/bin/ck-nats-engine"
if bash "$SCRIPT_DIR/check-glibc.sh" "$fixture/bin" > "$fixture/output" 2>&1; then
  echo 'dynamic NATS engine unexpectedly passed' >&2; exit 1
fi
grep -Fq 'ck-nats-engine: expected a static ELF' "$fixture/output"
printf 'check-glibc: passed (4 checks)\n'
finished=1
