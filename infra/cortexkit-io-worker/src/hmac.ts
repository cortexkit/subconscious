import { bytesToHex, timingSafeEqualHex } from "./hex";

/**
 * GitHub signs the raw request body; re-serializing JSON would invalidate
 * legitimate deliveries and is how an attacker with a mutated body slips through.
 */
export async function verifyGitHubSignature(
  secret: string,
  body: ArrayBuffer,
  header: string | null,
): Promise<boolean> {
  if (!secret || !header) return false;
  const prefix = "sha256=";
  if (!header.startsWith(prefix)) return false;
  const presented = header.slice(prefix.length);
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("HMAC", key, body);
  return timingSafeEqualHex(bytesToHex(new Uint8Array(sig)), presented);
}
