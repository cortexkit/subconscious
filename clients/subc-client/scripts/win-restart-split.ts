// Split-process copy of the two tests/managed-client.test.ts tests (at 8766895e)
// that exited abnormally most often on Windows Bun 1.4.3 when run alone:
//   key-rotation       "recovers across a daemon restart that rotates the key
//                       (stale-file auth race is transient)", lines 493-551
//   consumer-identity  "re-attaches the same consumer_identity when a later call
//                       opens after reconnect", lines 553-596
// In the tests, the fake daemon and the client share one Bun process. Here the
// fake daemon runs in its own Bun process (role server) and the managed client
// in another (role client), started either as a plain script or as a single
// bun:test test (scripts/win-restart-split-client.test.ts). A third process, the
// observer, owns no sockets: it starts both, relays their restart handshake over
// Bun IPC, and records each one's exit code, signal and Bun crash text.
//
//   bun scripts/win-restart-split.ts key-rotation|consumer-identity
//       [--client-runner run|test] [--result PATH] [--timeout-ms 60000]
//
// The sequence keeps the tests' order:
//   server  daemon 1 listens (original key; closes each connection right after
//           its first data response, closeAfterDataResponses: 1) and writes the
//           connection file; tells the observer "ready".
//   client  connects (reconnectBackoff 5/10/4 and an instant sleep hook, as the
//           tests pass), calls echo n=1, polls until its read loop has observed
//           the close (the tests' waitFor), then asks for "restart".
//   server  stops daemon 1 (destroys its sockets, closes the listener). Daemon 2
//           listens on a NEW port (listen(0), as the tests do), with the rotated
//           key for key-rotation or the same key for consumer-identity. The
//           server rewrites the connection file with the new port and the
//           ORIGINAL key, then answers "restarted".
//   client  key-rotation: opens a two-sleep stale window; when the window
//           closes, its sleep hook writes the rotated key into the connection
//           file, exactly as the test's sleep hook does (in the test process).
//           Calls echo n=2 (this call reconnects), checks it, then closes.
//   server  after the client process has exited, the observer sends "stop";
//           the server stops daemon 2, reports both daemons' counters, exits.
import assert from "node:assert/strict";
import { chmodSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import type { Socket } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { buildFlags, buildFrame, FrameType, Priority, SubcClient } from "../src/index.js";
import {
  authenticateFakeServer, BACKOFF, DAEMON_ID, IDENTITY, KEY, parseJson, readFrame,
  responseFrame, writeFrame,
} from "./win-reconnect-managed.js";
import { SocketReader, startServer, type Stats } from "./win-reconnect-transport.js";

export type Scenario = "key-rotation" | "consumer-identity";
type Runner = "run" | "test";
type Role = "server" | "client";
type Message = { type: string; [key: string]: unknown };

// Same values as the tests' rotatedKey (line 507) and consumerIdentity (line 561).
const ROTATED_KEY = Uint8Array.from(Array(32).fill(0x99));
const CONSUMER_IDENTITY = { module_id: "reserved-module", launch_nonce: "nonce-abc" };
const PACKAGE_DIR = dirname(import.meta.dir);
const CRASH_TEXT = /Bun has crashed|Segmentation fault|panic\([^)]*thread\)/;

const started = performance.now();
function log(role: string, message: string): void {
  console.log(`RESTARTDIAG t=${(performance.now() - started).toFixed(1)}ms role=${role} pid=${process.pid} ${message}`);
}

// Same layout and permissions as writeConnectionFile in
// tests/managed-client.test.ts (lines 2011-2025).
// The pid is the fake daemon's; the client does not check it.
function writeConnectionFile(path: string, port: number, key: Uint8Array, pid: number): void {
  writeFileSync(path, JSON.stringify({
    schema: 1,
    endpoints: [{ host: "127.0.0.1", port }],
    key: Array.from(key),
    daemon_id: Array.from(DAEMON_ID),
    pid,
    daemon_ver: "fake-subc",
  }), { mode: 0o600 });
  chmodSync(path, 0o600);
}

// Bun IPC with the observer. Messages are queued until a caller asks for them,
// so a reply that arrives before next() is called is not lost.
function ipcChannel() {
  const queue: Message[] = [];
  const waiters: { type: string; resolve: (m: Message) => void; reject: (e: Error) => void }[] = [];
  let disconnected = false;
  const onMessage = (raw: unknown) => {
    const message = raw as Message;
    const i = waiters.findIndex((w) => w.type === message?.type);
    if (i >= 0) waiters.splice(i, 1)[0]!.resolve(message);
    else queue.push(message);
  };
  const onDisconnect = () => {
    disconnected = true;
    for (const w of waiters.splice(0)) w.reject(new Error(`observer disconnected while waiting for ${w.type}`));
  };
  process.on("message", onMessage);
  process.once("disconnect", onDisconnect);
  return {
    send(message: Message): void {
      process.send?.(message);
    },
    next(type: string): Promise<Message> {
      const i = queue.findIndex((m) => m?.type === type);
      if (i >= 0) return Promise.resolve(queue.splice(i, 1)[0]!);
      if (disconnected) return Promise.reject(new Error(`observer disconnected before ${type}`));
      return new Promise((resolve, reject) => waiters.push({ type, resolve, reject }));
    },
    close(): void {
      process.off("message", onMessage);
      process.off("disconnect", onDisconnect);
      if (!disconnected) process.disconnect?.();
    },
  };
}

// ---------------------------------------------------------------------------
// Server role: the fake daemon, reduced to the arms these two tests reach in the
// test's handleFakeConnection (1616-1818): Ping, catalog.list, route.open (with
// its consumer_identity recorded), data echo, and closeAfterDataResponses.
// ---------------------------------------------------------------------------

interface DaemonCounters {
  label: string;
  port: number;
  key: "original" | "rotated";
  connections: number;
  closedConnections: number;
  dataRequests: number;
  routeOpens: number;
  consumerIdentities: unknown[];
  // Why each connection's handler ended, e.g. a stale-key client closing during
  // the handshake. Every handler ends with an error once its socket closes.
  handlerEnds: string[];
}

async function startFakeDaemon(label: string, key: Uint8Array, closeAfterDataResponses?: number) {
  const consumerIdentities: unknown[] = [];
  const handlerEnds: string[] = [];
  async function handle(socket: Socket, stats: Stats): Promise<void> {
    const reader = new SocketReader(socket);
    const deadline = Date.now() + 5_000;
    await authenticateFakeServer(reader, socket, deadline, key, DAEMON_ID);
    let routeChannel = 41;
    for (;;) {
      const frame = await readFrame(reader, deadline);
      if (frame.header.ty === FrameType.Ping && frame.header.channel === 0) {
        await writeFrame(socket, buildFrame(FrameType.Pong, buildFlags(false, Priority.Interactive, false), 0, 0, frame.header.corr, new Uint8Array()), deadline);
        continue;
      }
      if (frame.header.ty !== FrameType.Request) continue;
      if (frame.header.channel === 0) {
        const request = parseJson(frame.body) as { op?: string; consumer_identity?: unknown };
        if (request.op === "catalog.list") {
          await writeFrame(socket, responseFrame(frame, { op: "catalog.list", modules: [] }), deadline);
        } else if (request.op === "route.open") {
          stats.routeOpens += 1;
          consumerIdentities.push(request.consumer_identity);
          await writeFrame(socket, responseFrame(frame, { op: "route.open", route_channel: routeChannel++, route_epoch: 1 }), deadline);
        }
        continue;
      }
      stats.dataRequests += 1;
      const body = parseJson(frame.body);
      await writeFrame(socket, responseFrame(frame, body), deadline);
      if (closeAfterDataResponses !== undefined && stats.dataRequests >= closeAfterDataResponses) {
        socket.destroy();
        return;
      }
    }
  }
  const server = await startServer(async (socket, stats) => {
    log("server", `${label} accept connection=${stats.connections} remote_port=${socket.remotePort}`);
    try {
      await handle(socket, stats);
      handlerEnds.push("closed-after-data-response");
    } catch (error) {
      handlerEnds.push(error instanceof Error ? error.message : String(error));
      throw error;
    }
  });
  log("server", `${label} listening port=${server.port}`);
  return {
    port: server.port,
    stop: () => server.stop(),
    counters: (): DaemonCounters => ({
      label, port: server.port, key: key === ROTATED_KEY ? "rotated" : "original",
      ...server.stats, consumerIdentities, handlerEnds,
    }),
  };
}

async function runServer(scenario: Scenario, connFile: string): Promise<void> {
  const ipc = ipcChannel();
  const daemons: Awaited<ReturnType<typeof startFakeDaemon>>[] = [];
  try {
    const first = await startFakeDaemon("daemon-1", KEY, 1);
    daemons.push(first);
    writeConnectionFile(connFile, first.port, KEY, process.pid);
    ipc.send({ type: "ready", port: first.port });

    await ipc.next("restart");
    log("server", "restart: stopping daemon-1");
    await first.stop();
    log("server", "daemon-1 stopped");
    const second = await startFakeDaemon("daemon-2", scenario === "key-rotation" ? ROTATED_KEY : KEY);
    daemons.push(second);
    // key-rotation: the new port with the OLD key is the stale file the client
    // reads first (test line 537); consumer-identity: the normal rewrite (583).
    writeConnectionFile(connFile, second.port, KEY, process.pid);
    log("server", `connection file rewritten port=${second.port} key=original`);
    ipc.send({ type: "restarted", port: second.port, pid: process.pid });

    await ipc.next("stop");
    log("server", "stop: stopping daemon-2");
    await second.stop();
  } catch (error) {
    log("server", `failure ${error instanceof Error ? error.stack ?? error.message : String(error)}`);
    ipc.send({ type: "failure", error: error instanceof Error ? error.message : String(error) });
    throw error;
  } finally {
    for (const daemon of daemons) await daemon.stop();
    ipc.send({ type: "counters", daemons: daemons.map((daemon) => daemon.counters()) });
    log("server", "done");
    ipc.close();
  }
}

// ---------------------------------------------------------------------------
// Client role. `check` is how each call's result is asserted: the bun:test
// client passes the tests' `await expect(p).resolves.toEqual(...)`, the plain
// client a node:assert deepEqual of the awaited value.
// ---------------------------------------------------------------------------

export type CallCheck = (call: Promise<unknown>, expected: unknown) => Promise<void>;

// Same polling as waitFor in tests/managed-client.test.ts (lines 2038-2045).
async function waitFor(predicate: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`timed out waiting for ${label}`);
}

function clientClosedErr(client: SubcClient): Error | null {
  return (client as unknown as { closedErr: Error | null }).closedErr;
}

export async function runRestartClient(scenario: Scenario, connFile: string, runner: Runner, check: CallCheck): Promise<void> {
  const ipc = ipcChannel();
  log("client", `start scenario=${scenario} runner=${runner} bun=${Bun.version} platform=${process.platform}`);
  let client: SubcClient | undefined;
  try {
    let staleWindowRemaining = 0;
    let rotatedPort = 0;
    let daemonPid = 0;
    // key-rotation's sleep hook (test lines 513-521): each reconnect backoff
    // sleep consumes one stale-window tick; the last one writes the rotated key.
    const sleep = scenario === "key-rotation"
      ? async () => {
          if (staleWindowRemaining > 0) {
            staleWindowRemaining -= 1;
            log("client", `backoff-sleep staleWindowRemaining=${staleWindowRemaining}`);
            if (staleWindowRemaining === 0) {
              writeConnectionFile(connFile, rotatedPort, ROTATED_KEY, daemonPid);
              log("client", `connection file rewritten port=${rotatedPort} key=rotated`);
            }
          }
        }
      : async () => {};
    const callOptions = scenario === "consumer-identity" ? { consumerIdentity: CONSUMER_IDENTITY } : undefined;

    client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF, sleep });
    log("client", "connected");
    await check(client.call("managed-provider", "echo", { n: 1 }, callOptions), { method: "echo", params: { n: 1 } });
    log("client", "call n=1 resolved");
    const connected = client;
    await waitFor(() => clientClosedErr(connected) !== null, "client to observe first daemon close");
    log("client", `observed close: ${clientClosedErr(connected)?.message}`);

    ipc.send({ type: "restart" });
    const restarted = await ipc.next("restarted");
    rotatedPort = restarted.port as number;
    daemonPid = restarted.pid as number;
    log("client", `daemon restarted port=${rotatedPort}`);
    if (scenario === "key-rotation") staleWindowRemaining = 2;

    await check(client.call("managed-provider", "echo", { n: 2 }, callOptions), { method: "echo", params: { n: 2 } });
    log("client", "call n=2 resolved");
    if (scenario === "key-rotation") assert.equal(staleWindowRemaining, 0, "the stale window was not consumed");
    ipc.send({ type: "client-done" });
  } catch (error) {
    log("client", `failure ${error instanceof Error ? error.stack ?? error.message : String(error)}`);
    ipc.send({ type: "failure", error: error instanceof Error ? error.message : String(error) });
    throw error;
  } finally {
    client?.close();
    log("client", "closed");
    ipc.close();
  }
}

// ---------------------------------------------------------------------------
// Observer.
// ---------------------------------------------------------------------------

interface ProcessResult {
  role: Role;
  runner: Runner;
  pid: number;
  exitCode: number | null;
  signal: string | null;
  // The observer killed it (timeout, or cleanup after the other process died).
  forced: boolean;
  // A JS failure the process reported over IPC before exiting.
  error?: string;
  crashText: boolean;
  // The client finished the whole sequence before exiting.
  completed: boolean;
  lastLine?: string;
}

async function forwardLines(stream: ReadableStream<Uint8Array>, role: Role, seen: { crashText: boolean; lastLine?: string }) {
  const decoder = new TextDecoder();
  let pending = "";
  const emit = (line: string) => {
    if (CRASH_TEXT.test(line)) seen.crashText = true;
    if (line.trim()) seen.lastLine = line.slice(0, 300);
    console.log(`[${role}] ${line}`);
  };
  for await (const chunk of stream) {
    pending += decoder.decode(chunk, { stream: true });
    let newline: number;
    while ((newline = pending.indexOf("\n")) >= 0) {
      emit(pending.slice(0, newline).replace(/\r$/, ""));
      pending = pending.slice(newline + 1);
    }
  }
  pending += decoder.decode();
  if (pending) emit(pending);
}

function spawnChild(role: Role, runner: Runner, cmd: string[], env: Record<string, string | undefined>, onMessage: (m: Message) => void) {
  const seen: { crashText: boolean; lastLine?: string } = { crashText: false };
  let error: string | undefined;
  let completed = false;
  let forced = false;
  let finished = false;
  const child = Bun.spawn(cmd, {
    cwd: PACKAGE_DIR,
    env,
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
    ipc(raw: unknown) {
      const message = raw as Message;
      if (message?.type === "failure") error = String(message.error);
      if (message?.type === "client-done") completed = true;
      onMessage(message);
    },
  });
  log("observer", `spawned role=${role} runner=${runner} pid=${child.pid}: ${cmd.slice(1).join(" ")}`);
  const output = Promise.all([
    forwardLines(child.stdout, role, seen),
    forwardLines(child.stderr, role, seen),
  ]);
  const exited = Promise.all([child.exited, output]).then(([exitCode]): ProcessResult => {
    finished = true;
    const result: ProcessResult = {
      role, runner, pid: child.pid, exitCode, signal: child.signalCode ?? null, forced, error,
      crashText: seen.crashText, completed, lastLine: seen.lastLine,
    };
    log("observer", `exit ${JSON.stringify(result)}`);
    return result;
  });
  return {
    role,
    exited,
    send(message: Message) { if (!finished) child.send(message); },
    kill() { if (!finished) { forced = true; child.kill(); } },
  };
}

function abnormal(p: ProcessResult): boolean {
  if (p.forced) return false;
  if (p.crashText || p.signal !== null) return true;
  // A reported JS failure exits 1 (plain bun and bun test alike); any other
  // nonzero code, or 1 without a reported failure, is not a JS failure.
  return p.exitCode !== 0 && !(p.exitCode === 1 && p.error !== undefined);
}

interface Options { scenario: Scenario; runner: Runner; timeoutMs: number; result?: string; role?: Role; connFile?: string }

async function observe(opts: Options): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "subc-win-restart-"));
  const connFile = join(dir, "subc-connection.json");
  const children: ReturnType<typeof spawnChild>[] = [];
  let failure: string | undefined;
  let timedOut = false;
  let daemons: DaemonCounters[] | undefined;
  const timer = setTimeout(() => {
    timedOut = true;
    log("observer", "timeout: killing children");
    for (const child of children) child.kill();
  }, opts.timeoutMs);
  log("observer", `scenario=${opts.scenario} client-runner=${opts.runner} bun=${Bun.version} platform=${process.platform}`);
  try {
    let client: ReturnType<typeof spawnChild> | undefined;
    let resolveReady: () => void = () => {};
    const ready = new Promise<void>((resolve) => { resolveReady = resolve; });
    const server = spawnChild("server", "run", [process.execPath, import.meta.path, opts.scenario, "--role", "server", "--conn-file", connFile], process.env, (message) => {
      if (message?.type === "ready") resolveReady();
      if (message?.type === "restarted") client?.send(message);
      if (message?.type === "counters") daemons = message.daemons as DaemonCounters[];
    });
    children.push(server);
    await Promise.race([ready, server.exited.then(() => { throw new Error("server exited before it was ready"); })]);

    const clientCmd = opts.runner === "run"
      ? [process.execPath, import.meta.path, opts.scenario, "--role", "client", "--conn-file", connFile]
      : [process.execPath, "test", "./scripts/win-restart-split-client.test.ts"];
    client = spawnChild("client", opts.runner, clientCmd, {
      ...process.env,
      WIN_BUN_REPRO: "1",
      WIN_RESTART_SCENARIO: opts.scenario,
      WIN_RESTART_CONN_FILE: connFile,
    }, (message) => {
      if (message?.type === "restart") server.send(message);
    });
    children.push(client);

    const first = await Promise.race([server.exited, client.exited]);
    if (first.role === "server") {
      failure = "server exited while the client was running";
      client.kill();
    } else {
      server.send({ type: "stop" });
    }
    await Promise.all(children.map((child) => child.exited));
  } catch (error) {
    failure = error instanceof Error ? error.message : String(error);
  } finally {
    for (const child of children) child.kill();
    await Promise.all(children.map((child) => child.exited));
    clearTimeout(timer);
    rmSync(dir, { recursive: true, force: true });
  }

  const processes = await Promise.all(children.map((child) => child.exited));
  const client = processes.find((p) => p.role === "client");
  if (!failure && !timedOut && processes.every((p) => p.exitCode === 0)) {
    try {
      assert.ok(client?.completed, "client exited 0 without finishing the sequence (opt-in or filter skipped it?)");
      assert.ok(daemons && daemons.length === 2, "server did not report both daemons");
      const [first, second] = daemons;
      assert.equal(first!.dataRequests, 1, "daemon-1 data requests");
      assert.equal(second!.dataRequests, 1, "daemon-2 data requests (test line 544)");
      if (opts.scenario === "consumer-identity") {
        assert.deepEqual(first!.consumerIdentities, [CONSUMER_IDENTITY], "daemon-1 consumer_identity (test line 591)");
        assert.deepEqual(second!.consumerIdentities, [CONSUMER_IDENTITY], "daemon-2 consumer_identity (test line 592)");
      }
    } catch (error) {
      failure = error instanceof Error ? error.message : String(error);
    }
  }
  const crashed = processes.filter(abnormal).map((p) => p.role);
  const status = timedOut ? "timeout" : crashed.length ? "crash" : failure || processes.some((p) => p.exitCode !== 0) ? "failure" : "pass";
  const result = {
    experiment: "restart-split", scenario: opts.scenario, clientRunner: opts.runner, bun: Bun.version,
    platform: process.platform, status, crashed, failure, processes, daemons,
  };
  console.log(`RESULT ${JSON.stringify(result)}`);
  if (opts.result) {
    mkdirSync(dirname(opts.result), { recursive: true });
    writeFileSync(opts.result, JSON.stringify(result, null, 2));
  }
  process.exitCode = status === "pass" ? 0 : 1;
}

function optionsFrom(args: string[]): Options {
  const scenario = args.shift();
  if (scenario !== "key-rotation" && scenario !== "consumer-identity") {
    throw new Error("usage: win-restart-split.ts key-rotation|consumer-identity [--client-runner run|test] [--result PATH] [--timeout-ms N]");
  }
  const opts: Options = { scenario, runner: "run", timeoutMs: 60_000 };
  while (args.length) {
    const flag = args.shift();
    const value = args.shift();
    if (value === undefined) throw new Error(`missing value for ${flag}`);
    switch (flag) {
      case "--client-runner":
        if (value !== "run" && value !== "test") throw new Error("--client-runner must be run or test");
        opts.runner = value;
        break;
      case "--timeout-ms": {
        const n = Number(value);
        if (!Number.isSafeInteger(n) || n <= 0) throw new Error(`expected a positive integer, got ${value}`);
        opts.timeoutMs = n;
        break;
      }
      case "--result": opts.result = value; break;
      case "--conn-file": opts.connFile = value; break;
      case "--role":
        if (value !== "server" && value !== "client") throw new Error("invalid internal role");
        opts.role = value;
        break;
      default: throw new Error(`unknown option ${flag}`);
    }
  }
  return opts;
}

if (import.meta.main) {
  try {
    const opts = optionsFrom(process.argv.slice(2));
    if (opts.role === "server") {
      assert.ok(opts.connFile, "server requires --conn-file");
      await runServer(opts.scenario, opts.connFile);
    } else if (opts.role === "client") {
      assert.ok(opts.connFile, "client requires --conn-file");
      await runRestartClient(opts.scenario, opts.connFile, "run", async (call, expected) => {
        assert.deepEqual(await call, expected);
      });
    } else {
      await observe(opts);
    }
  } catch (error) {
    console.error(error);
    process.exitCode = 1;
  }
}
