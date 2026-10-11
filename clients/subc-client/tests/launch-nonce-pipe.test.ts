import { describe, expect, test } from "bun:test";
import { Socket, createServer } from "node:net";
import {
  LaunchNonceCell, LaunchNonceError, readLaunchNoncePipe,
  launchNonce, launchNonceAsync, launchNonceOrUndefined, resetLaunchNonceForTests,
  SUBC_LAUNCH_NONCE_ENV, SUBC_LAUNCH_NONCE_PIPE_ENV,
  SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV,
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

  test("phase_one_failed_pipe_uses_and_caches_the_explicit_environment_copy", async () => {
    let reads = 0;
    let sealed = false;
    const env: Record<string, string> = {
      [SUBC_LAUNCH_NONCE_PIPE_ENV]: "gone",
      [SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV]: "env",
      [SUBC_LAUNCH_NONCE_ENV]: "phase-one-secret",
    };
    const cell = new LaunchNonceCell((key) => {
      if (sealed) throw new Error("must not read environment after caching");
      return env[key];
    }, "win32", undefined, async () => {
      reads += 1;
      throw new LaunchNonceError("PipeNotOpen", "gone pipe");
    });
    const nonce = await cell.getAsync();
    sealed = true;
    expect(nonce?.value).toBe("phase-one-secret");
    expect(nonce?.source).toBe("env");
    expect(cell.get()).toBe(nonce);
    expect(await cell.getAsync()).toBe(nonce);
    expect(reads).toBe(1);
  });

  test("phase_one_matching_pipe_keeps_pipe_source_but_partial_bytes_use_env", async () => {
    const env: Record<string, string> = {
      [SUBC_LAUNCH_NONCE_PIPE_ENV]: "pipe",
      [SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV]: "env",
      [SUBC_LAUNCH_NONCE_ENV]: "pipe-secret",
    };
    const matching = new LaunchNonceCell((key) => env[key], "win32", undefined,
      async (name) => readLaunchNoncePipe(name, () => fakeSocket(succeeds)));
    expect((await matching.getAsync())?.source).toBe("pipe");
    const partial = new LaunchNonceCell((key) => env[key], "win32", undefined,
      async () => ({ value: "pipe-", source: "pipe" }));
    const nonce = await partial.getAsync();
    expect(nonce?.source).toBe("env");
    expect(nonce?.value).toBe("pipe-secret");
  });

  test("phase_one_sync_reader_retains_environment_startup_without_opening_pipe", async () => {
    const env: Record<string, string> = {
      [SUBC_LAUNCH_NONCE_PIPE_ENV]: "pipe",
      [SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV]: "env",
      [SUBC_LAUNCH_NONCE_ENV]: "phase-one-secret",
    };
    const cell = new LaunchNonceCell((key) => env[key], "win32", undefined,
      async () => { throw new Error("cached sync fallback must not open the pipe"); });
    const nonce = cell.get();
    expect(nonce?.source).toBe("env");
    expect(nonce?.value).toBe("phase-one-secret");
    expect(await cell.getAsync()).toBe(nonce);
  });

  test("phase_one_sync_fallback_winning_during_async_init_is_not_overwritten", async () => {
    const env: Record<string, string> = {
      [SUBC_LAUNCH_NONCE_PIPE_ENV]: "pipe",
      [SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV]: "env",
      [SUBC_LAUNCH_NONCE_ENV]: "phase-one-secret",
    };
    let finish: (() => void) | undefined;
    const cell = new LaunchNonceCell((key) => env[key], "win32", undefined,
      () => new Promise((resolve) => { finish = () => resolve({ value: "phase-one-secret", source: "pipe" }); }));
    const pending = cell.getAsync();
    const nonce = cell.get();
    expect(nonce?.source).toBe("env");
    finish!();
    expect(await pending).toBe(nonce);
    expect(cell.get()).toBe(nonce);
  });

  test("pipe_fallback_requires_the_exact_marker_and_a_nonempty_environment_copy", async () => {
    for (const [marker, copy] of [["true", "env-secret"], ["env", ""]]) {
      const env: Record<string, string | undefined> = {
        [SUBC_LAUNCH_NONCE_PIPE_ENV]: "gone",
        [SUBC_LAUNCH_NONCE_PIPE_FALLBACK_ENV]: marker,
        [SUBC_LAUNCH_NONCE_ENV]: copy,
      };
      const cell = new LaunchNonceCell((key) => env[key], "win32", undefined,
        async () => { throw new LaunchNonceError("PipeNotOpen", "gone pipe"); });
      expect(await cell.getAsync().catch(errorKind)).toBe("PipeNotOpen");
    }
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
