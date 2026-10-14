#!/usr/bin/env python3
"""Fail when a test the documentation cites does not exist where it says.

The contracts in docs/ name the test that proves each rule, written as

    `test_name` (`path/to/file.rs`)

This check reads every tracked Markdown file under docs/ plus the root
README.md, CONTRIBUTING.md and SECURITY.md. For every `name` (`path`) pair
whose path is in this repository, the file must exist and contain the name.
On a line that mentions a test, or follows one that does, the name must be a
test function defined in that file: a Rust `fn` carrying a test attribute (`#[test]`, `#[tokio::test]`
and the like). The same form outside a test context, such as a type or
function named with its source file, is checked only for existence.

It also refuses citations it cannot check: a backticked name of at least
three words that is a test function somewhere in the repository but is
written without its path. Without the path a renamed or moved test would
leave a citation that still reads as proof. (Shorter names such as `restart`
are field and op names far more often than test names, so they are skipped.)

The check refuses to pass on nothing: if it finds no citation at all it
reports itself blind and exits 2.

Exit 0 when every citation resolves, 1 when one does not, 2 when blind.
"""

import os
import re
import subprocess
import sys

CITATION = re.compile(r"`([A-Za-z_][A-Za-z0-9_]*)` \(`([^`\s]+)`\)")
BACKTICKED = re.compile(r"`([a-z_][a-z0-9_]*)`")
TEST_WORD = re.compile(r"\b[Tt]ests?\b")
FENCE = re.compile(r"^\s{0,3}(```+|~~~+)")
TEST_FN = re.compile(
    r"#\[(?:[\w]+::)*test\b[^\]]*\]\s*(?:(?://[^\n]*|#\[[^\]]*\])\s*)*"
    r"(?:pub(?:\([^)]*\))?\s+)?(?:async\s+)?fn\s+([A-Za-z_][A-Za-z0-9_]*)"
)


def git(root, *args):
    return subprocess.run(
        ["git", *args], cwd=root, check=True, capture_output=True, text=True
    ).stdout


def test_functions(root, path):
    """Names of test functions defined in one Rust file."""
    with open(os.path.join(root, path), encoding="utf-8") as handle:
        text = handle.read()
    return set(TEST_FN.findall(text))


def doc_files(root):
    tracked = git(root, "ls-files", "-z", "--", "docs/*.md", "README.md", "CONTRIBUTING.md", "SECURITY.md")
    return [p for p in tracked.split("\0") if p]


def prose_lines(text):
    fence = None
    for number, line in enumerate(text.splitlines(), start=1):
        opener = FENCE.match(line)
        if fence is None and opener:
            fence = opener.group(1)[0] * 3
            continue
        if fence is not None:
            if line.strip().startswith(fence):
                fence = None
            continue
        yield number, line


def main():
    root = git(os.getcwd(), "rev-parse", "--show-toplevel").strip()
    rust_files = [p for p in git(root, "ls-files", "-z", "--", "*.rs").split("\0") if p]
    index = {}
    for path in rust_files:
        for name in test_functions(root, path):
            index.setdefault(name, set()).add(path)

    top_level = set(os.listdir(root))
    problems = []
    checked = 0
    for doc in doc_files(root):
        with open(os.path.join(root, doc), encoding="utf-8") as handle:
            text = handle.read()
        previous = ""
        for number, line in prose_lines(text):
            cited_spans = []
            # A citation list often starts with "Tests:" on the line before.
            is_test_context = TEST_WORD.search(previous + " " + line) is not None
            previous = line
            for match in CITATION.finditer(line):
                name, path = match.groups()
                if "/" not in path or path.split("/", 1)[0] not in top_level:
                    continue  # a parenthetical that is not a path in this repository
                cited_spans.append(match.span(1))
                where = f"{doc}:{number}: `{name}` (`{path}`)"
                full = os.path.join(root, path)
                if not os.path.isfile(full):
                    problems.append(f"{where}: no such file")
                    continue
                tests_here = test_functions(root, path) if path.endswith(".rs") else set()
                if name in tests_here:
                    checked += 1
                    continue
                if is_test_context and name in index:
                    problems.append(
                        f"{where}: no test of that name in the file; defined in "
                        f"{', '.join(sorted(index[name]))}"
                    )
                    continue
                with open(full, encoding="utf-8") as handle:
                    found = re.search(r"\b" + re.escape(name) + r"\b", handle.read())
                if not found:
                    problems.append(f"{where}: name not found in the file")
                elif is_test_context and path.endswith(".rs"):
                    problems.append(f"{where}: cited as a test, but no test of that name is in the file")
            for match in BACKTICKED.finditer(line):
                name = match.group(1)
                inside = any(start <= match.start(1) < end for start, end in cited_spans)
                if not inside and name in index and name.count("_") >= 2:
                    problems.append(
                        f"{doc}:{number}: `{name}` is a test in "
                        f"{', '.join(sorted(index[name]))} but is cited without its path"
                    )
    if problems:
        print("test citations that do not resolve:", file=sys.stderr)
        for line in problems:
            print(f"  {line}", file=sys.stderr)
        return 1
    if checked == 0:
        print("BLIND: found no test citation in the documentation", file=sys.stderr)
        return 2
    print(f"{checked} test citations resolve to a test of that name in the cited file")
    return 0


if __name__ == "__main__":
    sys.exit(main())
