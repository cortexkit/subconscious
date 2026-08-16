import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, test } from "bun:test";

import {
  CLIENT_AUTH_DOMAIN,
  computeProof,
  SERVER_PROOF_DOMAIN,
} from "../src/auth.js";
import {
  buildFlags,
  buildFrame,
  buildFrameWithVersion,
  decodeHeader,
  encodeFrame,
  FrameType,
  HEADER_LEN,
  HELLO_CORR,
  managementSurfaceManifest,
  Priority,
  PROTOCOL_VERSION,
  SubcProvider,
  type Frame,
  type ManifestInput,
  type ProviderConnectionState,
} from "../src/index.js";
import { createRouteHandle, newConnectionToken, type RouteHandle } from "../src/route-handle.js";

const KEY = Uint8Array.from(Array(32).fill(0x4b));
const DAEMON_ID = Uint8Array.from(Array(16).fill(0x6d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => i + 1));
const CONTROL_FLAGS = buildFlags(false, Priority.Passive, false);
const RECONNECT_BACKOFF = { baseMs: 5, capMs: 5, maxAttempts: 1 };

type ProviderReconnectInternals = {
  sock: unknown;
  generation: number;
  handleUnexpectedDrop(sock: unknown, generation: number, cause: Error): void;
};

function reconnectInternals(provider: SubcProvider): ProviderReconnectInternals {
  return provider as unknown as ProviderReconnectInternals;
}

const tempDirs: string[] = [];
const scriptedDaemons: ScriptedProviderDaemon[] = [];

afterEach(async () => {
  for (const daemon of scriptedDaemons.splice(0)) await daemon.stop();
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("managementSurfaceManifest", () => {
  test("builds the minimal ManagementSurface manifest shape", () => {
    expect(
      managementSurfaceManifest({
        moduleId: "test-effect-provider",
        operations: ["echo", { name: "wake", kind: "mutate" }],
        moduleVersion: "1.2.3",
      }),
    ).toEqual({
      module_id: "test-effect-provider",
      module_version: "1.2.3",
      protocol_ver: PROTOCOL_VERSION,
      trust_tier: "first_party",
      provides: [
        {
          role: "management_surface",
          operations: [
            { name: "echo", kind: "query" },
            { name: "wake", kind: "mutate" },
          ],
          config_schema: { type: "object" },
          observability: [],
          identity_scope: [],
          concurrency: "module_managed",
        },
      ],
      consumes: [],
      bindings: {
        storage: { kind: "sqlite", scope: "project", owns_schema: false },
        vault_grants: [],
        identity: { requires: [], optional: [] },
      },
    });
  });
});

describe("SubcProvider serve loop", () => {
  test("replies to channel-0 Ping with Pong preserving version, flags, and corr", async () => {
    const manifest = managementSurfaceManifest({ moduleId: "ping-provider", operations: ["echo"] });
    const server = await listenFakeServer();
    const dir = mkdtempSync(join(tmpdir(), "subc-provider-ping-"));
    const connFile = writeConnectionFile(dir, server.port);

    let sawPong!: () => void;
    const pongSeen = new Promise<void>((resolve) => {
      sawPong = resolve;
    });
    const serverDone = new Promise<void>((resolve, reject) => {
      server.server.once("connection", (socket) => {
        void runPingPeer(socket, manifest, sawPong).then(resolve, reject);
      });
    });

    let provider: SubcProvider | undefined;
    try {
      provider = await SubcProvider.connect({
        connectionFile: connFile,
        manifest,
        handler: async (_routeChannel, body) => body,
        launchNonce: "",
      });
      await pongSeen;
      await provider.close();
      await serverDone;
    } finally {
      await provider?.close().catch(() => undefined);
      server.server.close();
      rmSync(dir, { recursive: true, force: true });
    }
  });
  test("answers health.check with default ok report", async () => {
    const writes: Frame[] = [];
    const sock = fakeWritableSocket(writes);
    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      opts: { handler: () => Uint8Array; health: () => { status: "ok" } };
      handleControlRequest(frame: Frame, sock: unknown, generation: number): Promise<void>;
    };
    provider.sock = sock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.opts = {
      handler: () => new Uint8Array(0),
      health: () => ({ status: "ok" }),
    };

    await provider.handleControlRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, CONTROL_FLAGS, 0, 0, 88n, encodeJson({ op: "health.check" })),
      sock,
      1,
    );

    await waitForCondition(() => writes.length === 1, "health response");
    const response = writes[0]!;
    expect(response.header.ty).toBe(FrameType.Response);
    expect(response.header.channel).toBe(0);
    expect(response.header.corr).toBe(88n);
    expect(parseJson(response.body)).toEqual({ op: "health.check", status: "ok" });
  });

  test("health.check waits behind saturated provider request capacity", async () => {
    const writes: Frame[] = [];
    const sock = fakeWritableSocket(writes);
    const gate = createPermitGate(2);
    let entered = 0;
    let releaseHandler!: () => void;
    const blocked = new Promise<Uint8Array>(() => undefined);
    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      requestGate: { acquire(): Promise<() => void> };
      opts: { handler: () => Promise<Uint8Array>; health: () => { status: "ok" } };
      handleDataRequest(frame: Frame, handle: RouteHandle, sock: unknown, generation: number): Promise<void>;
      handleControlRequest(frame: Frame, sock: unknown, generation: number): Promise<void>;
    };
    provider.sock = sock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.requestGate = gate;
    provider.opts = {
      handler: async () => {
        entered += 1;
        releaseHandler = () => undefined;
        return await blocked;
      },
      health: () => ({ status: "ok" }),
    };

    const handle = createRouteHandle(7, 1, newConnectionToken());
    void provider.handleDataRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 1n, encodeJson({ n: 1 })),
      handle,
      sock,
      1,
    );
    void provider.handleDataRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 2n, encodeJson({ n: 2 })),
      handle,
      sock,
      1,
    );
    await waitForCondition(() => entered === 2, "saturated handler gate");

    await provider.handleControlRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, CONTROL_FLAGS, 0, 0, 89n, encodeJson({ op: "health.check" })),
      sock,
      1,
    );
    await new Promise((resolve) => setTimeout(resolve, 30));
    expect(writes).toEqual([]);
    releaseHandler();
  });

  test("does not invoke a capacity-queued handler after its request is cancelled", async () => {
    const writes: Frame[] = [];
    const sock = fakeWritableSocket(writes);
    const gate = createPermitGate(1);
    const token = newConnectionToken();
    const handle = createRouteHandle(7, 1, token);
    const handled: number[] = [];
    let releaseFirst!: () => void;
    const firstBlocked = new Promise<void>((resolve) => {
      releaseFirst = resolve;
    });
    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      pending: Map<string, unknown>;
      liveRoutes: Map<number, RouteHandle>;
      connectionToken: object;
      requestGate: { acquire(): Promise<() => void> };
      opts: { handler: (_handle: RouteHandle, body: Uint8Array) => Promise<Uint8Array> };
      handleDataRequest(frame: Frame, handle: RouteHandle, sock: unknown, generation: number): Promise<void>;
      dispatch(frame: Frame, sock: unknown, generation: number): Promise<boolean>;
    };
    provider.sock = sock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.pending = new Map();
    provider.liveRoutes = new Map([[handle.channel, handle]]);
    provider.connectionToken = token;
    provider.requestGate = gate;
    provider.opts = {
      handler: async (_handle, body) => {
        const request = parseJson(body) as { n: number };
        handled.push(request.n);
        if (request.n === 1) await firstBlocked;
        return encodeJson({ n: request.n });
      },
    };

    const first = provider.handleDataRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 1n, encodeJson({ n: 1 })),
      handle,
      sock,
      1,
    );
    await waitForCondition(() => handled.length === 1, "first handler to hold provider capacity");

    const cancelled = provider.handleDataRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 2n, encodeJson({ n: 2 })),
      handle,
      sock,
      1,
    );
    await waitForCondition(() => provider.inflight.has("7:1:2"), "queued request registration");
    await provider.dispatch(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Cancel, buildFlags(false, Priority.Interactive, false), 7, 1, 2n, new Uint8Array(0)),
      sock,
      1,
    );

    releaseFirst();
    await Promise.all([first, cancelled]);

    expect(handled).toEqual([1]);
    expect(writes).toHaveLength(2);
    expect(writes[0]?.header).toMatchObject({ ty: FrameType.Response, channel: 7, epoch: 1, corr: 1n });
    expect(parseJson(writes[0]!.body)).toEqual({ n: 1 });
    expect(writes[1]?.header).toMatchObject({ ty: FrameType.Error, channel: 7, epoch: 1, corr: 2n });
    expect(parseJson(writes[1]!.body)).toEqual({ code: "cancelled", message: "request cancelled" });
    expect(provider.inflight.size).toBe(0);
  });

  test("emits a cancelled terminal when the request is aborted during its handler", async () => {
    const writes: Frame[] = [];
    const sock = fakeWritableSocket(writes);
    const token = newConnectionToken();
    const handle = createRouteHandle(7, 1, token);
    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      liveRoutes: Map<number, RouteHandle>;
      connectionToken: object;
      opts: { handler: () => Promise<Uint8Array> };
      handleDataRequest(frame: Frame, handle: RouteHandle, sock: unknown, generation: number): Promise<void>;
    };
    provider.sock = sock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.liveRoutes = new Map([[handle.channel, handle]]);
    provider.connectionToken = token;
    provider.opts = {
      handler: async () => {
        provider.inflight.get("7:1:3")!.abort();
        return encodeJson({ ignored: true });
      },
    };

    await provider.handleDataRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 3n, encodeJson({ n: 3 })),
      handle,
      sock,
      1,
    );

    expect(writes).toHaveLength(1);
    expect(writes[0]?.header).toMatchObject({ ty: FrameType.Error, channel: 7, epoch: 1, corr: 3n });
    expect(parseJson(writes[0]!.body)).toEqual({ code: "cancelled", message: "request cancelled" });
  });

  test("emits a cancelled terminal when an aborted handler rejects", async () => {
    const writes: Frame[] = [];
    const sock = fakeWritableSocket(writes);
    const token = newConnectionToken();
    const handle = createRouteHandle(7, 1, token);
    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      liveRoutes: Map<number, RouteHandle>;
      connectionToken: object;
      opts: { handler: () => Promise<Uint8Array> };
      handleDataRequest(frame: Frame, handle: RouteHandle, sock: unknown, generation: number): Promise<void>;
    };
    provider.sock = sock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.liveRoutes = new Map([[handle.channel, handle]]);
    provider.connectionToken = token;
    provider.opts = {
      handler: async () => {
        provider.inflight.get("7:1:4")!.abort();
        throw new Error("handler stopped after abort");
      },
    };

    await provider.handleDataRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 4n, encodeJson({ n: 4 })),
      handle,
      sock,
      1,
    );

    expect(writes).toHaveLength(1);
    expect(writes[0]?.header).toMatchObject({ ty: FrameType.Error, channel: 7, epoch: 1, corr: 4n });
    expect(parseJson(writes[0]!.body)).toEqual({ code: "cancelled", message: "request cancelled" });
  });

  test("does not write a cancelled terminal after route teardown bumps the generation", async () => {
    const writes: Frame[] = [];
    const sock = fakeWritableSocket(writes);
    const token = newConnectionToken();
    const handle = createRouteHandle(7, 1, token);
    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      liveRoutes: Map<number, RouteHandle>;
      connectionToken: object;
      opts: { handler: () => Promise<Uint8Array> };
      handleDataRequest(frame: Frame, handle: RouteHandle, sock: unknown, generation: number): Promise<void>;
    };
    provider.sock = sock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.liveRoutes = new Map([[handle.channel, handle]]);
    provider.connectionToken = token;
    provider.opts = {
      handler: async () => {
        provider.liveRoutes.delete(handle.channel);
        provider.generation = 2;
        provider.inflight.get("7:1:5")!.abort();
        return encodeJson({ ignored: true });
      },
    };

    await provider.handleDataRequest(
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 5n, encodeJson({ n: 5 })),
      handle,
      sock,
      1,
    );

    expect(writes).toEqual([]);
  });

  // Wire spec 3.3.0: a route.bind on an installed channel with a strictly higher
  // epoch replaces the stale install (the daemon freed that binding; its route-gone
  // GOODBYE is best-effort and can be dropped), firing the replaced install's
  // onRouteGone. Equal-or-lower epoch is a protocol violation: rejected loud, the
  // installed route untouched.
  test("route.bind on an installed channel replaces on higher epoch only", async () => {
    const writes: Frame[] = [];
    const sock = fakeWritableSocket(writes);
    const gone: { channel: number; epoch: number }[] = [];
    const bound: { channel: number; epoch: number }[] = [];
    const token = newConnectionToken();
    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      pending: Map<string, unknown>;
      liveRoutes: Map<number, RouteHandle>;
      connectionToken: object;
      opts: {
        handler: () => Uint8Array;
        onBound: (handle: RouteHandle) => void;
        onRouteGone: (handle: RouteHandle) => void;
      };
      handleControlRequest(frame: Frame, sock: unknown, generation: number): Promise<void>;
    };
    provider.sock = sock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.pending = new Map();
    provider.liveRoutes = new Map();
    provider.connectionToken = token;
    provider.opts = {
      handler: () => new Uint8Array(0),
      onBound: (handle) => bound.push({ channel: handle.channel, epoch: handle.epoch }),
      onRouteGone: (handle) => gone.push({ channel: handle.channel, epoch: handle.epoch }),
    };

    const bindFrame = (epoch: number, corr: bigint) =>
      buildFrameWithVersion(
        PROTOCOL_VERSION,
        FrameType.Request,
        CONTROL_FLAGS,
        0,
        0,
        corr,
        encodeJson({ op: "route.bind", route_channel: 8, epoch, target: { kind: "tool_provider", module_id: "m" }, identity: { project_root: "/tmp/p", harness: "test", session: "s" } }),
      );

    // Install epoch 4.
    await provider.handleControlRequest(bindFrame(4, 90n), sock, 1);
    expect(writes.at(-1)?.header.ty).toBe(FrameType.Response);
    expect(bound).toEqual([{ channel: 8, epoch: 4 }]);
    expect(gone).toEqual([]);

    // Same epoch: rejected, install untouched.
    await provider.handleControlRequest(bindFrame(4, 91n), sock, 1);
    expect(writes.at(-1)?.header.ty).toBe(FrameType.Error);
    expect(parseJson(writes.at(-1)!.body)).toMatchObject({ code: "route_rejected" });
    expect(provider.liveRoutes.get(8)?.epoch).toBe(4);
    expect(gone).toEqual([]);

    // Lower epoch: same rejection.
    await provider.handleControlRequest(bindFrame(3, 92n), sock, 1);
    expect(writes.at(-1)?.header.ty).toBe(FrameType.Error);
    expect(provider.liveRoutes.get(8)?.epoch).toBe(4);
    expect(gone).toEqual([]);

    // Strictly higher epoch: implicit replace — stale install torn down, new bound.
    await provider.handleControlRequest(bindFrame(5, 93n), sock, 1);
    expect(writes.at(-1)?.header.ty).toBe(FrameType.Response);
    expect(gone).toEqual([{ channel: 8, epoch: 4 }]);
    expect(bound).toEqual([
      { channel: 8, epoch: 4 },
      { channel: 8, epoch: 5 },
    ]);
    expect(provider.liveRoutes.get(8)?.epoch).toBe(5);
  });

  test("cancel handles write rejection and still sends on a healthy socket", async () => {
    const failed = providerControlHarness(rejectingWritableSocket());
    const unhandled = await recordUnhandledRejections(() => {
      expect(failed.provider.cancel(failed.handle, 42n)).toBeUndefined();
    });
    expect(unhandled).toEqual([]);

    const writes: Frame[] = [];
    const healthy = providerControlHarness(fakeWritableSocket(writes));
    expect(healthy.provider.cancel(healthy.handle, 43n)).toBeUndefined();
    await Promise.resolve();

    expect(writes).toHaveLength(1);
    expect(writes[0]?.header).toMatchObject({
      ty: FrameType.Cancel,
      channel: healthy.handle.channel,
      epoch: healthy.handle.epoch,
      corr: 43n,
    });
  });

  test("closeRoute handles write rejection and still sends on a healthy socket", async () => {
    const failed = providerControlHarness(rejectingWritableSocket());
    const unhandled = await recordUnhandledRejections(() => {
      expect(failed.provider.closeRoute(failed.handle)).toBeUndefined();
    });
    expect(unhandled).toEqual([]);

    const writes: Frame[] = [];
    const healthy = providerControlHarness(fakeWritableSocket(writes));
    expect(healthy.provider.closeRoute(healthy.handle)).toBeUndefined();
    await Promise.resolve();

    expect(writes).toHaveLength(1);
    expect(writes[0]?.header).toMatchObject({
      ty: FrameType.Goodbye,
      channel: healthy.handle.channel,
      epoch: healthy.handle.epoch,
      corr: 0n,
    });
  });
});

describe("SubcProvider managed reconnect", () => {
  test("drops stale handler responses after a reconnect generation replaces the socket", async () => {
    const oldWrites: Frame[] = [];
    const newWrites: Frame[] = [];
    const oldSock = fakeWritableSocket(oldWrites);
    const newSock = fakeWritableSocket(newWrites);
    let releaseHandler!: (body: Uint8Array) => void;

    const provider = Object.create(SubcProvider.prototype) as {
      sock: unknown;
      generation: number;
      closeStarted: boolean;
      closedErr: Error | null;
      inflight: Map<string, AbortController>;
      opts: { handler: (routeChannel: number, body: Uint8Array) => Promise<Uint8Array> };
      handleDataRequest(frame: Frame, handle: RouteHandle, sock: unknown, generation: number): Promise<void>;
    };
    provider.sock = oldSock;
    provider.generation = 1;
    provider.closeStarted = false;
    provider.closedErr = null;
    provider.inflight = new Map();
    provider.opts = {
      handler: async () =>
        await new Promise<Uint8Array>((resolve) => {
          releaseHandler = resolve;
        }),
    };

    const request = buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Request, buildFlags(false, Priority.Interactive, false), 7, 1, 99n, encodeJson({ method: "slow" }));
    const handle = createRouteHandle(7, 1, newConnectionToken());
    const handling = provider.handleDataRequest(request, handle, oldSock, 1);
    await waitForCondition(() => releaseHandler !== undefined, "handler entered");

    provider.sock = newSock;
    provider.generation = 2;
    releaseHandler(encodeJson({ ok: true }));
    await handling;

    expect(oldWrites).toEqual([]);
    expect(newWrites).toEqual([]);
  });

  test("supersedes a stalled reconnect after a later drop and re-registers", async () => {
    const daemon = await ScriptedProviderDaemon.start(["ack", "drop", "ack"]);
    const dir = trackedTempDir("subc-provider-stalled-reconnect-");
    const connFile = writeConnectionFile(dir, daemon.port);
    const sleep = createManualSleep();
    const provider = await SubcProvider.connect({
      connectionFile: connFile,
      manifest: managementSurfaceManifest({ moduleId: "stalled-reconnect-provider", operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
      reconnectBackoff: RECONNECT_BACKOFF,
      sleep: sleep.sleep,
    });

    try {
      daemon.dropLatest();
      await daemon.waitForHelloCount(2);
      await waitForCondition(
        () => sleep.calls.includes(RECONNECT_BACKOFF.baseMs),
        "stalled reconnect backoff",
      );

      const internals = reconnectInternals(provider);
      internals.handleUnexpectedDrop(
        internals.sock,
        internals.generation,
        new Error("second daemon restart"),
      );

      await daemon.waitForHelloCount(3);
      await waitForCondition(() => provider.currentEpoch() === 2, "provider epoch after superseding reconnect");
      expect(daemon.helloCount).toBe(3);
    } finally {
      sleep.resolveAll();
      await provider.close();
    }
  });

  test("single-flights duplicate drops for the same generation", async () => {
    const daemon = await ScriptedProviderDaemon.start();
    const dir = trackedTempDir("subc-provider-single-flight-");
    const connFile = writeConnectionFile(dir, daemon.port);
    const provider = await SubcProvider.connect({
      connectionFile: connFile,
      manifest: managementSurfaceManifest({ moduleId: "single-flight-provider", operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
      reconnectBackoff: RECONNECT_BACKOFF,
    });

    try {
      const internals = reconnectInternals(provider);
      const { sock, generation } = internals;
      internals.handleUnexpectedDrop(sock, generation, new Error("first drop"));
      internals.handleUnexpectedDrop(sock, generation, new Error("duplicate drop"));

      await daemon.waitForHelloCount(2);
      await waitForCondition(() => provider.currentEpoch() === 2, "provider epoch after one re-registration");
      expect(daemon.helloCount).toBe(2);
    } finally {
      await provider.close();
    }
  });

  test("orders a superseding reconnect after down and ignores stale reconnect events", async () => {
    const daemon = await ScriptedProviderDaemon.start(["ack", "drop", "ack"]);
    const dir = trackedTempDir("subc-provider-reconnect-events-");
    const connFile = writeConnectionFile(dir, daemon.port);
    const sleep = createManualSleep();
    const events: ProviderConnectionState[] = [];
    const provider = await SubcProvider.connect({
      connectionFile: connFile,
      manifest: managementSurfaceManifest({ moduleId: "reconnect-events-provider", operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
      reconnectBackoff: RECONNECT_BACKOFF,
      sleep: sleep.sleep,
      onConnectionState: (event) => {
        events.push(event);
      },
    });

    try {
      await waitForCondition(() => events.some((event) => event.state === "connected"), "connected event");
      daemon.dropLatest();
      await daemon.waitForHelloCount(2);
      await waitForCondition(
        () => sleep.calls.includes(RECONNECT_BACKOFF.baseMs),
        "stalled reconnect backoff",
      );

      const internals = reconnectInternals(provider);
      internals.handleUnexpectedDrop(
        internals.sock,
        internals.generation,
        new Error("second daemon restart"),
      );

      await daemon.waitForHelloCount(3);
      await waitForCondition(() => provider.currentEpoch() === 2, "provider epoch after superseding reconnect");
      await waitForCondition(
        () => events.filter((event) => event.state === "reconnecting").length === 2,
        "reconnecting events",
      );
      expect(events.map((event) => event.state)).toEqual([
        "connected",
        "down",
        "reconnecting",
        "down",
        "reconnecting",
      ]);

      const down = events.filter(
        (event): event is Extract<ProviderConnectionState, { state: "down" }> => event.state === "down",
      );
      expect(down[1]?.cause.message).toBe("second daemon restart");

      sleep.resolveAll();
      await new Promise((resolve) => setTimeout(resolve, 20));
      expect(events.filter((event) => event.state === "reconnecting")).toEqual([
        { state: "reconnecting", attempt: 1 },
        { state: "reconnecting", attempt: 1 },
      ]);
      expect(daemon.helloCount).toBe(3);
    } finally {
      sleep.resolveAll();
      await provider.close();
    }
  });

  test("coalesces restored events while currentEpoch advances for each completed re-registration", async () => {
    const daemon = await ScriptedProviderDaemon.start();
    const dir = trackedTempDir("subc-provider-debounce-");
    const connFile = writeConnectionFile(dir, daemon.port);
    const sleep = createManualSleep();
    const events: ProviderConnectionState[] = [];

    const provider = await SubcProvider.connect({
      connectionFile: connFile,
      manifest: managementSurfaceManifest({ moduleId: "debounce-provider", operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
      reconnectBackoff: RECONNECT_BACKOFF,
      restoredDebounceMs: 50,
      sleep: sleep.sleep,
      onConnectionState: (event) => {
        events.push(event);
      },
    });

    try {
      expect(provider.currentEpoch()).toBe(1);
      daemon.dropLatest();
      await daemon.waitForHelloCount(2);
      await waitForCondition(() => provider.currentEpoch() === 2, "provider epoch 2");
      daemon.dropLatest();
      await daemon.waitForHelloCount(3);
      await waitForCondition(() => provider.currentEpoch() === 3, "provider epoch 3");

      expect(sleep.calls).toEqual([50, 50]);
      sleep.resolveAll();
      await waitForCondition(
        () => events.some((event) => event.state === "restored" && event.epoch === 3),
        "coalesced restored event",
      );

      const restored = events.filter((event): event is Extract<ProviderConnectionState, { state: "restored" }> => event.state === "restored");
      expect(restored).toEqual([{ state: "restored", epoch: 3 }]);
    } finally {
      await provider.close();
    }
  });

  test("retries duplicate_module_id on re-HELLO but treats it as fatal on initial connect", async () => {
    const initialDuplicate = await ScriptedProviderDaemon.start([
      { code: "duplicate_module_id", message: "already registered" },
    ]);
    const initialDir = trackedTempDir("subc-provider-initial-dup-");
    const initialConnFile = writeConnectionFile(initialDir, initialDuplicate.port);

    await expect(
      SubcProvider.connect({
        connectionFile: initialConnFile,
        manifest: managementSurfaceManifest({ moduleId: "initial-dup-provider", operations: ["echo"] }),
        handler: async (_routeChannel, body) => body,
      }),
    ).rejects.toMatchObject({ code: "duplicate_module_id" });
    await initialDuplicate.stop();

    const daemon = await ScriptedProviderDaemon.start([
      "ack",
      { code: "duplicate_module_id", message: "stale registration" },
      "ack",
    ]);
    const dir = trackedTempDir("subc-provider-rehello-dup-");
    const connFile = writeConnectionFile(dir, daemon.port);
    const sleeps: number[] = [];

    const provider = await SubcProvider.connect({
      connectionFile: connFile,
      manifest: managementSurfaceManifest({ moduleId: "rehello-dup-provider", operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
      reconnectBackoff: RECONNECT_BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      daemon.dropLatest();
      await daemon.waitForHelloCount(3);
      await waitForCondition(() => provider.currentEpoch() === 2, "provider epoch after duplicate retry");
      expect(sleeps).toEqual([5]);
    } finally {
      await provider.close();
    }
  });

  test("close after a drop stops reconnect attempts", async () => {
    const daemon = await ScriptedProviderDaemon.start();
    const dir = trackedTempDir("subc-provider-close-drop-");
    const connFile = writeConnectionFile(dir, daemon.port);
    const sleep = createManualSleep();
    const events: ProviderConnectionState[] = [];

    const provider = await SubcProvider.connect({
      connectionFile: connFile,
      manifest: managementSurfaceManifest({ moduleId: "close-drop-provider", operations: ["echo"] }),
      handler: async (_routeChannel, body) => body,
      reconnectBackoff: RECONNECT_BACKOFF,
      restoredDebounceMs: 1,
      sleep: sleep.sleep,
      onConnectionState: (event) => {
        events.push(event);
      },
    });

    await daemon.stop();
    await waitForCondition(() => events.some((event) => event.state === "down"), "provider down event");
    await waitForCondition(() => sleep.calls.includes(5), "provider reconnect backoff sleep");
    await provider.close();
    sleep.resolveAll();
    await waitForCondition(() => provider.currentEpoch() === 1, "provider remains at initial epoch");
    expect(daemon.helloCount).toBe(1);
  });
});


async function listenFakeServer(): Promise<{ server: Server; port: number }> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address() as AddressInfo;
  return { server, port: address.port };
}

function writeConnectionFile(dir: string, port: number): string {
  const path = join(dir, "subc-connection.json");
  writeFileSync(
    path,
    JSON.stringify({
      schema: 1,
      endpoints: [{ host: "127.0.0.1", port }],
      key: Array.from(KEY),
      daemon_id: Array.from(DAEMON_ID),
      pid: process.pid,
      daemon_ver: "fake-subc",
    }),
    { mode: 0o600 },
  );
  chmodSync(path, 0o600);
  return path;
}

async function runPingPeer(socket: Socket, manifest: ManifestInput, sawPong: () => void): Promise<void> {
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 10_000;
  await authenticateFakeServer(reader, socket, deadline);

  const hello = await readFrame(reader, deadline);
  expect(hello.header.ty).toBe(FrameType.Hello);
  expect(hello.header.channel).toBe(0);
  expect(hello.header.corr).toBe(HELLO_CORR);
  expect(hello.header.flags).toBe(CONTROL_FLAGS);
  expect(Buffer.from(hello.body).toString("utf8")).toBe(
    JSON.stringify({ manifest, protocol_ver: PROTOCOL_VERSION, control_ops: ["health.check"] }),
  );

  await writeFrame(
    socket,
    buildFrameWithVersion(PROTOCOL_VERSION, FrameType.HelloAck, CONTROL_FLAGS, 0, 0, hello.header.corr, encodeJson({
      negotiated_ver: PROTOCOL_VERSION,
      subc_ops: ["server.describe", "catalog.list", "route.open", "route.poll"],
      subc_capabilities: ["manifest_registration_v1"],
    })),
    deadline,
  );

  await writeFrame(
    socket,
    buildFrame(FrameType.Ping, buildFlags(false, Priority.Interactive, false), 0, 0, 77n, new Uint8Array(0)),
    deadline,
  );
  const pong = await readFrame(reader, deadline);
  expect(pong.header.ty).toBe(FrameType.Pong);
  expect(pong.header.ver).toBe(PROTOCOL_VERSION);
  expect(pong.header.channel).toBe(0);
  expect(pong.header.corr).toBe(77n);
  expect(pong.header.flags).toBe(buildFlags(false, Priority.Interactive, false));
  expect(pong.body.length).toBe(0);
  sawPong();

  const goodbye = await readFrame(reader, deadline);
  expect(goodbye.header.ty).toBe(FrameType.Goodbye);
  expect(goodbye.header.channel).toBe(0);
  socket.destroy();
}

async function authenticateFakeServer(reader: SocketReader, socket: Socket, deadline: number): Promise<void> {
  const hello = await readAuthMessage<{ client_nonce: number[]; role: string }>(reader, deadline);
  expect(hello.role).toBe("client");
  const clientNonce = Uint8Array.from(hello.client_nonce);
  const serverProof = computeProof(KEY, SERVER_PROOF_DOMAIN, clientNonce, SERVER_NONCE, DAEMON_ID);
  await writeAuthMessage(
    socket,
    {
      daemon_id: Array.from(DAEMON_ID),
      server_nonce: Array.from(SERVER_NONCE),
      daemon_ver: "fake-subc",
      server_proof: Array.from(serverProof),
    },
    deadline,
  );

  const auth = await readAuthMessage<{ client_auth: number[] }>(reader, deadline);
  const expected = computeProof(KEY, CLIENT_AUTH_DOMAIN, clientNonce, SERVER_NONCE, DAEMON_ID);
  expect(Buffer.from(auth.client_auth).equals(Buffer.from(expected))).toBe(true);
}

async function readAuthMessage<T>(reader: SocketReader, deadline: number): Promise<T> {
  const lenBytes = await reader.readExact(4, deadline);
  const len = new DataView(lenBytes.buffer, lenBytes.byteOffset, 4).getUint32(0, true);
  const body = len === 0 ? new Uint8Array(0) : await reader.readExact(len, deadline);
  return JSON.parse(Buffer.from(body).toString("utf8")) as T;
}

async function writeAuthMessage(socket: Socket, value: unknown, deadline: number): Promise<void> {
  const body = Buffer.from(JSON.stringify(value), "utf8");
  const len = new Uint8Array(4);
  new DataView(len.buffer).setUint32(0, body.length, true);
  await writeAll(socket, len, deadline);
  await writeAll(socket, body, deadline);
}

async function readFrame(reader: SocketReader, deadline: number): Promise<Frame> {
  const header = decodeHeader(await reader.readExact(HEADER_LEN, deadline));
  const body = header.len === 0 ? new Uint8Array(0) : await reader.readExact(header.len, deadline);
  return { header, body };
}

async function writeFrame(socket: Socket, frame: Frame, deadline: number): Promise<void> {
  await writeAll(socket, encodeFrame(frame), deadline);
}

function encodeJson(value: unknown): Uint8Array {
  return new Uint8Array(Buffer.from(JSON.stringify(value), "utf8"));
}

// Decode the way the SHIPPED client does. This helper existed in two forms
// across the test files: `Buffer.from(...).toString("utf8")` here and in the
// other suites, and `new TextDecoder().decode(...)` in this one. They agree on
// every ordinary body -- including subarray views into a larger read buffer,
// which is what a frame body is -- so the drift was invisible.
//
// They diverge on exactly one input: a UTF-8 BOM. TextDecoder STRIPS it and
// parses; Buffer keeps it and JSON.parse throws. So the same wire bytes were
// accepted by this suite and rejected by the other two, and a test asserting
// how the client handles a BOM-prefixed body would have proved opposite things
// depending on which file it lived in.
//
// src/client.ts and src/provider.ts both use the Buffer form, so THAT is what a
// test helper must mirror: a helper that is more permissive than the code under
// test cannot observe the code being too strict.
function parseJson(bytes: Uint8Array): unknown {
  return JSON.parse(Buffer.from(bytes).toString("utf8"));
}

function createPermitGate(capacity: number): { acquire(): Promise<() => void> } {
  let available = capacity;
  const waiters: Array<() => void> = [];
  return {
    async acquire(): Promise<() => void> {
      if (available > 0) {
        available -= 1;
      } else {
        await new Promise<void>((resolve) => waiters.push(resolve));
      }
      let released = false;
      return () => {
        if (released) return;
        released = true;
        const next = waiters.shift();
        if (next) next();
        else available += 1;
      };
    },
  };
}

async function writeAll(socket: Socket, bytes: Uint8Array, deadline: number): Promise<void> {
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("timed out writing fake daemon bytes")), Math.max(0, deadline - Date.now()));
    socket.write(Buffer.from(bytes), (err) => {
      clearTimeout(timer);
      if (err) reject(err);
      else resolve();
    });
  });
}

interface Waiter {
  need: number;
  resolve: (bytes: Uint8Array) => void;
  reject: (err: Error) => void;
  timer: ReturnType<typeof setTimeout> | null;
}

class SocketReader {
  private chunks: Buffer[] = [];
  private buffered = 0;
  private waiter: Waiter | null = null;
  private closedErr: Error | null = null;

  constructor(socket: Socket) {
    socket.on("data", (chunk: Buffer) => {
      this.chunks.push(chunk);
      this.buffered += chunk.length;
      this.tryServe();
    });
    const fail = (err: Error) => {
      if (!this.closedErr) this.closedErr = err;
      this.tryServe();
    };
    socket.on("error", (err) => fail(err instanceof Error ? err : new Error(String(err))));
    socket.on("end", () => fail(new Error("fake daemon socket ended")));
    socket.on("close", () => fail(new Error("fake daemon socket closed")));
  }

  readExact(n: number, deadline: number): Promise<Uint8Array> {
    if (this.waiter) return Promise.reject(new Error("concurrent readExact is not supported"));
    if (n === 0) return Promise.resolve(new Uint8Array(0));
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.waiter = null;
        reject(new Error(`timed out waiting for ${n} fake daemon bytes`));
      }, Math.max(0, deadline - Date.now()));
      this.waiter = { need: n, resolve, reject, timer };
      this.tryServe();
    });
  }

  private tryServe(): void {
    const waiter = this.waiter;
    if (!waiter) return;
    if (this.buffered >= waiter.need) {
      const out = this.take(waiter.need);
      this.waiter = null;
      if (waiter.timer) clearTimeout(waiter.timer);
      waiter.resolve(out);
      return;
    }
    if (this.closedErr) {
      this.waiter = null;
      if (waiter.timer) clearTimeout(waiter.timer);
      waiter.reject(this.closedErr);
    }
  }

  private take(n: number): Uint8Array {
    const out = Buffer.allocUnsafe(n);
    let off = 0;
    while (off < n) {
      const head = this.chunks[0]!;
      const want = n - off;
      if (head.length <= want) {
        head.copy(out, off);
        off += head.length;
        this.chunks.shift();
      } else {
        head.copy(out, off, 0, want);
        this.chunks[0] = head.subarray(want);
        off += want;
      }
    }
    this.buffered -= n;
    return out;
  }
}


type HelloResult = "ack" | "drop" | { code: string; message: string };

class ScriptedProviderDaemon {
  readonly sockets = new Set<Socket>();
  helloCount = 0;
  private readonly waiters: Array<{ count: number; resolve: () => void }> = [];
  private stopped = false;

  private constructor(
    readonly server: Server,
    readonly port: number,
    private readonly helloResults: HelloResult[],
  ) {}

  static async start(helloResults: HelloResult[] = []): Promise<ScriptedProviderDaemon> {
    const server = createServer();
    await new Promise<void>((resolve, reject) => {
      server.once("error", reject);
      server.listen(0, "127.0.0.1", resolve);
    });
    const daemon = new ScriptedProviderDaemon(server, (server.address() as AddressInfo).port, [...helloResults]);
    server.on("connection", (socket) => {
      daemon.sockets.add(socket);
      socket.once("close", () => daemon.sockets.delete(socket));
      void daemon.handleConnection(socket).catch(() => socket.destroy());
    });
    scriptedDaemons.push(daemon);
    return daemon;
  }

  async waitForHelloCount(count: number): Promise<void> {
    if (this.helloCount >= count) return;
    await new Promise<void>((resolve) => {
      this.waiters.push({ count, resolve });
    });
  }

  dropLatest(): void {
    const socket = Array.from(this.sockets).at(-1);
    socket?.destroy();
  }

  async stop(): Promise<void> {
    if (this.stopped) return;
    this.stopped = true;
    for (const socket of this.sockets) socket.destroy();
    await new Promise<void>((resolve) => this.server.close(() => resolve()));
  }

  private async handleConnection(socket: Socket): Promise<void> {
    const reader = new SocketReader(socket);
    const deadline = Date.now() + 10_000;
    await authenticateFakeServer(reader, socket, deadline);
    const hello = await readFrame(reader, deadline);
    expect(hello.header.ty).toBe(FrameType.Hello);
    expect(hello.header.channel).toBe(0);
    expect(hello.header.corr).toBe(HELLO_CORR);

    const result = this.helloResults.shift() ?? "ack";
    if (result === "ack") {
      await writeFrame(
        socket,
        buildFrameWithVersion(PROTOCOL_VERSION, FrameType.HelloAck, CONTROL_FLAGS, 0, 0, hello.header.corr, encodeJson({
          negotiated_ver: PROTOCOL_VERSION,
          subc_ops: ["server.describe", "catalog.list", "route.open", "route.poll"],
          subc_capabilities: ["manifest_registration_v1"],
        })),
        deadline,
      );
      this.recordHello();
      await this.drainUntilClose(reader);
      return;
    }

    if (result === "drop") {
      this.recordHello();
      socket.destroy();
      return;
    }

    await writeFrame(
      socket,
      buildFrameWithVersion(PROTOCOL_VERSION, FrameType.Error, CONTROL_FLAGS, 0, 0, hello.header.corr, encodeJson(result)),
      deadline,
    );
    this.recordHello();
  }

  private async drainUntilClose(reader: SocketReader): Promise<void> {
    for (;;) {
      await readFrame(reader, Date.now() + 60_000);
    }
  }

  private recordHello(): void {
    this.helloCount += 1;
    for (let i = this.waiters.length - 1; i >= 0; i -= 1) {
      const waiter = this.waiters[i]!;
      if (this.helloCount >= waiter.count) {
        this.waiters.splice(i, 1);
        waiter.resolve();
      }
    }
  }
}

function trackedTempDir(prefix: string): string {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  tempDirs.push(dir);
  return dir;
}

function fakeWritableSocket(writes: Frame[]): unknown {
  return {
    async write(bytes: Uint8Array): Promise<void> {
      const header = decodeHeader(bytes.subarray(0, HEADER_LEN));
      const body = header.len === 0 ? new Uint8Array(0) : bytes.subarray(HEADER_LEN, HEADER_LEN + header.len);
      writes.push({ header, body });
    },
    close(): void {
      // Unit tests use this fake only to observe writes; there is no OS socket to close.
    },
  };
}

function rejectingWritableSocket(): unknown {
  return {
    write(): Promise<void> {
      return Promise.reject(new Error("write failed"));
    },
  };
}

function providerControlHarness(
  sock: unknown,
  channel = 8,
  epoch = 3,
): { provider: SubcProvider; handle: RouteHandle } {
  const provider = Object.create(SubcProvider.prototype) as SubcProvider;
  const internals = provider as unknown as {
    sock: unknown;
    generation: number;
    connectionToken: object;
    closeStarted: boolean;
    closedErr: Error | null;
    inflight: Map<string, AbortController>;
    pending: Map<string, unknown>;
    liveRoutes: Map<number, RouteHandle>;
  };
  const token = newConnectionToken();
  const handle = createRouteHandle(channel, epoch, token);
  Object.assign(internals, {
    sock,
    generation: 1,
    connectionToken: token,
    closeStarted: false,
    closedErr: null,
    inflight: new Map(),
    pending: new Map(),
    liveRoutes: new Map([[channel, handle]]),
  });
  return { provider, handle };
}

async function recordUnhandledRejections(run: () => void): Promise<unknown[]> {
  const reasons: unknown[] = [];
  const record = (reason: unknown): void => {
    reasons.push(reason);
  };
  process.on("unhandledRejection", record);
  try {
    run();
    await Promise.resolve();
    await new Promise((resolve) => setTimeout(resolve, 0));
    return reasons;
  } finally {
    process.off("unhandledRejection", record);
  }
}

function createManualSleep(): {
  calls: number[];
  sleep: (ms: number) => Promise<void>;
  resolveAll: () => void;
} {
  const calls: number[] = [];
  const waiters: Array<() => void> = [];
  return {
    calls,
    sleep(ms: number): Promise<void> {
      calls.push(ms);
      return new Promise((resolve) => {
        waiters.push(resolve);
      });
    },
    resolveAll(): void {
      for (const resolve of waiters.splice(0)) resolve();
    },
  };
}

async function waitForCondition(predicate: () => boolean, label: string, timeoutMs = 2_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  throw new Error(`timed out waiting for ${label}`);
}
