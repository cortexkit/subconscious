import { describe, expect, test } from "bun:test";
import { Socket, createServer } from "node:net";
import {
  LaunchNonceCell, LaunchNonceError, readLaunchNoncePipe,
  launchNonce, launchNonceAsync, launchNonceOrUndefined, resetLaunchNonceForTests,
  SUBC_LAUNCH_NONCE_ENV, SUBC_LAUNCH_NONCE_PIPE_ENV,
} from "../src/launch-nonce.js";

function fakeSocket(run: (socket: Socket) => void): Socket {
  const socket = new Socket();
  queueMicrotask(() => run(socket));
  return socket;
}

function succeeds(socket: Socket): void {
  socket.emit("connect");
  socket.emit("data", Buffer.from("pipe-secret"));
  socket.emit("end");
}

function errorKind(error: unknown): string | undefined {
  return error instanceof LaunchNonceError ? error.kind : undefined;
}

describe("Windows launch nonce pipe reader", () => {
  test("busy_pipe_retries_then_reads_to_eof", async () => {
    let connects = 0;
    const nonce = await readLaunchNoncePipe("pipe", () => {
      connects += 1;
      return fakeSocket((socket) => {
        if (connects === 1) socket.emit("error", Object.assign(new Error("busy"), { code: "EBUSY" }));
        else succeeds(socket);
      });
    });
    expect(connects).toBe(2);
    expect(nonce.value).toBe("pipe-secret");
    expect(nonce.source).toBe("pipe");
    expect(JSON.stringify(nonce)).not.toContain("pipe-secret");
  });

  test("busy_pipe_retry_has_a_bounded_window", async () => {
    let connects = 0;
    const result = readLaunchNoncePipe("pipe", () => {
      connects += 1;
      return fakeSocket((socket) => socket.emit("error", Object.assign(new Error("busy"), { code: "EBUSY" })));
    }, 30);
    expect(await result.catch(errorKind)).toBe("PipeNotOpen");
    expect(connects).toBeGreaterThan(1);
    expect(connects).toBeLessThan(10);
  });

  test("gone_pipe_never_falls_back_and_caches_error", async () => {
    let reads = 0;
    const cell = new LaunchNonceCell((key) => {
      if (key === SUBC_LAUNCH_NONCE_PIPE_ENV) return "gone";
      if (key === SUBC_LAUNCH_NONCE_ENV) throw new Error("environment fallback must not be read");
      return undefined;
    }, "win32", undefined, async (name) => {
      reads += 1;
      return readLaunchNoncePipe(name, () => fakeSocket((socket) => socket.emit("error", Object.assign(new Error("gone"), { code: "ENOENT" }))));
    });
    expect(await cell.getAsync().catch(errorKind)).toBe("PipeNotOpen");
    expect(await cell.getAsync().catch(errorKind)).toBe("PipeNotOpen");
    expect(() => cell.get()).toThrow(LaunchNonceError);
    expect(reads).toBe(1);
  });

  test("sync_pipe_accessor_requires_async_then_returns_shared_cached_nonce", async () => {
    let reads = 0;
    const cell = new LaunchNonceCell((key) => key === SUBC_LAUNCH_NONCE_PIPE_ENV ? "pipe" : "env-copy", "win32", undefined,
      async (name) => {
        reads += 1;
        return readLaunchNoncePipe(name, () => fakeSocket(succeeds));
      });
    try { cell.get(); throw new Error("sync call must throw"); }
    catch (error) { expect(errorKind(error)).toBe("PipeNeedsAsync"); }
    const [first, concurrent] = await Promise.all([cell.getAsync(), cell.getAsync()]);
    expect(first?.source).toBe("pipe");
    expect(first?.value).toBe("pipe-secret");
    expect(concurrent).toBe(first);
    expect(cell.get()).toBe(first);
    expect(await cell.getAsync()).toBe(first);
    expect(reads).toBe(1);
  });

  test("pipe_read_requires_eof_and_times_out_without_caching_partial_bytes", async () => {
    const result = readLaunchNoncePipe("pipe", () => fakeSocket((socket) => {
      socket.emit("connect");
      socket.emit("data", Buffer.from("partial-secret"));
    }), 100, 20);
    expect(await result.catch(errorKind)).toBe("PipeUnreadable");
  });

  test("empty_pipe_and_invalid_utf8_are_named_errors", async () => {
    expect(await readLaunchNoncePipe("pipe", () => fakeSocket((socket) => {
      socket.emit("connect"); socket.emit("end");
    })).catch(errorKind)).toBe("PipeEmpty");
    expect(await readLaunchNoncePipe("pipe", () => fakeSocket((socket) => {
      socket.emit("connect"); socket.emit("data", Buffer.from([0xff])); socket.emit("end");
    })).catch(errorKind)).toBe("PipeNotUtf8");
  });

  test("async_accessor_preserves_environment_rules_when_no_pipe_is_named", async () => {
    const cell = new LaunchNonceCell((key) => key === SUBC_LAUNCH_NONCE_ENV ? "env-secret" : undefined, "linux");
    expect(await cell.getAsync()).toBe(cell.get());
    expect(cell.get()?.source).toBe("env");
  });

  test.skipIf(process.platform !== "win32")("windows_net_connector_reads_real_named_pipe", async () => {
    const name = `\\\\.\\pipe\\subc-client-test-${process.pid}-${crypto.randomUUID()}`;
    const server = createServer((socket) => socket.end("windows-secret"));
    await new Promise<void>((resolve, reject) => { server.once("error", reject); server.listen(name, resolve); });
    const savedPipe = process.env[SUBC_LAUNCH_NONCE_PIPE_ENV];
    const savedEnv = process.env[SUBC_LAUNCH_NONCE_ENV];
    try {
      process.env[SUBC_LAUNCH_NONCE_PIPE_ENV] = name;
      process.env[SUBC_LAUNCH_NONCE_ENV] = "not-the-pipe-secret";
      resetLaunchNonceForTests();
      try { launchNonce(); throw new Error("must require async initialization"); }
      catch (error) { expect(errorKind(error)).toBe("PipeNeedsAsync"); }
      const nonce = await launchNonceAsync();
      expect(nonce?.value).toBe("windows-secret");
      expect(nonce?.source).toBe("pipe");
      expect(launchNonce()).toBe(nonce);
      expect(launchNonceOrUndefined()).toBe(nonce);
      expect(await launchNonceAsync()).toBe(nonce);
      expect(process.env[SUBC_LAUNCH_NONCE_ENV]).toBe("not-the-pipe-secret");
    } finally {
      if (savedPipe === undefined) delete process.env[SUBC_LAUNCH_NONCE_PIPE_ENV];
      else process.env[SUBC_LAUNCH_NONCE_PIPE_ENV] = savedPipe;
      if (savedEnv === undefined) delete process.env[SUBC_LAUNCH_NONCE_ENV];
      else process.env[SUBC_LAUNCH_NONCE_ENV] = savedEnv;
      resetLaunchNonceForTests();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    }
  });
});
