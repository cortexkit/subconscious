// Simulate an older module: register over TCP without adopting fd 4, then
// deliberately pass that still-inheritable endpoint to a new-SDK descendant.
import { spawn } from "node:child_process";
import { once } from "node:events";
import { readSync, writeSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { readConnectionFile } from "../../src/connection-file";
import { authenticateClient } from "../../src/auth";
import { SubcSocket } from "../../src/socket";
import { buildFrame, encodeFrame, FrameType, PROTOCOL_VERSION } from "../../src/envelope";

const connectionFile = process.argv[3]!;
const conn = await readConnectionFile(connectionFile);
const deadline = Date.now() + 8000;
const sock = await SubcSocket.connect(conn.endpoints[0]!.host, conn.endpoints[0]!.port, deadline);
await authenticateClient(sock, conn, deadline);
await sock.write(encodeFrame(buildFrame(FrameType.Hello, 0, 0, 0, 1n, Buffer.from(JSON.stringify({
  manifest: {module_id: "test-channel", module_version: "1.0.0", protocol_ver: PROTOCOL_VERSION,
    trust_tier: "first_party", provides: [], consumes: [], registration_channel: true,
    bindings: {storage: {kind: "sqlite", scope: "project", owns_schema: false}, vault_grants: [], identity: {requires: [], optional: []}}},
  protocol_ver: PROTOCOL_VERSION, launch_nonce: "inherited-nonce",
})))), deadline);
if ((await sock.readFrame(deadline, deadline)).header.ty !== FrameType.HelloAck) throw new Error("legacy registration failed");
writeSync(4, "REGISTERED");
if (readSync(4, Buffer.alloc(1), 0, 1, null) !== 0) throw new Error("channel was not retired");
const [nonceFd, nonceInode] = process.env.SUBC_LAUNCH_NONCE_FD!.split(":");
const child = spawn(process.execPath, [fileURLToPath(new URL("registration-child.ts", import.meta.url)), "fallback-leaked", connectionFile], {
  env: {...process.env, SUBC_LAUNCH_NONCE_FD: `3:${nonceInode}`}, stdio: ["ignore", "inherit", "inherit", Number(nonceFd), 4],
});
const [code] = await once(child, "exit");
sock.close();
process.exit(code === 0 ? 0 : 1);
