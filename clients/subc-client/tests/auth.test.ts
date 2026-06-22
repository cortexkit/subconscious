import { createHmac } from "node:crypto";
import { describe, expect, test } from "bun:test";

import {
  CLIENT_AUTH_DOMAIN,
  computeProof,
  PROOF_LEN,
  SERVER_PROOF_DOMAIN,
} from "../src/auth.js";

const KEY = Uint8Array.from(Array(32).fill(0xab));
const CN = Uint8Array.from(Array(32).fill(0x01));
const SN = Uint8Array.from(Array(32).fill(0x02));
const DID = Uint8Array.from(Array(16).fill(0x03));

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
