#!/usr/bin/env bash
set -euo pipefail

# This is a pattern check of tracked scripts, not shell or control-flow analysis.
repo_root=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd -P)

# An optional repository root lets the tests use small tracked fixtures.
python3 - "${1:-$repo_root}" <<'PY'
import pathlib
import re
import subprocess
import sys

root = pathlib.Path(sys.argv[1])
paths = subprocess.check_output(
    ["git", "-C", str(root), "ls-files", "-z"], text=True
).split("\0")
whole_array = re.compile(r"\$\{[a-zA-Z_][a-zA-Z_0-9]*\[[@*]\]\}")
safe_array = re.compile(
    r'\$\{(?P<array>[a-zA-Z_][a-zA-Z_0-9]*\[[@*]\])\+"\$\{(?P=array)\}"\}'
)
set_command = re.compile(r"(?:^|[;\s])set\s+([^;\n]+)", re.MULTILINE)
trap_command = re.compile(
    r'''(?:^|;)[ \t]*trap\s+(?:--\s+)?(?P<body>'[^']*'|"(?:\\.|[^"\\])*"|[^\s;]+)[ \t]+(?P<signals>(?:[A-Z0-9]+[ \t]*)+)(?=$|[;#\n])''',
    re.MULTILINE,
)
finished_read = re.compile(r"\$(?:finished\b|\{finished(?::[-+?=][^}]*)?\})")
finished_set = re.compile(r"(?:^|[;\n])[ \t]*finished=1(?:\s|;|$)")


def uses_nounset(text):
    for command in set_command.finditer(text):
        options = command[1].split("#", 1)[0].split()
        for index, option in enumerate(options):
            if option == "--":
                break
            if option == "-o" and options[index + 1:index + 2] == ["nounset"]:
                return True
            if re.fullmatch(r"-[a-zA-Z]*u[a-zA-Z]*", option):
                return True
    return False


errors = []
checked = 0
for name in sorted(filter(None, paths)):
    path = root / name
    if not path.is_file():
        continue
    candidate = name.endswith((".sh", ".test"))
    if not candidate and not name.startswith(("scripts/", ".github/actions/")):
        continue
    text = path.read_text(errors="replace")
    if not candidate and not re.match(r"^#![^\n]*\bbash\b", text):
        continue
    text = re.sub(r"(?m)^[ \t]*#.*$", "", text)
    if not uses_nounset(text):
        continue
    checked += 1
    for number, line in enumerate(text.splitlines(), 1):
        if line.lstrip().startswith("#") or re.search(r"#\s*nonempty-array\b", line):
            continue
        if whole_array.search(safe_array.sub("", line)):
            errors.append(f"{name}:{number}: whole-array pattern lacks safe form or # nonempty-array")
    for trap in trap_command.finditer(text):
        if not {"EXIT", "0"}.intersection(trap["signals"].split()) or trap["body"] == "-":
            continue
        number = text.count("\n", 0, trap.start()) + 1
        body = trap["body"]
        # A named cleanup function may own the guard instead of the trap string.
        words = body.strip("\"'").split()
        handler = words[0] if words else ""
        definitions = list(re.finditer(
            r"^[ \t]*" + re.escape(handler) + r"\s*\(\s*\)\s*\{(.*?)(?:^[ \t]*\}|;[ \t]*\})",
            text, re.MULTILINE | re.DOTALL,
        ))
        # For reused handler names, check the nearest preceding definition pattern.
        preceding = [definition for definition in definitions if definition.start() < trap.start()]
        function = preceding[-1] if preceding else (definitions[0] if definitions else None)
        if not finished_set.search(text):
            errors.append(f"{name}:{number}: EXIT trap pattern is missing finished=1 success assignment")
        if not finished_read.search(body) and not (function and finished_read.search(function[1])):
            errors.append(f"{name}:{number}: EXIT trap/handler pattern must read finished to fail closed")

for error in errors:
    print(error, file=sys.stderr)
print(f"no-unbound-fail-open: pattern-checked {checked} nounset scripts; {len(errors)} violations")
sys.exit(1 if errors else 0)
PY
