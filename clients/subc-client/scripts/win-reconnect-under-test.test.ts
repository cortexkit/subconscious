// Opt in with WIN_BUN_REPRO=1 bun test scripts/win-reconnect-under-test.test.ts.
// Bun also discovers scripts/*.test.ts during a bare bun test; keep this stress
// diagnostic skipped unless explicitly enabled, even though it lives outside tests/.
import { describe, expect, test } from "bun:test";
import { runSameProcess } from "./win-reconnect-repro.js";

const ITERATIONS = 50;

describe.skipIf(process.env.WIN_BUN_REPRO !== "1")("Windows Bun test-runner reconnect diagnostic", () => {
  test("managed same-process reconnects inside bun:test", async () => {
    console.log(`workload=managed phase=start pid=${process.pid}`);
    const stats = await runSameProcess("managed", ITERATIONS);
    expect(stats).toEqual({ connections: 100, closedConnections: 100, dataRequests: 100, routeOpens: 100 });
  }, 30_000);

  test("raw same-process reconnects inside bun:test", async () => {
    console.log(`workload=raw phase=start pid=${process.pid}`);
    const stats = await runSameProcess("raw", ITERATIONS);
    expect(stats).toEqual({ connections: 100, closedConnections: 100, dataRequests: 100, routeOpens: 0 });
  }, 30_000);
});
