import { bytesToHex } from "./hex";

const ZIP_NAME = /^(.+)-(darwin|linux|windows)-(arm64|x64)\.zip$/;

export interface ParsedZipName {
  binary: string;
  os: "darwin" | "linux" | "windows";
  arch: "arm64" | "x64";
}

export function parseZipName(name: string): ParsedZipName | null {
  const m = ZIP_NAME.exec(name);
  if (!m) return null;
  return {
    binary: m[1],
    os: m[2] as ParsedZipName["os"],
    arch: m[3] as ParsedZipName["arch"],
  };
}

/** First whitespace-delimited 64-hex token (shasum `hash  file` or bare hex). */
export function parseSidecar(text: string): string | null {
  for (const token of text.trim().split(/\s+/)) {
    if (/^[0-9a-fA-F]{64}$/.test(token)) return token.toLowerCase();
  }
  return null;
}

export async function sha256Hex(data: BufferSource): Promise<string> {
  const digest = await crypto.subtle.digest("SHA-256", data);
  return bytesToHex(new Uint8Array(digest));
}

export function platformKey(os: string, arch: string): string {
  return `${os}-${arch}`;
}
