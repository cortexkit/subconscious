import { once } from "node:events";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { createServer, type AddressInfo, type Server } from "node:net";
import { join } from "node:path";
import { expect } from "bun:test";
import {
  buildFrame, CLIENT_AUTH_DOMAIN, computeProof, encodeFrame, FrameType, managementSurfaceManifest,
  PROTOCOL_VERSION, SERVER_PROOF_DOMAIN, SubcSocket, type Frame, type ModuleHelloAckBody,
  type ModuleHelloBody, type ModuleUpgrading,
} from "../src/index.js";

const key = new Uint8Array(32).fill(51);
const id = new Uint8Array(16).fill(61);
const serverNonce = new Uint8Array(32).fill(71);
const budget = () => Date.now() + 2000;

export function deferred<T>() {
  let resolve!: (value: T) => void;
  const promise = new Promise<T>((done) => { resolve = done; });
  return { promise, resolve };
}

class Connections {
  private readonly queue: SubcSocket[] = [];
  private readonly all: SubcSocket[] = [];
  private waiter = deferred<void>();
  readonly server: Server = createServer((raw) => {
    const socket = SubcSocket.adopt(raw);
    this.queue.push(socket);
    this.all.push(socket);
    this.waiter.resolve();
  });

  get count() { return this.all.length; }

  async accept() {
    while (this.queue.length === 0) {
      await this.waiter.promise;
      this.waiter = deferred<void>();
    }
    return this.queue.shift()!;
  }

  async close() {
    for (const socket of this.all) socket.close();
    if (this.server.listening) await new Promise<void>((done) => { this.server.close(() => done()); });
  }
}

export class UpgradePeer {
  readonly tcp = new Connections();
  readonly unix = new Connections();
  readonly manifest = managementSurfaceManifest({ moduleId: "upgrade-worker", operations: [] });
  readonly connectionFile: string;
  readonly unixPath: string;

  private constructor(private readonly dir: string) {
    this.connectionFile = join(dir, "conn.json");
    this.unixPath = join(dir, "local");
  }

  static async start(unix = true): Promise<UpgradePeer> {
    mkdirSync("target/scratch", { recursive: true });
    const peer = new UpgradePeer(mkdtempSync("target/scratch/up-"));
    await once(peer.tcp.server.listen(0, "127.0.0.1"), "listening");
    if (unix) await once(peer.unix.server.listen(peer.unixPath), "listening");
    peer.publish();
    return peer;
  }

  publish(path = this.unixPath, staleKey = false) {
    const address = this.tcp.server.address() as AddressInfo;
    writeFileSync(this.connectionFile, JSON.stringify({ schema: 1, key: [...key].map((v) => staleKey ? v + 1 : v),
      daemon_id: [...id], pid: process.pid, endpoints: [{ host: address.address, port: address.port }],
      local_endpoint: { kind: "unix", path },
    }), { mode: 0o600 });
  }

  async nextTcp() { const socket = await this.tcp.accept(); await authenticate(socket); return socket; }
  async nextUnix() { const socket = await this.unix.accept(); await authenticate(socket); return socket; }
  read(socket: SubcSocket) { return socket.readFrame(budget(), budget()); }
  write(socket: SubcSocket, ...frames: Frame[]) { return socket.write(Buffer.concat(frames.map(encodeFrame)), budget()); }
  ack(corr: bigint, fields: Partial<ModuleHelloAckBody> = {}) {
    return frame(FrameType.HelloAck, corr, { negotiated_ver: PROTOCOL_VERSION, subc_ops: ["scope.sync"], subc_capabilities: [], ...fields });
  }

  async stop() {
    await Promise.all([this.tcp.close(), this.unix.close()]);
    rmSync(this.dir, { force: true, recursive: true });
  }
}

export function frame(ty: FrameType, corr = 0n, body?: unknown, channel = 0, epoch = 0): Frame {
  return buildFrame(ty, 0, channel, epoch, corr, body === undefined ? new Uint8Array() : Buffer.from(JSON.stringify(body)));
}

export function notice(reconnectByMs = Date.now() + 2000): Frame {
  return frame(FrameType.Push, 0n, { op: "module.upgrading", reconnect_by_ms: reconnectByMs } satisfies ModuleUpgrading);
}

export function json(frame: Frame): Record<string, unknown> {
  return JSON.parse(Buffer.from(frame.body).toString());
}

export async function readHello(socket: SubcSocket): Promise<ModuleHelloBody> {
  const hello = await socket.readFrame(budget(), budget());
  expect(hello.header).toMatchObject({ ty: FrameType.Hello, channel: 0, corr: 1n });
  return json(hello) as unknown as ModuleHelloBody;
}

export async function authenticate(socket: SubcSocket): Promise<void> {
  const read = async () => {
    const len = Buffer.from(await socket.readExact(4, budget())).readUInt32LE();
    return JSON.parse(Buffer.from(await socket.readExact(len, budget())).toString());
  };
  const hello = await read();
  const clientNonce = Uint8Array.from(hello.client_nonce);
  const reply = Buffer.from(JSON.stringify({ daemon_id: [...id], server_nonce: [...serverNonce], daemon_ver: "test",
    server_proof: [...computeProof(key, SERVER_PROOF_DOMAIN, clientNonce, serverNonce, id)],
  }));
  const header = Buffer.alloc(4);
  header.writeUInt32LE(reply.length);
  await socket.write(Buffer.concat([header, reply]), budget());
  expect((await read()).client_auth).toEqual([...computeProof(key, CLIENT_AUTH_DOMAIN, clientNonce, serverNonce, id)]);
}
