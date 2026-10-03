"""Test scripts/release.sh end to end: bump a crate's version, update Cargo.lock,
run the gates against that manifest and lock, commit both, tag, and push.

The test runs in a scratch clone whose origin is a local bare repository, so it
never touches this repository's remote or a package registry.
"""

import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[3]


class ReleaseTests(unittest.TestCase):
    def test_bump_updates_and_commits_lock_before_locked_gates(self):
        with tempfile.TemporaryDirectory() as temporary:
            scratch = Path(temporary)
            seed = scratch / "seed"
            seed.mkdir()
            env = dict(os.environ, GIT_CONFIG_GLOBAL=os.devnull,
                       GIT_CONFIG_NOSYSTEM="1", GIT_AUTHOR_NAME="Fixture",
                       GIT_AUTHOR_EMAIL="fixture@example.invalid",
                       GIT_COMMITTER_NAME="Fixture",
                       GIT_COMMITTER_EMAIL="fixture@example.invalid")

            def run(*args, cwd=seed):
                return subprocess.run(args, cwd=cwd, env=env, check=True,
                                      text=True, capture_output=True)

            run("git", "init", "-b", "main")
            (seed / "Cargo.toml").write_text(
                '[workspace]\nresolver = "2"\nmembers = ["crates/subc-transport"]\n')
            crate = seed / "crates/subc-transport"
            (crate / "src").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "subc-transport"\nversion = "0.1.0"\n'
                'edition = "2021"\n')
            (crate / "src/lib.rs").write_text("pub fn fixture() {}\n")
            (seed / "scripts").mkdir()
            shutil.copy2(ROOT / "scripts/release.sh", seed / "scripts/release.sh")
            run("cargo", "generate-lockfile", "--offline")
            run("git", "add", ".")
            run("git", "commit", "-m", "fixture")
            remote = scratch / "origin.git"
            run("git", "init", "--bare", str(remote))
            clone = scratch / "clone"
            run("git", "clone", str(seed), str(clone))
            run("git", "remote", "set-url", "origin", str(remote), cwd=clone)
            run("git", "push", "-u", "origin", "main", cwd=clone)

            # Stub cargo so the gates run fast and offline. `fmt` passes. `clippy`
            # (run by the script with --locked) becomes `cargo metadata --locked`,
            # which still does real Cargo resolution and fails if Cargo.lock does
            # not match the bumped manifest; that is the property under test.
            # `publish` only checks it was asked for a dry run of this crate.
            # Compiling and publishing are not needed to exercise the release
            # steps, and no registry is ever contacted.
            cargo = shutil.which("cargo")
            stubs = scratch / "stubs"
            stubs.mkdir()
            (stubs / "cargo").write_text('''#!/usr/bin/env bash
set -euo pipefail
case "$1" in
  fmt) exit 0 ;;
  clippy) exec "$REAL_CARGO" metadata --format-version 1 --offline --locked ;;
  publish) [[ "$*" == "publish --package subc-transport --dry-run" ]] ;;
  *) exec "$REAL_CARGO" "$@" ;;
esac
''')
            (stubs / "cargo").chmod(0o755)
            env.update(PATH=str(stubs) + os.pathsep + env["PATH"], REAL_CARGO=cargo)
            result = subprocess.run(["bash", "scripts/release.sh", "subc-transport", "0.1.1"],
                                    cwd=clone, env=env, text=True, capture_output=True)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            run("cargo", "metadata", "--format-version", "1", "--offline", "--locked", cwd=clone)
            self.assertEqual(run("git", "status", "--porcelain", cwd=clone).stdout, "")
            self.assertEqual(set(run("git", "diff-tree", "--no-commit-id", "--name-only",
                                     "-r", "HEAD", cwd=clone).stdout.splitlines()),
                             {"crates/subc-transport/Cargo.toml", "Cargo.lock"})
            self.assertIn('version = "0.1.1"', (clone / "Cargo.lock").read_text())
            self.assertIn("refs/tags/subc-transport-v0.1.1",
                          run("git", "ls-remote", "--tags", "origin", cwd=clone).stdout)


if __name__ == "__main__":
    unittest.main()
