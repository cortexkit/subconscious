import { dlopen, ptr } from "bun:ffi";

export function openWindowsPipe(name: string, daemonPid: string | undefined): { read(bytes: Buffer): number; close(): void } {
  if (!/^\\\\\.\\pipe\\subc-launch-[0-9a-f]{32}$/.test(name) || !/^[1-9]\d*$/.test(daemonPid ?? "")) {
    throw new Error("script_pipe_refused: invalid pipe name or daemon PID");
  }
  const api = dlopen("kernel32.dll", {
    CreateFileW: { args: ["ptr", "u32", "u32", "ptr", "u32", "u32", "ptr"], returns: "ptr" },
    GetFileType: { args: ["ptr"], returns: "u32" },
    GetNamedPipeServerProcessId: { args: ["ptr", "ptr"], returns: "bool" },
    ReadFile: { args: ["ptr", "ptr", "u32", "ptr", "ptr"], returns: "bool" },
    GetLastError: { args: [], returns: "u32" },
    CloseHandle: { args: ["ptr"], returns: "bool" },
  });
  const wide = Buffer.from(`${name}\0`, "utf16le");
  // Identification lets the server check identity without impersonating the client.
  const handle = api.symbols.CreateFileW(ptr(wide), 0x80000000, 0, null, 3, 0x110000, null);
  if (!handle || Number(handle) < 0 || Number(handle) > Number.MAX_SAFE_INTEGER) {
    api.close();
    throw new Error("script_pipe_refused: could not open pipe");
  }
  const close = () => { api.symbols.CloseHandle(handle); api.close(); };
  const serverPid = new Uint32Array(1);
  if (api.symbols.GetFileType(handle) !== 3 ||
      !api.symbols.GetNamedPipeServerProcessId(handle, ptr(serverPid)) ||
      BigInt(serverPid[0]) !== BigInt(daemonPid!)) {
    close();
    throw new Error("script_pipe_refused: wrong pipe server");
  }
  return {
    read(bytes) {
      const count = new Uint32Array(1);
      if (!api.symbols.ReadFile(handle, ptr(bytes), bytes.length, ptr(count), null)) {
        if (api.symbols.GetLastError() === 109) return 0;
        throw new Error("script_pipe_refused: pipe read failed");
      }
      return count[0];
    },
    close,
  };
}
