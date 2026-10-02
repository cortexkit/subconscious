"""Check the Swift test step's effective package directory without compiling."""

import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest


ROOT = Path(__file__).resolve().parents[2]


class SwiftWorkflowTests(unittest.TestCase):
    def test_swift_workflow_runs_in_the_existing_package(self):
        source = (ROOT / ".github/workflows/subc-fed.yml").read_text()
        job = re.search(r"^  test:\n(.*?)(?=^  [\w-]+:|\Z)", source, re.M | re.S)
        self.assertIsNotNone(job, "test job was not found")
        working_directory = re.search(r"^        working-directory: (.+)$", job[1], re.M)
        self.assertIsNotNone(working_directory, "test job defaults were not found")
        step = re.search(r"      - name: Run package tests\n        run: \|\n((?:          .*\n)+)", job[1])
        self.assertIsNotNone(step, "Swift test step was not found")
        with tempfile.TemporaryDirectory() as temporary:
            swift = Path(temporary) / "swift"
            # Resolve the actual arguments in the actual job working directory.
            # This stand-in tests package discovery, not Swift compilation.
            swift.write_text(f"#!{sys.executable}\n" + '''
import pathlib
import sys
assert sys.argv[1] == "test", sys.argv
package = pathlib.Path(".")
if "--package-path" in sys.argv:
    package = pathlib.Path(sys.argv[sys.argv.index("--package-path") + 1])
if not (package / "Package.swift").is_file():
    sys.exit(f"no package at {package.resolve()}")
''')
            swift.chmod(0o755)
            result = subprocess.run(["bash", "-e", "-c", textwrap.dedent(step[1])],
                                    cwd=ROOT / working_directory[1],
                                    env=dict(os.environ, PATH=temporary + os.pathsep + os.environ["PATH"]),
                                    text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
