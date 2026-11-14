import { closeSync, fstatSync, readSync } from "node:fs";
import { createHash } from "node:crypto";

export const MAX_SCRIPT_BYTES = 32 * 1024 * 1024;

export async function readScript(): Promise<string> {
  const value = process.env.SUBC_SCRIPT_FD;
  if (!value) throw new Error("script_pipe_refused: SUBC_SCRIPT_FD is required");
  let pipe: { read(bytes: Buffer): number; close(): void };
  if (process.platform === "win32") {
    const { openWindowsPipe } = await import("./windows-pipe.ts");
    pipe = openWindowsPipe(value, process.env.SUBC_DAEMON_PID);
  } else {
    const match = /^(\d+):(\d+)$/.exec(value);
    if (!match || Number(match[1]) !== 6) throw new Error("script_pipe_refused: invalid descriptor identity");
    const fd = Number(match[1]);
    const stat = fstatSync(fd, { bigint: true });
    // Do not read or close a descriptor belonging to Bun's own IPC or other code.
    if (!stat.isFIFO() || BigInt.asUintN(64, stat.ino) !== BigInt(match[2])) throw new Error("script_pipe_refused: wrong pipe identity");
    pipe = { read: bytes => readSync(fd, bytes), close: () => closeSync(fd) };
  }
  delete process.env.SUBC_SCRIPT_FD;
  const exact = (length: number): Buffer => {
    const bytes = Buffer.alloc(length);
    let offset = 0;
    while (offset < length) {
      const count = pipe.read(bytes.subarray(offset));
      if (!count) throw new Error("script_pipe_refused: short pipe");
      offset += count;
    }
    return bytes;
  };
  try {
    const header = exact(52);
    if (!header.subarray(0, 8).equals(Buffer.from("CKSCRIPT")) || header.readUInt32LE(8) !== 1) {
      throw new Error("script_pipe_refused: bad magic or version");
    }
    const length = header.readBigUInt64LE(12);
    if (length > BigInt(MAX_SCRIPT_BYTES)) throw new Error("script_pipe_refused: script exceeds 32 MiB");
    const bytes = exact(Number(length));
    if (pipe.read(Buffer.alloc(1))) throw new Error("script_pipe_refused: extra byte");
    if (!createHash("sha256").update(bytes).digest().equals(header.subarray(20))) {
      throw new Error("script_pipe_refused: digest mismatch");
    }
    return new TextDecoder("utf-8", { fatal: true }).decode(bytes);
  } finally { pipe.close(); }
}
