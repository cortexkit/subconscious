import { expect, test } from "bun:test";
import { createServer, type AddressInfo, type Socket } from "node:net";
import { cpSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { join, resolve } from "node:path";
import { SubcSocket, SocketClosedError } from "../src/socket";
import { computeProof, SERVER_PROOF_DOMAIN, CLIENT_AUTH_DOMAIN } from "../src/auth";
import { buildFrame, encodeFrame, FrameType, PROTOCOL_VERSION } from "../src/envelope";

const key = new Uint8Array(32).fill(71), id = new Uint8Array(16).fill(17), nonce = new Uint8Array(32).fill(39);
const fixture = fileURLToPath(new URL("fixtures/registration-child.ts", import.meta.url));
const launcher = fileURLToPath(new URL("fixtures/registration-launcher.py", import.meta.url));
const env = {...process.env};
for (const name of Object.keys(env)) if (name.startsWith("SUBC_")) delete env[name];
mkdirSync(resolve("target/scratch"), {recursive: true});

async function auth(sock: SubcSocket): Promise<void> {
  const deadline = Date.now() + 8000;
  async function read(): Promise<Record<string, number[]>> {
    const header = await sock.readExact(4, deadline);
    const len = Buffer.from(header).readUInt32LE();
    return JSON.parse(Buffer.from(await sock.readExact(len, deadline)).toString());
  }
  const hello = await read();
  const clientNonce = Uint8Array.from(hello.client_nonce!);
  const body = Buffer.from(JSON.stringify({daemon_id: [...id], server_nonce: [...nonce], daemon_ver: "fixture",
    server_proof: [...computeProof(key, SERVER_PROOF_DOMAIN, clientNonce, nonce, id)]}));
  const length = Buffer.alloc(4); length.writeUInt32LE(body.length);
  await sock.write(Buffer.concat([length, body]), deadline);
  const answer = await read();
  expect(answer.client_auth).toEqual([...computeProof(key, CLIENT_AUTH_DOMAIN, clientNonce, nonce, id)]);
}

async function run(mode: string): Promise<{code: number; out: string; err: string; operations: string[]; presented: string[]; purposes: string[]}> {
  const dir = mkdtempSync(resolve("target/scratch/registration-"));
  const operations: string[] = [], presented: string[] = [], purposes: string[] = [], errors: unknown[] = [];
  const sockets: Socket[] = [];
  let sequence = 0;
  let directChildren = 0;
  let routeRequests = 0;
  const minted = new Map<string, string>();
  const fallback = mode.startsWith("fallback-");
  async function peer(socket: Socket): Promise<void> {
    const sock = SubcSocket.adopt(socket);
    await auth(sock);
    let bound = mode === "direct";
    let registered = false;
    for (;;) {
      const frame = await sock.readFrame(Date.now() + 8000, Date.now() + 8000);
      if (frame.header.ty === FrameType.Goodbye) return;
      const body = JSON.parse(Buffer.from(frame.body).toString()) as Record<string, unknown>;
      const op = String(body.op ?? "HELLO");
      operations.push(op);
      async function reply(ty: FrameType, value: unknown): Promise<void> {
        await sock.write(encodeFrame(buildFrame(ty, 0, 0, 0, frame.header.corr, Buffer.from(JSON.stringify(value)))), Date.now() + 8000);
      }
      if (frame.header.ty === FrameType.Hello) {
        expect((body.manifest as {registration_channel: boolean}).registration_channel).toBe(true);
        expect(body.launch_nonce).toBe(fallback ? (mode === "fallback-explicit" ? "explicit-nonce" : "inherited-nonce") : undefined);
        registered = true;
        if (mode === "before-hello") { socket.end(); return; }
        await reply(FrameType.HelloAck, {negotiated_ver: PROTOCOL_VERSION, subc_ops: ["claim.mint", "catalog.list"]});
        if (mode === "eof") {
          // A PONG proves the provider consumed HELLO_ACK and started serving.
          // Windows disconnect discards unread pipe bytes, unlike Unix EOF.
          await sock.write(encodeFrame(buildFrame(FrameType.Ping, 0, 0, 0, 991n, new Uint8Array())), Date.now() + 8000);
          const pong = await sock.readFrame(Date.now() + 8000, Date.now() + 8000);
          expect(pong.header.ty).toBe(FrameType.Pong);
          expect(pong.header.corr).toBe(991n);
          socket.end(); return;
        }
      } else if (fallback && op === "server.describe") {
        await reply(FrameType.Response, {op, capabilities: []});
      } else if (fallback && op === "route.open") {
        const explicit = mode === "fallback-explicit" || (mode === "fallback-consumer" && routeRequests > 0);
        routeRequests++;
        expect(body.consumer_identity).toEqual({module_id: "test-channel", launch_nonce: explicit ? "route-nonce" : "inherited-nonce"});
        await reply(FrameType.Error, {code: "target_not_found", message: "fixture target"});
      } else if (op === "claim.mint") {
        expect(registered).toBe(true);
        if (mode === "mint-refused") {
          await reply(FrameType.Error, {code: "claim_mint_not_allowed", message: "fixture refusal"});
          continue;
        }
        const claim = (++sequence).toString().padStart(22, "X");
        minted.set(claim, String(body.purpose));
        purposes.push(String(body.purpose));
        await reply(FrameType.Response, {op, claim, expires_at_ms: Date.now() + 30000});
      } else if (op === "claim.present") {
        expect(registered).toBe(false);
        expect(bound).toBe(false);
        expect(minted.has(String(body.claim))).toBe(true);
        expect(presented).not.toContain(String(body.claim));
        presented.push(String(body.claim));
        if (mode === "present-refused") {
          await reply(FrameType.Error, {code: "claim_invalid", message: "fixture refusal"});
          continue;
        }
        bound = true;
        await reply(FrameType.Response, {op});
      } else {
        if (!bound && mode === "success" && !registered && op === "catalog.list") {
          directChildren++;
          expect(directChildren).toBe(1);
          operations.push("direct.catalog.list");
          bound = true;
        }
        expect(bound).toBe(true);
        expect(op).toBe("catalog.list");
        await reply(FrameType.Response, {op, modules: []});
      }
    }
  }
  const server = createServer(socket => {
    sockets.push(socket);
    void peer(socket).catch(error => { if (!(error instanceof SocketClosedError) && (error as {code?: string}).code !== "ECONNRESET") errors.push(error); });
  });
  await new Promise<void>(r => server.listen(0, "127.0.0.1", r));
  const port = (server.address() as AddressInfo).port;
  const connectionFile = join(dir, "connection.json");
  writeFileSync(connectionFile, JSON.stringify({schema: 1, daemon_id: [...id], key: [...key], pid: process.pid, endpoints: [{host: "127.0.0.1", port}]}), {mode: 0o600});
  let childFixture = fixture;
  if (mode === "fallback-missing") {
    cpSync(fileURLToPath(new URL("../src", import.meta.url)), join(dir, "src"), {recursive: true});
    cpSync(fileURLToPath(new URL("fixtures", import.meta.url)), join(dir, "tests/fixtures"), {recursive: true});
    childFixture = join(dir, "tests/fixtures/registration-child.ts");
  }
  const args = [process.execPath, childFixture, mode, connectionFile];
  const command = mode === "direct" ? args : [process.platform === "win32" ? "python" : "python3", launcher, String(port), ...args];
  const child = Bun.spawn(command, {env: {...env, SUBC_MODULE_ID: "test-channel", ...(mode === "direct" ? {SUBC_REGISTRATION_FD: "999:1", SUBC_REGISTRATION_PIPE: "\\\\.\\pipe\\subc-test-absent", SUBC_DAEMON_PID: "1"} : {})}, stdout: "pipe", stderr: "pipe"});
  const timer = setTimeout(() => child.kill(), 10000);
  try {
    const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
    if (code !== (mode === "eof" ? 1 : 0) || errors.length > 0) throw new Error(`child failed (${mode}, ${code}): ${err}; peer errors: ${errors.map(String).join("; ")}`);
    return {code, out, err, operations, presented, purposes};
  } finally {
    clearTimeout(timer);
    for (const socket of sockets) socket.destroy();
    server.close(); rmSync(dir, {recursive: true, force: true});
  }
}

test("adopt fails closed when a named registration channel cannot load its helper", async () => {
  const dir = mkdtempSync(resolve("target/scratch/no-native-"));
  cpSync(fileURLToPath(new URL("../src", import.meta.url)), join(dir, "src"), {recursive: true});
  try {
    const file = join(dir, "src/registration-channel.ts");
    for (const named of [false, true]) {
      const script = `
        const {registrationOwner}=await import(${JSON.stringify(file)});
        const {ClaimError}=await import(${JSON.stringify(join(dir, "src/claim.ts"))});
        const o=registrationOwner();
        for(let i=0;i<2;i++) {
          let caught;
          try { o.adopt(); } catch(error) { caught=error; }
          if (${named}) {
            if(!(caught instanceof ClaimError) || caught.code!=='registration_helper_unavailable'
              || typeof caught.cause?.message !== 'string' || !caught.message.includes(caught.cause.message)
              || !caught.message.includes('Cannot find module')) throw new Error('missing helper did not fail closed: '+caught);
          } else if(caught || o.supervised || o.take()!==undefined) throw new Error('plain client loaded helper');
        }
        console.log('ADOPTION_CHECKED');`;
      const child = Bun.spawn([process.execPath, "-e", script], {env: {...env, ...(named ? {SUBC_REGISTRATION_FD: "999:1", SUBC_REGISTRATION_PIPE: "\\\\.\\pipe\\subc-test-absent", SUBC_DAEMON_PID: "1"} : {})}, stdout: "pipe", stderr: "pipe"});
      const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
      expect({code, err}).toEqual({code: 0, err: ""});
      expect(out).toBe("ADOPTION_CHECKED\n");
    }
  } finally { rmSync(dir, {recursive: true, force: true, maxRetries: 10, retryDelay: 100}); }
});

test("import neither loads the native helper nor validates inherited registration metadata", async () => {
  const dir = mkdtempSync(resolve("target/scratch/lazy-registration-"));
  cpSync(fileURLToPath(new URL("../src", import.meta.url)), join(dir, "src"), {recursive: true});
  mkdirSync(join(dir, "native"));
  writeFileSync(join(dir, "native/index.js"), `console.warn('NATIVE_LOADED'); module.exports = { supervised() { console.warn('VALIDATED'); return false; }, registrationRefusal() { return 'refused'; } };`);
  try {
    const script = `await import(${JSON.stringify(join(dir, "src/index.ts"))}); console.log('IMPORTED');`;
    const child = Bun.spawn([process.execPath, "-e", script], {env: {...env, SUBC_REGISTRATION_FD: "999:1", SUBC_REGISTRATION_PIPE: "unused"}, stdout: "pipe", stderr: "pipe"});
    expect(await child.exited).toBe(0);
    expect(await new Response(child.stdout).text()).toBe("IMPORTED\n");
    expect(await new Response(child.stderr).text()).toBe("");
  } finally { rmSync(dir, {recursive: true, force: true}); }
});

test("adopt returns quietly when the loaded helper rejects a named channel as not ours", async () => {
  const file = fileURLToPath(new URL("../src/registration-channel.ts", import.meta.url));
  const script = `
    const {registrationOwner}=await import(${JSON.stringify(file)});
    const owner=registrationOwner();
    owner.adopt(); owner.adopt();
    if(owner.supervised || owner.take()!==undefined) throw new Error('unexpected adoption');
    console.log('NOT_SUPERVISED');`;
  const child = Bun.spawn([process.execPath, "-e", script], {env: {...env, SUBC_REGISTRATION_FD: "999:1",
    SUBC_REGISTRATION_PIPE: "\\\\.\\pipe\\subc-test-absent", SUBC_DAEMON_PID: "1"}, stdout: "pipe", stderr: "pipe"});
  const [code, out, err] = await Promise.all([child.exited, new Response(child.stdout).text(), new Response(child.stderr).text()]);
  expect({code, err}).toEqual({code: 0, err: ""});
  expect(out).toBe("NOT_SUPERVISED\n");
});

test("a missing helper refuses provider registration instead of falling back to the launch nonce", async () => {
  const result = await run("fallback-missing");
  expect(result.out).toContain("MISSING_HELPER_REFUSED");
  expect(result.operations).toEqual([]);
  expect(result.err).toBe("");
}, 15000);

for (const mode of ["fallback-refused", "fallback-explicit", "fallback-consumer", ...(process.platform === "win32" ? [] : ["fallback-leaked"])]) {
  test(`${mode}: non-adopted provider keeps TCP HELLO nonce and route identity`, async () => {
    const result = await run(mode);
    expect(result.out).toContain("FALLBACK_OK");
    expect(result.operations.filter(op => op === "HELLO")).toHaveLength(mode === "fallback-leaked" ? 2 : mode === "fallback-consumer" ? 0 : 1);
    expect(result.operations).toContain("route.open");
    expect(result.operations).not.toContain("claim.mint");
    expect(result.err).toBe("");
  }, 15000);
}

test("adopted provider close does not exit the process", async () => {
  const result = await run("close");
  expect(result.out).toContain("LOCAL_CLOSE_OK");
}, 15000);

test("EOF before registration does not exit the process", async () => {
  const result = await run("before-hello");
  expect(result.out).toContain("REGISTRATION_FAILED_OK");
}, 15000);

test("an inherited registration variable without its descriptor connects direct", async () => {
  const result = await run("direct");
  expect(result.err).not.toContain("claim");
  expect(result.code).toBe(0); expect(result.out).toContain("DIRECT_OK");
  expect(result.operations).toEqual(["catalog.list"]);
}, 15000);

test("adopted registration EOF ends the process even with reconnect requested", async () => {
  const result = await run("eof");
  expect(result.code).toBe(1); expect(result.out).toContain("CLOSED_HOOK");
  expect(result.operations).toEqual(["HELLO"]);
}, 15000);

for (const mode of ["mint-refused", "present-refused"]) {
  test(`after adoption ${mode} fails the consumer and never goes direct`, async () => {
    const result = await run(mode);
    expect(result.code).toBe(0); expect(result.out).toContain("REFUSED_OK");
    expect(result.operations).not.toContain("catalog.list");
    if (mode === "present-refused") expect(result.presented).toHaveLength(1);
  }, 15000);
}

test("one channel owner pools distinct self claims, refuses identity override and hands helper a one-read pipe", async () => {
  const result = await run("success");
  expect(result.code).toBe(0); expect(result.out).toContain("CHANNEL_OK");
  expect(result.presented).toHaveLength(3);
  expect(result.operations).toContain("direct.catalog.list");
  expect(result.purposes).toEqual(["self", "self", "self", "self", "helper"]);
  expect(result.err).not.toContain(result.presented[0]!);
}, 15000);
