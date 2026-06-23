import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import {
  managementSurfaceManifest,
  SubcClient,
  SubcProvider,
  type CatalogEntry,
} from "../src/index.js";
import { startLiveDaemon, type LiveDaemon } from "./live-daemon.js";

describe("SubcProvider live routing against real subc-core", () => {
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
    const client = await SubcClient.connect({ connectionFile: live.connFile });

    try {
      const entry = (await client.catalogList()).find((module) => module.module_id === moduleId);
      expect(entry).toBeDefined();
      const role = managementSurfaceRole(entry);
      expect(role).toBeDefined();
      expect(role?.operations).toContainEqual({ name: "echo", kind: "query" });

      const routeChannel = await client.routeOpen(
        { kind: "management_surface", module_id: moduleId },
        { project_root: live.configDir, harness: "opencode", session: "session-echo" },
      );
      const request = { method: "echo", params: { text: "hello", n: 7 } };
      await expect(client.request(routeChannel, request, { timeoutMs: 10_000 })).resolves.toEqual(request);
    } finally {
      client.close();
      await provider.close();
    }
  });
});

function managementSurfaceRole(entry: CatalogEntry | undefined): { operations?: unknown[] } | undefined {
  return (entry?.roles as Array<{ role?: string; operations?: unknown[] }> | undefined)?.find(
    (role) => role.role === "management_surface",
  );
}

describe("SubcProvider receives a delivered storage descriptor", () => {
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
