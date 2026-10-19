// Opt in with WIN_BUN_REPRO=1, one variant per process:
//   WIN_BUN_REPRO=1 bun test scripts/win-push-timeout.test.ts -t "variant=exact$"
// Bun also discovers scripts/*.test.ts during a bare bun test; keep these skipped
// unless explicitly enabled.
//
// Logged copy of tests/managed-client.test.ts "daemon-originated control pushes >
// route.closing reaches the observer parsed; a garbage push is ignored and the
// stream survives" (lines 142-172 at 8766895e), whose fake daemon writes control
// pushes before each catalog.list response (lines 1651-1673). All logging lives on
// the test side: the fake daemon logs every frame it reads and every write it
// queues and completes, and a second 'data' listener on the client's own node:net
// socket logs every chunk the client process receives and re-parses those bytes
// into frames. Comparing that tap with the client's buffered byte count and
// observer calls shows whether bytes reached the client socket and whether the
// client's reader consumed them. No library code is changed.
//
// Variant "exact" sends the same sequence as the original test (valid
// route.closing push, unparseable push, response, for each of two catalog.list
// calls). The others change only the pushes before each response: "no-push"
// (none), "valid-only" (route.closing only), "garbage-first" (unparseable, then
// route.closing). Each variant is meant to run as the first and only test of a
// fresh process, like the original is the first test in its file.
import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import type { Socket } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  buildFlags, buildFrame, decodeHeader, encodeFrame, FrameType, HEADER_LEN,
  Priority, SubcClient, type ControlPush, type Frame,
} from "../src/index.js";
import {
  authenticateFakeServer, DAEMON_ID, encodeJson, IDENTITY, KEY, parseJson,
  readFrame, responseFrame,
} from "./win-reconnect-managed.js";
import { SocketReader, startServer } from "./win-reconnect-transport.js";

type PushKind = "valid" | "garbage";
const VARIANTS: Record<string, PushKind[]> = {
  exact: ["valid", "garbage"],
  "no-push": [],
  "valid-only": ["valid"],
  "garbage-first": ["garbage", "valid"],
};

// The original test fails at bun:test's default 5000 ms. Like the original, the
// fake daemon gives up on a connection 5 s after accepting it, so a stalled
// exchange normally ends near 5 s as outcome=error with a server handler-error.
// The test timeout is longer so that ending is logged instead of being cut off
// by bun:test; the watchdog fires only if the client never notices the close.
// "late" means every check passed but after the original's 5 s budget.
const ORIGINAL_TIMEOUT_MS = 5_000;
const WATCHDOG_MS = 15_000;
const TEST_TIMEOUT_MS = 20_000;

function controlPushFrame(body: Uint8Array): Frame {
  // Same shape as the test's controlPushFrame: channel 0, epoch 0, corr 0.
  return buildFrame(FrameType.Push, buildFlags(false, Priority.Interactive, false), 0, 0, 0n, body);
}

function pushBody(kind: PushKind): Uint8Array {
  return kind === "valid"
    ? encodeJson({ op: "route.closing", module_id: "fake-aft", reason: "restart" })
    : new TextEncoder().encode("not json {");
}

function describeFrame(ty: number, channel: number, corr: bigint, body: Uint8Array): string {
  let content: string;
  try {
    const parsed = JSON.parse(Buffer.from(body).toString("utf8")) as { op?: unknown };
    content = `op=${String(parsed?.op)}`;
  } catch {
    content = "body=unparseable";
  }
  return `ty=${FrameType[ty] ?? ty} ch=${channel} corr=${corr} len=${body.length} ${content}`;
}

async function runVariant(variant: string): Promise<void> {
  const pushes = VARIANTS[variant]!;
  const started = performance.now();
  const log = (side: string, message: string) =>
    console.log(`PUSHDIAG t=${(performance.now() - started).toFixed(1)}ms variant=${variant} side=${side} ${message}`);
  log("test", `start bun=${Bun.version} platform=${process.platform} pushes=[${pushes.join(",")}]`);

  // Copy of the test fake's per-connection handler, reduced to the Ping and
  // catalog.list arms this test reaches. It keeps the same absolute 5 s
  // per-connection deadline for every read and write; a handler error is logged
  // here because startServer otherwise destroys the socket silently.
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

  async function handleConnection(socket: Socket): Promise<void> {
    const reader = new SocketReader(socket);
    const deadline = Date.now() + 5_000;
    await authenticateFakeServer(reader, socket, deadline);
    log("server", "handshake-done");
    let catalogLists = 0;
    for (;;) {
      const frame = await readFrame(reader, deadline);
      log("server", `read-frame ${describeFrame(frame.header.ty, frame.header.channel, frame.header.corr, frame.body)}`);
      if (frame.header.ty === FrameType.Ping && frame.header.channel === 0) {
        await writeLogged(socket, buildFrame(FrameType.Pong, buildFlags(false, Priority.Interactive, false), 0, 0, frame.header.corr, new Uint8Array()), "pong", deadline);
        continue;
      }
      if (frame.header.ty !== FrameType.Request || frame.header.channel !== 0) continue;
      const request = parseJson(frame.body) as { op?: string };
      if (request.op !== "catalog.list") continue;
      catalogLists += 1;
      for (const [i, kind] of pushes.entries()) {
        await writeLogged(socket, controlPushFrame(pushBody(kind)), `catalog${catalogLists}-push${i + 1}-${kind}`, deadline);
      }
      await writeLogged(socket, responseFrame(frame, { op: "catalog.list", modules: [] }), `catalog${catalogLists}-response`, deadline);
    }
  }

  const server = await startServer(async (socket) => {
    log("server", `accept remote=${socket.remotePort} local=${socket.localPort}`);
    socket.on("end", () => log("server", "socket-end"));
    socket.on("close", () => log("server", "socket-close"));
    socket.on("error", (err) => log("server", `socket-error ${String(err)}`));
    try {
      await handleConnection(socket);
    } catch (error) {
      log("server", `handler-error ${error instanceof Error ? error.message : String(error)}`);
      throw error;
    }
  });
  const dir = mkdtempSync(join(tmpdir(), "subc-win-push-"));
  const connFile = join(dir, "subc-connection.json");
  let client: SubcClient | undefined;
  let watchdog: ReturnType<typeof setTimeout> | undefined;
  let stuckReported = false;
  let dumpState = () => "client-not-connected";
  // The fake daemon's own 5 s per-connection deadline closes the connection if
  // the exchange stalls, which changes the client's state. Record that state
  // shortly before the fake gives up so a stall is visible as it was.
  const snapshot = setTimeout(() => log("test", `snapshot-before-fake-deadline ${dumpState()}`), 4_500);
  try {
    writeFileSync(connFile, JSON.stringify({
      schema: 1,
      endpoints: [{ host: "127.0.0.1", port: server.port }],
      key: Array.from(KEY),
      daemon_id: Array.from(DAEMON_ID),
      pid: process.pid,
      daemon_ver: "fake-subc",
    }), { mode: 0o600 });
    chmodSync(connFile, 0o600);
    log("test", `connection-file-written port=${server.port}`);

    const seen: ControlPush[] = [];
    client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      onControlPush: (push) => {
        log("client", `observer op=${push.op} body=${JSON.stringify(push.body)}`);
        seen.push(push);
      },
    });
    log("test", "connected");

    // Reach the library's private sockets at runtime only to observe them: the
    // SubcClient's SubcSocket and its node:net socket. Adding a second 'data'
    // listener does not change how the client's own listener receives chunks.
    const subcSocket = (client as unknown as { sock?: { bufferedBytes(): number; sock?: Socket; waiter?: { need: number } | null } }).sock;
    const netSocket = subcSocket?.sock;
    let tapped = 0;
    let tapBuffer = Buffer.alloc(0);
    const state = () => {
      const internals = client as unknown as { pending?: Map<string, unknown>; closedErr?: Error | null; readerActive?: boolean };
      return `tapped=${tapped} clientBuffered=${subcSocket?.bufferedBytes() ?? "n/a"} readerWaitingFor=${subcSocket?.waiter?.need ?? "none"} ` +
        `pending=${internals.pending?.size ?? "n/a"} readerActive=${internals.readerActive} closedErr=${internals.closedErr ? internals.closedErr.message : "none"} observerCalls=${seen.length}`;
    };
    dumpState = state;
    if (!netSocket) {
      log("client", "tap-unavailable (SubcClient internals changed); only server-side and observer logs follow");
    } else {
      log("client", `tap-attached local=${netSocket.localPort} remote=${netSocket.remotePort}`);
      netSocket.on("data", (chunk: Buffer) => {
        tapped += chunk.length;
        log("client", `net-data chunk=${chunk.length} total=${tapped}`);
        tapBuffer = Buffer.concat([tapBuffer, chunk]);
        while (tapBuffer.length >= HEADER_LEN) {
          const header = decodeHeader(tapBuffer.subarray(0, HEADER_LEN));
          if (tapBuffer.length < HEADER_LEN + header.len) break;
          const body = tapBuffer.subarray(HEADER_LEN, HEADER_LEN + header.len);
          log("client", `net-frame ${describeFrame(header.ty, header.channel, header.corr, body)}`);
          tapBuffer = tapBuffer.subarray(HEADER_LEN + header.len);
        }
      });
      netSocket.on("end", () => log("client", "net-end"));
      netSocket.on("close", () => log("client", "net-close"));
      netSocket.on("error", (err) => log("client", `net-error ${String(err)}`));
    }

    const stuck = new Promise<never>((_, reject) => {
      watchdog = setTimeout(() => {
        log("test", `watchdog ${state()}`);
        stuckReported = true;
        console.log(`PUSHDIAG RESULT variant=${variant} outcome=stuck elapsed_ms=${(performance.now() - started).toFixed(0)}`);
        reject(new Error(`push diagnostic stuck after ${WATCHDOG_MS} ms`));
      }, WATCHDOG_MS);
    });
    const expectedPerCall = pushes.filter((kind) => kind === "valid").length;
    for (const call of [1, 2]) {
      log("test", `catalog${call}-start ${state()}`);
      const modules = await Promise.race([client.catalogList(), stuck]);
      log("test", `catalog${call}-resolved ${state()}`);
      expect(modules).toEqual([]);
      expect(seen.length).toBe(expectedPerCall * call);
      for (const push of seen) {
        expect(push.op).toBe("route.closing");
        expect(push.body.module_id).toBe("fake-aft");
        expect(push.body.reason).toBe("restart");
      }
    }
    const elapsed = performance.now() - started;
    // "late" completed correctly but would have exceeded the original test's
    // timeout; the wrapper reports it separately from a pass.
    const outcome = elapsed > ORIGINAL_TIMEOUT_MS ? "late" : "ok";
    console.log(`PUSHDIAG RESULT variant=${variant} outcome=${outcome} elapsed_ms=${elapsed.toFixed(0)}`);
  } catch (error) {
    log("test", `error ${error instanceof Error ? error.message : String(error)} ${dumpState()}`);
    if (!stuckReported) {
      console.log(`PUSHDIAG RESULT variant=${variant} outcome=error elapsed_ms=${(performance.now() - started).toFixed(0)}`);
    }
    throw error;
  } finally {
    clearTimeout(snapshot);
    if (watchdog) clearTimeout(watchdog);
    client?.close();
    await server.stop();
    rmSync(dir, { recursive: true, force: true });
    log("test", "cleaned-up");
  }
}

describe.skipIf(process.env.WIN_BUN_REPRO !== "1")("Windows Bun control-push diagnostic", () => {
  for (const variant of Object.keys(VARIANTS)) {
    test(`variant=${variant}`, () => runVariant(variant), TEST_TIMEOUT_MS);
  }
});
