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

// RequestOptions.signal is the only way a consumer can cancel its own in-flight
// request: corr is minted inside the client and never exposed, so cancel(handle,
// corr) is reachable today only by code that minted the corr itself. prefrontal
// needs this to interrupt an in-flight computer-plane sequence when a person hits
// stop, and cerebellum needs the frame to arrive with the right corr.
//
// Cancellation is best-effort by the wire's construction -- the daemon forwards on
// a live route and drops on a departed one, with no acknowledgement at any layer --
// so these tests assert what the CLIENT emits, which is the only part it owns.

const KEY = Uint8Array.from(Array(32).fill(0x4c));
const DAEMON_ID = Uint8Array.from(Array(16).fill(0x7d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => 0xa0 + i));
const IDENTITY: BindIdentity = { project_root: "/tmp/subc-cancel-test", harness: "bun", session: "s1" };
const TOOL_TARGET = { kind: "tool_provider", module_id: "cerebellum" } as const;

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
  /** Every CANCEL the daemon saw, in arrival order. */
  cancels: { channel: number; epoch: number; corr: bigint }[];
  /** Corr of each REQUEST seen on a route channel, in order. */
  requestCorrs: bigint[];
  routeFrames: FrameType[];
  routeOpenGate?: Promise<void>;
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

describe("request cancellation via AbortSignal", () => {
  test("a signal firing mid-flight emits CANCEL carrying that request's own corr", async () => {
    const { client, daemon } = await connectClient();
    const handle = await client.routeOpen(TOOL_TARGET, IDENTITY);

    // Hold the reply so the request is genuinely in flight when the signal fires.
    const dataGate = deferred<void>();
    daemon.state.dataGate = dataGate.promise;

    const controller = new AbortController();
    const reqPromise = client.request(handle, { name: "drag", arguments: {} }, { signal: controller.signal });
    await waitFor(() => daemon.state.requestCorrs.length === 1, "request reaches the daemon");

    controller.abort();
    await waitFor(() => daemon.state.cancels.length === 1, "CANCEL reaches the daemon");

    const cancel = daemon.state.cancels[0]!;
    const requestCorr = daemon.state.requestCorrs[0]!;
    expect(cancel.corr).toBe(requestCorr);
    expect(cancel.channel).toBe(handle.channel);
    expect(cancel.epoch).toBe(handle.epoch);

    // The promise is NOT rejected by the cancel: only the module's answer settles
    // it, and a normal completion that beat the cancel is a legitimate outcome.
    dataGate.resolve();
    await reqPromise;
    client.close();
  });

  test("a signal firing after the reply has settled emits nothing", async () => {
    const { client, daemon } = await connectClient();
    const handle = await client.routeOpen(TOOL_TARGET, IDENTITY);

    const controller = new AbortController();
    await client.request(handle, { name: "quick", arguments: {} }, { signal: controller.signal });

    controller.abort();
    await new Promise((r) => setTimeout(r, 40));
    expect(daemon.state.cancels).toEqual([]);
    client.close();
  });

  test("an already-aborted signal still cancels the request it is passed to", async () => {
    // A caller whose signal fired between building the request and sending it
    // must not silently get an uncancellable request: addEventListener never
    // fires for an already-aborted signal, so this arm needs its own check.
    const { client, daemon } = await connectClient();
    const handle = await client.routeOpen(TOOL_TARGET, IDENTITY);

    const dataGate = deferred<void>();
    daemon.state.dataGate = dataGate.promise;

    const controller = new AbortController();
    controller.abort();
    const reqPromise = client.request(handle, { name: "late", arguments: {} }, { signal: controller.signal });
    try {
      await waitFor(() => daemon.state.routeFrames.length === 2, "REQUEST and CANCEL for an already-aborted signal");
      expect(daemon.state.routeFrames).toEqual([FrameType.Request, FrameType.Cancel]);
      expect(daemon.state.cancels[0]!.corr).toBe(daemon.state.requestCorrs[0]!);
    } finally {
      dataGate.resolve();
      await reqPromise;
      client.close();
    }
  });

  test("an already-aborted managed signal sends REQUEST before CANCEL", async () => {
    const { client, daemon } = await connectClient();
    const dataGate = deferred<void>();
    daemon.state.dataGate = dataGate.promise;
    const controller = new AbortController();
    controller.abort();
    const reqPromise = client.call(TOOL_TARGET.module_id, "late", {}, { signal: controller.signal });
    try {
      await waitFor(() => daemon.state.routeFrames.length === 2, "managed REQUEST and CANCEL");
      expect(daemon.state.routeFrames).toEqual([FrameType.Request, FrameType.Cancel]);
      expect(daemon.state.cancels[0]!.corr).toBe(daemon.state.requestCorrs[0]!);
    } finally {
      dataGate.resolve();
      await reqPromise;
      client.close();
    }
  });

  test("a settled request removes its abort listener from a reused signal", async () => {
    // A process-wide signal (prefrontal's stop button) is reused across many
    // requests. If each request's listener outlives its request, the signal
    // accumulates one closure per request forever.
    const { client, daemon } = await connectClient();
    const handle = await client.routeOpen(TOOL_TARGET, IDENTITY);

    const controller = new AbortController();
    const counts = countAbortListeners(controller.signal);

    const requests = 5;
    for (let i = 0; i < requests; i += 1) {
      await client.request(handle, { name: `call-${i}`, arguments: {} }, { signal: controller.signal });
    }
    expect(counts.added).toBe(requests);
    expect(counts.removed).toBe(requests);
    client.close();
  });

  test("a signal cancels only its own request, never a sibling on the same connection", async () => {
    // prefrontal holds ONE connection process-wide with unrelated requests in
    // flight on it (a board write, a work-graph read, a wake delivery). Cancelling
    // one must not disturb the others -- the property that makes the shared
    // connection safe.
    const { client, daemon } = await connectClient();
    const handle = await client.routeOpen(TOOL_TARGET, IDENTITY);

    const dataGate = deferred<void>();
    daemon.state.dataGate = dataGate.promise;

    const cancelled = new AbortController();
    const first = client.request(handle, { name: "sequence", arguments: {} }, { signal: cancelled.signal });
    const sibling = client.request(handle, { name: "board-write", arguments: {} });
    await waitFor(() => daemon.state.requestCorrs.length === 2, "both requests reach the daemon");

    cancelled.abort();
    await waitFor(() => daemon.state.cancels.length === 1, "one CANCEL");
    expect(daemon.state.cancels[0]!.corr).toBe(daemon.state.requestCorrs[0]!);
    expect(daemon.state.cancels.map((c) => c.corr)).not.toContain(daemon.state.requestCorrs[1]!);

    dataGate.resolve();
    await Promise.all([first, sibling]);
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
  const state: FakeState = { routeOpens: 0, goodbyeChannels: [], openedChannels: [], cancels: [], requestCorrs: [], routeFrames: [] };
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
    // Per-frame deadline: one connection-lifetime deadline would expire mid-test
    // and look like a client fault.
    const frame = await readFrame(reader, Date.now() + 10_000);
    if (frame.header.channel !== 0) state.routeFrames.push(frame.header.ty);
    if (frame.header.ty === FrameType.Goodbye) {
      state.goodbyeChannels.push(frame.header.channel);
      continue;
    }
    if (frame.header.ty === FrameType.Cancel) {
      state.cancels.push({ channel: frame.header.channel, epoch: frame.header.epoch, corr: frame.header.corr });
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
        await writeFrame(socket, responseFrame(frame, { op: "route.open", route_channel: channel, route_epoch: 1 }), Date.now() + 10_000);
      }
      continue;
    }

    // Data request on a route channel: echo it back, but NEVER block the read loop
    // on the gate -- a gated reply must not stop the daemon from reading the CANCEL
    // that arrives while the request is in flight, which is the whole case here.
    state.requestCorrs.push(frame.header.corr);
    const gate = state.dataGate;
    const reply = responseFrame(frame, parseJson(frame.body));
    void (async () => {
      if (gate) await gate;
      await writeFrame(socket, reply, Date.now() + 5_000).catch(() => undefined);
    })();
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

/** Wraps a signal so the test can count abort listeners added and removed. */
function countAbortListeners(signal: AbortSignal): { added: number; removed: number } {
  const counts = { added: 0, removed: 0 };
  const originalAdd = signal.addEventListener.bind(signal);
  const originalRemove = signal.removeEventListener.bind(signal);
  signal.addEventListener = ((type: string, listener: unknown, options?: unknown) => {
    if (type === "abort") counts.added += 1;
    originalAdd(type, listener as EventListener, options as AddEventListenerOptions);
  }) as typeof signal.addEventListener;
  signal.removeEventListener = ((type: string, listener: unknown, options?: unknown) => {
    if (type === "abort") counts.removed += 1;
    originalRemove(type, listener as EventListener, options as EventListenerOptions);
  }) as typeof signal.removeEventListener;
  return counts;
}

async function waitFor(predicate: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`timed out waiting for ${label}`);
}
