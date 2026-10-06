import contextlib
from datetime import date, datetime, timedelta, timezone
import io
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import replay


class HostDaemonObservation(unittest.TestCase):
    def read_at(self, instant, previous=None):
        class FixedDate(date):
            @classmethod
            def today(cls):
                return instant.date()

        class FixedDatetime(datetime):
            @classmethod
            def now(cls, tz=None):
                return instant.astimezone(tz) if tz else instant.replace(tzinfo=None)

        with patch.object(replay.datetime, "date", FixedDate), \
                patch.object(replay.datetime, "datetime", FixedDatetime):
            return replay.starts(previous=previous)

    def test_start_after_local_midnight_is_observed_in_utc_log(self):
        instant = datetime(2026, 10, 7, 0, 21, tzinfo=timezone(timedelta(hours=2)))
        with tempfile.TemporaryDirectory() as root, \
                patch.dict(os.environ, {"CK_MUTATE_HOST_LOG_DIR": root}):
            logs = Path(root) / "logs"
            logs.mkdir()
            (logs / "subc.2026-10-05.log").write_text("subc daemon starting\n" * 2)
            current = logs / "subc.2026-10-06.log"
            current.write_text("2026-10-06T20:15:26Z subc daemon starting\n")
            before = self.read_at(instant)
            with current.open("a") as log:
                log.write("2026-10-06T22:22:00Z subc daemon starting\n")
            after = self.read_at(instant + timedelta(minutes=1), previous=before)
            self.assertNotEqual(before, after, "UTC log starts must remain visible after local midnight")
            self.assertEqual((before[str(current)], after[str(current)]), (1, 2))
            self.assertIn(str(logs / "subc.2026-10-05.log"), before)
            self.assertEqual(before[str(logs / "subc.2026-10-05.log")], 2)
            self.assertTrue(replay.start_count_increased(before, after))

    def test_log_paths_use_explicit_clock_instead_of_wall_clock(self):
        instant = datetime(2026, 10, 7, 0, 21, tzinfo=timezone(timedelta(hours=2)))
        with patch.dict(os.environ, {"CK_MUTATE_HOST_LOG_DIR": "/host-run"}):
            self.assertEqual(replay.host_daemon_logs(instant),
                             [Path("/host-run/logs/subc.2026-10-06.log"),
                              Path("/host-run/logs/subc.2026-10-05.log")])

    def test_start_after_utc_midnight_is_added_to_previous_day_count(self):
        before_time = datetime(2026, 10, 6, 23, 59, tzinfo=timezone.utc)
        after_time = datetime(2026, 10, 7, 0, 1, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as root, \
                patch.dict(os.environ, {"CK_MUTATE_HOST_LOG_DIR": root}):
            logs = Path(root) / "logs"
            logs.mkdir()
            (logs / "subc.2026-10-06.log").write_text("subc daemon starting\n" * 2)
            before = self.read_at(before_time)
            (logs / "subc.2026-10-07.log").write_text("2026-10-07T00:00:30Z subc daemon starting\n")
            after = self.read_at(after_time, previous=before)
            self.assertEqual(before[str(logs / "subc.2026-10-06.log")], 2)
            self.assertEqual(after[str(logs / "subc.2026-10-06.log")], 2)
            self.assertEqual(after[str(logs / "subc.2026-10-07.log")], 1)
            self.assertTrue(replay.start_count_increased(before, after))

    def test_missing_utc_logs_refuse_even_when_legacy_log_exists(self):
        instant = datetime(2026, 10, 6, 22, 21, tzinfo=timezone.utc)
        with tempfile.TemporaryDirectory() as root, \
                patch.dict(os.environ, {"CK_MUTATE_HOST_LOG_DIR": root}):
            (Path(root) / "subc.log").write_text("subc daemon starting\n")
            with self.assertRaisesRegex(RuntimeError, "cannot observe the host daemon"):
                self.read_at(instant)

    def test_utc_rollover_cannot_cancel_a_new_start_against_a_dropped_day(self):
        instant = [datetime(2026, 10, 6, 23, 59, tzinfo=timezone.utc)]

        class FixedDatetime(datetime):
            @classmethod
            def now(cls, tz=None):
                return instant[0].astimezone(tz) if tz else instant[0].replace(tzinfo=None)

        with tempfile.TemporaryDirectory() as root:
            logs = Path(root) / "logs"
            logs.mkdir()
            (logs / "subc.2026-10-05.log").write_text("subc daemon starting\n")
            (logs / "subc.2026-10-06.log").write_text("subc daemon starting\n" * 2)
            env = dict(CK_MUTATE_HOST_LOG_DIR=root, CK_MUTATE_INVOCATIONS=str(Path(root) / "audit"),
                       XDG_DATA_HOME="data", XDG_RUNTIME_DIR="run", XDG_CONFIG_HOME="config")

            def restart(_argv):
                instant[0] = datetime(2026, 10, 7, 0, 1, tzinfo=timezone.utc)
                (logs / "subc.2026-10-07.log").write_text("subc daemon starting\n")
                return 0

            with patch.dict(os.environ, env), patch.object(replay.datetime, "datetime", FixedDatetime), \
                    patch.object(subprocess, "check_output", return_value=""), \
                    patch.object(subprocess, "call", side_effect=restart), \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaisesRegex(RuntimeError, "start count changed"):
                    replay.guarded(["unused"])

    def test_host_without_run_directory_records_absence_and_proceeds(self):
        previous = Path.cwd()
        with tempfile.TemporaryDirectory() as root:
            root = Path(root)
            home = root / "home"
            try:
                with patch.object(replay, "ROOT", root), patch.dict(os.environ, {"HOME": str(home)}), \
                        patch.object(subprocess, "check_output", return_value=""), \
                        patch.object(subprocess, "call", return_value=0), \
                        patch.object(replay.shutil, "which", return_value="/bin/unused-cargo"), \
                        contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                    self.assertEqual(replay.main(["selftest"]), 0)
                sessions = list((root / "target/mutations").glob("*.session.json"))
                self.assertEqual(len(sessions), 1)
                session = json.loads(sessions[0].read_text())
                self.assertEqual(session["host_daemon_observation"], "host daemon: none on this host")
                self.assertEqual(session["starts_before"], {})
                self.assertEqual(session["starts_after"], {})
                self.assertFalse((home / ".local/share/cortexkit/run").exists())
            finally:
                os.chdir(previous)

    def test_utc_rollover_without_a_start_does_not_refuse_replay(self):
        with tempfile.TemporaryDirectory() as root:
            logs = Path(root) / "logs"
            logs.mkdir()
            old = logs / "subc.2026-10-05.log"
            old.write_text("subc daemon starting\n")
            (logs / "subc.2026-10-06.log").write_text("subc daemon starting\n" * 2)
            with patch.dict(os.environ, {"CK_MUTATE_HOST_LOG_DIR": root}):
                before = self.read_at(datetime(2026, 10, 6, 23, 59, tzinfo=timezone.utc))
                after = self.read_at(datetime(2026, 10, 7, 0, 1, tzinfo=timezone.utc), previous=before)
                self.assertEqual(after[str(old)], 1, "the after-read must retain before-time files")
                self.assertFalse(replay.start_count_increased(before, after))

    def test_new_empty_log_is_not_a_daemon_start(self):
        with tempfile.TemporaryDirectory() as root:
            logs = Path(root) / "logs"
            logs.mkdir()
            instant = datetime(2026, 10, 6, 22, 21, tzinfo=timezone.utc)
            (logs / "subc.2026-10-05.log").write_text("")
            with patch.dict(os.environ, {"CK_MUTATE_HOST_LOG_DIR": root}):
                before = self.read_at(instant)
                (logs / "subc.2026-10-06.log").write_text("")
                after = self.read_at(instant, previous=before)
                self.assertFalse(replay.start_count_increased(before, after))


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
