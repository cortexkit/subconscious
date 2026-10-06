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


def starts():
    """Count today's real host log, not the sandbox's log (missing is explicit)."""
    directory = Path(os.environ["CK_MUTATE_HOST_LOG_DIR"])
    today = datetime.date.today().isoformat()
    paths = [directory / "logs" / f"subc.{today}.log", directory / "subc.log"]
    result = {}
    for path in paths:
        if not path.exists():
            result[str(path)] = None
            continue
        proc = subprocess.run(
            ["grep", "-c", "subc daemon starting", str(path)],
            capture_output=True, text=True, check=False,
        )
        if proc.returncode not in (0, 1):
            raise RuntimeError(f"cannot read host daemon log: {proc.stderr}")
        result[str(path)] = int(proc.stdout.strip())
    return result


def guarded(argv):
    for key in ("XDG_DATA_HOME", "XDG_RUNTIME_DIR", "XDG_CONFIG_HOME"):
        if not os.environ.get(key):
            raise RuntimeError(f"refusing an unsandboxed invocation: {key} is absent")
    before = starts()
    diff = subprocess.check_output(["git", "diff", "--stat"], cwd=ROOT, text=True)
    if diff:
        print(f"NON-VACUITY BREAK applied:\n{diff}", file=sys.stderr, flush=True)
    began = time.monotonic()
    status = subprocess.call(argv, cwd=ROOT)
    after = starts()
    record = {
        "argv": argv, "exit_code": status, "wall_s": time.monotonic() - began,
        "starts_before": before, "starts_after": after, "diff_stat": diff,
    }
    with open(os.environ["CK_MUTATE_INVOCATIONS"], "a", encoding="utf-8") as audit:
        audit.write(json.dumps(record) + "\n")
    print(f"host daemon starts: {before} -> {after}", file=sys.stderr, flush=True)
    if before != after:
        raise RuntimeError("operator daemon start count changed; stop and investigate")
    # A signal is an infrastructure failure, not a command-test catch.
    return status if status >= 0 else 127


def cargo(args):
    real = os.environ["CK_MUTATE_REAL_CARGO"]
    if args and args[0] == "test" and "subc-daemon" in args and "--no-run" in args:
        status = guarded([real, "build", "--locked", "-p", "subc-daemon", "--bins",
                          "--features", "test-support"])
        if status:
            return status
    return guarded([real, *args])


def main(args):
    if args and args[0] == "--cargo":
        return cargo(args[1:])
    if args and args[0] == "--command":
        if args[1:] != [COMMAND_TEST]:
            raise RuntimeError("unknown command-test id")
        return guarded(["bash", COMMAND_TEST])
    if sys.platform != "darwin":
        raise RuntimeError("this catalogue requires macOS; ignored Darwin tests are not proofs")
    if not args:
        raise RuntimeError("usage: python3 mutations/replay.py baseline|check|run|prove ...")
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
        if args == ["baseline"]:
            status = cargo(["build", "--locked", "-p", "subc-daemon", "--bins",
                            "--features", "test-support"])
            for selection in BASELINES:
                if status:
                    break
                status = cargo(["test", "--locked", *selection])
            if not status:
                status = guarded(["bash", COMMAND_TEST])
        else:
            status = subprocess.call([runner, *args], cwd=ROOT)
        after = starts()
        elapsed = time.monotonic() - began
        diff = subprocess.check_output(["git", "diff", "--stat"], cwd=ROOT, text=True)
        session = {"args": args, "wall_s": elapsed, "exit_code": status,
                   "starts_before": before, "starts_after": after,
                   "restored_diff_stat": diff, "invocations": str(audit.relative_to(ROOT))}
        audit.with_suffix(".session.json").write_text(json.dumps(session, indent=2) + "\n")
        print(f"replay wall time: {elapsed:.3f}s; restored diff stat: {diff!r}", flush=True)
        if before != after:
            raise RuntimeError("operator daemon start count changed; stop and investigate")
        return status


if __name__ == "__main__":
    try:
        sys.exit(main(sys.argv[1:]))
    except (RuntimeError, OSError, subprocess.SubprocessError) as error:
        print(f"mutation replay infrastructure error: {error}", file=sys.stderr)
        sys.exit(127)
