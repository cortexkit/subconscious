import { afterEach, describe, expect, test } from "bun:test";
import { createServer, type AddressInfo, type Server, type Socket } from "node:net";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
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
  SERVER_PROOF_DOMAIN,
  SubcCallError,
  SubcClient,
  type BindIdentity,
  type CatalogEntry,
  type Frame,
} from "../src/index.js";

const KEY = Uint8Array.from(Array(32).fill(0x4c));
const DAEMON_ID = Uint8Array.from(Array(16).fill(0x7d));
const SERVER_NONCE = Uint8Array.from(Array.from({ length: 32 }, (_, i) => 0xa0 + i));
const IDENTITY: BindIdentity = { project_root: "/tmp/subc-client-test", harness: "bun", session: "managed" };
const BACKOFF = { baseMs: 5, capMs: 10, maxAttempts: 4 };

const tempDirs: string[] = [];
const daemons: FakeDaemon[] = [];

interface FakeStats {
  routeOpens: number;
  dataRequests: number;
  dataBodies: unknown[];
  requestFrames: { channel: number; controlOp?: string }[];
  // The consumer_identity sent on each route.open, in order (undefined when absent),
  // so a test can assert the principal survives a reconnect reopen.
  routeOpenConsumerIdentities: (unknown | undefined)[];
  // Count of accepted TCP connections — a test asserts a healthy-socket deadline
  // does NOT trigger a reconnect (count stays 1).
  connections: number;
}

interface FakeDaemonOptions {
  stats: FakeStats;
  // Daemon-side HMAC key — a daemon started with a different key models the
  // key rotation that happens on every real daemon restart.
  key?: Uint8Array;
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
  dataMode?: "echo" | "drop" | "error" | "delay-body" | "silent-hold" | "half-open" | "unknown-channel-once" | "unknown-channel-always" | "stale-epoch-once";
  delayBodyMs?: number;
  routeOpenError?: { code: string; message: string };
  // Reject the first N route.open requests with this code (a booting-target
  // simulation), then serve subsequent ones normally.
  routeOpenFailFirst?: { count: number; code: string; message: string };
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
    requestFrames: [],
    routeOpenConsumerIdentities: [],
    connections: 0,
  };
}

afterEach(async () => {
  for (const daemon of daemons.splice(0)) await daemon.stop();
  for (const dir of tempDirs.splice(0)) rmSync(dir, { recursive: true, force: true });
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
      await expect(client.catalogList()).resolves.toEqual([]);
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
      await expect(client.catalogList()).resolves.toEqual([]);
    } finally {
      client.close();
    }
  });
});

describe("SubcClient capability resolution", () => {
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
      await expect(client.resolveProviders("missing-provider/v1")).resolves.toEqual([]);
      await expect(client.resolveProvider("missing-provider/v1")).rejects.toMatchObject({
        code: "capability_unprovided",
      });
      await expect(client.resolveProvider("single-provider/v1")).resolves.toBe("single-provider");
      await expect(client.resolveProviders("many-provider/v1")).resolves.toEqual(["a-provider", "z-provider"]);
      await expect(client.resolveProvider("many-provider/v1")).rejects.toMatchObject({
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
      await expect(client.resolveProviders("fallback-only/v1")).resolves.toEqual([]);
      await expect(client.resolveProvider("fallback-only/v1")).rejects.toMatchObject({
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
      await expect(client.resolveProviders("Bad/v1")).rejects.toMatchObject({
        code: "invalid_capability_identifier",
      });
      expect(stats.requestFrames).toEqual([]);
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
      await expect(client.call("managed-provider", "echo", { n: 1 })).resolves.toEqual({
        method: "echo",
        params: { n: 1 },
      });
      await expect(client.call("managed-provider", "echo", { n: 2 })).resolves.toEqual({
        method: "echo",
        params: { n: 2 },
      });
      await expect(client.call("managed-provider", "echo", { n: 3 })).resolves.toEqual({
        method: "echo",
        params: { n: 3 },
      });

      expect(stats.routeOpens).toBe(1);
      expect(stats.dataRequests).toBe(3);
    } finally {
      client.close();
    }
  });

  test("auto-retries a provable not_sent request after reconnecting and re-opening the cached route", async () => {
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
      await expect(client.call("managed-provider", "echo", { n: 1 })).resolves.toEqual({
        method: "echo",
        params: { n: 1 },
      });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({ stats: secondStats });
      writeConnectionFile(connFile, second.port);

      await expect(client.call("managed-provider", "echo", { n: 2 })).resolves.toEqual({
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
      await expect(client.call("managed-provider", "echo", { n: 1 })).resolves.toEqual({
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

      await expect(client.call("managed-provider", "echo", { n: 2 })).resolves.toEqual({
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

  test("re-attaches the same consumer_identity when reopening a cached route after reconnect", async () => {
    // A route opened with a consumer_identity (principal attestation) must send the
    // SAME consumer_identity when it is reopened on a fresh connection after a
    // reconnect. Dropping it there would let the daemon re-stamp the route with a
    // weaker principal than it was originally bound under — a silent post-reconnect
    // trust downgrade. This drives the bulk reopenCachedRoutes path (not the lazy
    // per-call openCachedRoute path) by having a route already installed before the
    // connection drops.
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
      await expect(
        client.call("managed-provider", "echo", { n: 1 }, { consumerIdentity }),
      ).resolves.toEqual({ method: "echo", params: { n: 1 } });
      await waitFor(() => clientClosedErr(client) !== null, "client to observe first daemon close");
      await first.stop();

      const secondStats = newStats();
      const second = await startFakeDaemon({ stats: secondStats });
      writeConnectionFile(connFile, second.port);

      // Second call triggers reconnect + bulk reopen of the installed route.
      await expect(
        client.call("managed-provider", "echo", { n: 2 }, { consumerIdentity }),
      ).resolves.toEqual({ method: "echo", params: { n: 2 } });

      // The reopen on connection 2 must carry the identical consumer_identity.
      expect(firstStats.routeOpenConsumerIdentities).toEqual([consumerIdentity]);
      expect(secondStats.routeOpenConsumerIdentities).toEqual([consumerIdentity]);
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
      await expect(client.call("managed-provider", "mutate", { n: 1 })).rejects.toMatchObject({
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
      await expect(client.call("managed-provider", "explode", {})).rejects.toMatchObject({
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
      await expect(client.call("managed-provider", "echo", { n: 1 })).resolves.toEqual({
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
      await expect(client.call("managed-provider", "echo", { n: 1 })).resolves.toEqual({
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
      await expect(client.call("managed-provider", "echo", { n: 1 })).rejects.toMatchObject({
        kind: "terminal",
        code: "unknown_channel",
      });
      // Exactly two attempts: original + the single in-place retry.
      expect(stats.dataRequests).toBe(2);
      expect(stats.routeOpens).toBe(2);

      await expect(client.call("managed-provider", "echo", { n: 2 })).rejects.toMatchObject({
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

      await expect(client.call("managed-provider", "echo", { n: 2 })).rejects.toMatchObject({
        kind: "not_sent",
      });
      expect(sleeps).toEqual([5, 10, 10]);
    } finally {
      client.close();
    }
  });

  test("does not retry a non-transient route re-open error after reconnect", async () => {
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

      await expect(client.call("managed-provider", "echo", { n: 2 })).rejects.toMatchObject({
        kind: "terminal",
        code: "route_rejected",
      });
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
      await expect(reloadingClient.call("managed-provider", "echo", { n: 1 })).rejects.toMatchObject({
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
      await expect(removedClient.call("managed-provider", "echo", { n: 1 })).rejects.toMatchObject({
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
      await expect(client.call("managed-provider", "mutate", { n: 1 }, { timeoutMs: 60 })).rejects.toMatchObject({
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
      await expect(client.call("managed-provider", "mutate", { n: 2 }, { timeoutMs: 60 })).rejects.toMatchObject({
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

      await expect(client.call("managed-provider", "mutate", { n: 1 }, { timeoutMs: 60 })).rejects.toMatchObject({
        code: "deadline_exceeded_no_drop_observed",
      });
      // Probe window elapses; WITHOUT the gate this convicts and the next call
      // would open a second connection.
      await new Promise((resolve) => setTimeout(resolve, 150));
      await expect(client.call("managed-provider", "mutate", { n: 2 }, { timeoutMs: 60 })).rejects.toMatchObject({
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
      await expect(client.call("managed-provider", "mutate", { n: 1 }, { timeoutMs: 60 })).rejects.toMatchObject({
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

async function startFakeDaemon(options: FakeDaemonOptions): Promise<FakeDaemon> {
  const sockets = new Set<Socket>();
  const server = createServer((socket) => {
    options.stats.connections += 1;
    sockets.add(socket);
    socket.once("close", () => sockets.delete(socket));
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
  const reader = new SocketReader(socket);
  const deadline = Date.now() + 5_000;
  await authenticateFakeServer(reader, socket, deadline, options.key ?? KEY);
  let routeChannel = 41;
  // half-open mode: once tripped, this connection reads forever and answers
  // NOTHING (Pings included) — TCP stays open, the peer is effectively gone.
  let deaf = false;

  for (;;) {
    const frame = await readFrame(reader, deadline);
    if (deaf) continue;
    if (frame.header.ty === FrameType.Ping && frame.header.channel === 0) {
      // The real daemon always answers channel-0 Pings (subc-core control.rs);
      // the liveness probe's exonerate arm depends on it.
      await writeFrame(socket, pongFrame(frame), deadline);
      continue;
    }
    if (frame.header.ty !== FrameType.Request) continue;

    const requestFrame: { channel: number; controlOp?: string } = { channel: frame.header.channel };
    options.stats.requestFrames.push(requestFrame);
    if (frame.header.channel === 0) {
      const request = parseJson(frame.body) as { op?: string };
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
        options.stats.routeOpenConsumerIdentities.push(
          (request as { consumer_identity?: unknown }).consumer_identity,
        );
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
          await writeFrame(socket, responseFrame(frame, { op: "route.open", route_channel: routeChannel++, route_epoch: 1 }), deadline);
        }
      }
      continue;
    }

    options.stats.dataRequests += 1;
    const body = parseJson(frame.body);
    options.stats.dataBodies.push(body);

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
): Promise<void> {
  const hello = await readAuthMessage<{ client_nonce: number[]; role: string }>(reader, deadline);
  expect(hello.role).toBe("client");
  const clientNonce = Uint8Array.from(hello.client_nonce);
  const serverProof = computeProof(key, SERVER_PROOF_DOMAIN, clientNonce, SERVER_NONCE, DAEMON_ID);
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
  const expected = computeProof(key, CLIENT_AUTH_DOMAIN, clientNonce, SERVER_NONCE, DAEMON_ID);
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

function writeConnectionFile(path: string, port: number, key: Uint8Array = KEY): void {
  writeFileSync(
    path,
    JSON.stringify({
      schema: 1,
      endpoints: [{ host: "127.0.0.1", port }],
      key: Array.from(key),
      daemon_id: Array.from(DAEMON_ID),
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
