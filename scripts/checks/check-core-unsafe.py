#!/usr/bin/env python3
"""Keep the inherited-descriptor call as subc-core's only unsafe exception."""

import os
from pathlib import Path
import re
import subprocess
import sys


def main():
    root = Path("crates/subc-core")
    allowed = root / "src/main.rs"
    files = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", str(root)]
    )
    occurrences = []
    correct_site = False
    for name in files.split(b"\0"):
        if not name:
            continue
        path = Path(os.fsdecode(name))
        if not path.is_file():
            continue
        text = path.read_text(encoding="utf-8", errors="replace")
        if path == allowed:
            correct_site = re.search(
                r"#\[allow\(unsafe_code\)\]\s+let inherited\s*=\s*unsafe\s*\{\s*"
                r"subc_os::child_spawn::InheritedDescriptors::capture_daemon_resume\(", text
            ) is not None
        for match in re.finditer(r"\ballow\s*\(([^)]*)\)", text):
            if re.search(r"\bunsafe_code\b", match[1]):
                occurrences.append((path, text.count("\n", 0, match.start()) + 1))
    if correct_site and len(occurrences) == 1 and occurrences[0][0] == allowed:
        print(f"Checked {root}: one unsafe lint exception in {allowed}")
        return 0
    print(f"Expected exactly one unsafe lint exception in {allowed}", file=sys.stderr)
    for path, line in occurrences:
        print(f"  {path}:{line}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    sys.exit(main())
