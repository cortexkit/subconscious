import type { Env } from "./env";
import { timingSafeEqualString } from "./hex";
import { verifyGitHubSignature } from "./hmac";
import { KV_REFUSALS, loadIndexBundle, rebuild } from "./rebuild";

// Serves the canonical CortexKit install scripts at cortexkit.io/install.
// The scripts stay repo-canonical in cortexkit/subconscious; this worker
// proxies raw GitHub content so a script change needs no redeploy here.
const REPO_RAW = "https://raw.githubusercontent.com/cortexkit/subconscious/master/scripts/install";

const PATHS: Record<string, { file: string; type: string }> = {
  "/install": { file: "install.sh", type: "text/x-shellscript; charset=utf-8" },
  "/install/win": { file: "install.ps1", type: "text/plain; charset=utf-8" },
  "/install.ps1": { file: "install.ps1", type: "text/plain; charset=utf-8" },
};

const RELEASE_ACTIONS = new Set(["published", "edited", "deleted"]);

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);
    const path = url.pathname.replace(/\/$/, "");

    if (path === "/releases/v1/index.json" && request.method === "GET") {
      return serveIndex(env, "json");
    }
    if (path === "/releases/v1/index.json.sig" && request.method === "GET") {
      return serveIndex(env, "sig");
    }
    if (path === "/releases/v1/refusals.json" && request.method === "GET") {
      return serveRefusals(request, env);
    }
    if (path === "/releases/v1/reingest" && request.method === "POST") {
      return handleReingest(request, env);
    }
    if (path === "/webhooks/github" && request.method === "POST") {
      return handleWebhook(request, env);
    }

    return handleInstall(path);
  },

  async scheduled(_controller: ScheduledController, env: Env): Promise<void> {
    const result = await rebuild(env);
    if (!result.ok) {
      console.error(`scheduled rebuild failed: ${result.error}`);
    }
  },
} satisfies ExportedHandler<Env>;

async function handleInstall(path: string): Promise<Response> {
  if (path === "") {
    return new Response(
      "CortexKit installer\n\n  macOS/Linux:  curl -fsSL https://cortexkit.io/install | bash\n  Windows:      irm https://cortexkit.io/install/win | iex\n",
      { headers: { "content-type": "text/plain; charset=utf-8" } },
    );
  }
  const entry = PATHS[path];
  if (!entry) {
    return new Response("not found; use /install (sh) or /install/win (PowerShell)\n", { status: 404 });
  }
  const upstream = await fetch(`${REPO_RAW}/${entry.file}`, {
    cf: { cacheTtl: 300, cacheEverything: true },
  });
  if (!upstream.ok) {
    // Fail loud rather than serving an empty script into a pipe-to-shell.
    return new Response(`install script temporarily unavailable (upstream ${upstream.status})\n`, { status: 503 });
  }
  return new Response(upstream.body, {
    status: 200,
    headers: { "content-type": entry.type, "cache-control": "public, max-age=300" },
  });
}

async function serveIndex(env: Env, view: "json" | "sig"): Promise<Response> {
  const loaded = await loadIndexBundle(env);
  if (!loaded.ok && loaded.reason === "missing") {
    return new Response(JSON.stringify({ error: "index_not_built" }), {
      status: 404,
      headers: { "content-type": "application/json" },
    });
  }
  if (!loaded.ok) {
    // Never serve an index body whose signature does not verify; a torn or
    // corrupted KV value must fail closed rather than look like a current release.
    return new Response(JSON.stringify({ error: "index_inconsistent" }), {
      status: 503,
      headers: { "content-type": "application/json" },
    });
  }
  if (view === "sig") {
    return new Response(loaded.sig, {
      status: 200,
      headers: {
        "content-type": "text/plain",
        "cache-control": "public, max-age=60",
      },
    });
  }
  return new Response(loaded.body, {
    status: 200,
    headers: {
      "content-type": "application/json",
      "cache-control": "public, max-age=60",
      "X-CortexKit-Signature-Ed25519": loaded.sig,
    },
  });
}

async function serveRefusals(request: Request, env: Env): Promise<Response> {
  if (!adminAuthorized(request, env)) {
    return new Response("unauthorized\n", { status: 401 });
  }
  const body = (await env.RELEASE_INDEX.get(KV_REFUSALS)) ?? "[]";
  return new Response(body, {
    status: 200,
    headers: { "content-type": "application/json" },
  });
}

async function handleReingest(request: Request, env: Env): Promise<Response> {
  if (!adminAuthorized(request, env)) {
    return new Response("unauthorized\n", { status: 401 });
  }
  const result = await rebuild(env);
  if (!result.ok) {
    return new Response(`${result.error}\n`, { status: 500 });
  }
  return new Response("ok\n", { status: 200 });
}

async function handleWebhook(request: Request, env: Env): Promise<Response> {
  const raw = await request.arrayBuffer();
  const ok = await verifyGitHubSignature(
    env.GITHUB_WEBHOOK_SECRET,
    raw,
    request.headers.get("X-Hub-Signature-256"),
  );
  if (!ok) {
    return new Response("unauthorized\n", { status: 401 });
  }
  const event = request.headers.get("X-GitHub-Event") ?? "";
  if (event !== "release") {
    return new Response(null, { status: 204 });
  }
  let payload: { action?: string };
  try {
    payload = JSON.parse(new TextDecoder().decode(raw)) as { action?: string };
  } catch {
    return new Response("invalid json\n", { status: 400 });
  }
  if (!RELEASE_ACTIONS.has(payload.action ?? "")) {
    return new Response(null, { status: 204 });
  }
  const result = await rebuild(env);
  if (!result.ok) {
    return new Response(`${result.error}\n`, { status: 500 });
  }
  return new Response("ok\n", { status: 200 });
}

function adminAuthorized(request: Request, env: Env): boolean {
  if (!env.ADMIN_TOKEN) return false;
  const header = request.headers.get("Authorization");
  if (!header) return false;
  const match = /^Bearer\s+(\S+)$/i.exec(header);
  if (!match) return false;
  return timingSafeEqualString(match[1], env.ADMIN_TOKEN);
}
