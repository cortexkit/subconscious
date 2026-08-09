import { afterAll, beforeAll, describe, expect, test } from "bun:test";

import {
  managementSurfaceManifest,
  SubcClient,
  SubcProvider,
  type ProviderRequestContext,
} from "../src/index.js";
import { startLiveDaemon, type LiveDaemon } from "./live-daemon.js";

const decode = (b: Uint8Array): string => Buffer.from(b).toString("utf8");
const encode = (s: string): Uint8Array => new Uint8Array(Buffer.from(s, "utf8"));

async function waitFor(predicate: () => boolean, timeoutMs = 5_000): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (!predicate()) {
    if (Date.now() > deadline) throw new Error("timed out waiting for condition");
    await new Promise((r) => setTimeout(r, 10));
  }
}

// Live tests need a compiled subc-core daemon (Rust toolchain). They are
// OFF by default so the unit suite runs in a bun-only environment (npm
// release gate, bun-only CI job); set RUN_SUBC_LIVE=1 to run them.
const LIVE = process.env.RUN_SUBC_LIVE === "1";

describe.skipIf(!LIVE)("SubcProvider streaming subscription against real subc-core", () => {
  let live: LiveDaemon;

  beforeAll(async () => {
    live = await startLiveDaemon("subc-streaming-live");
  });

  afterAll(() => {
    live?.stop();
  });

  test("a held-open subscription streams events, then unsubscribe ends with StreamEnd", async () => {
    const moduleId = "stream-effect-provider";
    // The provider emits one event per "tick" the consumer's subscribe holds open,
    // and ends (StreamEnd) when the consumer cancels.
    let started = 0;
    let aborted = false;
    const provider = await SubcProvider.connect({
      connectionFile: live.connFile,
      manifest: managementSurfaceManifest({ moduleId, operations: ["events"] }),
      handler: async (_routeChannel, body, ctx: ProviderRequestContext) => {
        const req = JSON.parse(decode(body)) as { method: string; params?: { count?: number } };
        if (req.method !== "events") return encode(JSON.stringify({ error: "unknown" }));
        started++;
        const count = req.params?.count ?? 3;
        for (let i = 0; i < count; i++) {
          if (ctx.signal.aborted) break;
          await ctx.emit(encode(JSON.stringify({ event: "tick", seq: i })));
          await new Promise((r) => setTimeout(r, 5));
        }
        // Hold open until the consumer cancels (or the route goes away).
        await new Promise<void>((resolve) => {
          if (ctx.signal.aborted) return resolve();
          ctx.signal.addEventListener("abort", () => resolve(), { once: true });
        });
        aborted = ctx.signal.aborted;
        // Returning void ends the streaming handler, so the provider sends StreamEnd
        // and the consumer's subscription resolves.
        return;
      },
    });
    const client = await SubcClient.connect({ connectionFile: live.connFile });

    try {
      const routeChannel = await client.routeOpen(
        { kind: "management_surface", module_id: moduleId },
        { project_root: live.configDir, harness: "opencode", session: "session-stream" },
      );

      const events: Array<{ event: string; seq: number }> = [];
      const sub = client.subscribe(
        routeChannel,
        { method: "events", params: { count: 4 } },
        (ev) => events.push(JSON.parse(decode(ev))),
      );

      // All 4 streamed events arrive, in order, on the held-open request.
      await waitFor(() => events.length === 4);
      expect(events.map((e) => e.seq)).toEqual([0, 1, 2, 3]);
      expect(started).toBe(1);

      // Unsubscribe cancels the held-open request; the provider's handler aborts.
      //
      // `closed` CANNOT be the barrier for that. unsubscribe() settles the
      // subscription LOCALLY and then sends a best-effort cancel frame, so
      // `closed` resolves before the frame has reached the module -- awaiting it
      // and asserting on provider state immediately is a race the test loses on a
      // correct client. This assertion has never run in CI (the live suite was
      // ungated until today) and fails deterministically here, which is what
      // surfaced it.
      //
      // Wait for the remote effect itself. The local settle is a separate
      // property and is asserted by `await sub.closed` completing at all.
      sub.unsubscribe();
      await sub.closed;
      await waitFor(() => aborted, 5_000);
      expect(aborted).toBe(true);
    } finally {
      client.close();
      await provider.close();
    }
  });

  test("a throwing event handler does not take down the connection", async () => {
    // The handler runs inside the client's read loop. Before this was guarded, an
    // escaping throw unwound into that loop's catch, which treats any error as a
    // socket failure: it rejected every in-flight request on the connection --
    // sibling routes included -- and stopped reading, reporting the caller's own
    // error as the transport cause.
    //
    // The assertion is therefore about the SIBLING, not about the throwing
    // subscription: a request on a different route must still complete after the
    // handler has thrown. Asserting only that the stream survived would pass on a
    // client that had already torn down everything else.
    const moduleId = "stream-throwing-handler-provider";
    const provider = await SubcProvider.connect({
      connectionFile: live.connFile,
      manifest: managementSurfaceManifest({ moduleId, operations: ["events", "ping"] }),
      handler: async (_routeChannel, body, ctx: ProviderRequestContext) => {
        const req = JSON.parse(decode(body)) as { method: string };
        if (req.method === "ping") return encode(JSON.stringify({ pong: true }));
        await ctx.emit(encode(JSON.stringify({ event: "first" })));
        await new Promise<void>((resolve) => {
          ctx.signal.addEventListener("abort", () => resolve(), { once: true });
        });
        return;
      },
    });
    const client = await SubcClient.connect({ connectionFile: live.connFile });

    try {
      const identity = { project_root: live.configDir, harness: "opencode", session: "session-throwing" };
      const streamRoute = await client.routeOpen({ kind: "management_surface", module_id: moduleId }, identity);
      const siblingRoute = await client.routeOpen({ kind: "management_surface", module_id: moduleId }, identity);

      let handlerCalls = 0;
      const sub = client.subscribe(streamRoute, { method: "events" }, () => {
        handlerCalls++;
        throw new Error("handler blew up");
      });
      sub.closed.catch(() => {});

      // Control: the throw actually happened. Without this the test would pass
      // against a client that never delivered the event at all, which is the same
      // green for the opposite reason.
      await waitFor(() => handlerCalls === 1);

      // The load-bearing assertion: a request on a DIFFERENT route still completes.
      // `request` already parses the reply body, so this is the decoded object.
      const reply = await client.request(siblingRoute, { method: "ping" });
      expect(reply).toEqual({ pong: true });

      sub.unsubscribe();
    } finally {
      client.close();
      await provider.close();
    }
  });

  test("route teardown aborts an in-flight streaming handler", async () => {
    const moduleId = "stream-teardown-provider";
    let sawAbort = false;
    const provider = await SubcProvider.connect({
      connectionFile: live.connFile,
      manifest: managementSurfaceManifest({ moduleId, operations: ["events"] }),
      handler: async (_routeChannel, _body, ctx: ProviderRequestContext) => {
        await ctx.emit(encode(JSON.stringify({ event: "first" })));
        await new Promise<void>((resolve) => {
          ctx.signal.addEventListener("abort", () => resolve(), { once: true });
        });
        sawAbort = ctx.signal.aborted;
        return;
      },
    });
    const client = await SubcClient.connect({ connectionFile: live.connFile });

    try {
      const routeChannel = await client.routeOpen(
        { kind: "management_surface", module_id: moduleId },
        { project_root: live.configDir, harness: "opencode", session: "session-teardown" },
      );
      const events: unknown[] = [];
      const sub = client.subscribe(routeChannel, { method: "events" }, (ev) =>
        events.push(JSON.parse(decode(ev))),
      );
      // Closing the consumer rejects the in-flight subscription's `closed`; that is
      // expected here, so swallow it rather than leak an unhandled rejection.
      sub.closed.catch(() => {});
      await waitFor(() => events.length === 1);

      // Closing the consumer connection tears the route down; subc sends the module a
      // GOODBYE on that channel, which aborts the held-open handler.
      client.close();
      await waitFor(() => sawAbort, 5_000);
      expect(sawAbort).toBe(true);
    } finally {
      await provider.close();
    }
  });
});
