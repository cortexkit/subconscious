import { afterEach, describe, expect, test } from "bun:test";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { chmodSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  buildFlags,
  buildFrame,
  CLIENT_AUTH_DOMAIN,
  computeProof,
  decodeHeader,
  encodeFrame,
  FrameType,
  HEADER_LEN,
  Priority,
  ReverseRequestRegistry,
  SERVER_PROOF_DOMAIN,
  SUBSCRIPTION_FLAG,
  SocketClosedError,
  SubcCallError,
  DaemonUnavailableError,
  SubcClient,
  type BindIdentity,
  type CatalogEntry,
  type CatalogEventDeclaration,
  type Frame,
  type RouteScope,
} from "../src/index.js";
import { rejectionOf } from "./settle.js";

const KEY = Uint8Array.from(Array(32).fill(0x4c));
const DAEMON_ID = Uint8Array.from(Array(16).fill(0x7d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => 0xa0 + i));
const IDENTITY: BindIdentity = { project_root: "/tmp/subc-client-test", harness: "bun", session: "managed" };
const BACKOFF = { baseMs: 5, capMs: 10, maxAttempts: 4 };
const DAEMON_UNAVAILABLE_MESSAGE = "CortexKit's daemon is not running or can't be reached on this computer, so this tool call was not sent. Retrying won't start it. Stop calling CortexKit tools and ask the user to start the daemon (run `ck setup`, or restart the CortexKit service), then try again after they confirm it is running.";

const tempDirs: string[] = [];
const daemons: FakeDaemon[] = [];

interface FakeStats {
  routeOpens: number;
  dataRequests: number;
  dataBodies: unknown[];
  dataFlags: number[];
  reverseReplies: Frame[];
  requestFrames: { channel: number; controlOp?: string }[];
  // The consumer_identity sent on each route.open, in order (undefined when absent),
  // so a test can assert the principal survives a reconnect reopen.
  routeOpenConsumerIdentities: (unknown | undefined)[];
  routeOpenBodies: Buffer[];
  // Count of accepted TCP connections — a test asserts a healthy-socket deadline
  // does NOT trigger a reconnect (count stays 1).
  connections: number;
  closedConnections: number;
}

interface FakeDaemonOptions {
  stats: FakeStats;
  // Daemon-side HMAC key — a daemon started with a different key models the
  // key rotation that happens on every real daemon restart.
  key?: Uint8Array;
  daemonId?: Uint8Array;
  // "delay-body": reply with the frame HEADER immediately, then the body after
  //   `delayBodyMs` — so the client's read loop is mid-frame (bytes present) when a
  //   short request deadline fires, deterministically exercising timeout arbitration.
  // "silent-hold": accept the data request and never reply, keeping the socket
  //   healthy — a genuine deadline with no drop. (The fake still answers Pings,
  //   like the real daemon: the liveness probe must EXONERATE this socket.)
  // "half-open": accept the FIRST data request then go deaf — never answer
  //   anything again on that connection, Pings included, while keeping TCP
  //   open. The post-sleep/wake peer-vanished shape: the liveness probe must
  //   CONVICT it. Later connections serve normally so recovery is provable.
  // "unknown-channel-once": reject the FIRST data request with the daemon
  //   router's unknown_channel ERROR (a stale bind after a module restart), then
  //   serve subsequent requests normally — the re-opened route works.
  // "module-reloading-once": reject the FIRST data request with the daemon's
  //   drain refusal (module_reloading), refuse the next two route.opens with
  //   module_reloading while the module "restarts", then serve normally.
  // "module-reloading-always": reject EVERY data request with module_reloading.
  dataMode?: "echo" | "drop" | "error" | "delay-body" | "silent-hold" | "half-open" | "unknown-channel-once" | "unknown-channel-always" | "stale-epoch-once" | "module-reloading-once" | "module-reloading-always";
  delayBodyMs?: number;
  routeOpenError?: { code: string; message: string };
  // Refuse this module on every connection at or after the selected connection;
  // unlike fail-first, a refused identity never becomes healthy by attempt count.
  routeOpenRefuseModule?: {
    module_id: string;
    code: string;
    message: string;
    refuseFromConnection?: number;
  };
  // Drop the transport while opening this module, so reconnect tests can prove a
  // dead socket remains connection-wide rather than looking like a route refusal.
  routeOpenDropModule?: { module_id: string; dropFromConnection?: number };
  // Reject the first N route.open requests with this code (a booting-target
  // simulation), then serve subsequent ones normally.
  routeOpenFailFirst?: { count: number; code: string; message: string };
  // Take over every route.open answer. The hook is not awaited: it answers
  // through `answer` whenever it likes (no argument accepts the open, an error
  // body refuses it), so a test can hold opens outstanding or script refusals
  // against a fake clock.
  onRouteOpen?: (
    moduleId: string | undefined,
    answer: (error?: { code: string; message: string; detail?: unknown }) => Promise<void>,
  ) => void;
  closeAfterDataResponses?: number;
  // Before answering catalog.list, emit daemon-originated channel-0 control
  // pushes: first a well-formed route.closing, then one with an UNPARSEABLE
  // body. Models the #31 push family arriving interleaved with control traffic,
  // including the garbage arm the MUST-ignore clause exists for.
  controlPushesBeforeCatalogList?: boolean;
  // Accept catalog.list and never answer it (keep reading). Models the daemon's
  // connection loop parked in a slow inline channel-0 handler (route.open's
  // bind relay waits up to ~12s in production) — the case where OUR ping going
  // unanswered is explained by OUR own in-flight control op.
  holdCatalogList?: boolean;
  catalogModules?: CatalogEntry[];
  onHeldData?: (reply: () => Promise<void>) => void;
  reverseRequest?: { body: Uint8Array; corr: bigint };
}

interface FakeDaemon {
  port: number;
  stop(): Promise<void>;
}

function newStats(): FakeStats {
  return {
    routeOpens: 0,
    dataRequests: 0,
    dataBodies: [],
    dataFlags: [],
    reverseReplies: [],
    requestFrames: [],
    routeOpenConsumerIdentities: [],
    routeOpenBodies: [],
    connections: 0,
    closedConnections: 0,
  };
}

afterEach(async () => {
  for (const daemon of daemons.splice(0)) await daemon.stop();
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("daemon unavailable", () => {
  test("missing connection file requires user action and preserves the cause", async () => {
    const { connFile } = tempConnectionFile();
    const error = await SubcClient.connect({ connectionFile: connFile }).catch((err: unknown) => err);
    expect(error).toBeInstanceOf(DaemonUnavailableError);
    expect(error).toMatchObject({
      kind: "not_sent",
      recovery: "user_action_required",
      code: "ENOENT",
      message: DAEMON_UNAVAILABLE_MESSAGE,
      cause: { code: "ENOENT", path: connFile },
    });
  });

  test("refused port requires user action and preserves the cause", async () => {
    const { connFile } = tempConnectionFile();
    writeConnectionFile(connFile, await closedPort());
    const error = await SubcClient.connect({ connectionFile: connFile }).catch((err: unknown) => err);
    expect(error).toBeInstanceOf(DaemonUnavailableError);
    expect(error).toMatchObject({
      kind: "not_sent",
      recovery: "user_action_required",
      code: "ECONNREFUSED",
      message: DAEMON_UNAVAILABLE_MESSAGE,
      cause: { code: "ECONNREFUSED" },
    });
  });
});

describe("daemon-originated control pushes", () => {
  test("route.closing reaches the observer parsed; a garbage push is ignored and the stream survives", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, controlPushesBeforeCatalogList: true });
    writeConnectionFile(connFile, daemon.port);

    const seen: { op: string; body: Record<string, unknown> }[] = [];
    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      onControlPush: (push) => seen.push(push),
    });
    try {
      // The fake daemon writes BOTH pushes (one well-formed, one unparseable)
      // ahead of the catalog.list response on the same stream, so the resolved
      // reply proves the read loop dispatched past both.
      const modules = await client.catalogList();
      expect(modules).toEqual([]);

      expect(seen.length).toBe(1);
      expect(seen[0]!.op).toBe("route.closing");
      expect(seen[0]!.body.module_id).toBe("fake-aft");
      expect(seen[0]!.body.reason).toBe("restart");

      // Aliveness after the garbage push: a further control roundtrip works on
      // the same connection (the MUST-ignore clause held, nothing failed).
      expect(await client.catalogList()).toEqual([]);
    } finally {
      client.close();
    }
  });

  test("an observer throw cannot fail the read loop or the in-flight control call", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, controlPushesBeforeCatalogList: true });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      onControlPush: () => {
        throw new Error("observer bug");
      },
    });
    try {
      // The push (and the observer throw) land before this response on the same
      // stream; a resolved reply means the throw was contained.
      expect(await client.catalogList()).toEqual([]);
    } finally {
      client.close();
    }
  });
});

describe("SubcClient capability resolution", () => {
  test("catalogList preserves opaque role declarations and tolerates their absence", async () => {
    const { connFile } = tempConnectionFile();
    const role_declarations = {
      "routing/v1": { consumers: [{ name: "chat", default_demand: 2 }] },
      "future/v2": { unknown: [null, true, { nested: "λ" }] },
    };
    const entry = { ...catalogEntry("role-provider", undefined), role_declarations };
    const legacy = catalogEntry("legacy-provider", undefined);
    const daemon = await startFakeDaemon({ stats: newStats(), catalogModules: [entry, legacy] });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile });
    try {
      const modules = await client.catalogList();
      expect(modules.find((module) => module.module_id === "role-provider")?.role_declarations).toEqual(role_declarations);
      expect(modules.find((module) => module.module_id === "legacy-provider")).not.toHaveProperty("role_declarations");
    } finally {
      client.close();
    }
  });

  test("catalogList preserves open operation delivery and tolerates unknown entry fields", async () => {
    const { connFile } = tempConnectionFile();
    const operation_delivery = { status: "watch", future: "stream-v2" };
    const entry = { ...catalogEntry("watch-provider", undefined), operation_delivery, future_catalog_field: true };
    const legacy = catalogEntry("unary-provider", undefined);
    const daemon = await startFakeDaemon({ stats: newStats(), catalogModules: [entry, legacy] });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile });
    try {
      const modules = await client.catalogList();
      expect(modules.find((module) => module.module_id === "watch-provider")?.operation_delivery).toEqual(operation_delivery);
      expect(modules.find((module) => module.module_id === "unary-provider")?.operation_delivery).toBeUndefined();
    } finally {
      client.close();
    }
  });

  test("catalogList preserves declared bus events", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const events: CatalogEventDeclaration[] = [
      {
        name: "task_completed",
        version: 2,
        headers: ["project_id"],
        summary: "A task completed",
      },
    ];
    const daemon = await startFakeDaemon({
      stats,
      catalogModules: [{ ...catalogEntry("publisher", undefined), events }],
    });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile });
    try {
      const module = (await client.catalogList()).find((entry) => entry.module_id === "publisher");
      expect(module?.events).toEqual(events);
    } finally {
      client.close();
    }
  });

  test("resolves only explicit capability claims across zero, one, and many claimant arms", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({
      stats,
      catalogModules: [
        catalogEntry("fallback-only", undefined),
        catalogEntry("single-provider", ["single-provider/v1"]),
        catalogEntry("z-provider", ["many-provider/v1"]),
        catalogEntry("a-provider", ["many-provider/v1"]),
      ],
    });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile });
    try {
      expect(await client.resolveProviders("missing-provider/v1")).toEqual([]);
      expect(await rejectionOf(client.resolveProvider("missing-provider/v1"))).toMatchObject({
        code: "capability_unprovided",
      });
      expect(await client.resolveProvider("single-provider/v1")).toBe("single-provider");
      expect(await client.resolveProviders("many-provider/v1")).toEqual(["a-provider", "z-provider"]);
      expect(await rejectionOf(client.resolveProvider("many-provider/v1"))).toMatchObject({
        code: "capability_ambiguous",
      });
    } finally {
      client.close();
    }
  });

  test("does not fall back to module_id equality when a module does not claim", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({
      stats,
      catalogModules: [catalogEntry("fallback-only", undefined)],
    });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile });
    try {
      expect(await client.resolveProviders("fallback-only/v1")).toEqual([]);
      expect(await rejectionOf(client.resolveProvider("fallback-only/v1"))).toMatchObject({
        code: "capability_unprovided",
      });
    } finally {
      client.close();
    }
  });

  test("rejects malformed capability identifiers before catalog I/O", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, catalogModules: [] });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile });
    try {
      expect(await rejectionOf(client.resolveProviders("Bad/v1"))).toMatchObject({
        code: "invalid_capability_identifier",
      });
      expect(stats.requestFrames).toEqual([]);
    } finally {
      client.close();
    }
  });
});

describe("subscription request flag", () => {
  // The daemon excludes marked held subscriptions from module-drain quiescence waits.
  test("marks held subscriptions but not ordinary calls", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      await client.call("flags-provider", "ordinary");
      const handle = await client.routeOpen(
        { kind: "tool_provider", module_id: "flags-provider" },
        IDENTITY,
      );
      const subscription = client.subscribe(handle, { method: "held" }, () => undefined);
      await subscription.closed;

      expect(stats.dataFlags).toHaveLength(2);
      expect(stats.dataFlags[0]! & SUBSCRIPTION_FLAG).toBe(0);
      expect(stats.dataFlags[1]! & SUBSCRIPTION_FLAG).toBe(SUBSCRIPTION_FLAG);
    } finally {
      client.close();
    }
  });
});

describe("reverse-request lane", () => {
  test("no handler returns reverse_request_unhandled instead of dropping the request", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const reverseBody = encodeJson({ jsonrpc: "2.0", id: 7, method: "elicitation/create", params: { prompt: "ok?" } });
    const daemon = await startFakeDaemon({
      stats,
      reverseRequest: { body: reverseBody, corr: 900n },
    });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      expect(await client.call<unknown>("reverse-provider", "echo")).toEqual({ method: "echo" });
      await waitFor(() => stats.reverseReplies.length === 1, "unhandled reverse reply");
      expect(stats.reverseReplies[0]!.header.ty).toBe(FrameType.Error);
      expect(parseJson(stats.reverseReplies[0]!.body)).toMatchObject({ code: "reverse_request_unhandled" });
    } finally {
      client.close();
    }
  });

  test("round trip preserves raw body and corr and returns raw handler bytes", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const reverseBody = encodeJson({ jsonrpc: "2.0", id: "raw", method: "elicitation/create", params: { z: 1, a: 2 } });
    const reverseResponse = encodeJson({ jsonrpc: "2.0", id: "raw", result: { action: "accept" } });
    const reverseRequests = new ReverseRequestRegistry();
    let seenBody: Uint8Array | undefined;
    let seenCorr: bigint | undefined;
    let seenMethod: string | undefined;
    reverseRequests.onRequest("elicitation", (body, ctx) => {
      seenBody = body;
      seenCorr = ctx.corr;
      seenMethod = ctx.method;
      return reverseResponse;
    });
    const daemon = await startFakeDaemon({
      stats,
      reverseRequest: { body: reverseBody, corr: 901n },
    });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      expect(await client.call<unknown>("reverse-provider", "echo", undefined, { reverseRequests })).toEqual({ method: "echo" });
      await waitFor(() => stats.reverseReplies.length === 1, "handled reverse reply");
      expect(Array.from(seenBody ?? [])).toEqual(Array.from(reverseBody));
      expect(seenCorr).toBe(901n);
      expect(seenMethod).toBe("elicitation/create");
      expect(stats.reverseReplies[0]!.header.ty).toBe(FrameType.Response);
      expect(stats.reverseReplies[0]!.header.corr).toBe(901n);
      expect(Array.from(stats.reverseReplies[0]!.body)).toEqual(Array.from(reverseResponse));
    } finally {
      client.close();
    }
  });

  test("throwing handler returns typed refusal while sibling in-flight requests still resolve", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const reverseRequests = new ReverseRequestRegistry();
    reverseRequests.onRequest("sampling", () => {
      throw new Error("person prompt exploded");
    });
    const daemon = await startFakeDaemon({
      stats,
      reverseRequest: {
        body: encodeJson({ jsonrpc: "2.0", id: 8, method: "sampling/createMessage", params: {} }),
        corr: 902n,
      },
    });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      const first = client.call("reverse-provider", "first", undefined, { reverseRequests });
      const sibling = client.call("reverse-provider", "sibling", undefined, { reverseRequests });
      expect(await Promise.all([first, sibling])).toEqual([{ method: "first" }, { method: "sibling" }]);
      await waitFor(() => stats.reverseReplies.length === 1, "throwing-handler reverse reply");
      expect(stats.reverseReplies[0]!.header.ty).toBe(FrameType.Error);
      expect(parseJson(stats.reverseReplies[0]!.body)).toEqual({
        code: "reverse_request_unhandled",
        message: "person prompt exploded",
      });
      expect(stats.connections).toBe(1);
    } finally {
      client.close();
    }
  });
});

describe("SubcClient managed call", () => {
  test("reuses one route.open for repeated calls to the same module", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      expect(await client.call<unknown>("managed-provider", "echo", { n: 1 })).toEqual({
        method: "echo",
        params: { n: 1 },
      });
      expect(await client.call<unknown>("managed-provider", "echo", { n: 2 })).toEqual({
        method: "echo",
        params: { n: 2 },
      });
      expect(await client.call<unknown>("managed-provider", "echo", { n: 3 })).toEqual({
        method: "echo",
        params: { n: 3 },
      });

      expect(stats.routeOpens).toBe(1);
      expect(stats.dataRequests).toBe(3);
    } finally {
      client.close();
    }
  });

  test("opens a fresh route for a managed call after reconnect", async () => {
    const { connFile } = tempConnectionFile();
    const sleeps: number[] = [];
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      expect(await client.call<unknown>("managed-provider", "echo", { n: 1 })).toEqual({
        method: "echo",
        params: { n: 1 },
      });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({ stats: secondStats });
      writeConnectionFile(connFile, second.port);

      expect(await client.call<unknown>("managed-provider", "echo", { n: 2 })).toEqual({
        method: "echo",
        params: { n: 2 },
      });

      expect(firstStats.routeOpens).toBe(1);
      expect(secondStats.routeOpens).toBe(1);
      expect(firstStats.dataRequests).toBe(1);
      expect(secondStats.dataRequests).toBe(1);
      expect(sleeps).toEqual([]);
    } finally {
      client.close();
    }
  });

  test("recovers across a daemon restart that rotates the key (stale-file auth race is transient)", async () => {
    // The daemon rotates its key on every restart but keeps its fixed port. A
    // client racing the restart reads the pre-rotation connection file, connects
    // successfully (port unchanged), and fails the HMAC handshake. That proof
    // mismatch must be retried-with-re-read (the next attempt picks up the
    // rotated file), not treated as a permanent impostor verdict — permanent
    // treatment turns every daemon restart into a fleet-wide wedge that only a
    // host-app restart clears.
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const first = await startFakeDaemon({ stats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);

    let staleWindowRemaining = 0;
    const rotatedKey = Uint8Array.from(Array(32).fill(0x99));
    let rotatedPort = 0;
    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async () => {
        // Each reconnect backoff sleep consumes one "stale window" tick; when the
        // window closes, the rotated file lands on disk — modelling the client
        // racing ahead of the daemon's file publish.
        if (staleWindowRemaining > 0) {
          staleWindowRemaining -= 1;
          if (staleWindowRemaining === 0) writeConnectionFile(connFile, rotatedPort, rotatedKey);
        }
      },
    });

    try {
      expect(await client.call<unknown>("managed-provider", "echo", { n: 1 })).toEqual({
        method: "echo",
        params: { n: 1 },
      });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      // Restart: new daemon, ROTATED key. The old file (old key) stays on disk for
      // the first two reconnect attempts — both connect fine and fail auth.
      const restartStats = newStats();
      const second = await startFakeDaemon({ stats: restartStats, key: rotatedKey });
      rotatedPort = second.port;
      writeConnectionFile(connFile, second.port); // old KEY, new port: connects, proof mismatch
      staleWindowRemaining = 2;

      expect(await client.call<unknown>("managed-provider", "echo", { n: 2 })).toEqual({
        method: "echo",
        params: { n: 2 },
      });
      expect(restartStats.dataRequests).toBe(1);
      // The recovery consumed the stale window: at least one auth-failed attempt
      // happened before the rotated file landed (proving AuthError was retried).
      expect(staleWindowRemaining).toBe(0);
    } finally {
      client.close();
    }
  });

  test("re-attaches the same consumer_identity when a later call opens after reconnect", async () => {
    // A route opened with a consumer_identity (principal attestation) must send the
    // SAME consumer_identity when it is reopened on a fresh connection after a
    // reconnect. Dropping it there would let the daemon re-stamp the route with a
    // weaker principal than it was originally bound under — a silent post-reconnect
    // trust downgrade. Connection loss discards the old route, so the later call
    // must carry the original identity on its fresh route.open.
    const { connFile } = tempConnectionFile();
    const consumerIdentity = { module_id: "reserved-module", launch_nonce: "nonce-abc" };
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async () => {},
    });

    try {
      // First call installs the cached route on connection 1 (carrying the identity).
      expect(
        await client.call<unknown>("managed-provider", "echo", { n: 1 }, { consumerIdentity }),
      ).toEqual({ method: "echo", params: { n: 1 } });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({ stats: secondStats });
      writeConnectionFile(connFile, second.port);

      // The second call reconnects and opens a fresh route with the same identity.
      expect(
        await client.call<unknown>("managed-provider", "echo", { n: 2 }, { consumerIdentity }),
      ).toEqual({ method: "echo", params: { n: 2 } });

      // The new route on connection 2 must carry the identical consumer_identity.
      expect(firstStats.routeOpenConsumerIdentities).toEqual([consumerIdentity]);
      expect(secondStats.routeOpenConsumerIdentities).toEqual([consumerIdentity]);
    } finally {
      client.close();
    }
  });

  test("isolates a refused route while reconnecting healthy managed calls", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemonOptions: FakeDaemonOptions = {
      stats,
      routeOpenRefuseModule: {
        module_id: "beta",
        code: "project_origin_unclassifiable",
        message: "beta identity is no longer classifiable",
        refuseFromConnection: 2,
      },
    };
    const daemon = await startFakeDaemon(daemonOptions);
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
    });
    try {
      expect(await client.call<unknown>("alpha", "echo", { phase: "cache" })).toMatchObject({
        params: { phase: "cache" },
      });
      expect(await client.call<unknown>("beta", "echo", { phase: "cache" })).toMatchObject({
        params: { phase: "cache" },
      });

      // Arm the existing response-count drop only after both routes are cached.
      // The successful trigger response then closes connection 1; on reconnect,
      // only the route used by the next call is opened.
      daemonOptions.closeAfterDataResponses = 1;
      expect(await client.call<unknown>("alpha", "echo", { phase: "drop" })).toMatchObject({
        params: { phase: "drop" },
      });
      daemonOptions.closeAfterDataResponses = undefined;
      await waitFor(() => clientClosedErr(client) !== null, "client to observe the route-isolation drop");

      expect(await client.call<unknown>("alpha", "echo", { phase: "reconnected" })).toMatchObject({
        params: { phase: "reconnected" },
      });
      expect(stats.connections).toBe(2);

      expect(await rejectionOf(client.call("beta", "echo", { phase: "caller-owned-refusal" }))).toMatchObject({
        kind: "terminal",
        code: "project_origin_unclassifiable",
      });
      expect(stats.connections).toBe(2);
      expect(stats.routeOpens).toBe(4);
    } finally {
      client.close();
    }
  });

  test("does not wait out retryable route patience while opening the next managed route", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemonOptions: FakeDaemonOptions = {
      stats,
      routeOpenRefuseModule: {
        module_id: "beta",
        code: "module_reloading",
        message: "beta is reloading",
        refuseFromConnection: 2,
      },
    };
    const daemon = await startFakeDaemon(daemonOptions);
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      routeOpenRetryDeadlineMs: 120,
    });
    try {
      await client.call("alpha", "echo", { phase: "cache" });
      await client.call("beta", "echo", { phase: "cache" });
      daemonOptions.closeAfterDataResponses = 1;
      await client.call("alpha", "echo", { phase: "drop" });
      daemonOptions.closeAfterDataResponses = undefined;
      await waitFor(() => clientClosedErr(client) !== null, "client to observe the retryable-refusal drop");

      const reconnectStartedAt = Date.now();
      expect(await client.call<unknown>("alpha", "echo", { phase: "reconnected" })).toMatchObject({
        params: { phase: "reconnected" },
      });
      expect(Date.now() - reconnectStartedAt).toBeLessThan(2_000);
      expect(stats.connections).toBe(2);

      // Beta's old route was discarded with the connection. Its next call owns
      // the bounded module-reload patience and observes the daemon's refusal.
      expect(await rejectionOf(client.call("beta", "echo", { phase: "patient-open" }))).toMatchObject({
        kind: "not_sent",
        code: "module_reloading",
      });
    } finally {
      client.close();
    }
  });

  test("surfaces outcome_unknown and does not retry after bytes were written but no response arrived", async () => {
    const { connFile } = tempConnectionFile();
    const sleeps: number[] = [];
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "drop" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: { baseMs: 1, capMs: 1, maxAttempts: 1 },
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      expect(await rejectionOf(client.call("managed-provider", "mutate", { n: 1 }))).toMatchObject({
        kind: "outcome_unknown",
      });
      expect(stats.dataRequests).toBe(1);
      expect(stats.dataBodies).toEqual([{ method: "mutate", params: { n: 1 } }]);
      expect(sleeps).toEqual([]);
    } finally {
      client.close();
    }
  });

  test("wraps a module Error frame as a terminal managed call error", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "error" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      expect(await rejectionOf(client.call("managed-provider", "explode", {}))).toMatchObject({
        kind: "terminal",
        code: "module_boom",
        // The wire ErrorBody.detail payload must survive the parse verbatim:
        // typed refusal reasons (e.g. synapse certification refusals) ride it,
        // and consumers classify on it. Dropping it strands those reasons at
        // the transport boundary.
        detail: { reason: "certification_refused", lane: "embed" },
      });
      expect(stats.dataRequests).toBe(1);
    } finally {
      client.close();
    }
  });

  test("evicts the cached route and retries once in place on unknown_channel", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "unknown-channel-once" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      // First call: the daemon router rejects the stale bind with unknown_channel;
      // the client must evict the cached route, re-open, and retry once in place —
      // the caller sees success, not the terminal error.
      expect(await client.call<unknown>("managed-provider", "echo", { n: 1 })).toEqual({
        method: "echo",
        params: { n: 1 },
      });
      // Two route.opens (initial + post-eviction reopen), two data requests
      // (rejected + retried), all on ONE connection (no reconnect).
      expect(stats.routeOpens).toBe(2);
      expect(stats.dataRequests).toBe(2);
      expect(stats.connections).toBe(1);
    } finally {
      client.close();
    }
  });

  test("evicts the cached route and retries once in place on stale_route_epoch", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    // Issue #39's code: the daemon dropped the request BEFORE delivery (the
    // route's epoch was released mid-flight), so it is provably not-forwarded
    // and joins unknown_channel's evict-reopen-retry-once class. Same remedy,
    // sharper cause.
    const daemon = await startFakeDaemon({ stats, dataMode: "stale-epoch-once" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      expect(await client.call<unknown>("managed-provider", "echo", { n: 1 })).toEqual({
        method: "echo",
        params: { n: 1 },
      });
      // Two route.opens (initial + post-eviction reopen), two data requests
      // (rejected + retried), one connection (no reconnect).
      expect(stats.routeOpens).toBe(2);
      expect(stats.dataRequests).toBe(2);
      expect(stats.connections).toBe(1);
    } finally {
      client.close();
    }
  });

  test("evicts the retry route when a second unknown_channel surfaces terminal", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    // EVERY data request rejects with unknown_channel: the retry-once budget must
    // surface the terminal error on the second rejection instead of retrying
    // forever against a daemon that keeps refusing.
    const daemon = await startFakeDaemon({ stats, dataMode: "unknown-channel-always" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      expect(await rejectionOf(client.call("managed-provider", "echo", { n: 1 }))).toMatchObject({
        kind: "terminal",
        code: "unknown_channel",
      });
      // Exactly two attempts: original + the single in-place retry.
      expect(stats.dataRequests).toBe(2);
      expect(stats.routeOpens).toBe(2);

      expect(await rejectionOf(client.call("managed-provider", "echo", { n: 2 }))).toMatchObject({
        kind: "terminal",
        code: "unknown_channel",
      });
      // The third data attempt begins a new managed call. It must open a route
      // before sending data, rather than reuse the retry's dead channel.
      expect(stats.requestFrames[4]).toEqual({ channel: 0, controlOp: "route.open" });
    } finally {
      client.close();
    }
  });

  test("resends once on module_reloading after reopening past the drain's route.open refusals", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    // The daemon answers a request on an established route with module_reloading
    // only before forwarding it, while the module drains for a restart. The
    // client must evict the route, wait out the reopen refusals until the
    // module is back, and resend the request once: the caller sees success.
    const daemon = await startFakeDaemon({ stats, dataMode: "module-reloading-once" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF });
    try {
      expect(await client.call<unknown>("managed-provider", "echo", { n: 1 }, { timeoutMs: 5_000 })).toEqual({
        method: "echo",
        params: { n: 1 },
      });
      // Four route.opens (initial, two refused during the drain, the reopen
      // that succeeds) and two data requests (refused + resent) on ONE connection.
      expect(stats.routeOpens).toBe(4);
      expect(stats.dataRequests).toBe(2);
      expect(stats.connections).toBe(1);
    } finally {
      client.close();
    }
  });

  test("throws a second module_reloading instead of resending again", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "module-reloading-always" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      expect(await rejectionOf(client.call("managed-provider", "echo", { n: 1 }))).toMatchObject({
        code: "module_reloading",
      });
      // Exactly two attempts: the original and the single resend, which shares
      // the same one-resend budget as unknown_channel.
      expect(stats.dataRequests).toBe(2);
      expect(stats.routeOpens).toBe(2);
    } finally {
      client.close();
    }
  });

  test("does not resend after module_reloading once the call's signal is aborted", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "module-reloading-once" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF });
    try {
      const controller = new AbortController();
      controller.abort();
      // The caller asked for the work to stop, so the refusal is returned as it is.
      expect(
        await rejectionOf(client.call("managed-provider", "echo", { n: 1 }, { signal: controller.signal })),
      ).toMatchObject({ code: "module_reloading" });
      expect(stats.dataRequests).toBe(1);
      expect(stats.routeOpens).toBe(1);
    } finally {
      client.close();
    }
  });

  test("uses capped reconnect backoff for transient reconnect failures", async () => {
    const { connFile } = tempConnectionFile();
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);
    const sleeps: number[] = [];

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      await client.call("managed-provider", "echo", { n: 1 });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const refusedPort = await closedPort();
      writeConnectionFile(connFile, refusedPort);

      const error = await client.call("managed-provider", "echo", { n: 2 }).catch((err: unknown) => err);
      expect(error).toBeInstanceOf(DaemonUnavailableError);
      expect(error).toMatchObject({
        kind: "not_sent",
        recovery: "user_action_required",
        code: "ECONNREFUSED",
        message: DAEMON_UNAVAILABLE_MESSAGE,
        cause: { code: "ECONNREFUSED" },
      });
      expect(sleeps).toEqual([5, 10, 10]);
    } finally {
      client.close();
    }
  });

  test("surfaces a non-transient reconnect refusal from the caller's fresh route.open", async () => {
    const { connFile } = tempConnectionFile();
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);
    const sleeps: number[] = [];

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      await client.call("managed-provider", "echo", { n: 1 });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({
        stats: secondStats,
        routeOpenError: { code: "route_rejected", message: "provider rejected route" },
      });
      writeConnectionFile(connFile, second.port);

      expect(await rejectionOf(client.call("managed-provider", "echo", { n: 2 }))).toMatchObject({
        kind: "terminal",
        code: "route_rejected",
      });
      // The caller owns the fresh route.open after reconnect, so a non-transient
      // refusal reaches that call directly without an eager background open.
      expect(secondStats.routeOpens).toBe(1);
      expect(sleeps).toEqual([]);
    } finally {
      client.close();
    }
  });

  test("retries a retryable route.open rejection in-place and recovers when the target becomes available", async () => {
    // A daemon-restart boot window: the target is supervised but not yet live, so
    // the first two route.open attempts are rejected target_unavailable, then it
    // comes up. The retry is IN-PLACE (same connection, no socket reconnect), so
    // the call recovers transparently instead of surfacing a misleading error.
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const sleeps: number[] = [];
    const daemon = await startFakeDaemon({
      stats,
      routeOpenFailFirst: { count: 2, code: "target_unavailable", message: "supervised but not available" },
    });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });

    try {
      const reply = await client.call("managed-provider", "echo", { n: 1 });
      expect(reply).toEqual({ method: "echo", params: { n: 1 } });
      // Three route.opens: two rejected + the one that succeeded — all on the SAME
      // connection (no reconnect), proven by the two backoff sleeps between them.
      expect(stats.routeOpens).toBe(3);
      expect(sleeps.length).toBe(2);
    } finally {
      client.close();
    }
  });

  test("module_removed fails fast while module_reloading retries at the same managed route.open call site", async () => {
    // Both branches run through client.call's cached-route opener. A removal is
    // intentional and permanent, while a reload can complete during the retry
    // window; their attempt counts prove the classifier does not conflate them.
    // The reloading arm must retry until the DEADLINE binds — an attempt cap
    // used to share the condition and strictly dominated it (capped backoff
    // sums to ~3.1s against a 30s deadline), so the advertised reload patience
    // was never delivered and every >3s module restart failed managed callers.
    // The `greaterThan(maxAttempts)` assertion reddens if that conjunction
    // ever returns.
    const reloadingConnection = tempConnectionFile();
    const reloadingStats = newStats();
    const reloadingDaemon = await startFakeDaemon({
      stats: reloadingStats,
      routeOpenError: { code: "module_reloading", message: "target is reloading" },
    });
    writeConnectionFile(reloadingConnection.connFile, reloadingDaemon.port);
    const reloadingClient = await SubcClient.connect({
      connectionFile: reloadingConnection.connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      routeOpenRetryDeadlineMs: 200,
      sleep: async () => {},
    });

    try {
      expect(await rejectionOf(reloadingClient.call("managed-provider", "echo", { n: 1 }))).toMatchObject({
        kind: "not_sent",
        code: "module_reloading",
      });
      expect(reloadingStats.routeOpens).toBeGreaterThan(BACKOFF.maxAttempts);
    } finally {
      reloadingClient.close();
    }

    const removedConnection = tempConnectionFile();
    const removedStats = newStats();
    const removedDaemon = await startFakeDaemon({
      stats: removedStats,
      routeOpenError: { code: "module_removed", message: "target was removed" },
    });
    writeConnectionFile(removedConnection.connFile, removedDaemon.port);
    const removedClient = await SubcClient.connect({
      connectionFile: removedConnection.connFile,
      identity: IDENTITY,
      reconnectBackoff: BACKOFF,
      sleep: async () => {},
    });

    try {
      expect(await rejectionOf(removedClient.call("managed-provider", "echo", { n: 1 }))).toMatchObject({
        kind: "terminal",
        code: "module_removed",
      });
      expect(removedStats.routeOpens).toBe(1);
    } finally {
      removedClient.close();
    }
  });

  test("timeout arbitration recovers a reply that raced the deadline (no lost success, no reconnect)", async () => {
    // The demux-drop bug: under event-loop starvation a reply already in the socket
    // buffer is dispatched only after the request-timeout timer fires, so the naive
    // path deletes the waiter and drops the arriving reply. We reproduce the shape
    // deterministically: the daemon writes the reply HEADER promptly (the client
    // parks mid-frame on the body read → readerActive) then the BODY after the
    // deadline. With arbitration, the raced reply wins and the call RESOLVES.
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "delay-body", delayBodyMs: 200 });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      // Large grace so the mid-arrival body is deterministically inside the window;
      // the body arrives ~200ms after a header that lands sub-ms, past the 80ms
      // deadline but far inside the 2s grace.
      timeoutArbitrationGraceMs: 2_000,
    });

    try {
      const reply = await client.call("managed-provider", "echo", { n: 7 }, { timeoutMs: 80 });
      expect(reply).toEqual({ method: "echo", params: { n: 7 } });
      // The reply won on the SAME connection — arbitration did not trigger a reconnect.
      expect(stats.connections).toBe(1);
    } finally {
      client.close();
    }
  });

  test("a route GOODBYE preserves the shared socket and a sibling route's pending reply", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    let signalHeld!: (reply: () => Promise<void>) => void;
    const heldReady = new Promise<() => Promise<void>>((resolve) => { signalHeld = resolve; });
    const daemon = await startFakeDaemon({ stats, onHeldData: signalHeld });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF });
    try {
      const sibling = client.call("sibling-provider", "hold", { n: 1 }, { timeoutMs: 2_000 });
      const siblingOutcome = sibling.then(() => "reply", () => "rejected");
      const replyHeld = await heldReady;
      expect(await rejectionOf(client.call("closing-provider", "close", {}, { timeoutMs: 2_000 }))).toMatchObject({
        kind: "outcome_unknown", code: "route_closed",
      });
      // Give the asynchronous reconnect a chance to replace the connection before
      // the sibling response is released; the route closure must not cause one.
      await new Promise((resolve) => setTimeout(resolve, 100));
      await replyHeld().catch(() => undefined);
      const settled = await Promise.race([
        siblingOutcome,
        new Promise<string>((resolve) => setTimeout(() => resolve("still pending"), 200)),
      ]);
      expect(settled).toBe("reply");
      expect(await sibling).toEqual({ method: "hold", params: { n: 1 } });
      expect(stats.closedConnections).toBe(0);
      expect(stats.connections).toBe(1);
    } finally {
      client.close();
    }
  });

  test("a genuine deadline on a healthy socket is outcome_unknown + deadline code and does NOT reconnect", async () => {
    // The target accepts the request and never replies while the socket stays
    // healthy. The call must settle as outcome_unknown with the deadline-not-drop
    // code, and must NOT tear down the healthy connection (no reconnect) — only an
    // actual connection drop should trigger a reconnect, not a slow reply.
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "silent-hold" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: { baseMs: 1, capMs: 1, maxAttempts: 1 },
      timeoutArbitrationGraceMs: 10,
      // Short enough that the post-deadline liveness probe RUNS inside this
      // test: the fake answers its Ping, so the probe must EXONERATE the
      // socket — the connections===1 assertion below fences the keep arm.
      livenessProbeWindowMs: 30,
    });

    try {
      expect(await rejectionOf(client.call("managed-provider", "mutate", { n: 1 }, { timeoutMs: 60 }))).toMatchObject({
        kind: "outcome_unknown",
        code: "deadline_exceeded_no_drop_observed",
      });
      // Give the probe time to complete and any (erroneous) teardown/reconnect
      // a chance to open a second connection.
      await new Promise((resolve) => setTimeout(resolve, 100));
      expect(stats.connections).toBe(1);
      // The exonerated socket must still CARRY calls: a probe that convicts
      // despite the answered Ping closes it, and this second call would then
      // reconnect — the connection count fences the keep arm only through a
      // call that exercises the kept socket.
      expect(await rejectionOf(client.call("managed-provider", "mutate", { n: 2 }, { timeoutMs: 60 }))).toMatchObject({
        code: "deadline_exceeded_no_drop_observed",
      });
      expect(stats.connections).toBe(1);
    } finally {
      client.close();
    }
  });

  test("an in-flight channel-0 request suspends conviction: silence is self-explained", async () => {
    // The daemon's connection loop is FIFO and some channel-0 handlers park it
    // inline for seconds (route.open's bind relay). A probe Ping sent behind
    // one sits unread — silence explained by OUR OWN control op, not by a dead
    // socket — so the probe must not convict while one is in flight. The fake
    // parks catalog.list forever and goes deaf on data (worst case: even the
    // Ping is unanswered); the gate must still withhold conviction, keeping
    // the ORIGINAL connection: both later deadline errors ride it (accepts
    // stays 1) rather than reconnecting onto a fresh one.
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "half-open", holdCatalogList: true });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: { baseMs: 1, capMs: 5, maxAttempts: 4 },
      timeoutArbitrationGraceMs: 10,
      livenessProbeWindowMs: 40,
    });

    try {
      // Parked forever server-side: this channel-0 pending is the explanation
      // the gate consults. Settled locally by client.close() in finally.
      const held = client.catalogList().catch(() => {});
      await new Promise((resolve) => setTimeout(resolve, 20));

      expect(await rejectionOf(client.call("managed-provider", "mutate", { n: 1 }, { timeoutMs: 60 }))).toMatchObject({
        code: "deadline_exceeded_no_drop_observed",
      });
      // Probe window elapses; WITHOUT the gate this convicts and the next call
      // would open a second connection.
      await new Promise((resolve) => setTimeout(resolve, 150));
      expect(await rejectionOf(client.call("managed-provider", "mutate", { n: 2 }, { timeoutMs: 60 }))).toMatchObject({
        code: "deadline_exceeded_no_drop_observed",
      });
      expect(stats.connections).toBe(1);
      void held;
    } finally {
      client.close();
    }
  });

  test("a half-open socket is convicted by the liveness probe and the next call reconnects", async () => {
    // The peer accepts one request then vanishes WITHOUT a FIN/RST (host
    // sleep/wake shape): the deadline settles as deadline-no-drop, which
    // deliberately keeps the socket — so without the probe this client would
    // pin every future call to the corpse forever (the 2026-08-22 session
    // outage). The probe's Ping goes unanswered, the socket is convicted and
    // closed, and the NEXT call must reconnect and succeed.
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, dataMode: "half-open" });
    writeConnectionFile(connFile, daemon.port);

    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      reconnectBackoff: { baseMs: 1, capMs: 5, maxAttempts: 4 },
      timeoutArbitrationGraceMs: 10,
      livenessProbeWindowMs: 40,
    });

    try {
      expect(await rejectionOf(client.call("managed-provider", "mutate", { n: 1 }, { timeoutMs: 60 }))).toMatchObject({
        kind: "outcome_unknown",
        code: "deadline_exceeded_no_drop_observed",
      });
      // Let the probe window elapse and the conviction land.
      await new Promise((resolve) => setTimeout(resolve, 150));
      // The corpse is closed: this call reconnects (second daemon connection,
      // fresh route) and completes against the now-normal serving arm.
      const reply = await client.call("managed-provider", "mutate", { n: 2 }, { timeoutMs: 1_000 });
      expect(reply).toMatchObject({ params: { n: 2 } });
      expect(stats.connections).toBe(2);
    } finally {
      client.close();
    }
  });
});

describe("managed route.open across a module restart", () => {
  // A clock that only moves when the client sleeps, so a minute of module
  // restart runs in milliseconds and the deadline arithmetic is exact.
  function fakeClock(): { now: () => number; sleep: (ms: number) => Promise<void>; sleeps: number[] } {
    let t = 0;
    const sleeps: number[] = [];
    return {
      now: () => t,
      sleep: async (ms) => {
        sleeps.push(ms);
        t += ms;
      },
      sleeps,
    };
  }

  // A stand-in for a module that restarts: every route.open is refused with
  // module_reloading until the fake clock reaches `backAtMs`.
  async function restartingDaemon(clock: { now: () => number }, backAtMs: number): Promise<string> {
    const { connFile } = tempConnectionFile();
    const daemon = await startFakeDaemon({
      stats: newStats(),
      onRouteOpen: (_moduleId, answer) => {
        void answer(clock.now() < backAtMs ? { code: "module_reloading", message: "module is reloading" } : undefined);
      },
    });
    writeConnectionFile(connFile, daemon.port);
    return connFile;
  }

  // mulberry32: a small seeded generator, so jittered delays are reproducible.
  function seededRandom(seed: number): () => number {
    let state = seed >>> 0;
    return () => {
      state = (state + 0x6d2b79f5) >>> 0;
      let x = state;
      x = Math.imul(x ^ (x >>> 15), x | 1);
      x ^= x + Math.imul(x ^ (x >>> 7), x | 61);
      return ((x ^ (x >>> 14)) >>> 0) / 4294967296;
    };
  }

  test("the default retry deadline outlasts a 60s restart and the call succeeds", async () => {
    const clock = fakeClock();
    const connFile = await restartingDaemon(clock, 60_000);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, now: clock.now, sleep: clock.sleep });
    try {
      expect(await client.call<unknown>("restarting", "echo", { n: 1 })).toEqual({ method: "echo", params: { n: 1 } });
      expect(clock.now()).toBeGreaterThanOrEqual(60_000);
    } finally {
      client.close();
    }
  });

  test("a call timeout shorter than the retry deadline ends the retries at the call timeout", async () => {
    const clock = fakeClock();
    const connFile = await restartingDaemon(clock, 60_000);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, now: clock.now, sleep: clock.sleep });
    try {
      expect(await rejectionOf(client.call("restarting", "echo", { n: 1 }, { timeoutMs: 10_000 }))).toMatchObject({
        kind: "not_sent",
        code: "module_reloading",
      });
      // Retry sleeps are cut to the time left, so the loop stops at 10s exactly.
      expect(clock.now()).toBeGreaterThanOrEqual(9_999);
      expect(clock.now()).toBeLessThanOrEqual(10_001);
    } finally {
      client.close();
    }
  });

  test("two routes refused together retry after different jittered delays", async () => {
    const { connFile } = tempConnectionFile();
    const refusedOnce = new Set<string | undefined>();
    const daemon = await startFakeDaemon({
      stats: newStats(),
      onRouteOpen: (moduleId, answer) => {
        if (refusedOnce.has(moduleId)) {
          void answer();
          return;
        }
        refusedOnce.add(moduleId);
        void answer({ code: "module_reloading", message: "module is reloading" });
      },
    });
    writeConnectionFile(connFile, daemon.port);
    const sleeps: number[] = [];
    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      random: seededRandom(42),
      sleep: async (ms) => {
        sleeps.push(ms);
      },
    });
    try {
      await Promise.all([client.call("alpha", "echo", {}), client.call("beta", "echo", {})]);
      expect(sleeps.length).toBe(2);
      // Equal jitter keeps half of the 100ms base delay and randomizes the rest.
      for (const ms of sleeps) {
        expect(ms).toBeGreaterThanOrEqual(50);
        expect(ms).toBeLessThan(100);
      }
      expect(sleeps[0]).not.toBe(sleeps[1]);
    } finally {
      client.close();
    }
  });

  test("twenty concurrent opens never have more than eight outstanding at the daemon", async () => {
    const { connFile } = tempConnectionFile();
    let outstanding = 0;
    let maxOutstanding = 0;
    const held: (() => Promise<void>)[] = [];
    let flushTimer: ReturnType<typeof setTimeout> | undefined;
    // Answer the held opens only once no new open has arrived for a while, so
    // every open the client can send at once is outstanding together first.
    const scheduleFlush = (): void => {
      if (flushTimer !== undefined) clearTimeout(flushTimer);
      flushTimer = setTimeout(() => {
        for (const answer of held.splice(0)) {
          outstanding -= 1;
          void answer();
        }
      }, 25);
    };
    const daemon = await startFakeDaemon({
      stats: newStats(),
      onRouteOpen: (_moduleId, answer) => {
        outstanding += 1;
        maxOutstanding = Math.max(maxOutstanding, outstanding);
        held.push(() => answer());
        scheduleFlush();
      },
    });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      const calls = Array.from({ length: 20 }, (_, i) => client.call(`module-${i}`, "echo", { i }));
      const replies = await Promise.all(calls);
      expect(replies).toHaveLength(20);
      expect(maxOutstanding).toBe(8);
    } finally {
      client.close();
    }
  });

  test("an exhausted deadline names the module_reloading refusal, not the later admission pressure", async () => {
    const clock = fakeClock();
    const { connFile } = tempConnectionFile();
    let opens = 0;
    const daemon = await startFakeDaemon({
      stats: newStats(),
      onRouteOpen: (_moduleId, answer) => {
        opens += 1;
        void answer(
          opens <= 2
            ? { code: "module_reloading", message: "module is reloading", detail: { reason: "restart" } }
            : {
                code: "target_unavailable",
                message: "connection already has 8 route.open binds in flight (limit 8); retry after one settles",
              },
        );
      },
    });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({
      connectionFile: connFile,
      identity: IDENTITY,
      now: clock.now,
      sleep: clock.sleep,
      routeOpenRetryDeadlineMs: 5_000,
    });
    try {
      const error = await client.call("restarting", "echo", {}).catch((err: unknown) => err);
      expect(opens).toBeGreaterThan(2);
      // The error's kind and code still come from the most recent refusal;
      // only the description changes.
      expect(error).toMatchObject({ kind: "not_sent", code: "target_unavailable" });
      expect((error as Error).message).toStartWith(
        "route.open failed for module restarting: module_reloading (reason restart): module is reloading",
      );
    } finally {
      client.close();
    }
  });
});

describe("scoped route opens", () => {
  const scope: RouteScope = { owner: { kind: "reserved", module_id: "session-owner" }, ref: "session-α", scopeEpoch: 7 };

  test("bind proof selector matches the Rust golden on direct and managed opens", async () => {
    const golden = JSON.parse(readFileSync(new URL("../../../crates/subc-protocol/tests/golden/scope_selector_with_bind_proof.json", import.meta.url), "utf8")) as {
      owner: RouteScope["owner"]; ref: string; scope_epoch: number; bind_proof: string;
    };
    const selector: RouteScope = { owner: golden.owner, ref: golden.ref, scopeEpoch: golden.scope_epoch, bindProof: golden.bind_proof };
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      const target = { kind: "management_surface", module_id: "proof-provider" } as const;
      await client.routeOpen(target, IDENTITY, { scope: selector, consumerIdentity: null });
      await client.call(target.module_id, "echo", {}, { scope: selector, consumerIdentity: null });
      expect(stats.routeOpenBodies).toHaveLength(2);
      for (const body of stats.routeOpenBodies) expect(JSON.parse(body.toString()).scope).toEqual(golden);
      await client.call(target.module_id, "echo", {}, { scope: { ...selector, bindProof: "different" }, consumerIdentity: null });
      await client.call(target.module_id, "echo", {}, { scope: { ...selector, bindProof: undefined }, consumerIdentity: null });
      expect(stats.routeOpens).toBe(4);
    } finally { client.close(); }
  });

  for (const code of ["scope_bind_proof_required", "scope_bind_proof_mismatch"]) {
    test(`${code} never retries or sends data`, async () => {
      const { connFile } = tempConnectionFile();
      const stats = newStats();
      const daemon = await startFakeDaemon({ stats, routeOpenError: { code, message: "scope proof refused" } });
      writeConnectionFile(connFile, daemon.port);
      const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
      try {
        expect(await rejectionOf(client.call("proof-provider", "echo", {}, { scope, consumerIdentity: null }))).toMatchObject({ code, kind: "terminal" });
        expect(stats.routeOpens).toBe(1);
        expect(stats.dataRequests).toBe(0);
      } finally { client.close(); }
    });
  }

  test("scope selector is sent byte-for-byte on direct and managed opens", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      const target = { kind: "management_surface", module_id: "scoped-provider" } as const;
      await client.routeOpen(target, IDENTITY, { scope, consumerIdentity: null });
      await client.call(target.module_id, "echo", {}, { scope, consumerIdentity: null });
      const expected = Buffer.from('{"op":"route.open","target":{"kind":"management_surface","module_id":"scoped-provider"},"identity":{"project_root":"/tmp/subc-client-test","harness":"bun","session":"managed"},"scope":{"owner":{"kind":"reserved","module_id":"session-owner"},"ref":"session-α","scope_epoch":7}}', "utf8");
      expect(stats.routeOpenBodies).toEqual([expected, expected]);
    } finally { client.close(); }
  });

  test("unscoped direct and managed opens omit the scope key", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      await client.routeOpen({ kind: "management_surface", module_id: "plain" }, IDENTITY, { consumerIdentity: null });
      await client.call("plain", "echo", {}, { consumerIdentity: null });
      expect(stats.routeOpenBodies).toHaveLength(2);
      for (const body of stats.routeOpenBodies) expect(Object.hasOwn(JSON.parse(body.toString()), "scope")).toBe(false);
    } finally { client.close(); }
  });

  test("cached routes isolate owner ref and epoch from each other and from unscoped", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      const selectors = [
        scope,
        { ...scope, owner: { kind: "reserved", module_id: "other-owner" } as const },
        { ...scope, ref: "other-session" },
        { ...scope, scopeEpoch: 8 },
        undefined,
      ];
      for (const selector of selectors) {
        // Overlapping calls for the same tuple must still share one pending open.
        await Promise.all([1, 2].map(() => client.call("pooled", "echo", {}, { scope: selector, consumerIdentity: null })));
      }
      expect(stats.routeOpens).toBe(5);
      const channels = stats.requestFrames.filter((frame) => frame.channel !== 0).map((frame) => frame.channel);
      expect(channels).toHaveLength(10);
      expect(new Set(channels).size).toBe(5);
      for (let i = 0; i < channels.length; i += 2) expect(channels[i]).toBe(channels[i + 1]);
      // A fresh object with the same selector must find its existing route.
      await client.call("pooled", "echo", {}, { scope: { ...scope, owner: { ...scope.owner } }, consumerIdentity: null });
      expect(stats.routeOpens).toBe(5);
    } finally { client.close(); }
  });

  test("invalid scope selectors and missing epochs are refused locally", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      const invalid: unknown[] = [
        { owner: scope.owner, ref: scope.ref },
        ...[-1, 0.5, NaN, Infinity, Number.MAX_SAFE_INTEGER + 1, null, "7"].map((scopeEpoch) => ({ ...scope, scopeEpoch })),
        { ...scope, owner: { kind: "reserved", module_id: "" } },
        { ...scope, owner: "" },
        { ...scope, owner: null },
        { ...scope, ref: "" },
        { ...scope, ref: 123 },
      ];
      for (const selector of invalid) {
        const code = (selector as { scopeEpoch?: unknown }).scopeEpoch === undefined ? "scope_epoch_required" : "invalid_scope";
        const opts = { scope: selector as RouteScope, consumerIdentity: null };
        expect(await rejectionOf(client.routeOpen({ kind: "management_surface", module_id: "invalid" }, IDENTITY, opts))).toMatchObject({ code });
        expect(await rejectionOf(client.call("invalid", "echo", {}, opts))).toMatchObject({ code });
        expect(await rejectionOf(client.callBinary("invalid", new Uint8Array(), opts))).toMatchObject({ code });
      }
      expect(stats.routeOpens).toBe(0);
      expect(stats.dataRequests).toBe(0);
      expect(stats.requestFrames).toHaveLength(0);
      for (const scopeEpoch of [0, Number.MAX_SAFE_INTEGER]) {
        await client.routeOpen({ kind: "management_surface", module_id: "valid" }, IDENTITY, { scope: { ...scope, scopeEpoch }, consumerIdentity: null });
      }
      expect(stats.routeOpens).toBe(2);
      expect(stats.routeOpenBodies.map((body) => JSON.parse(body.toString()).scope.scope_epoch)).toEqual([0, Number.MAX_SAFE_INTEGER]);
    } finally { client.close(); }
  });

  test("non-reserved owner principals reach the daemon unchanged for policy refusal", async () => {
    const { connFile } = tempConnectionFile();
    const stats = newStats();
    const daemon = await startFakeDaemon({ stats, routeOpenError: { code: "scope_not_live", message: "owner owns no scopes" } });
    writeConnectionFile(connFile, daemon.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY });
    try {
      for (const kind of ["direct", "unverified"] as const) {
        expect(await rejectionOf(client.call("policy", "echo", {}, { scope: { ...scope, owner: { kind } }, consumerIdentity: null }))).toMatchObject({ code: "scope_not_live", kind: "terminal" });
      }
      expect(stats.routeOpenBodies.map((body) => JSON.parse(body.toString()).scope.owner)).toEqual([{ kind: "direct" }, { kind: "unverified" }]);
    } finally { client.close(); }
  });

  test("cached scopes are snapshotted and reopened on a new daemon incarnation", async () => {
    const { connFile } = tempConnectionFile();
    const firstStats = newStats();
    const first = await startFakeDaemon({ stats: firstStats, closeAfterDataResponses: 1 });
    writeConnectionFile(connFile, first.port);
    const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF, sleep: async () => {} });
    const original: RouteScope = { ...scope, owner: { ...scope.owner } };
    const mutable: RouteScope = { ...original, owner: { ...original.owner } };
    try {
      await client.call("reconnect", "echo", {}, { scope: mutable, consumerIdentity: null });
      await waitFor(() => clientClosedErr(client) !== null, "client observes daemon close");
      await first.stop();
      mutable.ref = "changed-by-caller";
      mutable.scopeEpoch = 99;
      if (mutable.owner.kind === "reserved") mutable.owner.module_id = "changed-owner";
      const secondStats = newStats();
      const daemonId = Uint8Array.from(Array(16).fill(0x55));
      const second = await startFakeDaemon({ stats: secondStats, daemonId });
      writeConnectionFile(connFile, second.port, KEY, daemonId);
      await client.call("reconnect", "echo", {}, { scope: original, consumerIdentity: null });
      expect(client.conn.daemonId).toEqual(daemonId);
      expect(firstStats.routeOpens).toBe(1);
      expect(secondStats.routeOpens).toBe(1);
      expect(secondStats.dataRequests).toBe(1);
      // The fake deliberately reuses the channel: a new route.open, not its number,
      // is the evidence that the earlier incarnation's handle was not shared.
      expect(secondStats.routeOpenBodies).toEqual(firstStats.routeOpenBodies);
    } finally { client.close(); }
  });

  for (const code of ["scope_not_synced", "scope_changed"]) {
    test(`${code} retries the same scoped selector within the caller deadline`, async () => {
      const { connFile } = tempConnectionFile();
      const stats = newStats();
      const daemon = await startFakeDaemon({ stats, routeOpenFailFirst: { count: 2, code, message: "retry scope" } });
      writeConnectionFile(connFile, daemon.port);
      const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF });
      try {
        expect(await client.call<unknown>("retry", "echo", {}, { scope, consumerIdentity: null, timeoutMs: 500 })).toEqual({ method: "echo", params: {} });
        expect(stats.routeOpens).toBe(3);
        expect(stats.routeOpenBodies[1]).toEqual(stats.routeOpenBodies[0]);
        expect(stats.routeOpenBodies[2]).toEqual(stats.routeOpenBodies[0]);
      } finally { client.close(); }
    });

    test(`${code} stops scoped retries at the caller deadline`, async () => {
      const { connFile } = tempConnectionFile();
      const stats = newStats();
      const daemon = await startFakeDaemon({ stats, routeOpenError: { code, message: "retry scope" } });
      writeConnectionFile(connFile, daemon.port);
      const clock = { now: 0 };
      const client = await SubcClient.connect({ connectionFile: connFile, identity: IDENTITY, reconnectBackoff: BACKOFF,
        now: () => clock.now, random: () => 1, sleep: async (ms) => { clock.now += ms; } });
      try {
        expect(await rejectionOf(client.call("retry", "echo", {}, { scope, consumerIdentity: null, timeoutMs: 20 }))).toMatchObject({ code, kind: "not_sent" });
        expect(clock.now).toBe(20);
        expect(stats.routeOpens).toBeGreaterThan(1);
        expect(stats.dataRequests).toBe(0);
      } finally { client.close(); }
    });
  }
});

async function startFakeDaemon(options: FakeDaemonOptions): Promise<FakeDaemon> {
  const sockets = new Set<Socket>();
  const server = createServer((socket) => {
    options.stats.connections += 1;
    sockets.add(socket);
    socket.once("close", () => {
      sockets.delete(socket);
      options.stats.closedConnections += 1;
    });
    void handleFakeConnection(socket, options).catch(() => socket.destroy());
  });
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = (server.address() as AddressInfo).port;
  let stopped = false;
  const daemon: FakeDaemon = {
    port,
    stop: async () => {
      if (stopped) return;
      stopped = true;
      for (const socket of sockets) socket.destroy();
      await new Promise<void>((resolve) => server.close(() => resolve()));
    },
  };
  daemons.push(daemon);
  return daemon;
}

async function handleFakeConnection(socket: Socket, options: FakeDaemonOptions): Promise<void> {
  const connectionNumber = options.stats.connections;
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 5_000;
  await authenticateFakeServer(reader, socket, deadline, options.key ?? KEY, options.daemonId ?? DAEMON_ID);
  let routeChannel = 41;
  // half-open mode: once tripped, this connection reads forever and answers
  // NOTHING (Pings included) — TCP stays open, the peer is effectively gone.
  let deaf = false;
  let reverseRequestSent = false;
  // Set by "module-reloading-once": how many more route.opens to refuse with
  // module_reloading, as the daemon does while a module drains and restarts.
  let drainRouteOpenRefusals = 0;

  for (;;) {
    const frame = await readFrame(reader, deadline);
    if (deaf) continue;
    if (frame.header.ty === FrameType.Ping && frame.header.channel === 0) {
      // The real daemon always answers channel-0 Pings (subc-core control.rs);
      // the liveness probe's exonerate arm depends on it.
      await writeFrame(socket, pongFrame(frame), deadline);
      continue;
    }
    if (
      options.reverseRequest &&
      frame.header.corr === options.reverseRequest.corr &&
      (frame.header.ty === FrameType.Response || frame.header.ty === FrameType.Error)
    ) {
      options.stats.reverseReplies.push(frame);
      continue;
    }
    if (frame.header.ty !== FrameType.Request) continue;

    const requestFrame: { channel: number; controlOp?: string } = { channel: frame.header.channel };
    options.stats.requestFrames.push(requestFrame);
    if (frame.header.channel === 0) {
      const request = parseJson(frame.body) as { op?: string; target?: { module_id?: string } };
      requestFrame.controlOp = request.op;
      if (request.op === "catalog.list") {
        if (options.holdCatalogList) {
          continue; // parked forever: the pending stays in flight client-side
        }
        if (options.controlPushesBeforeCatalogList) {
          await writeFrame(
            socket,
            controlPushFrame(
              encodeJson({ op: "route.closing", module_id: "fake-aft", reason: "restart" }),
            ),
            deadline,
          );
          await writeFrame(
            socket,
            controlPushFrame(new TextEncoder().encode("not json {")),
            deadline,
          );
        }
        await writeFrame(
          socket,
          responseFrame(frame, { op: "catalog.list", modules: options.catalogModules ?? [] }),
          deadline,
        );
      } else if (request.op === "route.open") {
        options.stats.routeOpens += 1;
        options.stats.routeOpenBodies.push(Buffer.from(frame.body));
        options.stats.routeOpenConsumerIdentities.push(
          (request as { consumer_identity?: unknown }).consumer_identity,
        );
        const moduleId = request.target?.module_id;
        if (drainRouteOpenRefusals > 0) {
          // The module is still draining: a reopen is refused until it is back.
          drainRouteOpenRefusals -= 1;
          await writeFrame(
            socket,
            errorFrame(frame, { code: "module_reloading", message: `module_id '${moduleId}' is reloading` }),
            deadline,
          );
          continue;
        }
        if (options.onRouteOpen) {
          options.onRouteOpen(moduleId, (error) =>
            writeFrame(
              socket,
              error
                ? errorFrame(frame, error)
                : responseFrame(frame, { op: "route.open", route_channel: routeChannel++, route_epoch: 1 }),
              Date.now() + 5_000,
            ),
          );
          continue;
        }
        const dropModule = options.routeOpenDropModule;
        if (
          dropModule &&
          moduleId === dropModule.module_id &&
          connectionNumber >= (dropModule.dropFromConnection ?? 1)
        ) {
          socket.destroy();
          return;
        }

        const refusedModule = options.routeOpenRefuseModule;
        if (
          refusedModule &&
          moduleId === refusedModule.module_id &&
          connectionNumber >= (refusedModule.refuseFromConnection ?? 1)
        ) {
          await writeFrame(
            socket,
            errorFrame(frame, { code: refusedModule.code, message: refusedModule.message }),
            deadline,
          );
        } else {
          const failFirst = options.routeOpenFailFirst;
          if (failFirst && options.stats.routeOpens <= failFirst.count) {
            await writeFrame(
              socket,
              errorFrame(frame, { code: failFirst.code, message: failFirst.message }),
              deadline,
            );
          } else if (options.routeOpenError) {
            await writeFrame(socket, errorFrame(frame, options.routeOpenError), deadline);
          } else {
            await writeFrame(
              socket,
              responseFrame(frame, { op: "route.open", route_channel: routeChannel++, route_epoch: 1 }),
              deadline,
            );
          }
        }
      }
      continue;
    }

    options.stats.dataRequests += 1;
    options.stats.dataFlags.push(frame.header.flags);
    const body = parseJson(frame.body);
    options.stats.dataBodies.push(body);

    if (options.reverseRequest && !reverseRequestSent) {
      reverseRequestSent = true;
      await writeFrame(
        socket,
        buildFrame(
          FrameType.Request,
          buildFlags(false, Priority.Interactive, false),
          frame.header.channel,
          frame.header.epoch,
          options.reverseRequest.corr,
          options.reverseRequest.body,
        ),
        deadline,
      );
    }

    if (options.onHeldData && (body as { method?: string }).method === "hold") {
      options.onHeldData(() => writeFrame(socket, responseFrame(frame, body), Date.now() + 5_000));
      continue;
    }
    if (options.onHeldData && (body as { method?: string }).method === "close") {
      await writeFrame(socket, buildFrame(FrameType.Goodbye, buildFlags(false, Priority.Interactive, false), frame.header.channel, frame.header.epoch, 0n, new Uint8Array()), deadline);
      continue;
    }

    if (options.dataMode === "drop") {
      socket.destroy();
      return;
    }
    if (options.dataMode === "silent-hold") {
      // Accept the request, never reply, keep the socket healthy — a genuine
      // deadline with no drop. Loop back to keep reading (and stay alive).
      continue;
    }
    if (options.dataMode === "half-open" && options.stats.dataRequests === 1) {
      // First data request only: later connections serve normally (echo), so a
      // test can prove recovery-after-conviction end to end.
      deaf = true;
      continue;
    }
    if (options.dataMode === "delay-body") {
      // Write the reply HEADER now, then the BODY after a delay, so the client's
      // read loop is parked mid-frame (bytes buffered) when a short deadline fires.
      const reply = responseFrame(frame, body);
      const full = encodeFrame(reply);
      await writeAll(socket, full.subarray(0, HEADER_LEN), deadline);
      await new Promise((resolve) => setTimeout(resolve, options.delayBodyMs ?? 30));
      await writeAll(socket, full.subarray(HEADER_LEN), deadline);
    } else if (options.dataMode === "error") {
      await writeFrame(socket, errorFrame(frame, { code: "module_boom", message: "boom", detail: { reason: "certification_refused", lane: "embed" } }), deadline);
    } else if (
      options.dataMode === "unknown-channel-always" ||
      (options.dataMode === "unknown-channel-once" && options.stats.dataRequests === 1)
    ) {
      await writeFrame(
        socket,
        errorFrame(frame, { code: "unknown_channel", message: `unknown channel ${frame.header.channel}` }),
        deadline,
      );
    } else if (options.dataMode === "stale-epoch-once" && options.stats.dataRequests === 1) {
      await writeFrame(
        socket,
        errorFrame(frame, {
          code: "stale_route_epoch",
          message: `stale epoch ${frame.header.epoch} on channel ${frame.header.channel}`,
        }),
        deadline,
      );
    } else if (
      options.dataMode === "module-reloading-always" ||
      (options.dataMode === "module-reloading-once" && options.stats.dataRequests === 1)
    ) {
      // The daemon's drain refusal on an established route, answered before
      // forwarding. The module stays down for the next two reopen attempts.
      if (options.dataMode === "module-reloading-once") drainRouteOpenRefusals = 2;
      await writeFrame(
        socket,
        errorFrame(frame, {
          code: "module_reloading",
          message: `module endpoint for route channel ${frame.header.channel} is reloading`,
        }),
        deadline,
      );
    } else {
      await writeFrame(socket, responseFrame(frame, body), deadline);
    }

    if (options.closeAfterDataResponses !== undefined && options.stats.dataRequests >= options.closeAfterDataResponses) {
      socket.destroy();
      return;
    }
  }
}

function catalogEntry(module_id: string, provides: string[] | undefined): CatalogEntry {
  return {
    module_id,
    roles: [],
    control_ops: [],
    ...(provides === undefined
      ? {}
      : {
          capabilities: {
            provides,
            requires: [],
            must_never_reach: [],
          },
        }),
  };
}

function responseFrame(request: Frame, body: unknown): Frame {
  return buildFrame(FrameType.Response, buildFlags(false, Priority.Interactive, false), request.header.channel, request.header.epoch, request.header.corr, encodeJson(body));
}

function errorFrame(request: Frame, body: { code: string; message: string; detail?: unknown }): Frame {
  return buildFrame(FrameType.Error, buildFlags(false, Priority.Interactive, false), request.header.channel, request.header.epoch, request.header.corr, encodeJson(body));
}

function pongFrame(ping: Frame): Frame {
  return buildFrame(FrameType.Pong, buildFlags(false, Priority.Interactive, false), 0, 0, ping.header.corr, new Uint8Array());
}

function controlPushFrame(body: Uint8Array): Frame {
  // Channel 0, epoch 0, daemon-chosen corr 0 -- the exact shape
  // send_route_control_pushes emits.
  return buildFrame(FrameType.Push, buildFlags(false, Priority.Interactive, false), 0, 0, 0n, body);
}

async function authenticateFakeServer(
  reader: SocketReader,
  socket: Socket,
  deadline: number,
  key: Uint8Array = KEY,
  daemonId: Uint8Array = DAEMON_ID,
): Promise<void> {
  const hello = await readAuthMessage<{ client_nonce: number[]; role: string }>(reader, deadline);
  expect(hello.role).toBe("client");
  const clientNonce = Uint8Array.from(hello.client_nonce);
  const serverProof = computeProof(key, SERVER_PROOF_DOMAIN, clientNonce, SERVER_NONCE, daemonId);
  await writeAuthMessage(
    socket,
    {
      daemon_id: Array.from(daemonId),
      server_nonce: Array.from(SERVER_NONCE),
      daemon_ver: "fake-subc",
      server_proof: Array.from(serverProof),
    },
    deadline,
  );

  const auth = await readAuthMessage<{ client_auth: number[] }>(reader, deadline);
  const expected = computeProof(key, CLIENT_AUTH_DOMAIN, clientNonce, SERVER_NONCE, daemonId);
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

function parseJson(body: Uint8Array): unknown {
  return JSON.parse(Buffer.from(body).toString("utf8"));
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

function tempConnectionFile(): { dir: string; connFile: string } {
  const dir = mkdtempSync(join(tmpdir(), "subc-managed-client-"));
  tempDirs.push(dir);
  return { dir, connFile: join(dir, "subc-connection.json") };
}

function writeConnectionFile(path: string, port: number, key: Uint8Array = KEY, daemonId: Uint8Array = DAEMON_ID): void {
  writeFileSync(
    path,
    JSON.stringify({
      schema: 1,
      endpoints: [{ host: "127.0.0.1", port }],
      key: Array.from(key),
      daemon_id: Array.from(daemonId),
      pid: process.pid,
      daemon_ver: "fake-subc",
    }),
    { mode: 0o600 },
  );
  chmodSync(path, 0o600);
}

async function closedPort(): Promise<number> {
  const server = createServer();
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const port = (server.address() as AddressInfo).port;
  await new Promise<void>((resolve) => server.close(() => resolve()));
  return port;
}

async function waitFor(predicate: () => boolean, label: string): Promise<void> {
  const deadline = Date.now() + 2_000;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((resolve) => setTimeout(resolve, 5));
  }
  throw new Error(`timed out waiting for ${label}`);
}

function clientClosedErr(client: SubcClient): Error | null {
  return (client as unknown as { closedErr: Error | null }).closedErr;
}
