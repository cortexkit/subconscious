"""Validate sweep manifests with real BSD and GNU stat (gstat on BSD hosts)."""

from datetime import datetime, timezone
import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]


class SweepTests(unittest.TestCase):
    def check_manifest(self, stat_command):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary)
            bin_dir = scratch / "bin"
            run_dir = scratch / "run"
            stubs = scratch / "stubs"
            for directory in (bin_dir, run_dir, stubs):
                directory.mkdir()
            (stubs / "stat").symlink_to(stat_command)
            live = bin_dir / "ck-module"
            live.write_text("live binary\n")
            expected = {}
            for index in range(1, 5):
                backup = bin_dir / f"ck-module.bak-{index}"
                contents = f"snapshot {index}\n".encode()
                backup.write_bytes(contents)
                modified = 946684800 + index
                os.utime(backup, (modified, modified))
                if index <= 2:
                    expected[backup.name] = [hashlib.sha256(contents).hexdigest(),
                                             str(len(contents)),
                                             datetime.fromtimestamp(modified, timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")]
            result = subprocess.run(["bash", str(ROOT / "scripts/fleet/sweep-bin-backups.sh"), "--dry-run"],
                                    cwd=scratch, text=True, capture_output=True,
                                    env=dict(os.environ, CK_BIN_DIR=str(bin_dir), CK_RUN_DIR=str(run_dir),
                                             PATH=str(stubs) + os.pathsep + os.environ["PATH"],
                                             TZ="Pacific/Honolulu"))
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual(result.stderr, "", "metadata collection emitted errors")
            manifests = list(run_dir.glob("*.manifest"))
            self.assertEqual(len(manifests), 1)
            rows = [line.split("  ", 3) for line in manifests[0].read_text().splitlines()
                    if not line.startswith("#")]
            self.assertTrue(all(len(row) == 4 for row in rows), rows)
            self.assertEqual({row[3]: row[:3] for row in rows}, expected)
            self.assertTrue(live.is_file())
            self.assertEqual(len(list(bin_dir.glob("ck-module.*"))), 4)

    def test_gnu_stat_manifest_records_exact_size_and_utc_mtime(self):
        gnu_stat = shutil.which("gstat") or shutil.which("stat")
        version = subprocess.run([gnu_stat, "--version"], capture_output=True)
        self.assertEqual(version.returncode, 0, "GNU stat is required (install coreutils on BSD hosts)")
        self.check_manifest(gnu_stat)

    def test_native_stat_manifest_records_exact_size_and_utc_mtime(self):
        self.check_manifest(shutil.which("stat"))

    def test_failed_metadata_collection_refuses_to_delete_snapshots(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary)
            bin_dir = scratch / "bin"
            run_dir = scratch / "run"
            stubs = scratch / "stubs"
            for directory in (bin_dir, run_dir, stubs):
                directory.mkdir()
            for index in range(4):
                backup = bin_dir / f"ck-module.bak-{index}"
                backup.write_text(f"snapshot {index}\n")
                os.utime(backup, (946684800 + index, 946684800 + index))
            (stubs / "stat").write_text('#!/bin/sh\n[ "$1" = "--version" ] && exit 0\nexit 23\n')
            (stubs / "stat").chmod(0o755)
            result = subprocess.run(["bash", str(ROOT / "scripts/fleet/sweep-bin-backups.sh")],
                                    cwd=scratch, text=True, capture_output=True,
                                    env=dict(os.environ, CK_BIN_DIR=str(bin_dir), CK_RUN_DIR=str(run_dir),
                                             PATH=str(stubs) + os.pathsep + os.environ["PATH"]))
            self.assertNotEqual(result.returncode, 0, "metadata failure was ignored")
            self.assertEqual(len(list(bin_dir.glob("ck-module.*"))), 4,
                             "snapshots were deleted without a valid manifest")


if __name__ == "__main__":
    unittest.main()
