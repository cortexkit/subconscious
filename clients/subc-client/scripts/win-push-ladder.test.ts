// Opt in with WIN_BUN_REPRO=1, one variant per process:
//   WIN_BUN_REPRO=1 bun test scripts/win-push-ladder.test.ts -t "ladder=real$"
// A bare `bun test` discovers this file but skips every variant.
//
// The control-push test "daemon-originated control pushes > route.closing
// reaches the observer parsed; a garbage push is ignored and the stream
// survives" (tests/managed-client.test.ts:142-172 at 8766895e) fails on Windows
// Bun 1.3.14: its second catalog.list rejects. The earlier logged copy,
// scripts/win-push-timeout.test.ts variant=exact, passes there. This file
// rebuilds the real test exactly, with its fake daemon (startFakeDaemon,
// handleFakeConnection, the expect-based handshake and the afterEach cleanup),
// and makes each way the logged copy differs a switch that can be turned on by
// itself or cumulatively:
//
//   plain-await       call 2 is `expect(await p).toEqual([])`, not the real
//                     `await expect(p).resolves.toEqual([])`
//   race-watchdog     both calls are raced against a 15 s watchdog promise
//   snapshot-timer    a 4.5 s timer logs the client state
//   timeout-20s       the test's own timeout is 20 s, not the default 5 s
//   client-tap        a second 'data' listener, plus end/close/error
//                     listeners, on the client's node:net socket
//   server-listeners  end/close/error listeners and an accept log on each
//                     fake-daemon socket
//   server-logs       the fake daemon logs its handshake, each frame it reads,
//                     and each write it queues, returns and completes
//   observer-log      the onControlPush observer logs before recording
//   node-assert       the fake's handshake checks use node:assert, not expect
//   in-test-cleanup   the fake daemon and temp dir are removed inside the
//                     test's finally, not by afterEach
//   call-logs         state lines after each call, plus the copy's extra
//                     observer checks after call 2
//
// Every variant, including the real one, attaches a side handler to each
// catalog.list promise. It logs the rejection's error class, name, code,
// kind, message, cause chain and stack, and the client state at that moment,
// without changing what the test awaits. Every variant except "real-silent"
// also logs the client state before each call. The afterEach hook logs when it
// starts and each step of the fake daemon's stop, so a run that ends in a
// bun:test timeout shows whether the test body or the cleanup was waiting.
import { afterEach, describe, expect, test } from "bun:test";
import { createServer, type AddressInfo, type Socket } from "node:net";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  buildFlags, buildFrame, CLIENT_AUTH_DOMAIN, computeProof, decodeHeader, encodeFrame,
  FrameType, HEADER_LEN, Priority, SERVER_PROOF_DOMAIN, SubcClient, type ControlPush, type Frame,
} from "../src/index.js";
import {
  authenticateFakeServer as authenticateWithNodeAssert, DAEMON_ID, encodeJson, IDENTITY, KEY,
  parseJson, readFrame, responseFrame, SERVER_NONCE, writeFrame,
} from "./win-reconnect-managed.js";
import { SocketReader, writeAll } from "./win-reconnect-transport.js";

const DIFFS = [
  "plain-await", "race-watchdog", "snapshot-timer", "timeout-20s", "client-tap", "server-listeners",
  "server-logs", "observer-log", "node-assert", "in-test-cleanup", "call-logs",
] as const;
type Diff = (typeof DIFFS)[number];

interface Variant { name: string; diffs: Set<Diff>; stateLogs: boolean }
const VARIANTS: Variant[] = [
  { name: "real-silent", diffs: new Set(), stateLogs: false },
  { name: "real", diffs: new Set(), stateLogs: true },
  // The real test with exactly one of the copy's differences.
  ...DIFFS.map((d) => ({ name: `only-${d}`, diffs: new Set<Diff>([d]), stateLogs: true })),
  // The real test with the first k differences; the last row has all of them.
  ...DIFFS.map((d, i) => ({ name: `cum-${String(i + 1).padStart(2, "0")}-${d}`, diffs: new Set<Diff>(DIFFS.slice(0, i + 1)), stateLogs: true })),
];

const WATCHDOG_MS = 15_000;

let started = performance.now();
let currentVariant = "none";
function log(side: string, message: string): void {
  console.log(`PUSHLADDER t=${(performance.now() - started).toFixed(1)}ms variant=${currentVariant} side=${side} ${message}`);
}

// --- Copy of the real test's fixture (managed-client.test.ts at 8766895e). ---

interface FakeStats {
  connections: number;
  closedConnections: number;
  requestFrames: { channel: number; controlOp?: string }[];
}
interface FakeDaemon { port: number; stop(): Promise<void> }

const tempDirs: string[] = [];
const daemons: FakeDaemon[] = [];

// Lines 136-139, with each step logged.
afterEach(async () => {
  log("hook", `afterEach-start daemons=${daemons.length} tempDirs=${tempDirs.length}`);
  for (const daemon of daemons.splice(0)) await daemon.stop();
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
  log("hook", "afterEach-done");
});

// The handshake as the real test checks it, with bun:test expect (1855-1880).
async function authenticateWithExpect(reader: SocketReader, socket: Socket, deadline: number): Promise<void> {
  const hello = await readAuthMessage<{ client_nonce: number[]; role: string }>(reader, deadline);
  expect(hello.role).toBe("client");
  const clientNonce = Uint8Array.from(hello.client_nonce);
  const serverProof = computeProof(KEY, SERVER_PROOF_DOMAIN, clientNonce, SERVER_NONCE, DAEMON_ID);
  await writeAuthMessage(socket, {
    daemon_id: Array.from(DAEMON_ID),
    server_nonce: Array.from(SERVER_NONCE),
    daemon_ver: "fake-subc",
    server_proof: Array.from(serverProof),
  }, deadline);
  const auth = await readAuthMessage<{ client_auth: number[] }>(reader, deadline);
  const expected = computeProof(KEY, CLIENT_AUTH_DOMAIN, clientNonce, SERVER_NONCE, DAEMON_ID);
  expect(Buffer.from(auth.client_auth).equals(Buffer.from(expected))).toBe(true);
}

async function readAuthMessage<T>(reader: SocketReader, deadline: number): Promise<T> {
  const lenBytes = await reader.readExact(4, deadline);
  const len = new DataView(lenBytes.buffer, lenBytes.byteOffset, 4).getUint32(0, true);
  const body = len === 0 ? new Uint8Array(0) : await reader.readExact(len, deadline);
  return JSON.parse(Buffer.from(body).toString("utf8")) as T;
}

async function writeAuthMessage(socket: Socket, value: unknown, deadline: number): Promise<void> {
  const body = Buffer.from(JSON.stringify(value), "utf8");
  const len = new Uint8Array(4);
  new DataView(len.buffer).setUint32(0, body.length, true);
  await writeAll(socket, len, deadline);
  await writeAll(socket, body, deadline);
}

function controlPushFrame(body: Uint8Array): Frame {
  return buildFrame(FrameType.Push, buildFlags(false, Priority.Interactive, false), 0, 0, 0n, body);
}

function describeFrame(frame: { header: { ty: number; channel: number; corr: bigint }; body: Uint8Array }): string {
  let content: string;
  try {
    content = `op=${String((JSON.parse(Buffer.from(frame.body).toString("utf8")) as { op?: unknown })?.op)}`;
  } catch {
    content = "body=unparseable";
  }
  return `ty=${FrameType[frame.header.ty] ?? frame.header.ty} ch=${frame.header.channel} corr=${frame.header.corr} len=${frame.body.length} ${content}`;
}

// The copy's logged write (win-push-timeout.test.ts writeLogged).
async function writeLogged(socket: Socket, frame: Frame, label: string, deadline: number): Promise<void> {
  const bytes = Buffer.from(encodeFrame(frame));
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => {
      log("server", `write-timeout ${label}`);
      reject(new Error("timed out writing fake daemon bytes"));
    }, Math.max(0, deadline - Date.now()));
    log("server", `write-queue ${label} bytes=${bytes.length} writableLength=${socket.writableLength}`);
    const accepted = socket.write(bytes, (err) => {
      clearTimeout(timer);
      log("server", `write-callback ${label} err=${err ? String(err) : "none"}`);
      if (err) reject(err);
      else resolve();
    });
    log("server", `write-returned ${label} accepted=${accepted}`);
  });
}

// handleFakeConnection (1616-1673), reduced to the arms this test reaches:
// Ping, and catalog.list with controlPushesBeforeCatalogList.
async function handleFakeConnection(socket: Socket, stats: FakeStats, has: (d: Diff) => boolean): Promise<void> {
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 5_000;
  if (has("node-assert")) await authenticateWithNodeAssert(reader, socket, deadline);
  else await authenticateWithExpect(reader, socket, deadline);
  if (has("server-logs")) log("server", "handshake-done");
  let catalogLists = 0;
  const write = (frame: Frame, label: string) =>
    has("server-logs") ? writeLogged(socket, frame, label, deadline) : writeFrame(socket, frame, deadline);
  for (;;) {
    const frame = await readFrame(reader, deadline);
    if (has("server-logs")) log("server", `read-frame ${describeFrame(frame)}`);
    if (frame.header.ty === FrameType.Ping && frame.header.channel === 0) {
      await write(buildFrame(FrameType.Pong, buildFlags(false, Priority.Interactive, false), 0, 0, frame.header.corr, new Uint8Array()), "pong");
      continue;
    }
    if (frame.header.ty !== FrameType.Request) continue;
    const requestFrame: { channel: number; controlOp?: string } = { channel: frame.header.channel };
    stats.requestFrames.push(requestFrame);
    if (frame.header.channel === 0) {
      const request = parseJson(frame.body) as { op?: string };
      requestFrame.controlOp = request.op;
      if (request.op === "catalog.list") {
        catalogLists += 1;
        await write(controlPushFrame(encodeJson({ op: "route.closing", module_id: "fake-aft", reason: "restart" })), `catalog${catalogLists}-push1-valid`);
        await write(controlPushFrame(new TextEncoder().encode("not json {")), `catalog${catalogLists}-push2-garbage`);
        await write(responseFrame(frame, { op: "catalog.list", modules: [] }), `catalog${catalogLists}-response`);
      }
      continue;
    }
  }
}

// startFakeDaemon (1586-1614), with the stop steps logged.
async function startFakeDaemon(stats: FakeStats, has: (d: Diff) => boolean, register: boolean): Promise<FakeDaemon> {
  const sockets = new Set<Socket>();
  const server = createServer((socket) => {
    stats.connections += 1;
    sockets.add(socket);
    socket.once("close", () => {
      sockets.delete(socket);
      stats.closedConnections += 1;
    });
    if (has("server-listeners")) {
      log("server", `accept remote=${socket.remotePort} local=${socket.localPort}`);
      socket.on("end", () => log("server", "socket-end"));
      socket.on("close", () => log("server", "socket-close"));
      socket.on("error", (err) => log("server", `socket-error ${String(err)}`));
    }
    void handleFakeConnection(socket, stats, has).catch((error) => {
      if (has("server-logs")) log("server", `handler-error ${error instanceof Error ? error.message : String(error)}`);
      socket.destroy();
    });
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = (server.address() as AddressInfo).port;
  let stopped = false;
  const daemon: FakeDaemon = {
    port,
    stop: async () => {
      if (stopped) return;
      stopped = true;
      log("fake", `stop destroying sockets=${sockets.size}`);
      for (const socket of sockets) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
      log("fake", "stop server.close callback");
    },
  };
  if (register) daemons.push(daemon);
  return daemon;
}

function writeConnectionFile(path: string, port: number): void {
  writeFileSync(path, JSON.stringify({
    schema: 1,
    endpoints: [{ host: "127.0.0.1", port }],
    key: Array.from(KEY),
    daemon_id: Array.from(DAEMON_ID),
    pid: process.pid,
    daemon_ver: "fake-subc",
  }), { mode: 0o600 });
  chmodSync(path, 0o600);
}

// --- Diagnostics: read-only views of state, and rejection details. ---

function errorDetails(error: unknown, depth = 0): string {
  if (!(error instanceof Error)) return `non-error=${JSON.stringify(String(error))}`;
  const e = error as Error & { code?: unknown; kind?: unknown; reason?: unknown; cause?: unknown };
  let out = `class=${e.constructor?.name} name=${e.name} code=${String(e.code ?? "none")} kind=${String(e.kind ?? "none")} message=${JSON.stringify(e.message)}`;
  if (e.reason !== undefined) out += ` reason=${String(e.reason)}`;
  if (e.cause !== undefined && depth < 3) out += ` cause=[${errorDetails(e.cause, depth + 1)}]`;
  return out;
}

async function runVariant(variant: Variant): Promise<void> {
  const has = (d: Diff) => variant.diffs.has(d);
  started = performance.now();
  currentVariant = variant.name;
  log("test", `start bun=${Bun.version} platform=${process.platform} diffs=[${[...variant.diffs].join(",")}]`);

  const dir = mkdtempSync(join(tmpdir(), "subc-managed-client-"));
  if (!has("in-test-cleanup")) tempDirs.push(dir);
  const connFile = join(dir, "subc-connection.json");
  const stats: FakeStats = { connections: 0, closedConnections: 0, requestFrames: [] };
  const daemon = await startFakeDaemon(stats, has, !has("in-test-cleanup"));
  writeConnectionFile(connFile, daemon.port);

  const seen: { op: string; body: Record<string, unknown> }[] = [];
  const client = await SubcClient.connect({
    connectionFile: connFile,
    identity: IDENTITY,
    onControlPush: has("observer-log")
      ? (push: ControlPush) => {
          log("client", `observer op=${push.op} body=${JSON.stringify(push.body)}`);
          seen.push(push);
        }
      : (push: ControlPush) => seen.push(push),
  });

  // Private client fields, read only.
  const internals = client as unknown as {
    sock?: { bufferedBytes(): number; sock?: Socket; waiter?: { need: number } | null; closedErr?: Error | null };
    pending?: Map<string, unknown>; closedErr?: Error | null; readerActive?: boolean; generation?: number; lastInboundAtMs?: number;
  };
  let tapped = 0;
  const state = () => {
    const sock = internals.sock;
    const inbound = internals.lastInboundAtMs ? `${Date.now() - internals.lastInboundAtMs}ms-ago` : "never";
    return `clientBuffered=${sock?.bufferedBytes() ?? "n/a"} readerWaitingFor=${sock?.waiter?.need ?? "none"} ` +
      `pending=${internals.pending?.size ?? "n/a"} pendingKeys=[${[...(internals.pending?.keys() ?? [])].join(",")}] ` +
      `readerActive=${internals.readerActive} generation=${internals.generation} lastInbound=${inbound} ` +
      `clientClosedErr=${internals.closedErr ? JSON.stringify(internals.closedErr.message) : "none"} ` +
      `socketClosedErr=${sock?.closedErr ? JSON.stringify(sock.closedErr.message) : "none"} ` +
      `localPort=${sock?.sock?.localPort ?? "n/a"} destroyed=${sock?.sock?.destroyed ?? "n/a"} ` +
      `observerCalls=${seen.length} fakeConnections=${stats.connections} fakeClosed=${stats.closedConnections} ` +
      `fakeRequests=[${stats.requestFrames.map((r) => r.controlOp ?? r.channel).join(",")}]` +
      (has("client-tap") ? ` tapped=${tapped}` : "");
  };

  // Logs how each call settles without changing what the test awaits.
  const watch = <T>(call: number, promise: Promise<T>): Promise<T> => {
    const issued = performance.now();
    promise.then(
      () => log("client", `call${call}-resolved after=${(performance.now() - issued).toFixed(1)}ms`),
      (error) => {
        console.log(`PUSHLADDER REJECTION variant=${variant.name} call=${call} after_ms=${(performance.now() - issued).toFixed(0)} ${errorDetails(error)}`);
        log("client", `call${call}-rejected-state ${state()}`);
        log("client", `call${call}-rejected-stack ${JSON.stringify(error instanceof Error ? error.stack : String(error))}`);
      },
    );
    return promise;
  };

  let watchdog: ReturnType<typeof setTimeout> | undefined;
  let snapshot: ReturnType<typeof setTimeout> | undefined;
  let stuck: Promise<never> | undefined;
  if (has("race-watchdog")) {
    stuck = new Promise<never>((_, reject) => {
      watchdog = setTimeout(() => {
        log("test", `watchdog ${state()}`);
        reject(new Error(`push diagnostic stuck after ${WATCHDOG_MS} ms`));
      }, WATCHDOG_MS);
    });
    stuck.catch(() => {});
  }
  if (has("snapshot-timer")) snapshot = setTimeout(() => log("test", `snapshot-4500ms ${state()}`), 4_500);
  const awaited = <T>(promise: Promise<T>): Promise<T> => (stuck ? Promise.race([promise, stuck]) : promise);

  if (has("client-tap")) {
    const netSocket = internals.sock?.sock;
    if (!netSocket) {
      log("client", "tap-unavailable (SubcClient internals changed)");
    } else {
      let tapBuffer = Buffer.alloc(0);
      netSocket.on("data", (chunk: Buffer) => {
        tapped += chunk.length;
        log("client", `net-data chunk=${chunk.length} total=${tapped}`);
        tapBuffer = Buffer.concat([tapBuffer, chunk]);
        while (tapBuffer.length >= HEADER_LEN) {
          const header = decodeHeader(tapBuffer.subarray(0, HEADER_LEN));
          if (tapBuffer.length < HEADER_LEN + header.len) break;
          log("client", `net-frame ${describeFrame({ header, body: tapBuffer.subarray(HEADER_LEN, HEADER_LEN + header.len) })}`);
          tapBuffer = tapBuffer.subarray(HEADER_LEN + header.len);
        }
      });
      netSocket.on("end", () => log("client", "net-end"));
      netSocket.on("close", () => log("client", "net-close"));
      netSocket.on("error", (err) => log("client", `net-error ${String(err)}`));
    }
  }

  try {
    if (variant.stateLogs) log("test", `call1-start ${state()}`);
    const modules = await awaited(watch(1, client.catalogList()));
    if (has("call-logs")) log("test", `call1-done ${state()}`);
    expect(modules).toEqual([]);

    expect(seen.length).toBe(1);
    expect(seen[0]!.op).toBe("route.closing");
    expect(seen[0]!.body.module_id).toBe("fake-aft");
    expect(seen[0]!.body.reason).toBe("restart");

    if (variant.stateLogs) log("test", `call2-start ${state()}`);
    if (has("plain-await")) {
      const second = await awaited(watch(2, client.catalogList()));
      expect(second).toEqual([]);
    } else {
      await expect(awaited(watch(2, client.catalogList()))).resolves.toEqual([]);
    }
    if (has("call-logs")) {
      log("test", `call2-done ${state()}`);
      expect(seen.length).toBe(2);
      for (const push of seen) {
        expect(push.op).toBe("route.closing");
        expect(push.body.module_id).toBe("fake-aft");
        expect(push.body.reason).toBe("restart");
      }
    }
    console.log(`PUSHLADDER RESULT variant=${variant.name} outcome=ok elapsed_ms=${(performance.now() - started).toFixed(0)}`);
  } catch (error) {
    console.log(`PUSHLADDER RESULT variant=${variant.name} outcome=error elapsed_ms=${(performance.now() - started).toFixed(0)} ${errorDetails(error)}`);
    throw error;
  } finally {
    if (watchdog) clearTimeout(watchdog);
    if (snapshot) clearTimeout(snapshot);
    client.close();
    if (has("in-test-cleanup")) {
      await daemon.stop();
      rmSync(dir, { recursive: true, force: true });
    }
    log("test", "body-finally-done");
  }
}

describe.skipIf(process.env.WIN_BUN_REPRO !== "1")("Windows Bun control-push ladder", () => {
  for (const variant of VARIANTS) {
    if (variant.diffs.has("timeout-20s")) test(`ladder=${variant.name}`, () => runVariant(variant), 20_000);
    else test(`ladder=${variant.name}`, () => runVariant(variant));
  }
});
