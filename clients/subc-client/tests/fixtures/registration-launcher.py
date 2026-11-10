"""Give the SDK a real inherited socketpair, proxying its peer to the test server."""
import os
import socket
import subprocess
import sys
import threading
import time

env = dict(os.environ)
env["SUBC_DAEMON_PID"] = str(os.getpid())
if os.name == "nt":
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
    process = subprocess.Popen(sys.argv[2:], env=env)
    while not kernel.ConnectNamedPipe(handle, None):
        error = ctypes.get_last_error()
        if error == 535:
            break
        assert error == 536
        time.sleep(0.005)
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
    process = subprocess.Popen(sys.argv[2:], env=env, pass_fds=(child.fileno(),))
    child.close()
remote = socket.create_connection(("127.0.0.1", int(sys.argv[1])))


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
    sys.exit(process.wait(timeout=8))
except subprocess.TimeoutExpired:
    # A negative-control child must not outlive its test launcher or retain the
    # captured output pipes after the outer test kills the launcher.
    process.kill()
    process.wait()
    sys.exit(124)
