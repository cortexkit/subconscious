import { afterEach, describe, expect, test } from "bun:test";
import { createServer, type AddressInfo, type Socket } from "node:net";
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
  SubcClient,
  SERVER_PROOF_DOMAIN,
  type BindIdentity,
  type Frame,
} from "../src/index.js";

// closeRoute is the per-route teardown primitive AFT's thin plugin needs: one
// process serving many OC sessions, each a distinct (target, identity) route, must
// release a route on session-end without dropping the whole client. The load-bearing
// case is the close-vs-reopen RACE — a closeRoute landing while a route.open is in
// flight must WIN (the opened channel is GOODBYE'd, not installed), proven below with
// a daemon that gates route.open.

const KEY = Uint8Array.from(Array(32).fill(0x4c));
const DAEMON_ID = Uint8Array.from(Array(16).fill(0x7d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => 0xa0 + i));
const IDENTITY: BindIdentity = { project_root: "/tmp/subc-close-test", harness: "bun", session: "s1" };
const TOOL_TARGET = { kind: "tool_provider", module_id: "aft" } as const;
const MGMT_TARGET = { kind: "management_surface", module_id: "managed-provider" } as const;

const tempDirs: string[] = [];
const daemons: FakeDaemon[] = [];

interface Deferred<T> {
  promise: Promise<T>;
  resolve: (v: T) => void;
}
function deferred<T>(): Deferred<T> {
  let resolve!: (v: T) => void;
  const promise = new Promise<T>((r) => {
    resolve = r;
  });
  return { promise, resolve };
}

interface FakeState {
  routeOpens: number;
  goodbyeChannels: number[];
  openedChannels: number[];
  /** When set, route.open handling awaits this before replying (race control). */
  routeOpenGate?: Promise<void>;
  /** When set, data-request handling awaits this before replying (drain control). */
  dataGate?: Promise<void>;
}

interface FakeDaemon {
  port: number;
  state: FakeState;
  stop(): Promise<void>;
}

afterEach(async () => {
  for (const daemon of daemons.splice(0)) await daemon.stop();
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("SubcClient.closeRoute", () => {
  test("sends a route GOODBYE and drops the cache so the next call re-opens", async () => {
    const { client, daemon } = await connectClient();
    await client.call("managed-provider", "echo", { n: 1 });
    expect(daemon.state.routeOpens).toBe(1);
    const channel = daemon.state.openedChannels[0]!;

    await client.closeManagedRoute(MGMT_TARGET, IDENTITY);
    await waitFor(() => daemon.state.goodbyeChannels.includes(channel), "module GOODBYE for the closed route");

    // A later call for the SAME key opens a FRESH route — closeRoute is not a tombstone.
    await client.call("managed-provider", "echo", { n: 2 });
    expect(daemon.state.routeOpens).toBe(2);
    client.close();
  });

  test("is an idempotent no-op when the route was never opened (and when called twice)", async () => {
    const { client, daemon } = await connectClient();
    // Never opened.
    await expect(client.closeManagedRoute(MGMT_TARGET, IDENTITY)).resolves.toBeUndefined();
    expect(daemon.state.goodbyeChannels).toEqual([]);

    await client.call("managed-provider", "echo", { n: 1 });
    await client.closeManagedRoute(MGMT_TARGET, IDENTITY);
    await waitFor(() => daemon.state.goodbyeChannels.length >= 1, "the route GOODBYE (one-way, best-effort)");
    // Second close is a no-op (entry already gone) — must not throw or double-GOODBYE.
    await expect(client.closeManagedRoute(MGMT_TARGET, IDENTITY)).resolves.toBeUndefined();
    await new Promise((r) => setTimeout(r, 30)); // give a stray second GOODBYE a chance to (wrongly) arrive.
    expect(daemon.state.goodbyeChannels.length).toBe(1);
    client.close();
  });

  test("closeRouteChannel tears down a raw routeOpen'd channel (the tool-route path)", async () => {
    const { client, daemon } = await connectClient();
    const handle = await client.routeOpen(TOOL_TARGET, IDENTITY);
    await client.request(handle, { name: "status", arguments: {} });
    await client.closeRouteChannel(handle);
    await waitFor(() => daemon.state.goodbyeChannels.includes(handle.channel), "GOODBYE for the raw tool route");
    client.close();
  });

  test("closeRoute WINS a race against an in-flight route.open (the channel is GOODBYE'd, not installed)", async () => {
    const { client, daemon } = await connectClient();
    // Gate route.open so the open hangs while we close.
    const gate = deferred<void>();
    daemon.state.routeOpenGate = gate.promise;

    // Fire a call() — it triggers openCachedRoute -> route.open, which now hangs.
    const callPromise = client.call("managed-provider", "echo", { n: 1 }).then(
      () => "resolved",
      (err) => err,
    );
    await waitFor(() => daemon.state.routeOpens >= 1, "route.open to be in flight");

    // Close while the open is in flight: channel is null, so closeRoute flips the
    // tombstone + removes the entry; the racing open must discard its channel.
    await client.closeManagedRoute(MGMT_TARGET, IDENTITY);

    // Release the gated route.open: the open completes, sees the tombstone, GOODBYEs
    // the channel it opened, and fails the call as RouteClosed.
    gate.resolve();
    const outcome = await callPromise;
    expect(outcome).toMatchObject({ code: "route_closed" });

    // The raced-open channel was GOODBYE'd (not leaked), and a fresh call re-opens.
    const racedChannel = daemon.state.openedChannels[0]!;
    await waitFor(() => daemon.state.goodbyeChannels.includes(racedChannel), "GOODBYE for the raced-open channel");
    daemon.state.routeOpenGate = undefined;
    await client.call("managed-provider", "echo", { n: 2 });
    expect(daemon.state.routeOpens).toBe(2);
    client.close();
  });

  test("drain waits for an in-flight unary to settle before tearing the route down", async () => {
    const { client, daemon } = await connectClient();
    const handle = await client.routeOpen(TOOL_TARGET, IDENTITY);

    // Gate the data response so the unary stays in flight.
    const dataGate = deferred<void>();
    daemon.state.dataGate = dataGate.promise;
    const reqPromise = client.request(handle, { name: "slow", arguments: {} });
    await waitFor(() => daemon.state.openedChannels.includes(handle.channel), "route open");

    // closeRouteChannel({ drain: true }) must NOT resolve while the unary is unsettled.
    let closeResolved = false;
    const closePromise = client.closeRouteChannel(handle, { drain: true }).then(() => {
      closeResolved = true;
    });
    await new Promise((r) => setTimeout(r, 30));
    expect(closeResolved).toBe(false); // still draining the in-flight unary.

    // Release the data response: the unary settles, then drain completes + GOODBYE fires.
    dataGate.resolve();
    await reqPromise;
    await closePromise;
    expect(closeResolved).toBe(true);
    await waitFor(() => daemon.state.goodbyeChannels.includes(handle.channel), "GOODBYE after drain completes");
    client.close();
  });
});

async function connectClient(): Promise<{ client: SubcClient; daemon: FakeDaemon }> {
  const daemon = await startFakeDaemon();
  const { connFile } = tempConnectionFile();
  writeConnectionFile(connFile, daemon.port);
  const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
  return { client, daemon };
}

async function startFakeDaemon(): Promise<FakeDaemon> {
  const state: FakeState = { routeOpens: 0, goodbyeChannels: [], openedChannels: [] };
  const sockets = new Set<Socket>();
  const server = createServer((socket) => {
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
    void handleConnection(socket, state).catch(() => socket.destroy());
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = (server.address() as AddressInfo).port;
  let stopped = false;
  const daemon: FakeDaemon = {
    port,
    state,
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

async function handleConnection(socket: Socket, state: FakeState): Promise<void> {
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 5_000;
  await authenticate(reader, socket, deadline);
  let nextChannel = 41;

  for (;;) {
    const frame = await readFrame(reader, deadline);
    if (frame.header.ty === FrameType.Goodbye) {
      state.goodbyeChannels.push(frame.header.channel);
      continue;
    }
    if (frame.header.ty !== FrameType.Request) continue;

    if (frame.header.channel === 0) {
      const request = parseJson(frame.body) as { op?: string };
      if (request.op === "route.open") {
        state.routeOpens += 1;
        const channel = nextChannel++;
        state.openedChannels.push(channel);
        if (state.routeOpenGate) await state.routeOpenGate;
        await writeFrame(socket, responseFrame(frame, { op: "route.open", route_channel: channel, route_epoch: 1 }), deadline);
      }
      continue;
    }

    // Data request on a route channel: echo it back (after the optional drain gate).
    if (state.dataGate) await state.dataGate;
    await writeFrame(socket, responseFrame(frame, parseJson(frame.body)), deadline);
  }
}

function responseFrame(request: Frame, body: unknown): Frame {
  return buildFrame(FrameType.Response, buildFlags(false, Priority.Interactive, false), request.header.channel, request.header.epoch, request.header.corr, encodeJson(body));
}

async function authenticate(reader: SocketReader, socket: Socket, deadline: number): Promise<void> {
  const hello = await readAuthMessage<{ client_nonce: number[]; role: string }>(reader, deadline);
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
  const dir = mkdtempSync(join(tmpdir(), "subc-close-route-"));
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

async function waitFor(predicate: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`timed out waiting for ${label}`);
}
