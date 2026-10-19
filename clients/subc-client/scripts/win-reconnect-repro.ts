// Windows Bun reconnect diagnostic. No library or existing test is modified.
// Managed fixture source: tests/managed-client.test.ts:1586-1610, 1809-1815,
// 1855-2003 at 8766895e (copied into the two sibling helper files).
//
// bun scripts/win-reconnect-repro.ts same-process [--iterations 200]
// bun scripts/win-reconnect-repro.ts split [--iterations 200]
// bun scripts/win-reconnect-repro.ts raw [same-process|split] [--iterations 200]
// Optional: --result PATH (JSON), --timeout-ms 120000 (whole experiment).
//
// An observer owns no workload sockets. It runs one combined child, or separate
// client/server children, so it can report both exits even if either Bun crashes.
// The raw path never imports subc-client: only node:net and the copied I/O helper.
import assert from "node:assert/strict";
import { connect } from "node:net";
import { mkdirSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { SocketReader, startServer, writeAll, type ReproServer, type Stats } from "./win-reconnect-transport.js";

type Role = "combined" | "client" | "server";
type Topology = "same-process" | "split";
interface Options {
  kind: "managed" | "raw";
  topology: Topology;
  iterations: number;
  timeoutMs: number;
  result?: string;
  role?: Role;
  port?: number;
}
interface ChildResult {
  role: Role;
  pid: number;
  exitCode: number | null;
  signal: string | null;
  forced: boolean;
  error?: string;
  stats?: Stats;
}

function optionsFrom(args: string[]): Options {
  const mode = args.shift();
  if (mode !== "same-process" && mode !== "split" && mode !== "raw") {
    throw new Error("usage: win-reconnect-repro.ts same-process|split|raw [same-process|split] [--iterations N] [--result PATH] [--timeout-ms N]");
  }
  let topology: Topology = mode === "split" ? "split" : "same-process";
  if (mode === "raw" && (args[0] === "split" || args[0] === "same-process")) topology = args.shift() as Topology;
  const opts: Options = { kind: mode === "raw" ? "raw" : "managed", topology, iterations: 200, timeoutMs: 120_000 };
  while (args.length) {
    const flag = args.shift();
    const value = args.shift();
    if (value === undefined) throw new Error(`missing value for ${flag}`);
    switch (flag) {
      case "--iterations": opts.iterations = positiveInt(value); break;
      case "--timeout-ms": opts.timeoutMs = positiveInt(value); break;
      case "--result": opts.result = value; break;
      case "--port": opts.port = positiveInt(value); break;
      case "--role":
        if (value !== "combined" && value !== "client" && value !== "server") throw new Error("invalid internal role");
        opts.role = value;
        break;
      default: throw new Error(`unknown option ${flag}`);
    }
  }
  return opts;
}

function positiveInt(value: string): number {
  const n = Number(value);
  if (!Number.isSafeInteger(n) || n <= 0) throw new Error(`expected a positive integer, got ${value}`);
  return n;
}

async function startWorkloadServer(opts: Options): Promise<ReproServer> {
  if (opts.kind === "managed") return (await import("./win-reconnect-managed.js")).startManagedServer();
  return startServer(async (socket, stats) => {
    const reader = new SocketReader(socket);
    const deadline = Date.now() + 5_000;
    const bytes = await reader.readExact(4, deadline);
    stats.dataRequests += 1;
    await writeAll(socket, bytes, deadline);
    // Match the managed fake's close primitive and callback-write timing, but
    // remove authentication, framing, route caching, and all client code.
    socket.destroy();
  });
}

async function rawExchange(port: number, value: number): Promise<void> {
  const socket = connect({ host: "127.0.0.1", port });
  socket.setNoDelay(true); // Same connect API and option as src/socket.ts:136-153.
  const reader = new SocketReader(socket);
  const closed = new Promise<void>((resolve) => socket.once("close", () => resolve()));
  const deadline = Date.now() + 5_000;
  const bytes = Buffer.alloc(4);
  bytes.writeUInt32LE(value);
  try {
    await writeAll(socket, bytes, deadline);
    assert.deepEqual(Buffer.from(await reader.readExact(4, deadline)), bytes);
    let timer: ReturnType<typeof setTimeout> | undefined;
    try {
      await Promise.race([closed, new Promise<never>((_, reject) => {
        timer = setTimeout(() => reject(new Error("raw client did not observe server close")), 5_000);
      })]);
    } finally {
      clearTimeout(timer);
    }
  } finally {
    socket.destroy();
  }
}

async function runClient(opts: Options, port: number): Promise<void> {
  const progress = (n: number) => console.log(`iteration=${n}/${opts.iterations} kind=${opts.kind} topology=${opts.topology} client_pid=${process.pid} calls=${2 * n} reconnects=${2 * n - 1}`);
  if (opts.kind === "managed") {
    await (await import("./win-reconnect-managed.js")).runManagedClient(port, opts.iterations, progress);
  } else {
    for (let i = 1; i <= opts.iterations; i++) {
      await rawExchange(port, 2 * i - 1);
      await rawExchange(port, 2 * i);
      progress(i);
    }
  }
}

// Run the same workload in the caller's runtime, without spawning an observer or
// worker. The bun:test diagnostic must exercise sockets inside the test runner.
export async function runSameProcess(kind: Options["kind"], iterations: number): Promise<Stats> {
  const opts: Options = { kind, topology: "same-process", iterations, timeoutMs: 120_000 };
  const server = await startWorkloadServer(opts);
  try {
    await runClient(opts, server.port);
  } finally {
    await server.stop();
  }
  return server.stats;
}

async function runWorker(opts: Options): Promise<void> {
  let server: ReproServer | undefined;
  try {
    if (opts.role === "client") {
      assert.ok(opts.port, "client requires a port");
      await runClient(opts, opts.port);
    } else {
      server = await startWorkloadServer(opts);
      if (opts.role === "combined") {
        await runClient(opts, server.port);
      } else {
        await new Promise<void>((resolve) => {
          process.on("message", (message: unknown) => {
            if ((message as { type?: string })?.type === "stop") resolve();
          });
          process.once("disconnect", resolve);
          process.send?.({ type: "ready", port: server!.port });
        });
      }
    }
  } finally {
    if (server) {
      await server.stop();
      process.send?.({ type: "stats", stats: server.stats });
    }
  }
}

function spawnWorker(opts: Options, role: Role, port?: number) {
  let resolveReady: (port: number) => void = () => {};
  const ready = new Promise<number>((resolve) => { resolveReady = resolve; });
  let failure: string | undefined;
  let stats: Stats | undefined;
  let forced = false;
  let finished = false;
  const args = [opts.kind === "raw" ? "raw" : opts.topology];
  if (opts.kind === "raw") args.push(opts.topology);
  args.push("--iterations", String(opts.iterations), "--role", role);
  if (port !== undefined) args.push("--port", String(port));
  const child = Bun.spawn([process.execPath, import.meta.path, ...args], {
    stdin: "ignore", stdout: "inherit", stderr: "inherit",
    ipc(message: unknown) {
      const event = message as { type?: string; port?: number; error?: string; stats?: Stats };
      if (event?.type === "ready" && typeof event.port === "number") resolveReady(event.port);
      if (event?.type === "failure") failure = event.error;
      if (event?.type === "stats") stats = event.stats;
    },
  });
  console.log(`process role=${role} pid=${child.pid} bun=${Bun.version}`);
  const exited = child.exited.then((exitCode): ChildResult => {
    finished = true;
    const result = { role, pid: child.pid, exitCode, signal: child.signalCode ?? null, forced, error: failure, stats };
    console.log(`exit ${JSON.stringify(result)}`);
    return result;
  });
  return {
    role, ready, exited,
    stop() { if (!finished) child.send({ type: "stop" }); },
    kill() { if (!finished) { forced = true; child.kill(); } },
  };
}

async function observe(opts: Options): Promise<void> {
  const children: ReturnType<typeof spawnWorker>[] = [];
  let timedOut = false;
  let failure: string | undefined;
  const timer = setTimeout(() => {
    timedOut = true;
    for (const child of children) child.kill();
  }, opts.timeoutMs);
  try {
    if (opts.topology === "same-process") {
      const combined = spawnWorker(opts, "combined");
      children.push(combined);
      await combined.exited;
    } else {
      const server = spawnWorker(opts, "server");
      children.push(server);
      const port = await Promise.race([server.ready, server.exited.then(() => {
        throw new Error("server exited before reporting its port");
      })]);
      const client = spawnWorker(opts, "client", port);
      children.push(client);
      const first = await Promise.race([server.exited, client.exited]);
      if (first.role === "server") {
        failure = "server exited while client was running";
        client.kill();
      } else {
        server.stop();
      }
      await Promise.all(children.map((child) => child.exited));
    }
  } catch (error) {
    failure = String(error);
  } finally {
    for (const child of children) child.kill();
    await Promise.all(children.map((child) => child.exited));
    clearTimeout(timer);
  }
  const processes = await Promise.all(children.map((child) => child.exited));
  const stats = processes.find((child) => child.stats)?.stats;
  // Count actual accepts and route opens, not just successful echo replies: a
  // run that accidentally reuses a socket must not masquerade as a reconnect run.
  if (!failure && !timedOut && processes.every((child) => child.exitCode === 0)) {
    try {
      assert.ok(stats, "server did not report counters");
      assert.equal(stats.connections, 2 * opts.iterations, "connection count");
      assert.equal(stats.dataRequests, 2 * opts.iterations, "data request count");
      assert.equal(stats.routeOpens, opts.kind === "managed" ? 2 * opts.iterations : 0, "route.open count");
    } catch (error) {
      failure = String(error);
    }
  }
  const crashed = processes.some((child) => !child.forced && !child.error && (child.exitCode !== 0 || child.signal !== null));
  const status = timedOut ? "timeout" : crashed ? "crash" : failure || processes.some((child) => child.exitCode !== 0 || child.error) ? "failure" : "pass";
  const result = { mode: opts.kind === "raw" ? `raw-${opts.topology}` : opts.topology, bun: Bun.version, iterations: opts.iterations, status, failure, processes };
  console.log(`RESULT ${JSON.stringify(result)}`);
  if (opts.result) {
    mkdirSync(dirname(opts.result), { recursive: true });
    writeFileSync(opts.result, JSON.stringify(result, null, 2));
  }
  process.exitCode = status === "pass" ? 0 : 1;
}

if (import.meta.main) {
  try {
    const opts = optionsFrom(process.argv.slice(2));
    if (opts.role) {
      try {
        await runWorker(opts);
      } catch (error) {
        process.send?.({ type: "failure", error: String(error) });
        throw error;
      } finally {
        process.disconnect?.();
      }
    } else {
      await observe(opts);
    }
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
  }
}
