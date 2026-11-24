"""Give the SDK a real inherited socketpair, proxying its peer to the test server."""
import os
import socket
import subprocess
import sys
import threading
import time


def timed_out(process, step):
    # Reap the child before returning so it cannot retain the captured output pipes.
    process.kill()
    process.wait()
    print(f"registration launcher: timed out waiting for {step}", file=sys.stderr, flush=True)
    sys.exit(124)


env = dict(os.environ)
env["SUBC_DAEMON_PID"] = str(os.getpid())
timeout_seconds = 8
if sys.argv[1].startswith("--timeout-seconds="):
    timeout_seconds = float(sys.argv.pop(1).split("=", 1)[1])
mode = sys.argv[4]
fallback = mode.startswith("fallback-")
deadline = None
if os.name == "nt":
    if fallback:
        env["SUBC_LAUNCH_NONCE"] = "inherited-nonce"
        env["SUBC_REGISTRATION_PIPE"] = "\\\\.\\pipe\\subc-test-absent"
        sys.exit(subprocess.call(sys.argv[2:], env=env))
    import ctypes
    import uuid
    from ctypes import wintypes

    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateNamedPipeW.restype = wintypes.HANDLE
    kernel.CreateNamedPipeW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                                       wintypes.DWORD, wintypes.DWORD, wintypes.DWORD, ctypes.c_void_p]
    kernel.ConnectNamedPipe.argtypes = [wintypes.HANDLE, ctypes.c_void_p]
    kernel.GetNamedPipeClientProcessId.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.ULONG)]
    kernel.ReadFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD, ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
    kernel.WriteFile.argtypes = kernel.ReadFile.argtypes
    kernel.DisconnectNamedPipe.argtypes = [wintypes.HANDLE]
    name = "\\\\.\\pipe\\subc-sdk-test-" + uuid.uuid4().hex
    # Nonblocking server I/O avoids a synchronous ReadFile serializing the
    # opposite-direction writer on the same duplex handle. The SDK uses asynchronous pipe I/O.
    handle = kernel.CreateNamedPipeW(name, 3 | 0x00080000, 1, 1, 65536, 65536, 0, None)
    assert handle != ctypes.c_void_p(-1).value
    env["SUBC_REGISTRATION_PIPE"] = name
    # The child may never open the pipe. Include connection setup in the
    # child budget so the outer test doesn't have to kill the launcher.
    deadline = time.monotonic() + timeout_seconds
    process = subprocess.Popen(sys.argv[2:], env=env)
    while not kernel.ConnectNamedPipe(handle, None):
        error = ctypes.get_last_error()
        if error == 535:
            break
        code = process.poll()
        if code is not None:
            print(f"registration launcher: child exited ({code}) before connecting to the named pipe", file=sys.stderr)
            sys.exit(code or 1)
        assert error == 536
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            timed_out(process, "the child to connect to the named pipe")
        time.sleep(min(0.005, remaining))
    client = wintypes.ULONG()
    assert kernel.GetNamedPipeClientProcessId(handle, ctypes.byref(client)) and client.value == process.pid

    class Pipe:
        def recv(self, size):
            buffer = ctypes.create_string_buffer(size)
            read = wintypes.DWORD()
            while not kernel.ReadFile(handle, buffer, size, ctypes.byref(read), None):
                if ctypes.get_last_error() != 232:
                    return b""
                time.sleep(0.005)
            return buffer.raw[:read.value]

        def sendall(self, data):
            written = wintypes.DWORD()
            if not kernel.WriteFile(handle, data, len(data), ctypes.byref(written), None):
                raise OSError("pipe write failed")
            assert written.value == len(data)

        def shutdown(self, _):
            kernel.DisconnectNamedPipe(handle)

    parent = Pipe()
else:
    parent, child = socket.socketpair()
    env["SUBC_REGISTRATION_FD"] = f"{child.fileno()}:{os.fstat(child.fileno()).st_ino}"
    args = sys.argv[2:]
    inherited = [child.fileno()]
    nonce_read = None
    retirement = None
    if fallback:
        nonce_read, nonce_write = os.pipe()
        os.write(nonce_write, b"inherited-nonce")
        os.close(nonce_write)
        env["SUBC_LAUNCH_NONCE_FD"] = f"{nonce_read}:{os.fstat(nonce_read).st_ino}"
        inherited.append(nonce_read)
        if mode in ("fallback-refused", "fallback-explicit", "fallback-consumer"):
            env["SUBC_REGISTRATION_FD"] = "999:1"
        if mode in ("fallback-leaked", "fallback-leaked-no-socket-wait"):
            # The old provider deliberately leaves slot 4 inheritable, then
            # starts the new SDK only after nonce registration retired it.
            assert child.fileno() == 4
            args[1] = os.path.join(os.path.dirname(args[1]), "registration-legacy-parent.ts")
            retirement = os.pipe()
            inherited.append(retirement[0])
            env["TEST_REGISTRATION_RETIRED_FD"] = str(retirement[0])
            if mode == "fallback-leaked-no-socket-wait":
                trap = os.path.join(os.path.dirname(args[1]), "registration-no-socket-read.ts")
                args[1:1] = ["--preload", trap]
    process = subprocess.Popen(args, env=env, pass_fds=inherited)
    child.close()
    if nonce_read is not None:
        os.close(nonce_read)
        if retirement is not None:
            retired_read, retired_write = retirement
            os.close(retired_read)
            parent.settimeout(8)
            assert parent.recv(10) == b"REGISTERED"
            parent.shutdown(socket.SHUT_RDWR)
            os.write(retired_write, b"\x01")
            os.close(retired_write)
        try:
            sys.exit(process.wait(timeout=8))
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            sys.exit(124)
remote = socket.create_connection(("127.0.0.1", int(sys.argv[1])))
if mode.startswith("providers-"):
    # The peer records source ports so tests can distinguish the inherited
    # channel's proxy connection from providers connecting directly over TCP.
    print("REGISTRATION_PORT=" + str(remote.getsockname()[1]), flush=True)


def copy(source, target):
    try:
        while data := source.recv(65536):
            target.sendall(data)
    except OSError:
        pass
    finally:
        try:
            target.shutdown(socket.SHUT_WR)
        except OSError:
            pass


for source, target in [(parent, remote), (remote, parent)]:
    threading.Thread(target=copy, args=(source, target), daemon=True).start()
try:
    sys.exit(process.wait(timeout=timeout_seconds if deadline is None else max(0, deadline - time.monotonic())))
except subprocess.TimeoutExpired:
    timed_out(process, "the child to exit")
