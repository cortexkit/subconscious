import { afterEach, describe, expect, test } from "bun:test";

import {
  SUBC_LAUNCH_NONCE_ENV,
  SUBC_MODULE_ID_ENV,
  SubcClient,
  type BindIdentity,
  type RouteTarget,
} from "../src/index.js";

const TARGET: RouteTarget = { kind: "tool_provider", module_id: "aft" };
const IDENTITY: BindIdentity = { project_root: "/tmp/subc-ts-test", harness: "bun", session: "s1" };

const savedModuleId = process.env[SUBC_MODULE_ID_ENV];
const savedLaunchNonce = process.env[SUBC_LAUNCH_NONCE_ENV];

afterEach(() => {
  restoreEnv(SUBC_MODULE_ID_ENV, savedModuleId);
  restoreEnv(SUBC_LAUNCH_NONCE_ENV, savedLaunchNonce);
});

describe("SubcClient route.open consumer identity", () => {
  test("omits consumer_identity when either env var is absent", async () => {
    delete process.env[SUBC_MODULE_ID_ENV];
    delete process.env[SUBC_LAUNCH_NONCE_ENV];

    const { client, captured } = routeOpenHarness();
    await client.routeOpen(TARGET, IDENTITY);

    expect(captured()).toEqual({ op: "route.open", target: TARGET, identity: IDENTITY });
  });

  test("attaches consumer_identity when both env vars are present", async () => {
    process.env[SUBC_MODULE_ID_ENV] = "subc-mcp";
    process.env[SUBC_LAUNCH_NONCE_ENV] = "nonce-123";

    const { client, captured } = routeOpenHarness();
    await client.routeOpen(TARGET, IDENTITY);

    expect(captured()).toEqual({
      op: "route.open",
      target: TARGET,
      identity: IDENTITY,
      consumer_identity: { module_id: "subc-mcp", launch_nonce: "nonce-123" },
    });
  });
});

function routeOpenHarness(): { client: SubcClient; captured: () => unknown } {
  let captured: unknown;
  const client = Object.create(SubcClient.prototype) as SubcClient;
  // Patch the private collaborators through an `unknown` cast: intersecting
  // SubcClient with a public re-declaration of these (private) members reduces
  // to `never` under tsc, so reach them via a separate structural view instead.
  const internals = client as unknown as {
    encode(value: unknown): Uint8Array;
    controlRpc(body: Uint8Array): Promise<unknown>;
    parseJson(frame: unknown): unknown;
  };
  internals.encode = (value: unknown): Uint8Array => {
    captured = value;
    return new Uint8Array([1]);
  };
  internals.controlRpc = async () => ({ ok: true });
  internals.parseJson = () => ({ op: "route.open", route_channel: 7 });
  return { client, captured: () => captured };
}

function restoreEnv(name: string, value: string | undefined): void {
  if (value === undefined) {
    delete process.env[name];
  } else {
    process.env[name] = value;
  }
}
