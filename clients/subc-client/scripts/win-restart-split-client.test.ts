// The client side of win-restart-split.ts run inside bun:test, so the client
// process is a test runner as in tests/managed-client.test.ts. The observer
// starts it with WIN_BUN_REPRO=1 and the scenario and connection file in the
// environment; a bare `bun test` discovers this file but skips it.
import { expect, test } from "bun:test";
import { runRestartClient, type Scenario } from "./win-restart-split.js";

const scenario = process.env.WIN_RESTART_SCENARIO as Scenario | undefined;
const connFile = process.env.WIN_RESTART_CONN_FILE;

// Default bun:test timeout (5 s), like the original tests.
test.skipIf(process.env.WIN_BUN_REPRO !== "1" || !scenario || !connFile)("restart split client", async () => {
  await runRestartClient(scenario!, connFile!, "test", async (call, expected) => {
    // The original tests assert each call this way.
    await expect(call).resolves.toEqual(expected);
  });
});
