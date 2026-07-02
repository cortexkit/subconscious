import { afterEach, describe, expect, test } from "bun:test";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  buildFlags,
  buildFrame,
  CLIENT_AUTH_DOMAIN,
  computeProof,
  decodeHeader,
  encodeFrame,
  FrameType,
  HEADER_LEN,
  Priority,
  SERVER_PROOF_DOMAIN,
  SubcCallError,
  SubcClient,
  type BindIdentity,
  type Frame,
} from "../src/index.js";

const KEY = Uint8Array.from(Array(32).fill(0x4c));
const DAEMON_ID = Uint8Array.from(Array(16).fill(0x7d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => 0xa0 + i));
const IDENTITY: BindIdentity = { project_root: "/tmp/subc-client-test", harness: "bun", session: "managed" };
const BACKOFF = { baseMs: 5, capMs: 10, maxAttempts: 4 };

const tempDirs: string[] = [];
const daemons: FakeDaemon[] = [];

interface FakeStats {
  routeOpens: number;
  dataRequests: number;
  dataBodies: unknown[];
  // The consumer_identity sent on each route.open, in order (undefined when absent),
  // so a test can assert the principal survives a reconnect reopen.
  routeOpenConsumerIdentities: (unknown | undefined)[];
}

interface FakeDaemonOptions {
  stats: FakeStats;
  dataMode?: "echo" | "drop" | "error";
  routeOpenError?: { code: string; message: string };
  closeAfterDataResponses?: number;
}

interface FakeDaemon {
  port: number;
  stop(): Promise<void>;
}

function newStats(): FakeStats {
  return { routeOpens: 0, dataRequests: 0, dataBodies: [], routeOpenConsumerIdentities: [] };
}

afterEach(async () => {
  for (const daemon of daemons.splice(0)) await daemon.stop();
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("SubcClient managed call", () => {
  test("reuses one route.open for repeated calls to the same module", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      await expect(client.call("managed-provider", "echo", { n: 1 })).resolves.toEqual({
        method: "echo",
        params: { n: 1 },
      });
      await expect(client.call("managed-provider", "echo", { n: 2 })).resolves.toEqual({
        method: "echo",
        params: { n: 2 },
      });
      await expect(client.call("managed-provider", "echo", { n: 3 })).resolves.toEqual({
        method: "echo",
        params: { n: 3 },
      });

      expect(stats.routeOpens).toBe(1);
      expect(stats.dataRequests).toBe(3);
    } finally {
      client.close();
    }
  });

  test("auto-retries a provable not_sent request after reconnecting and re-opening the cached route", async () => {
    const { connFile } = tempConnectionFile();
    const sleeps: number[] = [];
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      await expect(client.call("managed-provider", "echo", { n: 1 })).resolves.toEqual({
        method: "echo",
        params: { n: 1 },
      });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({ stats: secondStats });
      writeConnectionFile(connFile, second.port);

      await expect(client.call("managed-provider", "echo", { n: 2 })).resolves.toEqual({
        method: "echo",
        params: { n: 2 },
      });

      expect(firstStats.routeOpens).toBe(1);
      expect(secondStats.routeOpens).toBe(1);
      expect(firstStats.dataRequests).toBe(1);
      expect(secondStats.dataRequests).toBe(1);
      expect(sleeps).toEqual([]);
    } finally {
      client.close();
    }
  });

  test("re-attaches the same consumer_identity when reopening a cached route after reconnect", async () => {
    // A route opened with a consumer_identity (principal attestation) must send the
    // SAME consumer_identity when it is reopened on a fresh connection after a
    // reconnect. Dropping it there would let the daemon re-stamp the route with a
    // weaker principal than it was originally bound under — a silent post-reconnect
    // trust downgrade. This drives the bulk reopenCachedRoutes path (not the lazy
    // per-call openCachedRoute path) by having a route already installed before the
    // connection drops.
    const { connFile } = tempConnectionFile();
    const consumerIdentity = { module_id: "reserved-module", launch_nonce: "nonce-abc" };
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async () => {},
    });

    try {
      // First call installs the cached route on connection 1 (carrying the identity).
      await expect(
        client.call("managed-provider", "echo", { n: 1 }, { consumerIdentity }),
      ).resolves.toEqual({ method: "echo", params: { n: 1 } });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({ stats: secondStats });
      writeConnectionFile(connFile, second.port);

      // Second call triggers reconnect + bulk reopen of the installed route.
      await expect(
        client.call("managed-provider", "echo", { n: 2 }, { consumerIdentity }),
      ).resolves.toEqual({ method: "echo", params: { n: 2 } });

      // The reopen on connection 2 must carry the identical consumer_identity.
      expect(firstStats.routeOpenConsumerIdentities).toEqual([consumerIdentity]);
      expect(secondStats.routeOpenConsumerIdentities).toEqual([consumerIdentity]);
    } finally {
      client.close();
    }
  });

  test("surfaces outcome_unknown and does not retry after bytes were written but no response arrived", async () => {
    const { connFile } = tempConnectionFile();
    const sleeps: number[] = [];
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "drop" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: { baseMs: 1, capMs: 1, maxAttempts: 1 },
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      await expect(client.call("managed-provider", "mutate", { n: 1 })).rejects.toMatchObject({
        kind: "outcome_unknown",
      });
      expect(stats.dataRequests).toBe(1);
      expect(stats.dataBodies).toEqual([{ method: "mutate", params: { n: 1 } }]);
      expect(sleeps).toEqual([]);
    } finally {
      client.close();
    }
  });

  test("wraps a module Error frame as a terminal managed call error", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "error" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      await expect(client.call("managed-provider", "explode", {})).rejects.toMatchObject({
        kind: "terminal",
        code: "module_boom",
      });
      expect(stats.dataRequests).toBe(1);
    } finally {
      client.close();
    }
  });

  test("uses capped reconnect backoff for transient reconnect failures", async () => {
    const { connFile } = tempConnectionFile();
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);
    const sleeps: number[] = [];

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      await client.call("managed-provider", "echo", { n: 1 });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const refusedPort = await closedPort();
      writeConnectionFile(connFile, refusedPort);

      await expect(client.call("managed-provider", "echo", { n: 2 })).rejects.toMatchObject({
        kind: "not_sent",
      });
      expect(sleeps).toEqual([5, 10, 10]);
    } finally {
      client.close();
    }
  });

  test("does not retry a non-transient route re-open error after reconnect", async () => {
    const { connFile } = tempConnectionFile();
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);
    const sleeps: number[] = [];

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      await client.call("managed-provider", "echo", { n: 1 });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({
        stats: secondStats,
        routeOpenError: { code: "route_rejected", message: "provider rejected route" },
      });
      writeConnectionFile(connFile, second.port);

      await expect(client.call("managed-provider", "echo", { n: 2 })).rejects.toMatchObject({
        kind: "terminal",
        code: "route_rejected",
      });
      expect(secondStats.routeOpens).toBe(1);
      expect(sleeps).toEqual([]);
    } finally {
      client.close();
    }
  });
});

async function startFakeDaemon(options: FakeDaemonOptions): Promise<FakeDaemon> {
  const sockets = new Set<Socket>();
  const server = createServer((socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    void handleFakeConnection(socket, options).catch(() => socket.destroy());
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
      for (const socket of sockets) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
  daemons.push(daemon);
  return daemon;
}

async function handleFakeConnection(socket: Socket, options: FakeDaemonOptions): Promise<void> {
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 5_000;
  await authenticateFakeServer(reader, socket, deadline);
  let routeChannel = 41;

  for (;;) {
    const frame = await readFrame(reader, deadline);
    if (frame.header.ty !== FrameType.Request) continue;

    if (frame.header.channel === 0) {
      const request = parseJson(frame.body) as { op?: string };
      if (request.op === "catalog.list") {
        await writeFrame(socket, responseFrame(frame, { op: "catalog.list", modules: [] }), deadline);
      } else if (request.op === "route.open") {
        options.stats.routeOpens += 1;
        options.stats.routeOpenConsumerIdentities.push(
          (request as { consumer_identity?: unknown }).consumer_identity,
        );
        if (options.routeOpenError) {
          await writeFrame(socket, errorFrame(frame, options.routeOpenError), deadline);
        } else {
          await writeFrame(socket, responseFrame(frame, { op: "route.open", route_channel: routeChannel++ }), deadline);
        }
      }
      continue;
    }

    options.stats.dataRequests += 1;
    const body = parseJson(frame.body);
    options.stats.dataBodies.push(body);

    if (options.dataMode === "drop") {
      socket.destroy();
      return;
    }
    if (options.dataMode === "error") {
      await writeFrame(socket, errorFrame(frame, { code: "module_boom", message: "boom" }), deadline);
    } else {
      await writeFrame(socket, responseFrame(frame, body), deadline);
    }

    if (options.closeAfterDataResponses !== undefined && options.stats.dataRequests >= options.closeAfterDataResponses) {
      socket.destroy();
      return;
    }
  }
}

function responseFrame(request: Frame, body: unknown): Frame {
  return buildFrame(
    FrameType.Response,
    buildFlags(false, Priority.Interactive, false),
    request.header.channel,
    request.header.corr,
    encodeJson(body),
  );
}

function errorFrame(request: Frame, body: { code: string; message: string }): Frame {
  return buildFrame(
    FrameType.Error,
    buildFlags(false, Priority.Interactive, false),
    request.header.channel,
    request.header.corr,
    encodeJson(body),
  );
}

async function authenticateFakeServer(reader: SocketReader, socket: Socket, deadline: number): Promise<void> {
  const hello = await readAuthMessage<{ client_nonce: number[]; role: string }>(reader, deadline);
  expect(hello.role).toBe("client");
  const clientNonce = Uint8Array.from(hello.client_nonce);
  const serverProof = computeProof(KEY, SERVER_PROOF_DOMAIN, clientNonce, SERVER_NONCE, DAEMON_ID);
  await writeAuthMessage(
    socket,
    {
      daemon_id: Array.from(DAEMON_ID),
      server_nonce: Array.from(SERVER_NONCE),
      daemon_ver: "fake-subc",
      server_proof: Array.from(serverProof),
    },
    deadline,
  );

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

async function readFrame(reader: SocketReader, deadline: number): Promise<Frame> {
  const header = decodeHeader(await reader.readExact(HEADER_LEN, deadline));
  const body = header.len === 0 ? new Uint8Array(0) : await reader.readExact(header.len, deadline);
  return { header, body };
}

async function writeFrame(socket: Socket, frame: Frame, deadline: number): Promise<void> {
  await writeAll(socket, encodeFrame(frame), deadline);
}

function encodeJson(value: unknown): Uint8Array {
  return new Uint8Array(Buffer.from(JSON.stringify(value), "utf8"));
}

function parseJson(body: Uint8Array): unknown {
  return JSON.parse(Buffer.from(body).toString("utf8"));
}

async function writeAll(socket: Socket, bytes: Uint8Array, deadline: number): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("timed out writing fake daemon bytes")), Math.max(0, deadline - Date.now()));
    socket.write(Buffer.from(bytes), (err) => {
      clearTimeout(timer);
      if (err) reject(err);
      else resolve();
    });
  });
}

interface Waiter {
  need: number;
  resolve: (bytes: Uint8Array) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout> | null;
}

class SocketReader {
  private chunks: Buffer[] = [];
  private buffered = 0;
  private waiter: Waiter | null = null;
  private closedErr: Error | null = null;

  constructor(socket: Socket) {
    socket.on("data", (chunk: Buffer) => {
      this.chunks.push(chunk);
      this.buffered += chunk.length;
      this.tryServe();
    });
    const fail = (err: Error) => {
      if (!this.closedErr) this.closedErr = err;
      this.tryServe();
    };
    socket.on("error", (err) => fail(err instanceof Error ? err : new Error(String(err))));
    socket.on("end", () => fail(new Error("fake daemon socket ended")));
    socket.on("close", () => fail(new Error("fake daemon socket closed")));
  }

  readExact(n: number, deadline: number): Promise<Uint8Array> {
    if (this.waiter) return Promise.reject(new Error("concurrent readExact is not supported"));
    if (n === 0) return Promise.resolve(new Uint8Array(0));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiter = null;
        reject(new Error(`timed out waiting for ${n} fake daemon bytes`));
      }, Math.max(0, deadline - Date.now()));
      this.waiter = { need: n, resolve, reject, timer };
      this.tryServe();
    });
  }

  private tryServe(): void {
    const waiter = this.waiter;
    if (!waiter) return;
    if (this.buffered >= waiter.need) {
      const out = this.take(waiter.need);
      this.waiter = null;
      if (waiter.timer) clearTimeout(waiter.timer);
      waiter.resolve(out);
      return;
    }
    if (this.closedErr) {
      this.waiter = null;
      if (waiter.timer) clearTimeout(waiter.timer);
      waiter.reject(this.closedErr);
    }
  }

  private take(n: number): Uint8Array {
    const out = Buffer.allocUnsafe(n);
    let off = 0;
    while (off < n) {
      const head = this.chunks[0]!;
      const want = n - off;
      if (head.length <= want) {
        head.copy(out, off);
        off += head.length;
        this.chunks.shift();
      } else {
        head.copy(out, off, 0, want);
        this.chunks[0] = head.subarray(want);
        off += want;
      }
    }
    this.buffered -= n;
    return out;
  }
}

function tempConnectionFile(): { dir: string; connFile: string } {
  const dir = mkdtempSync(join(tmpdir(), "subc-managed-client-"));
  tempDirs.push(dir);
  return { dir, connFile: join(dir, "subc-connection.json") };
}

function writeConnectionFile(path: string, port: number): void {
  writeFileSync(
    path,
    JSON.stringify({
      schema: 1,
      endpoints: [{ host: "127.0.0.1", port }],
      key: Array.from(KEY),
      daemon_id: Array.from(DAEMON_ID),
      pid: process.pid,
      daemon_ver: "fake-subc",
    }),
    { mode: 0o600 },
  );
  chmodSync(path, 0o600);
}

async function closedPort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = (server.address() as AddressInfo).port;
  await new Promise<void>((resolve) => server.close(() => resolve()));
  return port;
}

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
