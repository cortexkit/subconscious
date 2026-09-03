/**
 * GitHub App private keys downloaded from the App settings page are PKCS#1
 * (`BEGIN RSA PRIVATE KEY`). WebCrypto only imports PKCS#8 (`BEGIN PRIVATE
 * KEY`), so PKCS#1 is wrapped rather than rejected.
 */
const RSA_ENCRYPTION_ALG_ID = Uint8Array.from([
  0x30, 0x0d, 0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01,
  0x05, 0x00,
]);

export function pemToDer(pem: string): Uint8Array {
  const b64 = pem
    .replace(/-----BEGIN [^-]+-----/, "")
    .replace(/-----END [^-]+-----/, "")
    .replace(/\s+/g, "");
  const bin = atob(b64);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}

export function isPkcs1RsaPem(pem: string): boolean {
  return /BEGIN RSA PRIVATE KEY/.test(pem);
}

export function rsaPrivateKeyPemToPkcs8Der(pem: string): Uint8Array {
  const der = pemToDer(pem);
  if (!isPkcs1RsaPem(pem)) return der;
  return wrapPkcs1InPkcs8(der);
}

export function wrapPkcs1InPkcs8(pkcs1: Uint8Array): Uint8Array {
  const version = Uint8Array.from([0x02, 0x01, 0x00]);
  return derSequence(concat(version, RSA_ENCRYPTION_ALG_ID, derOctetString(pkcs1)));
}

function derLength(len: number): Uint8Array {
  if (len < 0x80) return Uint8Array.from([len]);
  if (len < 0x100) return Uint8Array.from([0x81, len]);
  if (len < 0x10000) return Uint8Array.from([0x82, (len >> 8) & 0xff, len & 0xff]);
  throw new Error("DER length too large");
}

function derSequence(body: Uint8Array): Uint8Array {
  return derTlv(0x30, body);
}

function derOctetString(body: Uint8Array): Uint8Array {
  return derTlv(0x04, body);
}

function derTlv(tag: number, body: Uint8Array): Uint8Array {
  const len = derLength(body.length);
  const out = new Uint8Array(1 + len.length + body.length);
  out[0] = tag;
  out.set(len, 1);
  out.set(body, 1 + len.length);
  return out;
}

function concat(...parts: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const p of parts) total += p.length;
  const out = new Uint8Array(total);
  let offset = 0;
  for (const p of parts) {
    out.set(p, offset);
    offset += p.length;
  }
  return out;
}
