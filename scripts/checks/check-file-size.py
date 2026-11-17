#!/usr/bin/env python3
"""Enforce the 5,000-line limit on hand-written source and ratchet existing exceptions down.

Test and generated files are exempt.
"""

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PINS = ROOT / "scripts/checks/file-size-pins.txt"
LIMIT = 5000
EXTENSIONS = {".rs", ".ts", ".tsx", ".swift", ".go", ".py", ".sh"}
# Only Git-tracked source extensions are candidates. Generated/vendor directories,
# fixture and vector data, lockfiles, and conventional generated filenames are not
# hand-written source. Matching path components avoids excluding names like tests.
EXCLUDED_DIRS = {"generated", "vendor", "vendored", "node_modules", "fixtures", "fixture", "vectors", "testdata"}


# Test code is exempt from the limit, like generated code: a test directory, or a file
# whose name marks it as tests. Test modules inside a production Rust file are dropped
# from its count by counted_lines instead.
TEST_DIRS = {"tests", "test"}
TEST_SUFFIXES = ("_tests.rs", "_test.rs", ".test.ts", ".test.tsx", ".spec.ts", "tests.swift", "_test.go", "_test.py")


def is_test(path):
    parts = Path(path).parts
    name = parts[-1].lower()
    return (any(part.lower() in TEST_DIRS for part in parts[:-1])
            or name in ("tests.rs", "test.rs")
            or name.startswith("test_")
            or name.endswith(TEST_SUFFIXES))


def is_source(path):
    parts = Path(path).parts
    name = parts[-1].lower()
    return (Path(path).suffix.lower() in EXTENSIONS
            and not is_test(path)
            and not any(part.lower() in EXCLUDED_DIRS for part in parts[:-1])
            and not name.endswith((".generated.rs", ".generated.ts", ".generated.swift", ".gen.go", ".lock")))


RAW_STRING = re.compile(r'(?:br|cr|r)(#*)"')
CHARACTER = re.compile(r"'(?:\\(?:u\{[0-9a-fA-F_]+\}|x[0-9a-fA-F]{2}|.)|[^'\\\n])'")


def rust_code(source):
    """Mask comments and literals while preserving offsets and newlines."""
    out = list(source)
    n = len(source)
    i = 0

    def mask(start, end):
        for pos in range(start, end):
            if source[pos] != "\n":
                out[pos] = " "

    while i < n:
        start = i
        if source.startswith("//", i):
            end = source.find("\n", i)
            i = n if end < 0 else end
        elif source.startswith("/*", i):
            i += 2
            depth = 1
            while i < n and depth:
                if source.startswith("/*", i):
                    depth += 1
                    i += 2
                elif source.startswith("*/", i):
                    depth -= 1
                    i += 2
                else:
                    i += 1
        else:
            raw = RAW_STRING.match(source, i)
            if raw:
                end_marker = '"' + raw.group(1)
                end = source.find(end_marker, raw.end())
                i = n if end < 0 else end + len(end_marker)
            elif source[i] == '"':
                i += 1
                while i < n:
                    if source[i] == "\\":
                        i += 2
                    elif source[i] == '"':
                        i += 1
                        break
                    else:
                        i += 1
                i = min(i, n)
            elif source[i] == "'":
                # A lifetime such as 'a is not a character literal.
                char = CHARACTER.match(source, i)
                if not char:
                    i += 1
                    continue
                i = char.end()
            else:
                i += 1
                continue
        mask(start, i)
    return "".join(out)


TEST_MODULE = re.compile(
    r"#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]"
    r"(?:\s*#\s*\[[^\]]*\])*\s*"
    r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+[A-Za-z_][A-Za-z_0-9]*\s*\{"
)


def counted_lines(source, rust=False):
    if not rust:
        return len(source.splitlines())
    code = rust_code(source)
    spans = []
    for match in TEST_MODULE.finditer(code):
        if spans and match.start() < spans[-1][1]:
            continue
        depth = 1
        end = match.end()
        while end < len(code) and depth:
            if code[end] == "{":
                depth += 1
            elif code[end] == "}":
                depth -= 1
            end += 1
        if depth:
            raise ValueError("unclosed #[cfg(test)] module")
        spans.append((match.start(), end))
    # Count a boundary line if it also contains production code.
    remaining = list(source)
    for start, end in spans:
        for pos in range(start, end):
            if remaining[pos] != "\n":
                remaining[pos] = " "
    count = 0
    offset = 0
    for line in source.splitlines(keepends=True):
        line_end = offset + len(line)
        covered = any(start < line_end and end > offset for start, end in spans)
        if not covered or "".join(remaining[offset:line_end]).strip():
            count += 1
        offset = line_end
    return count


def read_pins(path):
    pins = {}
    for number, line in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        fields = line.split(maxsplit=2)
        if len(fields) != 3 or not fields[1].isdigit() or not fields[2].strip():
            raise ValueError(f"{path}:{number}: expected path, count, and reason")
        name, count, reason = fields
        if name in pins:
            raise ValueError(f"{path}:{number}: duplicate pin for {name}")
        pins[name] = (int(count), reason)
    return pins


def violations(counts, pins):
    errors = []
    for path, count in sorted(counts.items()):
        if path not in pins and count >= LIMIT:
            errors.append(f"{path}: {count} lines, limit is under {LIMIT}; split the file or add a pin with a reason")
        elif path in pins:
            pin = pins[path][0]
            if count < LIMIT:
                errors.append(f"{path}: {count} lines, below {LIMIT} (pin {pin}); remove the pin")
            elif count > pin:
                errors.append(f"{path}: {count} lines exceeds pin {pin}; split the file to avoid growth")
            elif count < pin:
                errors.append(f"{path}: {count} lines below pin {pin}; lower the pin to {count}")
    for path in sorted(pins.keys() - counts.keys()):
        errors.append(f"{path}: file absent or excluded (pin {pins[path][0]}); remove the dead pin")
    return errors


def main():
    try:
        tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=ROOT).decode("utf-8").split("\0")
        counts = {}
        for path in filter(None, tracked):
            if is_source(path) and (ROOT / path).exists():
                content = (ROOT / path).read_text(encoding="utf-8")
                counts[path] = counted_lines(content, path.endswith(".rs"))
        pins = read_pins(PINS)
        errors = violations(counts, pins)
    except (OSError, UnicodeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"file size check: {error}", file=sys.stderr)
        return 1
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"File size check passed: {len(counts)} tracked source files, {len(pins)} pins")
    return 0


if __name__ == "__main__":
    sys.exit(main())
