import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import {
  managementSurfaceManifest,
  SubcClient,
  SubcProvider,
  type CatalogEntry,
  type ProviderConnectionState,
} from "../src/index.js";
import { startLiveDaemon, waitFor, type LiveDaemon } from "./live-daemon.js";

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
      await expect(client.request(routeChannel, request, { timeoutMs: 10_000 })).resolves.toEqual(request);

      const managedRequest = { method: "echo", params: { text: "managed", n: 8 } };
      await expect(client.call(moduleId, "echo", managedRequest.params, { timeoutMs: 10_000 })).resolves.toEqual(
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
        await expect(client.call(moduleId, "echo", { reconnected: true }, { timeoutMs: 10_000 })).resolves.toEqual({
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
