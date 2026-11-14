"""Launch the compiled runner with the daemon's binary script-pipe format."""
import ctypes
import json
import os
import secrets
import subprocess
import sys
import tempfile
import threading

executable, scenario = sys.argv[1:]
payload = sys.stdin.buffer.read()
env = dict(os.environ)
for name in list(env):
    if name.startswith("SUBC_"):
        del env[name]
write_errors = []

if os.name != "nt":
    reader, writer = os.pipe()
    if reader != 6:
        os.dup2(reader, 6)
        os.close(reader)
    inode = os.fstat(6).st_ino
    if scenario == "not-pipe":
        replacement = tempfile.TemporaryFile(dir=os.path.dirname(executable))
        replacement.write(payload)
        replacement.seek(0)
        os.dup2(replacement.fileno(), 6)
        replacement.close()
        inode = os.fstat(6).st_ino
    env["SUBC_SCRIPT_FD"] = f"6:{inode + (scenario == 'wrong-identity')}"
    child = subprocess.Popen([executable], env=env, pass_fds=(6,),
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    os.close(6)

    def deliver():
        try:
            with os.fdopen(writer, "wb") as stream:
                stream.write(payload)
        except BrokenPipeError:
            pass
else:
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.CreateNamedPipeW.restype = wintypes.HANDLE
    kernel.CreateNamedPipeW.argtypes = [wintypes.LPCWSTR, wintypes.DWORD, wintypes.DWORD,
                                       wintypes.DWORD, wintypes.DWORD, wintypes.DWORD,
                                       wintypes.DWORD, ctypes.c_void_p]
    kernel.ConnectNamedPipe.argtypes = [wintypes.HANDLE, ctypes.c_void_p]
    kernel.WriteFile.argtypes = [wintypes.HANDLE, ctypes.c_void_p, wintypes.DWORD,
                                ctypes.POINTER(wintypes.DWORD), ctypes.c_void_p]
    kernel.GetNamedPipeClientProcessId.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.ULONG)]
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.FlushFileBuffers.argtypes = [wintypes.HANDLE]
    name = rf"\\.\pipe\subc-launch-{secrets.token_hex(16)}"
    if scenario == "wrong-name":
        name = rf"\\.\pipe\other-{secrets.token_hex(16)}"
    handle = kernel.CreateNamedPipeW(name, 2, 0, 1, 65536, 65536, 0, None)
    if handle == wintypes.HANDLE(-1).value:
        raise ctypes.WinError(ctypes.get_last_error())
    env["SUBC_SCRIPT_FD"] = name
    env["SUBC_DAEMON_PID"] = str(os.getpid() + (scenario == "wrong-identity"))
    child = subprocess.Popen([executable], env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)

    def deliver():
        try:
            if not kernel.ConnectNamedPipe(handle, None) and ctypes.get_last_error() != 535:
                raise ctypes.WinError(ctypes.get_last_error())
            client_pid = wintypes.ULONG()
            if not kernel.GetNamedPipeClientProcessId(handle, ctypes.byref(client_pid)) or client_pid.value != child.pid:
                raise RuntimeError("wrong pipe client PID")
            count = wintypes.DWORD()
            buffer = ctypes.create_string_buffer(payload)
            if not kernel.WriteFile(handle, buffer, len(payload), ctypes.byref(count), None):
                if ctypes.get_last_error() not in (109, 232):
                    raise ctypes.WinError(ctypes.get_last_error())
            else:
                kernel.FlushFileBuffers(handle)
        except Exception as error:
            write_errors.append(str(error))
        finally:
            kernel.CloseHandle(handle)

thread = threading.Thread(target=deliver, daemon=True)
thread.start()
try:
    stdout, stderr = child.communicate(timeout=10)
finally:
    if child.poll() is None:
        child.kill()
        child.wait()
thread.join(timeout=2)
print(json.dumps(dict(code=child.returncode, out=stdout.decode(), err=stderr.decode(), write_errors=write_errors)))
