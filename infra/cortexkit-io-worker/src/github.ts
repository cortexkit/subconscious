import type { Env, FetchFn } from "./env";
import { rsaPrivateKeyPemToPkcs8Der } from "./pem";
import type { ComponentSpec } from "./components";

const USER_AGENT = "cortexkit-io-worker";
const ACCEPT_API = "application/vnd.github+json";
const EARLY_EXPIRY_MS = 5 * 60 * 1000;

/**
 * A PAT is a long-lived user credential sitting in a Worker secret. An App
 * installation token is org-owned, short-lived, and scoped by the installation
 * — the same custody shape every other fleet GitHub caller uses.
 *
 * Cached per isolate. Tokens last ~1 hour; refresh 5 minutes early so a
 * request never presents a token that expires mid-rebuild.
 */
let cachedToken: { token: string; expiresAtMs: number } | null = null;

export function resetInstallationTokenCache(): void {
  cachedToken = null;
}

export function decodeJwtPayload(jwt: string): Record<string, unknown> {
  const parts = jwt.split(".");
  if (parts.length !== 3) throw new Error("jwt malformed");
  const padded = parts[1].replace(/-/g, "+").replace(/_/g, "/");
  const pad = "=".repeat((4 - (padded.length % 4)) % 4);
  const json = atob(padded + pad);
  return JSON.parse(json) as Record<string, unknown>;
}

export async function mintAppJwt(
  appId: string,
  privateKeyPem: string,
  nowSec = Math.floor(Date.now() / 1000),
): Promise<string> {
  const iss = /^\d+$/.test(appId) ? Number(appId) : appId;
  const header = base64UrlJson({ alg: "RS256", typ: "JWT" });
  const payload = base64UrlJson({ iat: nowSec - 60, exp: nowSec + 600, iss });
  const signingInput = `${header}.${payload}`;
  const key = await crypto.subtle.importKey(
    "pkcs8",
    rsaPrivateKeyPemToPkcs8Der(privateKeyPem),
    { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const sig = await crypto.subtle.sign("RSASSA-PKCS1-v1_5", key, new TextEncoder().encode(signingInput));
  return `${signingInput}.${base64Url(new Uint8Array(sig))}`;
}

export async function getInstallationToken(env: Env, fetchFn: FetchFn = fetch): Promise<string> {
  const now = Date.now();
  if (cachedToken && cachedToken.expiresAtMs - EARLY_EXPIRY_MS > now) {
    return cachedToken.token;
  }
  const jwt = await mintAppJwt(env.GITHUB_APP_ID, env.GITHUB_APP_PRIVATE_KEY);
  const url = `https://api.github.com/app/installations/${env.GITHUB_APP_INSTALLATION_ID}/access_tokens`;
  const res = await fetchFn(url, {
    method: "POST",
    headers: {
      Authorization: `Bearer ${jwt}`,
      Accept: ACCEPT_API,
      "User-Agent": USER_AGENT,
    },
  });
  if (!res.ok) {
    const text = await res.text();
    throw new Error(`github installation token ${res.status}: ${text.slice(0, 200)}`);
  }
  const body = (await res.json()) as { token: string; expires_at: string };
  cachedToken = { token: body.token, expiresAtMs: Date.parse(body.expires_at) };
  return cachedToken.token;
}

export interface GitHubAsset {
  name: string;
  browser_download_url: string;
  size: number;
}

export interface GitHubRelease {
  tag_name: string;
  draft: boolean;
  prerelease: boolean;
  created_at: string;
  published_at: string | null;
  assets: GitHubAsset[];
}

export async function githubApi(env: Env, fetchFn: FetchFn, url: string): Promise<Response> {
  const token = await getInstallationToken(env, fetchFn);
  return fetchFn(url, {
    headers: {
      Authorization: `Bearer ${token}`,
      Accept: ACCEPT_API,
      "User-Agent": USER_AGENT,
    },
  });
}

export async function githubDownload(env: Env, fetchFn: FetchFn, url: string): Promise<Response> {
  const token = await getInstallationToken(env, fetchFn);
  return fetchFn(url, {
    headers: {
      Authorization: `Bearer ${token}`,
      "User-Agent": USER_AGENT,
    },
  });
}

export async function listReleases(env: Env, fetchFn: FetchFn, repo: string): Promise<GitHubRelease[]> {
  const out: GitHubRelease[] = [];
  for (let page = 1; page <= 10; page++) {
    const url = `https://api.github.com/repos/${repo}/releases?per_page=100&page=${page}`;
    const res = await githubApi(env, fetchFn, url);
    if (!res.ok) {
      throw new Error(`list releases ${repo} ${res.status}`);
    }
    const batch = (await res.json()) as GitHubRelease[];
    if (!Array.isArray(batch)) throw new Error(`list releases ${repo} not an array`);
    out.push(...batch);
    if (batch.length < 100) break;
  }
  return out;
}

export function pickCurrentRelease(spec: ComponentSpec, releases: GitHubRelease[]): GitHubRelease | null {
  if (spec.resolve === "newest-ck-mc") {
    const candidates = releases.filter((r) => !r.draft && r.tag_name.startsWith("ck-mc-"));
    if (candidates.length === 0) return null;
    let newest = candidates[0];
    let newestMs = Date.parse(newest.created_at);
    for (let i = 1; i < candidates.length; i++) {
      const ms = Date.parse(candidates[i].created_at);
      if (ms > newestMs) {
        newest = candidates[i];
        newestMs = ms;
      }
    }
    return newest;
  }
  for (const release of releases) {
    if (!release.draft) return release;
  }
  return null;
}

function base64UrlJson(value: unknown): string {
  return base64Url(new TextEncoder().encode(JSON.stringify(value)));
}

function base64Url(bytes: Uint8Array): string {
  let bin = "";
  for (const b of bytes) bin += String.fromCharCode(b);
  return btoa(bin).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/g, "");
}
