#!/usr/bin/env bash
set -euo pipefail

script_dir=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd -P)
repo_root=$(cd -- "$script_dir/../.." && pwd -P)
mkdir -p "$repo_root/target/scratch"

python3 - "$BASH" "$script_dir/no-unbound-fail-open.sh" "$repo_root/target/scratch" <<'PY'
import pathlib
import subprocess
import sys
import tempfile

bash, check, scratch = sys.argv[1:]
count = 0
unguarded = subprocess.run([
    bash, "-c", "; ".join([
        "set -euo pipefail", "trap true EXIT", "echo DOLLAR{undefined_var}",
    ]).replace("DOLLAR", chr(36)),
], capture_output=True, text=True)
version = subprocess.check_output([bash, "--version"], text=True).splitlines()[0]
print(f"Unguarded scalar baseline exit: {unguarded.returncode} ({version})", flush=True)

with tempfile.TemporaryDirectory(prefix="unbound-check-", dir=scratch) as directory:
    root = pathlib.Path(directory)
    subprocess.run(["git", "-C", str(root), "init", "-q"], check=True)

    def fixture(label, lines, violation=None, name="fixture.sh", checked=1):
        global count
        for path in root.rglob("*"):
            if path.is_file() and ".git" not in path.parts:
                path.unlink()
        subprocess.run(["git", "-C", str(root), "rm", "-q", "--cached", "--ignore-unmatch", "-r", "."], check=True)
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        # Keep fixture expansions separate from the test script's own shell code.
        path.write_text("\n".join(lines).replace("DOLLAR", chr(36)) + "\n")
        subprocess.run(["git", "-C", str(root), "add", name], check=True)
        result = subprocess.run([bash, check, str(root)], capture_output=True, text=True)
        if violation is None:
            assert result.returncode == 0, (label, result.stdout, result.stderr)
            assert f"pattern-checked {checked} nounset scripts; 0 violations" in result.stdout, (label, result.stdout)
        else:
            assert result.returncode == 1, (label, result.stdout, result.stderr)
            assert f"{name}:" in result.stderr and violation in result.stderr, (label, result.stderr)
        count += 1
        print(f"PASS: {label}")
        return path

    for flags in ("-u", "-eu", "-euo pipefail", "-o nounset"):
        fixture(f"bare array rejected with set {flags}", [
            "#!/usr/bin/env bash", f"set {flags}", "values=()",
            'printf "%s\\n" "DOLLAR{values[@]}"',
        ], "whole-array pattern lacks safe form")

    for expansion in ('DOLLAR{values[@]}', '"DOLLAR{values[*]}"', 'DOLLAR{values[*]}'):
        fixture(f"bare array variant rejected: {expansion}", [
            "set -u", "values=()", f"echo {expansion}",
        ], "whole-array pattern lacks safe form")

    fixture("safe empty arrays accepted", [
        "set -euo pipefail", "values=()",
        'copy=(DOLLAR{values[@]+"DOLLAR{values[@]}"})',
        'echo DOLLAR{values[*]+"DOLLAR{values[*]}"}',
    ])
    fixture("marked nonempty array accepted", [
        "set -u", "values=(one)",
        'echo "DOLLAR{values[@]}" # nonempty-array: literal element',
    ])
    fixture("array length accepted", ["set -u", "values=()", 'echo "DOLLAR{#values[@]}"'])
    fixture("safe expansion does not hide a bare neighbor", [
        "set -u", 'echo DOLLAR{values[@]+"DOLLAR{values[@]}"} "DOLLAR{other[@]}"',
    ], "whole-array pattern lacks safe form")

    # No nounset means that empty array expansions cannot trigger this failure.
    fixture("file without nounset accepted", ["echo DOLLAR{values[@]}"], checked=0)

    fixture("bash shebang without suffix scanned", [
        "#!/usr/bin/env bash", "set -u", 'echo "DOLLAR{values[@]}"',
    ], "whole-array pattern lacks safe form", "scripts/check")
    fixture("sh shebang without suffix scanned", [
        "#!/bin/sh", "set -u", "trap 'true' EXIT",
    ], "missing finished=1", "scripts/sh-check")
    fixture("git hook scanned", [
        "#!/bin/sh", "set -u", "trap 'true' EXIT",
    ], "missing finished=1", ".githooks/pre-push")
    fixture("action shell script scanned", [
        "#!/usr/bin/env bash", "set -u", 'echo "DOLLAR{values[@]}"',
    ], "whole-array pattern lacks safe form", ".github/actions/example/run.sh")
    fixture("test suffix scanned", ["set -u", 'echo "DOLLAR{values[@]}"'],
            "whole-array pattern lacks safe form", "fixture.test")
    fixture("unguarded EXIT trap refused", [
        "set -euo pipefail", "trap 'true' EXIT", 'echo "DOLLAR{undefined_var}"',
    ], "missing finished=1")
    fixture("success assignment alone is insufficient", [
        "set -u", "trap 'true' EXIT", "finished=1",
    ], "EXIT trap/handler pattern must read finished")
    fixture("empty EXIT trap refused", [
        "set -u", "trap '' EXIT",
    ], "missing finished=1")
    fixture("EXIT among multiple signals refused", [
        "set -u", "trap 'true' HUP EXIT INT # cleanup", "finished=1",
    ], "EXIT trap/handler pattern must read finished")
    fixture("numeric EXIT signal refused", [
        "set -u; trap 'true' 0", "finished=1",
    ], "EXIT trap/handler pattern must read finished")
    fixture("trap read alone is insufficient", [
        "set -u", "trap 'echo DOLLAR{finished:-}' EXIT",
    ], "missing finished=1")

    guard = [
        "set -euo pipefail", "finished=", "cleanup() {", "  rc=DOLLAR?",
        "  printf 'cleanup\\n' >&2",
        '  if [ "DOLLARrc" -eq 0 ] && [ "DOLLAR{finished:-}" != 1 ]; then',
        "    echo 'fixture: exited early without finishing' >&2", "    rc=1", "  fi",
        '  if [ "DOLLARrc" -eq 0 ]; then printf "PASS fixture\\n"; fi',
        '  exit "DOLLARrc"', "}", "trap cleanup EXIT",
    ]
    fixture("redefined unguarded handler refused", guard + [
        "cleanup() {", "  true", "}", "trap cleanup EXIT", "finished=1",
    ], "EXIT trap/handler pattern must read finished")
    path = fixture("guarded EXIT trap accepted", guard + [
        'echo "DOLLAR{undefined_var}"', "finished=1",
    ])
    result = subprocess.run([bash, str(path)], capture_output=True, text=True)
    assert result.returncode != 0 and "cleanup" in result.stderr, result
    assert "PASS" not in result.stdout + result.stderr, result
    # Only Bash versions that return zero for the unguarded scalar need rescue.
    if unguarded.returncode == 0:
        assert "exited early without finishing" in result.stderr, result
    count += 1
    print("PASS: guarded unbound scalar exits nonzero and cleans up")

    path = fixture("guarded successful exit accepted", guard + ["finished=1", "exit 0"])
    result = subprocess.run([bash, str(path)], capture_output=True, text=True)
    assert result.returncode == 0 and "cleanup" in result.stderr and "PASS fixture" in result.stdout, result
    count += 1
    print("PASS: intended success remains zero and cleans up")

    path = fixture("guarded failure accepted", guard + ["exit 7", "finished=1"])
    result = subprocess.run([bash, str(path)], capture_output=True, text=True)
    assert result.returncode == 7 and "cleanup" in result.stderr, result
    count += 1
    print("PASS: nonzero status preserved and cleanup runs")

print(f"PASS: {count} checks")
PY
