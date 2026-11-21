"""Run the offline shell-bootstrap smoke checks from unittest as well."""

from pathlib import Path
import subprocess
import unittest


class InstallerTests(unittest.TestCase):
    def test_install_and_reinstall_defer_placement_to_the_journal_helper(self):
        smoke = Path(__file__).with_name("install.sh.test")
        result = subprocess.run(["bash", str(smoke)], text=True, capture_output=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == "__main__":
    unittest.main()
