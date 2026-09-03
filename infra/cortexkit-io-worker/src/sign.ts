import { pemToDer } from "./pem";

/**
 * Ed25519 over the canonical index bytes. The public half is derived from
 * the private JWK (`x`) so a write can be verified without a second secret.
 */
export async function signIndex(pem: string, bytes: BufferSource): Promise<string> {
  const key = await importPrivateKey(pem);
  const sig = await crypto.subtle.sign("Ed25519", key, bytes);
  return bufferToBase64(sig);
}

export async function verifyIndex(pem: string, bytes: BufferSource, signatureB64: string): Promise<boolean> {
  const privateKey = await importPrivateKey(pem);
  const publicKey = await publicKeyFromPrivate(privateKey);
  let signature: Uint8Array;
  try {
    signature = base64ToBytes(signatureB64);
  } catch {
    return false;
  }
  if (signature.byteLength !== 64) return false;
  return crypto.subtle.verify("Ed25519", publicKey, signature, bytes);
}

async function importPrivateKey(pem: string): Promise<CryptoKey> {
  return crypto.subtle.importKey("pkcs8", pemToDer(pem), { name: "Ed25519" }, true, ["sign"]);
}

async function publicKeyFromPrivate(privateKey: CryptoKey): Promise<CryptoKey> {
  const jwk = (await crypto.subtle.exportKey("jwk", privateKey)) as JsonWebKey;
  delete jwk.d;
  jwk.key_ops = ["verify"];
  return crypto.subtle.importKey("jwk", jwk, { name: "Ed25519" }, true, ["verify"]);
}

function bufferToBase64(buf: ArrayBuffer): string {
  const bytes = new Uint8Array(buf);
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin);
}

function base64ToBytes(b64: string): Uint8Array {
  const bin = atob(b64.trim());
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
