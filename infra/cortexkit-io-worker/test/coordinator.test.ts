import { env } from "cloudflare:workers";
import {
  createExecutionContext,
  reset,
  runDurableObjectAlarm,
  runInDurableObject,
  waitOnExecutionContext,
} from "cloudflare:test";
import { beforeEach, describe, expect, it } from "vitest";
import { bytesToHex } from "../src/hex";
import type { Env } from "../src/env";
import type { RebuildResult } from "../src/rebuild";
import worker from "../src/worker";
import { TEST_ADMIN_TOKEN, TEST_WEBHOOK_SECRET } from "./keys";

interface RebuildProbe {
  calls: number;
  inFlight: number;
  maxInFlight: number;
}

type TestableCoordinator = {
  request(reason: string, immediate?: boolean): Promise<void>;
  rebuildExecutor: (env: Env) => Promise<RebuildResult>;
  probe: RebuildProbe;
};

function testEnv(): Env {
  return env as unknown as Env;
}

function coordinatorStub() {
  const coordinator = testEnv().REBUILD_COORDINATOR;
  return coordinator.get(coordinator.idFromName("release-index"));
}

async function signatureHeader(body: string): Promise<string> {
  const key = await crypto.subtle.importKey(
    "raw",
    new TextEncoder().encode(TEST_WEBHOOK_SECRET),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign"],
  );
  const signature = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(body));
  return `sha256=${bytesToHex(new Uint8Array(signature))}`;
}

async function installProbe(requestDuringFirstRebuild = false): Promise<void> {
  const stub = coordinatorStub();
  await runInDurableObject(stub, (instance) => {
    const coordinator = instance as unknown as TestableCoordinator;
    coordinator.probe = { calls: 0, inFlight: 0, maxInFlight: 0 };
    coordinator.rebuildExecutor = async () => {
      coordinator.probe.calls += 1;
      coordinator.probe.inFlight += 1;
      coordinator.probe.maxInFlight = Math.max(coordinator.probe.maxInFlight, coordinator.probe.inFlight);
      try {
        if (requestDuringFirstRebuild && coordinator.probe.calls === 1) {
          await coordinator.request("during-rebuild");
        }
        return { ok: true };
      } finally {
        coordinator.probe.inFlight -= 1;
      }
    };
  });
}

async function probe(): Promise<RebuildProbe> {
  return runInDurableObject(coordinatorStub(), (instance) => {
    return (instance as unknown as TestableCoordinator).probe;
  });
}

describe("RebuildCoordinator", () => {
  beforeEach(async () => {
    await reset();
  });

  it("coalesces a webhook-sized burst without making each handler wait for rebuild", async () => {
    await installProbe();
    const body = JSON.stringify({ action: "edited" });
    const signature = await signatureHeader(body);
    const requests = Array.from(
      { length: 30 },
      () =>
        new Request("https://cortexkit.io/webhooks/github", {
          method: "POST",
          headers: {
            "content-type": "application/json",
            "X-GitHub-Event": "release",
            "X-Hub-Signature-256": signature,
          },
          body,
        }),
    );

    const burstStartedAt = performance.now();
    const deliveries = await Promise.all(
      requests.map(async (request) => {
        const ctx = createExecutionContext();
        const startedAt = performance.now();
        const response = await worker.fetch(request, testEnv(), ctx);
        return { response, elapsedMs: performance.now() - startedAt, ctx };
      }),
    );

    expect(deliveries.map(({ response }) => response.status)).toEqual(Array(30).fill(202));
    await expect(Promise.all(deliveries.map(({ response }) => response.text()))).resolves.toEqual(
      Array(30).fill('{"queued":true}'),
    );
    expect(performance.now() - burstStartedAt).toBeLessThan(1_000);
    expect(deliveries.every(({ elapsedMs }) => elapsedMs < 100)).toBe(true);
    await Promise.all(deliveries.map(({ ctx }) => waitOnExecutionContext(ctx)));
    expect(await probe()).toEqual({ calls: 0, inFlight: 0, maxInFlight: 0 });

    expect(await runDurableObjectAlarm(coordinatorStub())).toBe(true);
    expect(await probe()).toEqual({ calls: 1, inFlight: 0, maxInFlight: 1 });
    const status = await worker.fetch(
      new Request("https://cortexkit.io/releases/v1/status", {
        headers: { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` },
      }),
      testEnv(),
    );
    expect(await status.json()).toMatchObject({
      pending: false,
      running: false,
      last_rebuild: { outcome: "ok", refusal_count: 0 },
    });
  });

  it("runs one serialized follow-up when a request arrives during rebuild", async () => {
    await installProbe(true);
    const queued = await coordinatorStub().fetch("https://rebuild-coordinator/request", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ reason: "release:published" }),
    });
    expect(queued.status).toBe(202);

    expect(await runDurableObjectAlarm(coordinatorStub())).toBe(true);
    expect(await probe()).toEqual({ calls: 2, inFlight: 0, maxInFlight: 1 });
    expect(await runDurableObjectAlarm(coordinatorStub())).toBe(false);
  });

  it("queues an admin reingest immediately instead of waiting for the debounce window", async () => {
    await installProbe();
    const ctx = createExecutionContext();
    const queued = await worker.fetch(
      new Request("https://cortexkit.io/releases/v1/reingest", {
        method: "POST",
        headers: { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` },
      }),
      testEnv(),
      ctx,
    );
    expect(queued.status).toBe(202);
    expect(await queued.text()).toBe('{"queued":true}');
    await waitOnExecutionContext(ctx);

    const status = await worker.fetch(
      new Request("https://cortexkit.io/releases/v1/status", {
        headers: { Authorization: `Bearer ${TEST_ADMIN_TOKEN}` },
      }),
      testEnv(),
    );
    expect(await status.json()).toMatchObject({ pending: true, running: false });
    const alarmAt = await runInDurableObject(coordinatorStub(), (_instance, state) => state.storage.getAlarm());
    expect(alarmAt).not.toBeNull();
    expect(alarmAt!).toBeLessThan(Date.now() + 2_000);
    expect(await runDurableObjectAlarm(coordinatorStub())).toBe(true);
    expect(await probe()).toEqual({ calls: 1, inFlight: 0, maxInFlight: 1 });
  });
});
