"""Root-only real polkit fixture; all authentication runs as a disposable user."""

import fcntl
import json
import os
import pathlib
import pwd
import select
import signal
import subprocess
import sys
import termios
import time

import dbus


USER = os.environ["SUBC_POLKIT_TEST_USER"]
UID = pwd.getpwnam(USER).pw_uid
assert os.geteuid() == 0 and UID != 0
PASSWORD = os.environ["SUBC_POLKIT_TEST_PASSWORD"]
ACTION = f"io.cortexkit.subc.elevate.u{UID}"
POLICY = pathlib.Path(f"/usr/share/polkit-1/actions/{ACTION}.policy")
RULE = pathlib.Path(f"/etc/polkit-1/rules.d/49-cortexkit-prompt-test-u{UID}.rules")
SENTENCE = b"approve a request from polkit-test: literal $(x) & <write> (requested by a local program)"
BINARY = sys.argv[1]
XML = sys.stdin.read()
CREATED = []
assert f'id="{ACTION}"' in XML
assert "<allow_any>no</allow_any>" in XML
assert "<allow_inactive>no</allow_inactive>" in XML
assert "<allow_active>auth_self</allow_active>" in XML and "_keep" not in XML


def create(path, content):
    with path.open("x") as output:
        CREATED.append(path)
        output.write(content)
    path.chmod(0o644)


def read_until(fd, marker, timeout=20):
    output = b""
    deadline = time.monotonic() + timeout
    while marker not in output:
        remaining = deadline - time.monotonic()
        assert remaining > 0, (marker, output)
        assert select.select([fd], [], [], remaining)[0], (marker, output)
        data = os.read(fd, 65536)
        assert data, (marker, output)
        output += data
    return output


def start_child():
    child = subprocess.Popen(
        ["runuser", "-u", USER, "--", "env", "LC_ALL=C", "SUBC_POLKIT_CHILD=1",
         BINARY, "--exact", "polkit_child", "--nocapture"],
        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        start_new_session=True,
    )
    assert child.stdout is not None
    output = read_until(child.stdout.fileno(), b"CHILD_PID:")
    while b"\n" not in output.split(b"CHILD_PID:", 1)[1]:
        output += os.read(child.stdout.fileno(), 4096)
    pid = int(output.split(b"CHILD_PID:", 1)[1].splitlines()[0])
    stat = pathlib.Path(f"/proc/{pid}/stat").read_text()
    start = int(stat.rsplit(")", 1)[1].split()[19])
    return child, pid, start


def wait_policy(pid, start, challenge):
    """Wait for polkit's directory monitor, not a fixed sleep after file writes."""
    bus = dbus.SystemBus()
    authority = dbus.Interface(
        bus.get_object("org.freedesktop.PolicyKit1", "/org/freedesktop/PolicyKit1/Authority"),
        "org.freedesktop.PolicyKit1.Authority",
    )
    deadline = time.monotonic() + 10
    while True:
        try:
            result = authority.CheckAuthorization(
                ("unix-process", {"pid": dbus.UInt32(pid), "start-time": dbus.UInt64(start),
                                  "uid": dbus.Int32(UID)}),
                ACTION, dbus.Dictionary(signature="ss"), dbus.UInt32(0), "",
            )
            if not result[0] and bool(result[1]) == challenge:
                return
        except dbus.DBusException:
            pass
        assert time.monotonic() < deadline, "polkit did not load the expected fixture policy"
        time.sleep(0.05)


def start_agent(pid, start):
    master, slave = os.openpty()
    notify_read, notify_write = os.pipe()

    account = pwd.getpwnam(USER)

    # Drop to the test user in the child itself rather than through runuser:
    # runuser keeps a parent process that inherits the notify pipe, so the
    # agent closing its copy after registering would never reach us as EOF.
    def terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        os.initgroups(USER, account.pw_gid)
        os.setgid(account.pw_gid)
        os.setuid(account.pw_uid)

    agent = subprocess.Popen(
        ["pkttyagent", "--process", f"{pid},{start}", "--notify-fd", str(notify_write)],
        stdin=slave, stdout=slave, stderr=slave, pass_fds=(notify_write,), preexec_fn=terminal,
        env={"LC_ALL": "C", "HOME": account.pw_dir, "USER": USER, "LOGNAME": USER,
             "PATH": "/usr/sbin:/usr/bin:/sbin:/bin"},
    )
    os.close(slave)
    os.close(notify_write)
    try:
        registered = select.select([notify_read], [], [], 10)[0]
        assert registered, f"agent registration timed out; {agent_diagnostics(agent, master)}"
        assert os.read(notify_read, 1) == b""
        assert agent.poll() is None, f"agent exited instead of registering; {agent_diagnostics(agent, master)}"
    except BaseException:
        stop(agent)
        os.close(master)
        raise
    finally:
        os.close(notify_read)
    return agent, master


def agent_diagnostics(agent, master):
    """What the agent printed on its terminal and whether it exited, for a
    failure message; reads only what is already buffered."""
    output = b""
    while select.select([master], [], [], 0.2)[0]:
        try:
            chunk = os.read(master, 4096)
        except OSError:
            break
        if not chunk:
            break
        output += chunk
    return f"exit={agent.poll()!r} terminal={output!r}"


def stop(process):
    if process is not None and process.poll() is None:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            return
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait(timeout=5)


def exercise(case, expected, agent_mode=None, policy_ready=None):
    child, pid, start = start_child()
    assert child.stdin is not None and child.stdout is not None
    agent, master = None, None
    child_output, terminal_output = b"", b""
    try:
        if policy_ready is not None:
            wait_policy(pid, start, policy_ready)
        if agent_mode is not None:
            agent, master = start_agent(pid, start)
        child.stdin.write(b"go\n")
        child.stdin.flush()
        if agent_mode is not None:
            assert master is not None
            terminal_output = read_until(master, b"Password:")
            assert SENTENCE in terminal_output, terminal_output
            if agent_mode == "withdraw":
                child.stdin.write(b"withdraw\n")
                child.stdin.flush()
            else:
                answer = PASSWORD if agent_mode == "right" else "not-the-test-password"
                os.write(master, answer.encode() + b"\n")
        answered = terminal_output.count(b"Password:")
        deadline = time.monotonic() + 30
        while b"OUTCOME:" not in child_output or b"\n" not in child_output.split(b"OUTCOME:", 1)[-1]:
            remaining = deadline - time.monotonic()
            assert remaining > 0, (case, child_output, terminal_output)
            fds = [child.stdout.fileno()] + ([] if master is None else [master])
            readable = select.select(fds, [], [], remaining)[0]
            assert readable, (case, child_output, terminal_output)
            for fd in readable:
                data = os.read(fd, 65536)
                assert data, (case, child_output, terminal_output)
                if fd == child.stdout.fileno():
                    child_output += data
                else:
                    assert master is not None
                    terminal_output += data
                    prompts = terminal_output.count(b"Password:")
                    if prompts > answered:
                        assert agent_mode == "wrong" and prompts <= 5, terminal_output
                        os.write(master, b"not-the-test-password\n" * (prompts - answered))
                        answered = prompts
        outcome = child_output.split(b"OUTCOME:", 1)[1].splitlines()[0].decode()
        assert outcome == expected, (case, outcome, expected, child_output, terminal_output)
        assert child.wait(timeout=5) == 0, child_output
        if agent_mode == "withdraw":
            # The child already got its answer, so polkitd ended the check. The
            # text agent learns of it only from polkitd's CancelAuthentication,
            # which it cannot process while its password prompt blocks reading
            # the terminal (polkitagenttextlistener.c reads with getc inside the
            # request handler). End that read with an empty line; the queued
            # cancellation then prints "Cancelled". Graphical agents are not
            # blocked this way and close the dialog at once.
            if b"Cancelled" not in terminal_output:
                os.write(master, b"\n")
                terminal_output += read_until(master, b"Cancelled", timeout=10)
            assert b"AUTHENTICATION COMPLETE" not in terminal_output, terminal_output
        print(f"polkit case {case}: {outcome}; terminal={terminal_output!r}", flush=True)
    finally:
        stop(child)
        stop(agent)
        if master is not None:
            os.close(master)


def interrupted(signum, frame):
    raise SystemExit(f"interrupted by signal {signum}")


signal.signal(signal.SIGTERM, interrupted)
signal.signal(signal.SIGINT, interrupted)
assert not POLICY.exists() and not RULE.exists(), "refusing to overwrite an existing policy or rule"
try:
    exercise("missing policy", "SetupRequired")
    create(POLICY, XML)
    exercise("production policy without a local session", "NoPresence", policy_ready=False)
    # Headless CI has no active local logind session. Only this dedicated uid
    # and exact action get a one-shot authentication challenge in the fixture.
    # Older polkit releases expose the username but not subject.uid; resolve
    # that name back to its numeric uid without a shell before allowing it.
    create(RULE, f"""polkit.addRule(function(action, subject) {{
    if (action.id === {json.dumps(ACTION)} &&
        subject.user === {json.dumps(USER)} &&
        polkit.spawn(["/usr/bin/id", "-u", subject.user]).trim() === "{UID}") {{
        return polkit.Result.AUTH_SELF;
    }}
}});
""")
    exercise("right password and literal text", "Approved", "right", policy_ready=True)
    exercise("wrong password", "Declined", "wrong")
    exercise("no agent", "NoPresence")
    exercise("withdraw cancels the agent request", "Declined", "withdraw")
finally:
    for path in reversed(CREATED):
        path.unlink()
