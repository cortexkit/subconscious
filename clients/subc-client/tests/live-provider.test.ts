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
