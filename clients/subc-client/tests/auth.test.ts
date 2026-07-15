import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import {
  AuthError,
  authenticateClient,
  CLIENT_AUTH_DOMAIN,
  computeProof,
  PROOF_LEN,
  SERVER_PROOF_DOMAIN,
} from "../src/auth.js";
import type { ConnectionInfo } from "../src/connection-file.js";
import type { SubcSocket } from "../src/socket.js";

const KEY = Uint8Array.from(Array(32).fill(0xab));
const CN = Uint8Array.from(Array(32).fill(0x01));
const SN = Uint8Array.from(Array(32).fill(0x02));
const DID = Uint8Array.from(Array(16).fill(0x03));
const CONNECTION: ConnectionInfo = {
  schema: 1,
  endpoints: [{ host: "127.0.0.1", port: 8799 }],
  key: KEY,
  daemonId: DID,
  pid: 1,
  daemonVer: "test",
};

class ScriptedAuthSocket {
  private readonly inbound: Uint8Array;
  private offset = 0;

  constructor(message: unknown) {
    const body = Buffer.from(JSON.stringify(message), "utf8");
    const prefix = Buffer.alloc(4);
    prefix.writeUInt32LE(body.length);
    this.inbound = Buffer.concat([prefix, body]);
  }

  async readExact(n: number): Promise<Uint8Array> {
    const end = this.offset + n;
    if (end > this.inbound.length) throw new Error("scripted auth input exhausted");
    const bytes = this.inbound.slice(this.offset, end);
    this.offset = end;
    return bytes;
  }

  async write(_bytes: Uint8Array): Promise<void> {}
}

async function expectAuthByteError(message: unknown, expected: string): Promise<void> {
  const socket = new ScriptedAuthSocket(message) as unknown as SubcSocket;
  try {
    await authenticateClient(socket, CONNECTION, Date.now() + 1_000);
    throw new Error("authentication unexpectedly succeeded");
  } catch (error) {
    expect(error).toBeInstanceOf(AuthError);
    expect((error as Error).message).toContain(expected);
  }
}

describe("auth crypto", () => {
  // Pins the exact primitive computeProof relies on (node's HMAC-SHA256) against
  // a published vector, so a broken/substituted hash is caught here rather than
  // only at the live handshake.
  test("HMAC-SHA256 primitive matches RFC 4231 test case 2", () => {
    const mac = createHmac("sha256", Buffer.from("Jefe"));
    mac.update(Buffer.from("what do ya want for nothing?"));
    expect(mac.digest("hex")).toBe(
      "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843",
    );
  });

  test("computeProof returns 32 bytes and is deterministic", () => {
    const a = computeProof(KEY, SERVER_PROOF_DOMAIN, CN, SN, DID);
    const b = computeProof(KEY, SERVER_PROOF_DOMAIN, CN, SN, DID);
    expect(a.length).toBe(PROOF_LEN);
    expect(Buffer.from(a).equals(Buffer.from(b))).toBe(true);
  });

  // The proof binds the message segments in order (domain ‖ cn ‖ sn ‖ did).
  // Swapping the two nonces must change the proof, proving the inputs are not
  // concatenated commutatively — the property a reflection attack would exploit.
  test("computeProof is order-sensitive in the nonces", () => {
    const normal = computeProof(KEY, SERVER_PROOF_DOMAIN, CN, SN, DID);
    const swapped = computeProof(KEY, SERVER_PROOF_DOMAIN, SN, CN, DID);
    expect(Buffer.from(normal).equals(Buffer.from(swapped))).toBe(false);
  });

  // Domain separation: server-proof and client-auth must never collide, or a
  // server proof could be replayed as the client's auth.
  test("server and client domains yield different proofs", () => {
    const server = computeProof(KEY, SERVER_PROOF_DOMAIN, CN, SN, DID);
    const client = computeProof(KEY, CLIENT_AUTH_DOMAIN, CN, SN, DID);
    expect(Buffer.from(server).equals(Buffer.from(client))).toBe(false);
  });

  test("a different key yields a different proof", () => {
    const k2 = Uint8Array.from(Array(32).fill(0xcd));
    const a = computeProof(KEY, SERVER_PROOF_DOMAIN, CN, SN, DID);
    const b = computeProof(k2, SERVER_PROOF_DOMAIN, CN, SN, DID);
    expect(Buffer.from(a).equals(Buffer.from(b))).toBe(false);
  });
});

describe("auth message validation", () => {
  test("rejects an out-of-range proof byte before HMAC verification", async () => {
    const serverProof = Array(PROOF_LEN).fill(0);
    serverProof[0] = 427;
    await expectAuthByteError(
      {
        daemon_id: Array.from(DID),
        server_nonce: Array.from(SN),
        daemon_ver: "test",
        server_proof: serverProof,
      },
      "auth field 'server_proof' has invalid byte 427",
    );
  });

  test("rejects a non-array auth byte field as AuthError", async () => {
    await expectAuthByteError(
      {
        daemon_id: Array.from(DID),
        server_nonce: "not-an-array",
        daemon_ver: "test",
        server_proof: Array(PROOF_LEN).fill(0),
      },
      "auth field 'server_nonce' must be a byte array",
    );
  });
});
