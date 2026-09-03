import type { FetchFn } from "../src/env";
import { decodeJwtPayload, type GitHubRelease } from "../src/github";
import { TEST_INSTALLATION_ID, TEST_INSTALLATION_TOKEN } from "./keys";

export interface FakeGitHubCapture {
  jwt?: string;
  apiAuth: string[];
}

export interface FakeGitHubOptions {
  repos: Record<string, GitHubRelease[]>;
  blobs: Record<string, string | Uint8Array>;
  capture: FakeGitHubCapture;
}

export function fakeGitHub(opts: FakeGitHubOptions): FetchFn {
  return async (input, init) => {
    const req = new Request(input, init);
    const url = new URL(req.url);

    if (
      url.origin === "https://api.github.com" &&
      url.pathname === `/app/installations/${TEST_INSTALLATION_ID}/access_tokens` &&
      req.method === "POST"
    ) {
      const jwt = (req.headers.get("Authorization") ?? "").replace(/^Bearer\s+/i, "");
      opts.capture.jwt = jwt;
      const claims = decodeJwtPayload(jwt);
      if (claims.iss !== 4124360) {
        return new Response("bad iss", { status: 401 });
      }
      return Response.json(
        {
          token: TEST_INSTALLATION_TOKEN,
          expires_at: new Date(Date.now() + 3600_000).toISOString(),
        },
        { status: 201 },
      );
    }

    const auth = req.headers.get("Authorization") ?? "";
    if (url.origin === "https://api.github.com") {
      opts.capture.apiAuth.push(auth);
      if (auth !== `Bearer ${TEST_INSTALLATION_TOKEN}`) {
        return new Response("unauthorized", { status: 401 });
      }
      if (req.headers.get("Accept") !== "application/vnd.github+json") {
        return new Response("bad accept", { status: 400 });
      }
      const m = /^\/repos\/([^/]+\/[^/]+)\/releases$/.exec(url.pathname);
      if (m) {
        return Response.json(opts.repos[m[1]] ?? []);
      }
      return new Response("not found", { status: 404 });
    }

    const blob = opts.blobs[url.href];
    if (blob !== undefined) {
      if (auth !== `Bearer ${TEST_INSTALLATION_TOKEN}`) {
        return new Response("unauthorized", { status: 401 });
      }
      return new Response(blob);
    }
    return new Response("not found", { status: 404 });
  };
}

export function downloadUrl(repo: string, tag: string, name: string): string {
  return `https://github.com/${repo}/releases/download/${tag}/${name}`;
}

export function zipAsset(repo: string, tag: string, name: string, size: number) {
  return { name, browser_download_url: downloadUrl(repo, tag, name), size };
}
