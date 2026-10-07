import { expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { SubcClient } from "../src/index.js";
import { ROOT, startLiveDaemon, waitFor, type LiveDaemon } from "./live-daemon.js";

test.skipIf(process.env.RUN_SUBC_LIVE !== "1")("a real daemon admits a scoped owner open and stamps the provider bind", async () => {
  const scratch = mkdtempSync(join(tmpdir(), "subc-scoped-open-"));
  const syncPath = join(scratch, "scopes.json");
  const noncePath = join(scratch, "owner.nonce");
  const ownerEvents = join(scratch, "owner.jsonl");
  const providerEvents = join(scratch, "provider.jsonl");
  const stub = join(ROOT, "target", "debug", "fake-aft-stub");
  const owner = "ts-scoped-owner";
  const provider = "ts-scoped-provider";
  let live: LiveDaemon | undefined;
  let client: SubcClient | undefined;
  try {
    // fake-aft-stub is a supervised module that registers the scope (so it is the
    // owner) and records the route.bind it receives, so the test can check the stamp.
    expect(existsSync(stub)).toBe(true);
    writeFileSync(syncPath, JSON.stringify([{ ref: "session-a", scope_epoch: 3, kind: "head" }]));
    live = await startLiveDaemon("subc-scoped-open", {
      subcJsonc: JSON.stringify({ version: 1, modules: {
        [owner]: { program: stub, args: [], enabled: true, reserved: true, env: {
          FAKE_AFT_MODULE_ID: owner, FAKE_AFT_SCOPE_SYNC_PATH: syncPath,
          FAKE_AFT_LAUNCH_NONCE_PATH: noncePath, FAKE_AFT_EVENTS_PATH: ownerEvents,
        } },
        [provider]: { program: stub, args: [], enabled: true, reserved: false, env: {
          FAKE_AFT_MODULE_ID: provider, FAKE_AFT_EVENTS_PATH: providerEvents,
        } },
      } }),
    });
    await waitFor(() => events(ownerEvents).some((event) => event.kind === "scope_sync_response"), 10_000, "owner scope sync accepted");
    const sync = events(ownerEvents).find((event) => event.kind === "scope_sync_response")!;
    expect(sync.body_json?.results).toHaveLength(1);
    expect(sync.body_json!.results[0]!.outcome).not.toBe("refused");
    await waitFor(() => existsSync(noncePath), 10_000, "owner launch nonce");
    client = await SubcClient.connect({ connectionFile: live.connFile });
    const deadline = Date.now() + 10_000;
    while (!(await client.catalogList()).some((entry) => entry.module_id === provider)) {
      if (Date.now() >= deadline) throw new Error(`provider not registered: ${live.stderr()}`);
      await new Promise((resolve) => setTimeout(resolve, 50));
    }
    const route = await client.routeOpen(
      { kind: "tool_provider", module_id: provider },
      { project_root: scratch, harness: "bun", session: "session-a" },
      { scope: { owner: { kind: "reserved", module_id: owner }, ref: "session-a", scopeEpoch: 3 },
        consumerIdentity: { module_id: owner, launch_nonce: readFileSync(noncePath, "utf8") } },
    );
    await waitFor(() => events(providerEvents).some((event) => event.kind === "attach"), 10_000, "scoped provider bind");
    const bind = events(providerEvents).find((event) => event.kind === "attach")!;
    expect(bind.scope).toMatchObject({ owner: { kind: "reserved", module_id: owner }, ref: "session-a", scope_epoch: 3 });
    expect(bind.principal).toEqual({ kind: "reserved", module_id: owner });
    await client.closeRoute(route);
  } finally {
    client?.close();
    live?.stop();
    rmSync(scratch, { recursive: true, force: true });
  }
}, 30_000);

interface ScopeEvent {
  kind: string;
  body_json?: { results: { outcome: string }[] };
  scope?: unknown;
  principal?: unknown;
}

function events(path: string): ScopeEvent[] {
  if (!existsSync(path)) return [];
  return readFileSync(path, "utf8").split("\n").filter(Boolean).flatMap((line) => {
    // A writer may still be finishing its last line.
    try { return [JSON.parse(line) as ScopeEvent]; } catch { return []; }
  });
}
