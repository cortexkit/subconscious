// Live evidence harness for module-id reservation (`reserved: true`) at HELLO.
//
// Runs the three-arm ladder CKCRED's acceptance requires against a LIVE daemon
// and prints refusal CODES verbatim, because the code is the discriminator:
// the reserved check precedes the duplicate-id check, so a forged/absent nonce
// on an occupied name must read `reserved_module`, never `duplicate_module_id`
// (which would prove nothing about the gate). The POSITIVE arm — the real
// module registering with its daemon-minted nonce — is observed via
// catalog.list rather than forged here: only the daemon-spawned process holds
// the real nonce, which is the property under test.
//
// Usage: bun clients/subc-client/tests/reserved-proof.ts <module_id>
// Deliberately NOT a test-suite file: it targets production state and is run
// by hand for a recorded transcript.
import { authenticateClient } from "../src/auth";
import { SubcSocket } from "../src/socket";
import { readConnectionFile } from "../src/connection-file";
import { buildFrame, encodeFrame, decodeHeader, FrameType, HEADER_LEN, buildFlags, Priority } from "../src/envelope";

const moduleId = process.argv[2] ?? "prefrontal-core";
const connPath = `${process.env.HOME}/.local/share/cortexkit/run/subc-connection.json`;
const conn = await readConnectionFile(connPath);
const endpoint = conn.endpoints[0];
if (endpoint === undefined) {
  // A connection file with zero endpoints has nothing to probe against;
  // refuse loudly rather than letting strict-null narrowing hide the case.
  throw new Error(`connection file ${connPath} lists no endpoints`);
}
// Top-level narrowing does not cross into function bodies (TS cannot prove
// call order), so hand the probe an already-narrowed binding.
const probeTarget: { host: string; port: number } = endpoint;

function helloBody(launchNonce: string | undefined): Uint8Array {
  const manifest = {
    module_id: moduleId,
    module_version: "0.0.0-reserved-proof",
    protocol_ver: 2,
    trust_tier: "first_party",
    provides: [],
    consumes: [],
    bindings: {
      storage: { kind: "sqlite", scope: "project", owns_schema: false },
      vault_grants: [],
      identity: { requires: [], optional: [] },
    },
  };
  const body: Record<string, unknown> = { manifest, protocol_ver: 2 };
  if (launchNonce !== undefined) body.launch_nonce = launchNonce;
  return new TextEncoder().encode(JSON.stringify(body));
}

async function probe(label: string, nonce: string | undefined): Promise<void> {
  const deadline = Date.now() + 5000;
  const socket = await SubcSocket.connect(probeTarget.host, probeTarget.port, deadline);
  await authenticateClient(socket, conn, deadline);
  const hello = buildFrame(
    FrameType.Hello,
    buildFlags(false, Priority.Interactive, false),
    0,
    0,
    1n,
    helloBody(nonce),
  );
  await socket.write(encodeFrame(hello), deadline);
  const frame = await socket.readFrame(deadline, deadline);
  const header = frame.header;
  const bodyBytes = frame.body;
  const body = JSON.parse(new TextDecoder().decode(bodyBytes));
  const ty = FrameType[header.ty];
  if (header.ty === FrameType.Error) {
    console.log(`${label}: ${ty} code=${body.code} message=${JSON.stringify(body.message)}`);
  } else {
    console.log(`${label}: ${ty} ${JSON.stringify(body).slice(0, 160)}`);
  }
  socket.close();
}

console.log(`# reserved-proof for module_id=${moduleId} at ${new Date().toISOString()}`);
console.log(`# daemon: ${connPath} (${endpoint.host}:${endpoint.port}, ver=${conn.daemonVer ?? "?"})`);
await probe("ARM 1 forged-nonce", "forged-nonce-definitely-wrong");
await probe("ARM 2 absent-nonce", undefined);
console.log("# ARM 3 positive: recorded separately from catalog.list (the real module's own registration)");
