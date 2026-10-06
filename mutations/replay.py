#!/usr/bin/env python3
"""macOS replay setup and per-invocation protection of the operator's daemon."""

import datetime
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
BASELINES = [
    ["-p", "subc-client-rs", "--lib"],
    ["-p", "subc-core", "--test", "privacy_identity", "--test", "provenance"],
    ["-p", "subc-daemon", "--lib"],
    ["-p", "subc-os", "--lib"],
    ["-p", "subc-protocol", "--test", "golden_json"],
    ["-p", "subc-transport", "--lib"],
]
COMMAND_TEST = "scripts/checks/no-external-path-deps.test.sh"


def host_daemon_logs(now):
    """Select both UTC log dates from an explicit, timezone-aware clock value."""
    if now.tzinfo is None or now.utcoffset() is None:
        raise ValueError("host daemon observation requires a timezone-aware clock")
    directory = Path(os.environ["CK_MUTATE_HOST_LOG_DIR"])
    today = now.astimezone(datetime.timezone.utc).date()
    yesterday = today - datetime.timedelta(days=1)
    return [directory / "logs" / f"subc.{day.isoformat()}.log"
            for day in (today, yesterday)]


def starts(now=None, previous=None):
    """Keep per-file counts, including the prior observation's UTC log window."""
    if now is None:
        now = datetime.datetime.now(datetime.timezone.utc)
    directory = Path(os.environ["CK_MUTATE_HOST_LOG_DIR"])
    if not directory.exists():
        return {}
    paths = {str(path): path for path in host_daemon_logs(now)}
    paths.update({name: Path(name) for name in (previous or {})})
    result = {}
    observed = False
    for name, path in paths.items():
        if not path.exists():
            result[name] = None
            continue
        observed = True
        proc = subprocess.run(
            ["grep", "-c", "subc daemon starting", str(path)],
            capture_output=True, text=True, check=False,
        )
        if proc.returncode not in (0, 1):
            raise RuntimeError(f"cannot read host daemon log: {proc.stderr}")
        result[name] = int(proc.stdout.strip())
    if not observed:
        raise RuntimeError("cannot observe the host daemon")
    return result


def start_count_increased(before, after):
    return any(count is not None and count > (before.get(name) or 0)
               for name, count in after.items())


def host_observation_note(before, after):
    return ("host daemon: none on this host" if not before and not after
            else "host daemon: UTC log counts observed")


def shell_check_count(stdout, stderr):
    """Count completed shell cases, including the first assertion that failed."""
    return sum(line.startswith(("PASS:", "test failure:"))
               for line in (stdout + "\n" + stderr).splitlines())


def require_reviewed_breadth(rows):
    unreviewed = [row["id"] for row in rows if row["outcome"] == "CAUGHT_BROADLY"]
    if unreviewed:
        raise RuntimeError(f"unreviewed cross-target catches: {', '.join(unreviewed)}")


def broad_run(args):
    # --catalogue is global and may precede the subcommand.
    if args[0] == "--catalogue":
        args = args[2:]
    elif args[0].startswith("--catalogue="):
        args = args[1:]
    return bool(args) and args[0] == "run" and "--broad" in args


def guarded(argv, counted=False):
    for key in ("XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"):
        if not os.environ.get(key):
            raise RuntimeError(f"refusing an unsandboxed invocation: {key} is absent")
    before = starts()
    diff = subprocess.check_output(["git", "diff", "--stat"], cwd=ROOT, text=True)
    if diff:
        print(f"NON-VACUITY BREAK applied:\n{diff}", file=sys.stderr, flush=True)
    began = time.monotonic()
    # Cargo also runs inside command-test fixtures; preserve their working directory.
    if counted:
        proc = subprocess.run(argv, capture_output=True, text=True, check=False)
        print(proc.stdout, end="", flush=True)
        print(proc.stderr, end="", file=sys.stderr, flush=True)
        print(f"Executed {shell_check_count(proc.stdout, proc.stderr)} shell checks", flush=True)
        # A check's own infrastructure error must not masquerade as an assertion.
        assertion_failed = any(line.startswith("test failure:") for line in proc.stderr.splitlines())
        infrastructure_failed = "check itself failed:" in proc.stderr or (
            proc.returncode != 0 and not assertion_failed
        )
        status = 127 if infrastructure_failed else proc.returncode
    else:
        status = subprocess.call(argv)
    after = starts(previous=before)
    record = {
        "argv": argv, "exit_code": status, "wall_s": time.monotonic() - began,
        "starts_before": before, "starts_after": after, "diff_stat": diff,
        "host_daemon_observation": host_observation_note(before, after),
    }
    with open(os.environ["CK_MUTATE_INVOCATIONS"], "a", encoding="utf-8") as audit:
        audit.write(json.dumps(record) + "\n")
    # Do not append telemetry to nextest's JSON list output. The invocation
    # audit records both counts without contaminating the runner's parser.
    if start_count_increased(before, after):
        raise RuntimeError("operator daemon start count changed; stop and investigate")
    # A signal is an infrastructure failure, not a command-test catch.
    return status if status >= 0 else 127


def cargo(args):
    real = os.environ["CK_MUTATE_REAL_CARGO"]
    return guarded([real, *args])


def main(args):
    if args and args[0] == "--cargo":
        return cargo(args[1:])
    if args and args[0] == "--command":
        if args[1:] != [COMMAND_TEST]:
            raise RuntimeError("unknown command-test id")
        return guarded(["bash", COMMAND_TEST], counted=True)
    if not args:
        raise RuntimeError("usage: python3 mutations/replay.py selftest|baseline|check|run|prove ...")
    os.chdir(ROOT)
    real_cargo = shutil.which("cargo")
    runner = os.environ.get("CK_MUTATE", "ck-mutate")
    if not real_cargo or not shutil.which(runner):
        raise RuntimeError("install Cargo and the pinned ck-mutate first")
    if Path(real_cargo).resolve() == (ROOT / "mutations/bin/cargo").resolve():
        raise RuntimeError("do not put mutations/bin on PATH yourself; use this entry point")
    output = ROOT / "target/mutations"
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="sandbox-", dir=output) as sandbox:
        os.environ["CK_MUTATE_REAL_CARGO"] = real_cargo
        os.environ["CK_MUTATE_HOST_LOG_DIR"] = str(Path.home() / ".local/share/cortexkit/run")
        audit = output / f"invocations-{time.time_ns()}.jsonl"
        os.environ["CK_MUTATE_INVOCATIONS"] = str(audit)
        for key in ("XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"):
            path = Path(sandbox) / key
            path.mkdir()
            os.environ[key] = str(path)
        os.environ["TMPDIR"] = sandbox
        os.environ["PATH"] = str(ROOT / "mutations/bin") + os.pathsep + os.environ["PATH"]
        print(f"invocation evidence: {audit.relative_to(ROOT)}", flush=True)
        before = starts()
        began = time.monotonic()
        if args == ["selftest"]:
            status = guarded([sys.executable, "-m", "unittest", "discover", "-s", "mutations",
                              "-p", "test_*.py", "-v"])
        elif args == ["baseline"]:
            status = cargo(["build", "--locked", "-p", "subc-daemon", "--bins",
                            "--features", "test-support"])
            if not status:
                status = cargo(["build", "--locked", "-p", "subc-core", "--bins",
                                "--features", "test-support"])
            for selection in BASELINES:
                if status:
                    break
                status = cargo(["test", "--locked", *selection])
            if not status:
                status = guarded(["bash", COMMAND_TEST], counted=True)
        else:
            status = subprocess.call([runner, *args], cwd=ROOT)
            if status == 0 and broad_run(args):
                try:
                    report = next((arg.split("=", 1)[1] for arg in args if arg.startswith("--report=")), None)
                    if "--report" in args:
                        report = args[args.index("--report") + 1]
                    if not report:
                        raise RuntimeError("broad replay requires --report to enforce reviewed catches")
                    require_reviewed_breadth(json.loads(Path(report).read_text()))
                except (RuntimeError, OSError, ValueError) as error:
                    print(f"breadth policy refused replay: {error}", file=sys.stderr, flush=True)
                    status = 127
        after = starts(previous=before)
        elapsed = time.monotonic() - began
        diff = subprocess.check_output(["git", "diff", "--stat"], cwd=ROOT, text=True)
        session = {"args": args, "wall_s": elapsed, "exit_code": status,
                   "starts_before": before, "starts_after": after,
                   "host_daemon_observation": host_observation_note(before, after),
                   "restored_diff_stat": diff, "invocations": str(audit.relative_to(ROOT))}
        audit.with_suffix(".session.json").write_text(json.dumps(session, indent=2) + "\n")
        print(host_observation_note(before, after), flush=True)
        print(f"host daemon starts: {before} -> {after}", flush=True)
        print(f"replay wall time: {elapsed:.3f}s; restored diff stat: {diff!r}", flush=True)
        if start_count_increased(before, after):
            raise RuntimeError("operator daemon start count changed; stop and investigate")
        return status


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"mutation replay infrastructure error: {error}", file=sys.stderr)
        sys.exit(127)
