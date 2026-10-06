import contextlib
import io
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import replay


class ReplaySafety(unittest.TestCase):
    def test_global_catalogue_option_does_not_bypass_breadth_policy(self):
        previous = Path.cwd()
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            (root / "report.json").write_text('[{"id":"unreviewed","outcome":"CAUGHT_BROADLY"}]')
            try:
                with patch.object(replay, "ROOT", root), patch.dict(os.environ, os.environ.copy()), \
                        patch.object(replay, "starts", return_value={}), \
                        patch.object(subprocess, "check_output", return_value=""), \
                        patch.object(subprocess, "call", return_value=0), \
                        patch.object(replay.shutil, "which", return_value="/bin/unused-cargo"), \
                        contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    status = replay.main(["--catalogue", "custom.toml", "run", "--all", "--broad",
                                          "--report", "report.json"])
                    self.assertEqual(status, 127)
            finally:
                os.chdir(previous)

    def test_unreviewed_cross_target_catch_is_rejected(self):
        with self.assertRaisesRegex(RuntimeError, "unreviewed cross-target catches: fresh-target"):
            replay.require_reviewed_breadth([{"id": "fresh-target", "outcome": "CAUGHT_BROADLY"}])
        replay.require_reviewed_breadth([{"id": "approved", "outcome": "HUB"}])

    def test_shell_count_comes_from_executed_cases_not_planned_cases(self):
        self.assertEqual(replay.shell_check_count("PASS: inside\n", "test failure: outside\n"), 2)
        self.assertEqual(replay.shell_check_count("5 checks planned\n", ""), 0)

    def test_zero_event_command_reports_zero_not_a_synthetic_pass(self):
        env = dict(XDG_DATA_HOME="data", XDG_RUNTIME_DIR="run", XDG_CONFIG_HOME="config")
        with tempfile.TemporaryDirectory() as root:
            env["CK_MUTATE_INVOCATIONS"] = str(Path(root) / "audit")
            out = io.StringIO()
            with patch.dict(os.environ, env), patch.object(replay, "starts", return_value={}), \
                    patch.object(subprocess, "check_output", return_value=""), \
                    patch.object(subprocess, "run", return_value=subprocess.CompletedProcess([], 0, "", "")), \
                    contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(replay.guarded(["unused"], counted=True), 0)
            self.assertIn("Executed 0 shell checks", out.getvalue())

    def test_command_infrastructure_failure_is_not_a_red_assertion(self):
        env = dict(XDG_DATA_HOME="data", XDG_RUNTIME_DIR="run", XDG_CONFIG_HOME="config")
        with tempfile.TemporaryDirectory() as root:
            env["CK_MUTATE_INVOCATIONS"] = str(Path(root) / "audit")
            failure = subprocess.CompletedProcess([], 1, "PASS: inside\n",
                                                  "check itself failed: cargo metadata\ntest failure: outside\n")
            with patch.dict(os.environ, env), patch.object(replay, "starts", return_value={}), \
                    patch.object(subprocess, "check_output", return_value=""), \
                    patch.object(subprocess, "run", return_value=failure), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(replay.guarded(["unused"], counted=True), 127)

    def test_guarded_cargo_preserves_fixture_working_directory(self):
        env = dict(XDG_DATA_HOME="data", XDG_RUNTIME_DIR="run", XDG_CONFIG_HOME="config")
        with tempfile.TemporaryDirectory() as root:
            env["CK_MUTATE_INVOCATIONS"] = str(Path(root) / "audit")
            with patch.dict(os.environ, env), patch.object(replay, "starts", return_value={}), \
                    patch.object(subprocess, "check_output", return_value=""), \
                    patch.object(subprocess, "call", return_value=0) as call, \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(replay.guarded(["cargo", "metadata"]), 0)
            call.assert_called_once_with(["cargo", "metadata"])

    def test_setup_failure_after_a_pass_is_not_a_red_assertion(self):
        env = dict(XDG_DATA_HOME="data", XDG_RUNTIME_DIR="run", XDG_CONFIG_HOME="config")
        with tempfile.TemporaryDirectory() as root:
            env["CK_MUTATE_INVOCATIONS"] = str(Path(root) / "audit")
            failure = subprocess.CompletedProcess([], 101, "PASS: inside\n", "error: lock generation failed\n")
            with patch.dict(os.environ, env), patch.object(replay, "starts", return_value={}), \
                    patch.object(subprocess, "check_output", return_value=""), \
                    patch.object(subprocess, "run", return_value=failure), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                self.assertEqual(replay.guarded(["unused"], counted=True), 127)

    def test_start_count_change_refuses_a_pass(self):
        env = dict(XDG_DATA_HOME="data", XDG_RUNTIME_DIR="run", XDG_CONFIG_HOME="config")
        with tempfile.TemporaryDirectory() as root:
            env["CK_MUTATE_INVOCATIONS"] = str(Path(root) / "audit")
            with patch.dict(os.environ, env), \
                    patch.object(replay, "starts", side_effect=[{"host": 3}, {"host": 4}]), \
                    patch.object(subprocess, "check_output", return_value=""), \
                    patch.object(subprocess, "call", return_value=0), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "start count changed"):
                    replay.guarded(["unused"])

    def test_unsandboxed_invocation_is_refused_before_spawn(self):
        with patch.dict(os.environ, {}, clear=True), patch.object(subprocess, "call") as call:
            with self.assertRaisesRegex(RuntimeError, "XDG_DATA_HOME is absent"):
                replay.guarded(["unused"])
            call.assert_not_called()


if __name__ == "__main__":
    unittest.main()
