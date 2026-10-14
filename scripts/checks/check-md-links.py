#!/usr/bin/env python3
"""Fail when a relative link in a tracked Markdown file points at nothing.

Scans every Markdown file git tracks (or the files named on the command line),
finds inline links and images `[text](target)`, reference definitions
`[label]: target` and HTML `href`/`src` attributes, and checks that each
relative target exists in the working tree. Links inside fenced code blocks
and inline code spans are ignored, because those are examples rather than
navigation. External links (`http:`, `https:`, `mailto:` and other schemes)
and pure in-page anchors (`#section`) are not checked. A fragment after a
file (`file.md#section`) is stripped and only the file is checked.

The check refuses to pass on nothing: if it scanned no Markdown file, or found
no relative link at all, it reports itself blind and exits 2, because a
pattern that stopped matching would otherwise read as a clean result.

Exit 0 when every relative link resolves, 1 when one does not, 2 when blind.
"""

import os
import re
import subprocess
import sys

INLINE = re.compile(r"!?\[(?:[^\[\]]|\[[^\]]*\])*\]\(\s*<?([^)\s>]+)>?(?:\s+\"[^\"]*\")?\s*\)")
REFDEF = re.compile(r"^\s{0,3}\[[^\]]+\]:\s*<?(\S+?)>?(?:\s+.*)?$")
HTML = re.compile(r"""(?:href|src)\s*=\s*["']([^"']+)["']""")
SCHEME = re.compile(r"^[A-Za-z][A-Za-z0-9+.-]*:")
FENCE = re.compile(r"^\s{0,3}(```+|~~~+)")
CODE_SPAN = re.compile(r"(`+)(?:(?!\1).)+?\1")


def tracked_markdown(root):
    out = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.md"],
        cwd=root,
        check=True,
        capture_output=True,
    ).stdout.decode()
    return [p for p in out.split("\0") if p]


def links_in(text):
    """Yield (line number, target) for every link outside code."""
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
        line = CODE_SPAN.sub("", line)
        for pattern in (INLINE, HTML):
            for match in pattern.finditer(line):
                yield number, match.group(1)
        ref = REFDEF.match(line)
        if ref:
            yield number, ref.group(1)


def main(argv):
    root = subprocess.run(
        ["git", "rev-parse", "--show-toplevel"], check=True, capture_output=True, text=True
    ).stdout.strip()
    files = argv[1:] or tracked_markdown(root)
    broken = []
    checked = 0
    for rel in files:
        path = os.path.join(root, rel)
        with open(path, encoding="utf-8") as handle:
            text = handle.read()
        for number, target in links_in(text):
            if target.startswith("#") or SCHEME.match(target):
                continue
            file_part = target.split("#", 1)[0].split("?", 1)[0]
            if not file_part:
                continue
            checked += 1
            if file_part.startswith("/"):
                resolved = os.path.join(root, file_part.lstrip("/"))
            else:
                resolved = os.path.join(os.path.dirname(path), file_part)
            if not os.path.exists(resolved):
                broken.append(f"{rel}:{number}: {target}")
    if not files or checked == 0:
        print(
            f"BLIND: scanned {len(files)} Markdown files and found {checked} relative links",
            file=sys.stderr,
        )
        return 2
    if broken:
        print("relative links that point at nothing:", file=sys.stderr)
        for line in broken:
            print(f"  {line}", file=sys.stderr)
        return 1
    print(f"{checked} relative links in {len(files)} Markdown files resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
