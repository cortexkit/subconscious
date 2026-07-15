// Port of subc-transport's auth.rs client handshake. The proof construction,
// domain strings, message framing, and verification order must match the Rust
// byte-for-byte: a single byte of drift fails authentication outright.
//
// Handshake (client side):
//   1. send ClientHello { client_nonce, role }
//   2. receive ServerProof { daemon_id, server_nonce, daemon_ver, server_proof }
//   3. verify server_proof == HMAC(key, "subc-server-v1" ‖ cn ‖ sn ‖ did)  (constant-time)
//      and daemon_id == the id from the connection file
//   4. send ClientAuth { client_auth = HMAC(key, "subc-client-v1" ‖ cn ‖ sn ‖ did) }
//
// Each message on the wire is a 4-byte little-endian length prefix followed by
// the JSON body. Byte arrays (nonces, proofs, ids) serialize as JSON arrays of
// numbers, matching serde's default for [u8; N].

import { createHmac, randomBytes, timingSafeEqual } from "node:crypto";

import type { ConnectionInfo } from "./connection-file.js";
import { SubcSocket, writeBorrowed } from "./socket.js";

export const NONCE_LEN = 32;
export const PROOF_LEN = 32;
export const MAX_AUTH_MESSAGE_LEN = 4096;
export const SERVER_PROOF_DOMAIN = "subc-server-v1";
export const CLIENT_AUTH_DOMAIN = "subc-client-v1";
export const DEFAULT_CLIENT_ROLE = "client";

export class AuthError extends Error {}

/** HMAC-SHA256 over domain ‖ client_nonce ‖ server_nonce ‖ daemon_id. */
export function computeProof(
  key: Uint8Array,
  domain: string,
  clientNonce: Uint8Array,
  serverNonce: Uint8Array,
  daemonId: Uint8Array,
): Uint8Array {
  const mac = createHmac("sha256", Buffer.from(key));
  mac.update(Buffer.from(domain, "utf8"));
  mac.update(Buffer.from(clientNonce));
  mac.update(Buffer.from(serverNonce));
  mac.update(Buffer.from(daemonId));
  return new Uint8Array(mac.digest());
}

function constantTimeEq(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  return timingSafeEqual(Buffer.from(a), Buffer.from(b));
}

async function writeMessage(
  sock: SubcSocket,
  value: unknown,
  deadlineMs: number,
): Promise<void> {
  const json = Buffer.from(JSON.stringify(value), "utf8");
  if (json.length > MAX_AUTH_MESSAGE_LEN) {
    throw new AuthError(`auth message too large: ${json.length} > ${MAX_AUTH_MESSAGE_LEN}`);
  }
  const lenPrefix = new Uint8Array(4);
  new DataView(lenPrefix.buffer).setUint32(0, json.length, true);
  await writeBorrowed(sock, lenPrefix, deadlineMs);
  await writeBorrowed(sock, json, deadlineMs);
}

async function readMessage<T>(sock: SubcSocket, deadlineMs: number): Promise<T> {
  const lenBytes = await sock.readExact(4, deadlineMs);
  const len = new DataView(lenBytes.buffer, lenBytes.byteOffset, 4).getUint32(0, true);
  if (len > MAX_AUTH_MESSAGE_LEN) {
    throw new AuthError(`auth message too large: ${len} > ${MAX_AUTH_MESSAGE_LEN}`);
  }
  const body = len === 0 ? new Uint8Array(0) : await sock.readExact(len, deadlineMs);
  try {
    return JSON.parse(Buffer.from(body).toString("utf8")) as T;
  } catch (err) {
    throw new AuthError(`auth message JSON decode failed: ${String(err)}`);
  }
}

interface ServerProofMessage {
  daemon_id: unknown;
  server_nonce: unknown;
  daemon_ver: string;
  server_proof: unknown;
}

function authBytes(value: unknown, field: string): Uint8Array {
  if (!Array.isArray(value)) {
    throw new AuthError(`auth field '${field}' must be a byte array`);
  }
  for (const byte of value) {
    if (typeof byte !== "number" || !Number.isInteger(byte) || byte < 0 || byte > 255) {
      throw new AuthError(`auth field '${field}' has invalid byte ${String(byte)}`);
    }
  }
  return Uint8Array.from(value as number[]);
}

/**
 * Run the client handshake over an already-connected socket. Resolves on
 * success; throws AuthError on any proof/identity mismatch or framing fault.
 * The whole exchange is bounded by `deadlineMs` (epoch ms).
 */
export async function authenticateClient(
  sock: SubcSocket,
  conn: ConnectionInfo,
  deadlineMs: number,
): Promise<void> {
  const clientNonce = new Uint8Array(randomBytes(NONCE_LEN));

  await writeMessage(
    sock,
    { client_nonce: Array.from(clientNonce), role: DEFAULT_CLIENT_ROLE },
    deadlineMs,
  );

  const proof = await readMessage<ServerProofMessage>(sock, deadlineMs);
  const serverNonce = authBytes(proof.server_nonce, "server_nonce");
  const daemonId = authBytes(proof.daemon_id, "daemon_id");
  const serverProof = authBytes(proof.server_proof, "server_proof");

  const expected = computeProof(conn.key, SERVER_PROOF_DOMAIN, clientNonce, serverNonce, daemonId);
  if (!constantTimeEq(expected, serverProof)) {
    throw new AuthError("server proof mismatch — wrong key or impostor daemon");
  }
  if (!constantTimeEq(daemonId, conn.daemonId)) {
    throw new AuthError("daemon id mismatch — connection file points at a different daemon");
  }

  const clientAuth = computeProof(conn.key, CLIENT_AUTH_DOMAIN, clientNonce, serverNonce, daemonId);
  await writeMessage(sock, { client_auth: Array.from(clientAuth) }, deadlineMs);
}
