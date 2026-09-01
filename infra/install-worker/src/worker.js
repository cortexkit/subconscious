// Serves the canonical CortexKit install scripts at cortexkit.io/install.
// The scripts stay repo-canonical in cortexkit/subconscious; this worker
// proxies raw GitHub content so a script change needs no redeploy here.
const REPO_RAW = "https://raw.githubusercontent.com/cortexkit/subconscious/master/scripts/install";

const PATHS = {
  "/install": { file: "install.sh", type: "text/x-shellscript; charset=utf-8" },
  "/install/win": { file: "install.ps1", type: "text/plain; charset=utf-8" },
  "/install.ps1": { file: "install.ps1", type: "text/plain; charset=utf-8" },
};

export default {
  async fetch(request) {
    const url = new URL(request.url);
    const path = url.pathname.replace(/\/$/, "");
    if (path === "") {
      return new Response("CortexKit installer\n\n  macOS/Linux:  curl -fsSL https://cortexkit.io/install | sh\n  Windows:      irm https://cortexkit.io/install/win | iex\n", { headers: { "content-type": "text/plain; charset=utf-8" } });
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
  },
};
