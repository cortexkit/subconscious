import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, realpathSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  managementSurfaceManifest,
  SubcClient,
  SubcProvider,
  SubcError,
  type Frame,
  type ScopeSyncReply,
  type ScopeApplyReply,
  type CatalogEntry,
  type ProviderConnectionState,
} from "../src/index.js";
import { ROOT, startLiveDaemon, waitFor, type LiveDaemon } from "./live-daemon.js";
import { rejectionOf } from "./settle.js";

// Live tests need a compiled subc-core daemon (Rust toolchain). They are
// OFF by default so the unit suite runs in a bun-only environment (npm
// release gate, bun-only CI job); set RUN_SUBC_LIVE=1 to run them.
const LIVE = process.env.RUN_SUBC_LIVE === "1";

describe.skipIf(!LIVE)("SubcProvider live routing against real subc-core", () => {
  let live: LiveDaemon;

  beforeAll(async () => {
    live = await startLiveDaemon("subc-provider-live");
  });

  afterAll(() => {
    live?.stop();
  });

  test("registers a ManagementSurface and echoes a routed request", async () => {
    const moduleId = "test-effect-provider";
    const provider = await SubcProvider.connect({
      connectionFile: live.connFile,
      manifest: managementSurfaceManifest({ moduleId, operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
    });
    const identity = { project_root: live.configDir, harness: "opencode", session: "session-echo" };
    const client = await SubcClient.connect({ connectionFile: live.connFile, identity });

    try {
      const entry = (await client.catalogList()).find((module) => module.module_id === moduleId);
      expect(entry).toBeDefined();
      const role = managementSurfaceRole(entry);
      expect(role).toBeDefined();
      expect(role?.operations).toContainEqual({ name: "echo", kind: "query" });

      const routeChannel = await client.routeOpen(
        { kind: "management_surface", module_id: moduleId },
        identity,
      );
      const request = { method: "echo", params: { text: "hello", n: 7 } };
      expect(await client.request(routeChannel, request, { timeoutMs: 10_000 })).toEqual(request);

      const managedRequest = { method: "echo", params: { text: "managed", n: 8 } };
      expect(await client.call<unknown>(moduleId, "echo", managedRequest.params, { timeoutMs: 10_000 })).toEqual(
        managedRequest,
      );
    } finally {
      client.close();
      await provider.close();
    }
  });
});

describe.skipIf(!LIVE)("SubcProvider live managed reconnect against real subc-core", () => {
  let live: LiveDaemon;

  beforeAll(async () => {
    live = await startLiveDaemon("subc-provider-reconnect-live");
  });

  afterAll(() => {
    live?.stop();
  });

  test("re-registers after daemon restart and serves requests on the restored connection", async () => {
    const moduleId = "test-reconnect-provider";
    const events: ProviderConnectionState[] = [];
    const provider = await SubcProvider.connect({
      connectionFile: live.connFile,
      manifest: managementSurfaceManifest({ moduleId, operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
      reconnectBackoff: { baseMs: 50, capMs: 50, maxAttempts: 1 },
      restoredDebounceMs: 10,
      onConnectionState: (event) => {
        events.push(event);
      },
    });

    try {
      expect(provider.currentEpoch()).toBe(1);
      await live.restart();
      await waitFor(() => provider.currentEpoch() === 2, 10_000, "provider epoch 2 after daemon restart");
      await waitFor(
        () => events.some((event) => event.state === "restored" && event.epoch === 2),
        10_000,
        "provider restored event after daemon restart",
      );

      const downIndex = events.findIndex((event) => event.state === "down");
      const restoredIndex = events.findIndex((event) => event.state === "restored" && event.epoch === 2);
      expect(downIndex).toBeGreaterThanOrEqual(0);
      expect(restoredIndex).toBeGreaterThan(downIndex);

      const identity = { project_root: live.configDir, harness: "opencode", session: "session-reconnect" };
      const client = await SubcClient.connect({ connectionFile: live.connFile, identity });
      try {
        expect(await client.call<unknown>(moduleId, "echo", { reconnected: true }, { timeoutMs: 10_000 })).toEqual({
          method: "echo",
          params: { reconnected: true },
        });
      } finally {
        client.close();
      }
    } finally {
      await provider.close();
    }
  });
});


function managementSurfaceRole(entry: CatalogEntry | undefined): { operations?: unknown[] } | undefined {
  return (entry?.roles as Array<{ role?: string; operations?: unknown[] }> | undefined)?.find(
    (role) => role.role === "management_surface",
  );
}

describe.skipIf(!LIVE)("SubcProvider receives a delivered storage descriptor", () => {
  let live: LiveDaemon;

  beforeAll(async () => {
    // A daemon configured with central sqlite storage delivers each module its
    // own descriptor in HELLO_ACK.
    live = await startLiveDaemon("subc-storage-live", {
      subcJsonc: JSON.stringify({
        version: 1,
        storage: { backend: "sqlite", data_home: "/data" },
      }),
    });
  });

  afterAll(() => {
    live?.stop();
  });

  test("a managed module reads its sqlite descriptor from HELLO_ACK", async () => {
    const moduleId = "storage-consumer";
    const provider = await SubcProvider.connect({
      connectionFile: live.connFile,
      manifest: managementSurfaceManifest({ moduleId, operations: ["noop"] }),
      handler: async (_routeChannel, body) => body,
    });
    try {
      expect(provider.storage).toEqual({
        module_id: moduleId,
        storage_namespace: "default",
        isolation: { kind: "module" },
        backend: { backend: "sqlite", path: `/data/cortexkit/${moduleId}/store.db` },
      });
    } finally {
      await provider.close();
    }
  });
});

describe.skipIf(!LIVE)("SubcProvider receives the daemon's machine id", () => {
  let live: LiveDaemon;

  beforeAll(async () => {
    live = await startLiveDaemon("subc-machine-id-live");
  });

  afterAll(() => {
    live?.stop();
  });

  test("the provider's machineId equals the id the daemon stored under its data home", async () => {
    const provider = await SubcProvider.connect({
      connectionFile: live.connFile,
      manifest: managementSurfaceManifest({ moduleId: "machine-id-reader", operations: ["noop"] }),
      handler: async (_routeChannel, body) => body,
    });
    try {
      const stored = readFileSync(join(live.dataDir, "cortexkit", "machine-id"), "utf8").trimEnd();
      expect(stored).toMatch(/^[0-9a-f]{32}$/);
      expect(provider.machineId).toBe(stored);
    } finally {
      await provider.close();
    }
  });
});


describe.skipIf(!LIVE)("SubcProvider live scope ownership", () => {
  test("syncs a workdir for a carrier, stamps its bind, applies deltas and refuses unregistered calls", async () => {
    const scratch = mkdtempSync(join(tmpdir(), "subc-ts-scope-owner-"));
    const resultsPath = join(scratch, "results.json");
    const noncePath = join(scratch, "carrier.nonce");
    const targetEvents = join(scratch, "target.jsonl");
    const stub = join(ROOT, "target", "debug", "fake-aft-stub");
    const owner = "ts-scope-owner";
    const target = "ts-scope-target";
    let live: LiveDaemon | undefined;
    let client: SubcClient | undefined;
    try {
      expect(existsSync(stub)).toBe(true);
      live = await startLiveDaemon("subc-ts-scope-owner", {
        subcJsonc: JSON.stringify({ version: 1, modules: {
          [owner]: { program: process.execPath, args: [join(import.meta.dir, "fixtures", "scope-owner.ts")], enabled: true, reserved: true,
            env: { SCOPE_WORKDIR: scratch, SCOPE_RESULTS: resultsPath } },
          "ts-scope-carrier": { program: stub, args: [], enabled: true, reserved: true,
            env: { FAKE_AFT_MODULE_ID: "ts-scope-carrier", FAKE_AFT_LAUNCH_NONCE_PATH: noncePath } },
          [target]: { program: stub, args: [], enabled: true, reserved: false,
            env: { FAKE_AFT_MODULE_ID: target, FAKE_AFT_EVENTS_PATH: targetEvents,
              FAKE_AFT_CAPABILITIES: JSON.stringify({ provides: ["scope-workdir/v1"] }) } },
        } }),
      });
      await waitFor(() => existsSync(resultsPath) && existsSync(noncePath), 10_000, "TypeScript owner scope replies and carrier nonce");
      const replies = JSON.parse(readFileSync(resultsPath, "utf8")) as { sync: ScopeSyncReply; apply: ScopeApplyReply };
      expect(replies.sync).toMatchObject({ generation: 1, results: [{ ref: "rig", scope_epoch: 1, outcome: "created" }], ended: [] });
      expect(replies.apply).toMatchObject({ generation: 2, results: [{ ref: "forbidden-attribution", outcome: "refused", code: "scope_attribute_not_permitted" }], end_results: [], ended: [] });
      client = await SubcClient.connect({ connectionFile: live.connFile });
      const deadline = Date.now() + 10_000;
      while (!(await client.catalogList()).some((entry) => entry.module_id === target)) {
        if (Date.now() >= deadline) throw new Error(`scope target not registered: ${live.stderr()}`);
        await Bun.sleep(50);
      }
      const identity = { project_root: scratch, harness: "bun", session: "rig" };
      const route = await client.routeOpen({ kind: "tool_provider", module_id: target }, identity, {
        scope: { owner: { kind: "reserved", module_id: owner }, ref: "rig", scopeEpoch: 1 },
        consumerIdentity: { module_id: "ts-scope-carrier", launch_nonce: readFileSync(noncePath, "utf8") },
      });
      const events = () => !existsSync(targetEvents) ? [] : readFileSync(targetEvents, "utf8").split("\n").filter(Boolean).flatMap((line) => {
        try { return [JSON.parse(line) as { kind: string; scope?: unknown; principal?: unknown }]; } catch { return []; }
      });
      await waitFor(() => events().some((event) => event.kind === "attach"), 10_000, "scope workdir bind stamp");
      const bind = events().find((event) => event.kind === "attach")!;
      expect(bind.scope).toMatchObject({ owner: { kind: "reserved", module_id: owner }, ref: "rig", scope_epoch: 1, workdir: realpathSync(scratch) });
      expect(bind.principal).toEqual({ kind: "reserved", module_id: "ts-scope-carrier" });
      await client.closeRoute(route);

      const ownerRoute = await client.routeOpen({ kind: "management_surface", module_id: owner }, identity);
      const ended = await client.request(ownerRoute, { method: "end" }) as ScopeApplyReply;
      expect(ended).toMatchObject({ generation: 3, end_results: [{ ref: "rig", scope_epoch: 1, outcome: "ended" }], ended: [{ ref: "rig", scope_epoch: 1 }] });
      await client.closeRoute(ownerRoute);

      // A consumer socket is authenticated but has not registered a module with HELLO.
      const unregistered = client as unknown as { controlRpc(body: Uint8Array): Promise<Frame> };
      for (const body of [
        { op: "scope.sync", generation: 1, scopes: [] },
        { op: "scope.apply", generation: 2, upsert: [], end: [] },
      ]) {
        const refused = unregistered.controlRpc(Buffer.from(JSON.stringify(body)));
        expect(await rejectionOf(refused)).toBeInstanceOf(SubcError);
        expect(await rejectionOf(refused)).toMatchObject({ code: "not_registered" });
      }
    } finally {
      client?.close();
      live?.stop();
      rmSync(scratch, { recursive: true, force: true });
    }
  }, 30_000);
});
