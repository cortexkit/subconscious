import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "bun:test";

import {
  CLIENT_AUTH_DOMAIN,
  computeProof,
  SERVER_PROOF_DOMAIN,
} from "../src/auth.js";
import {
  buildFlags,
  buildFrame,
  buildFrameWithVersion,
  decodeHeader,
  encodeFrame,
  FrameType,
  HEADER_LEN,
  HELLO_CORR,
  managementSurfaceManifest,
  Priority,
  PROTOCOL_VERSION,
  SubcProvider,
  type Frame,
  type ManifestInput,
} from "../src/index.js";

const KEY = Uint8Array.from(Array(32).fill(0x4b));
const DAEMON_ID = Uint8Array.from(Array(16).fill(0x6d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => i + 1));
const CONTROL_FLAGS = buildFlags(false, Priority.Passive, false);

describe("managementSurfaceManifest", () => {
  test("builds the minimal ManagementSurface manifest shape", () => {
    expect(
      managementSurfaceManifest({
        moduleId: "test-effect-provider",
        operations: ["echo", { name: "wake", kind: "mutate" }],
        moduleVersion: "1.2.3",
      }),
    ).toEqual({
      module_id: "test-effect-provider",
      module_version: "1.2.3",
      protocol_ver: PROTOCOL_VERSION,
      trust_tier: "first_party",
      provides: [
        {
          role: "management_surface",
          operations: [
            { name: "echo", kind: "query" },
            { name: "wake", kind: "mutate" },
          ],
          config_schema: { type: "object" },
          observability: [],
          identity_scope: [],
        },
      ],
      consumes: [],
      scheduled_tasks: [],
      bindings: {
        storage: { kind: "sqlite", scope: "project", owns_schema: false },
        config: { source: "subc_mediated", tiers: [], expansion: {} },
        vault_grants: [],
        identity: { requires: [], optional: [] },
      },
    });
  });
});

describe("SubcProvider serve loop", () => {
  test("replies to channel-0 Ping with Pong preserving version, flags, and corr", async () => {
    const manifest = managementSurfaceManifest({ moduleId: "ping-provider", operations: ["echo"] });
    const server = await listenFakeServer();
    const dir = mkdtempSync(join(tmpdir(), "subc-provider-ping-"));
    const connFile = writeConnectionFile(dir, server.port);

    let sawPong!: () => void;
    const pongSeen = new Promise<void>((resolve) => {
      sawPong = resolve;
    });
    const serverDone = new Promise<void>((resolve, reject) => {
      server.server.once("connection", (socket) => {
        void runPingPeer(socket, manifest, sawPong).then(resolve, reject);
      });
    });

    let provider: SubcProvider | undefined;
    try {
      provider = await SubcProvider.connect({
        connectionFile: connFile,
        manifest,
        handler: async (_routeChannel, body) => body,
      });
      await pongSeen;
      await provider.close();
      await serverDone;
    } finally {
      await provider?.close().catch(() => undefined);
      server.server.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });
});

async function listenFakeServer(): Promise<{ server: Server; port: number }> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address() as AddressInfo;
  return { server, port: address.port };
}

function writeConnectionFile(dir: string, port: number): string {
  const path = join(dir, "subc-connection.json");
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
  return path;
}

async function runPingPeer(socket: Socket, manifest: ManifestInput, sawPong: () => void): Promise<void> {
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 10_000;
  await authenticateFakeServer(reader, socket, deadline);

  const hello = await readFrame(reader, deadline);
  expect(hello.header.ty).toBe(FrameType.Hello);
  expect(hello.header.channel).toBe(0);
  expect(hello.header.corr).toBe(HELLO_CORR);
  expect(hello.header.flags).toBe(CONTROL_FLAGS);
  expect(Buffer.from(hello.body).toString("utf8")).toBe(
    JSON.stringify({ manifest, protocol_ver: PROTOCOL_VERSION, control_ops: null }),
  );

  await writeFrame(
    socket,
    buildFrameWithVersion(
      PROTOCOL_VERSION,
      FrameType.HelloAck,
      CONTROL_FLAGS,
      0,
      hello.header.corr,
      encodeJson({
        negotiated_ver: PROTOCOL_VERSION,
        subc_ops: ["server.describe", "catalog.list", "route.open", "route.poll"],
        subc_capabilities: ["manifest_registration_v1"],
      }),
    ),
    deadline,
  );

  await writeFrame(
    socket,
    buildFrame(FrameType.Ping, buildFlags(false, Priority.Interactive, false), 0, 77n, new Uint8Array(0)),
    deadline,
  );
  const pong = await readFrame(reader, deadline);
  expect(pong.header.ty).toBe(FrameType.Pong);
  expect(pong.header.ver).toBe(PROTOCOL_VERSION);
  expect(pong.header.channel).toBe(0);
  expect(pong.header.corr).toBe(77n);
  expect(pong.header.flags).toBe(buildFlags(false, Priority.Interactive, false));
  expect(pong.body.length).toBe(0);
  sawPong();

  const goodbye = await readFrame(reader, deadline);
  expect(goodbye.header.ty).toBe(FrameType.Goodbye);
  expect(goodbye.header.channel).toBe(0);
  socket.destroy();
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
