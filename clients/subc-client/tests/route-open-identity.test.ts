import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { execFileSync } from "node:child_process";
import { closeSync, constants, fstatSync, mkdtempSync, openSync, readFileSync, rmSync, writeSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  SUBC_LAUNCH_NONCE_ENV,
  SUBC_MODULE_ID_ENV,
  ReverseRequestRegistry,
  SubcClient,
  type BindIdentity,
  type RouteTarget,
} from "../src/index.js";
import { launchNonce, resetLaunchNonceForTests, SUBC_LAUNCH_NONCE_FD_ENV } from "../src/launch-nonce.js";
import { createRouteHandle, newConnectionToken, type RouteHandle } from "../src/route-handle.js";
const TARGET: RouteTarget = { kind: "tool_provider", module_id: "aft" };
const IDENTITY: BindIdentity = { project_root: "/tmp/subc-ts-test", harness: "bun", session: "s1" };

const savedModuleId = process.env[SUBC_MODULE_ID_ENV];
const savedLaunchNonce = process.env[SUBC_LAUNCH_NONCE_ENV];
const savedLaunchNonceFd = process.env[SUBC_LAUNCH_NONCE_FD_ENV];
const scratchDirs: string[] = [];

// The launch nonce is read once per process and cached; these tests change the
// environment between cases, so each starts from an unread accessor.
beforeEach(() => {
  resetLaunchNonceForTests();
});

afterEach(() => {
  restoreEnv(SUBC_MODULE_ID_ENV, savedModuleId);
  restoreEnv(SUBC_LAUNCH_NONCE_ENV, savedLaunchNonce);
  restoreEnv(SUBC_LAUNCH_NONCE_FD_ENV, savedLaunchNonceFd);
  resetLaunchNonceForTests();
  for (const dir of scratchDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

/**
 * Make a FIFO without opening it, so a test can open it straight after the
 * accessor closes the launch-nonce descriptor and get that same descriptor
 * number. Spawning mkfifo in between could take the number first.
 */
function fifoPath(): string {
  const dir = mkdtempSync(join(tmpdir(), "subc-route-open-nonce-"));
  scratchDirs.push(dir);
  const path = join(dir, "fifo");
  execFileSync("mkfifo", [path]);
  return path;
}

/** A pipe holding `bytes`, returned with the `<fd>:<inode>` naming it. */
function pipeHolding(bytes: string, path = fifoPath()): { fd: number; fdValue: string } {
  const fd = openSync(path, constants.O_RDONLY | constants.O_NONBLOCK);
  const writer = openSync(path, constants.O_WRONLY);
  writeSync(writer, bytes);
  closeSync(writer);
  return { fd, fdValue: `${fd}:${BigInt.asUintN(64, fstatSync(fd, { bigint: true }).ino)}` };
}

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

  test("after HELLO's read, route.open gets the cached nonce and leaves the descriptor's number alone", async () => {
    process.env[SUBC_MODULE_ID_ENV] = "subc-mcp";
    process.env[SUBC_LAUNCH_NONCE_ENV] = "nonce-from-the-environment";

    // Put another pipe at the number HELLO's read closed. The lowest free
    // number is usually that one, but the runtime's own threads sometimes take
    // it first, so try again with a fresh pipe until the new one lands there.
    for (let attempt = 1; attempt <= 20; attempt += 1) {
      const daemon = pipeHolding("nonce-from-the-pipe");
      const squatterPath = fifoPath();
      process.env[SUBC_LAUNCH_NONCE_FD_ENV] = daemon.fdValue;
      resetLaunchNonceForTests();

      // The first reader, as the provider's HELLO would be.
      expect(launchNonce()?.value).toBe("nonce-from-the-pipe");
      const squatter = pipeHolding("someone else's bytes", squatterPath);
      if (squatter.fd !== daemon.fd) {
        closeSync(squatter.fd);
        continue;
      }

      const { client, captured } = routeOpenHarness();
      await client.routeOpen(TARGET, IDENTITY);

      expect(captured()).toEqual({
        op: "route.open",
        target: TARGET,
        identity: IDENTITY,
        consumer_identity: { module_id: "subc-mcp", launch_nonce: "nonce-from-the-pipe" },
      });
      // A reader that went back to the number would have taken these bytes.
      expect(readFileSync(squatter.fd, "utf8")).toBe("someone else's bytes");
      closeSync(squatter.fd);
      return;
    }
    throw new Error("a new pipe never landed on the number the accessor closed");
  });

  test("a refused descriptor opens the route without identity, never with the environment copy", async () => {
    process.env[SUBC_MODULE_ID_ENV] = "subc-mcp";
    process.env[SUBC_LAUNCH_NONCE_FD_ENV] = "3:not-an-inode";
    process.env[SUBC_LAUNCH_NONCE_ENV] = "nonce-from-the-environment";

    const { client, captured } = routeOpenHarness();
    await client.routeOpen(TARGET, IDENTITY);

    expect(captured()).toEqual({ op: "route.open", target: TARGET, identity: IDENTITY });
  });

  test("registered handlers derive route.open consumer_capabilities", async () => {
    delete process.env[SUBC_MODULE_ID_ENV];
    delete process.env[SUBC_LAUNCH_NONCE_ENV];
    const reverseRequests = new ReverseRequestRegistry();
    reverseRequests.onRequest("roots", () => new Uint8Array());
    reverseRequests.onRequest("elicitation", () => new Uint8Array());

    const { client, captured } = routeOpenHarness();
    await client.routeOpen(TARGET, IDENTITY, { reverseRequests });

    expect(captured()).toEqual({
      op: "route.open",
      target: TARGET,
      identity: IDENTITY,
      consumer_capabilities: ["elicitation", "roots"],
    });
  });

  test("route.open omits consumer_capabilities when no handlers are registered", async () => {
    delete process.env[SUBC_MODULE_ID_ENV];
    delete process.env[SUBC_LAUNCH_NONCE_ENV];

    const { client, captured } = routeOpenHarness();
    await client.routeOpen(TARGET, IDENTITY, { reverseRequests: new ReverseRequestRegistry() });

    const body = captured() as Record<string, unknown>;
    expect("consumer_capabilities" in body).toBe(false);
  });
});

function routeOpenHarness(): { client: SubcClient; captured: () => unknown } {
  let captured: unknown;
  const client = Object.create(SubcClient.prototype) as SubcClient;
  Object.assign(client, { routeModules: new Map() });
  // Patch the private collaborators through an `unknown` cast: intersecting
  // SubcClient with a public re-declaration of these (private) members reduces
  // to `never` under tsc, so reach them via a separate structural view instead.
  const internals = client as unknown as {
    encode(value: unknown): Uint8Array;
    controlRpc(body: Uint8Array, accept?: (frame: unknown) => boolean): Promise<unknown>;
    parseJson(frame: unknown): unknown;
    installRoute(channel: number, epoch: number): RouteHandle;
  };
  internals.encode = (value: unknown): Uint8Array => {
    captured = value;
    return new Uint8Array([1]);
  };
  const handle = createRouteHandle(7, 1, newConnectionToken());
  internals.controlRpc = async (_body, accept) => {
    accept?.({ header: { ty: 1 } });
    return { ok: true };
  };
  internals.parseJson = () => ({ op: "route.open", route_channel: 7, route_epoch: 1 });
  internals.installRoute = () => handle;
  return { client, captured: () => captured };
}

function restoreEnv(name: string, value: string | undefined): void {
  if (value === undefined) {
    delete process.env[name];
  } else {
    process.env[name] = value;
  }
}
