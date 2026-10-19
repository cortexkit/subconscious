// Minimal copy of tests/managed-client.test.ts at 8766895e:
// constants 30-34, listener 1586-1610 (in win-reconnect-transport.ts), handshake
// 1855-1905, and echo/close 1809-1815. The helpers are not exported by the test;
// copying avoids registering bun:test suites or rewriting the test at load time.
// In particular, preserve await writeFrame(...) followed immediately by
// socket.destroy(): changing that timing would change the crash experiment.
// The constants and handshake/frame helpers are exported for reuse by
// win-push-timeout.test.ts, which needs the same fake-daemon handshake.
import assert from "node:assert/strict";
import { mkdtempSync, rmSync, writeFileSync, chmodSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Socket } from "node:net";
import {
  buildFlags, buildFrame, CLIENT_AUTH_DOMAIN, computeProof, decodeHeader,
  encodeFrame, FrameType, HEADER_LEN, Priority, SERVER_PROOF_DOMAIN,
  SubcClient, type Frame,
} from "../src/index.js";
import { SocketReader, startServer, writeAll, type ReproServer, type Stats } from "./win-reconnect-transport.js";

export const KEY = Uint8Array.from(Array(32).fill(0x4c));
export const DAEMON_ID = Uint8Array.from(Array(16).fill(0x7d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => 0xa0 + i));
export const IDENTITY = { project_root: "/tmp/subc-client-test", harness: "bun", session: "managed" };
const BACKOFF = { baseMs: 5, capMs: 10, maxAttempts: 4 };

export function startManagedServer(): Promise<ReproServer> {
  return startServer(handleFakeConnection);
}

async function handleFakeConnection(socket: Socket, stats: Stats): Promise<void> {
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 5_000;
  await authenticateFakeServer(reader, socket, deadline);
  let routeChannel = 41;
  for (;;) {
    const frame = await readFrame(reader, deadline);
    if (frame.header.ty === FrameType.Ping && frame.header.channel === 0) {
      await writeFrame(socket, buildFrame(FrameType.Pong, buildFlags(false, Priority.Interactive, false), 0, 0, frame.header.corr, new Uint8Array()), deadline);
      continue;
    }
    if (frame.header.ty !== FrameType.Request) continue;
    if (frame.header.channel === 0) {
      const request = parseJson(frame.body) as { op?: string };
      if (request.op === "catalog.list") {
        await writeFrame(socket, responseFrame(frame, { op: "catalog.list", modules: [] }), deadline);
      } else if (request.op === "route.open") {
        stats.routeOpens += 1;
        await writeFrame(socket, responseFrame(frame, { op: "route.open", route_channel: routeChannel++, route_epoch: 1 }), deadline);
      }
      continue;
    }
    stats.dataRequests += 1;
    const body = parseJson(frame.body);
    await writeFrame(socket, responseFrame(frame, body), deadline);
    // closeAfterDataResponses: 1 in the test closes every connection's first
    // response, since its dataRequests counter is shared across connections.
    if (stats.dataRequests >= 1) {
      socket.destroy();
      return;
    }
  }
}

export function responseFrame(request: Frame, body: unknown): Frame {
  return buildFrame(FrameType.Response, buildFlags(false, Priority.Interactive, false), request.header.channel, request.header.epoch, request.header.corr, encodeJson(body));
}

export async function authenticateFakeServer(reader: SocketReader, socket: Socket, deadline: number): Promise<void> {
  const hello = await readAuthMessage<{ client_nonce: number[]; role: string }>(reader, deadline);
  assert.equal(hello.role, "client");
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
  assert.equal(Buffer.from(auth.client_auth).equals(Buffer.from(expected)), true);
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

export async function readFrame(reader: SocketReader, deadline: number): Promise<Frame> {
  const header = decodeHeader(await reader.readExact(HEADER_LEN, deadline));
  const body = header.len === 0 ? new Uint8Array(0) : await reader.readExact(header.len, deadline);
  return { header, body };
}

async function writeFrame(socket: Socket, frame: Frame, deadline: number): Promise<void> {
  await writeAll(socket, encodeFrame(frame), deadline);
}

export function encodeJson(value: unknown): Uint8Array {
  return new Uint8Array(Buffer.from(JSON.stringify(value), "utf8"));
}

export function parseJson(body: Uint8Array): unknown {
  return JSON.parse(Buffer.from(body).toString("utf8"));
}

export async function runManagedClient(port: number, iterations: number, progress: (n: number) => void): Promise<void> {
  const dir = mkdtempSync(join(tmpdir(), "subc-win-reconnect-"));
  const connFile = join(dir, "subc-connection.json");
  let client: SubcClient | undefined;
  try {
    writeFileSync(connFile, JSON.stringify({
      schema: 1,
      endpoints: [{ host: "127.0.0.1", port }],
      key: Array.from(KEY),
      daemon_id: Array.from(DAEMON_ID),
      pid: process.pid,
      daemon_ver: "fake-subc",
    }), { mode: 0o600 });
    chmodSync(connFile, 0o600);
    client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF });
    for (let i = 1; i <= iterations; i++) {
      for (const phase of ["before-reconnect", "after-reconnect"]) {
        assert.deepEqual(await client.call("managed-provider", "echo", { iteration: i, phase }), {
          method: "echo", params: { iteration: i, phase },
        });
        // Like the reconnect tests, wait until the read loop observes the close
        // before the next call. Do not replay an in-flight request after a drop.
        const deadline = Date.now() + 5_000;
        while ((client as unknown as { closedErr: Error | null }).closedErr === null) {
          if (Date.now() >= deadline) throw new Error("client did not observe server close");
          await Bun.sleep(5);
        }
      }
      progress(i);
    }
  } finally {
    client?.close();
    rmSync(dir, { recursive: true, force: true });
  }
}
