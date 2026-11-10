import { describe, expect, test } from "bun:test";
import * as sdk from "../src/index.js";
import { SubcClient, DEFAULT_REQUEST_TIMEOUT_MS, classifyRouteCloseReason, isRetryableRouteOpenCode, parseRouteCloseReason } from "../src/client.js";
import { SubcProvider, type RouteBindRequest } from "../src/provider.js";
import { buildFrame, decodeHeader, HEADER_LEN, SUBSCRIPTION_FLAG, FrameType, type Frame } from "../src/envelope.js";
import { newConnectionToken } from "../src/route-handle.js";
import { readFileSync } from "node:fs";

const json = (value: unknown): Uint8Array => new TextEncoder().encode(JSON.stringify(value));
const target = { kind: "management_surface" as const, module_id: "example" };
const identity = { project_root: "/project", harness: "test", session: "session" };
const caller = {
  incarnation: "daemon", spawn: 7, authority_epoch: 8, claim: "own", first_party: true,
  origin: "release", approved_declarations_digest: "approved",
  declaration: { future_provider_key: { enabled: true } }, declaration_digest: "entry",
} as const;
const attestedTarget = {
  incarnation: "daemon", kind: "spawn", spawn: 7, authority_epoch: 8,
  first_party: true, approved_declarations_digest: "approved",
} as const;

function harness() {
  const client = Object.create(SubcClient.prototype) as SubcClient;
  const internals = client as any;
  const requests: Frame[] = [];
  let onRequest = (frame: Frame): void => {
    const body = JSON.parse(new TextDecoder().decode(frame.body));
    reply(frame, body.op === "server.describe" ? { op: body.op, capabilities: [] } : {
      op: body.op, route_channel: requests.length + 1, route_epoch: 1,
    });
  };
  const socket = {
    write: async (bytes: Uint8Array) => { const frame = decode(bytes); requests.push(frame); onRequest(frame); },
    writeTracked(bytes: Uint8Array) { return { queued: true, completed: this.write(bytes) }; },
    close() {}, bufferedBytes: () => 0, localPort: () => null,
  };
  Object.assign(internals, {
    sock: socket, opts: { timeoutArbitrationGraceMs: 0 }, nextCorr: 1n,
    pending: new Map(), lateResponses: new Map(), routes: new Map(), liveRoutes: new Map(),
    routeModules: new Map(), routeEndReasons: new Map(), legacyChannelReasons: new Map(),
    connectionToken: newConnectionToken(), closedErr: null, generation: 1,
  });
  function reply(request: Frame, body: unknown, ty = FrameType.Response, flags = 0): void {
    internals.dispatch(buildFrame(ty, flags, 0, 0, request.header.corr, json(body)));
  }
  return { client, internals, requests, reply, setHandler(handler: typeof onRequest) { onRequest = handler; } };
}

function decode(bytes: Uint8Array): Frame {
  return { header: decodeHeader(bytes), body: bytes.subarray(HEADER_LEN) };
}

async function bind(value: unknown): Promise<RouteBindRequest> {
  const provider = Object.create(SubcProvider.prototype) as any;
  let captured!: RouteBindRequest;
  const socket = {};
  Object.assign(provider, {
    sock: socket, generation: 1, connectionToken: newConnectionToken(), liveRoutes: new Map(),
    opts: { onBind: (request: RouteBindRequest) => { captured = request; } },
    sendOn: async () => {}, reportRouteBound: async () => {},
  });
  await provider.handleControlRequest(buildFrame(FrameType.Request, 0, 0, 0, 1n, json({
    op: "route.bind", route_channel: 7, epoch: 1, target, identity, caller: value,
  })), socket, 1);
  return captured;
}

async function open(value: unknown) {
  const h = harness();
  h.setHandler((frame) => {
    const body = JSON.parse(new TextDecoder().decode(frame.body));
    h.reply(frame, body.op === "server.describe" ? { op: body.op, capabilities: [] } : {
      op: body.op, route_channel: 7, route_epoch: 1, target: value,
    });
  });
  return h.client.routeOpen(target, identity, { consumerIdentity: null });
}

describe("reader skew", () => {
  test("catalog fields decode with filtered declaration objects and legacy absence", async () => {
    const h = harness();
    const entry = {
      module_id: "example", roles: [], control_ops: [], incarnation: "daemon", spawn: Number.MAX_SAFE_INTEGER,
      first_party: true, origin: "future-origin", requested_declarations_digest: "requested",
      approved_declarations_digest: "approved", approved_declarations: { provider: { future: [1, 2] } },
      requested_declarations: { provider: { other: true } },
    };
    h.setHandler((frame) => h.reply(frame, { op: "catalog.list", modules: [entry, { module_id: "old", roles: [], control_ops: [] }] }));
    const [current, legacy] = await h.client.catalogList("example");
    expect(current).toEqual({ ...entry, origin: "other" });
    for (const key of ["incarnation", "spawn", "first_party", "origin", "approved_declarations_digest", "requested_declarations_digest", "approved_declarations", "requested_declarations"]) {
      expect((legacy as any)[key]).toBeUndefined();
    }
  });

  test("catalog refuses unsafe spawn", async () => {
    const h = harness();
    h.setHandler((frame) => h.reply(frame, { modules: [{ module_id: "example", spawn: Number.MAX_SAFE_INTEGER + 1 }] }));
    let failure: unknown;
    try { await h.client.catalogList(); } catch (error) { failure = error; }
    expect(failure).toBeInstanceOf(TypeError);
  });

  test("caller is carried into the provider bind with opaque declarations", async () => {
    expect((await bind(caller)).caller).toEqual(caller);
  });

  test("absent incomplete and unparseable callers are unmarked", async () => {
    for (const field of ["incarnation", "spawn", "authority_epoch", "claim", "first_party", "origin", "approved_declarations_digest"]) {
      const incomplete: Record<string, unknown> = { ...caller };
      delete incomplete[field];
      expect((await bind(incomplete)).caller).toBeUndefined();
      expect((await bind({ ...caller, [field]: {} })).caller).toBeUndefined();
    }
    for (const value of [undefined, null, [], "caller", { ...caller, declaration: [] }, { ...caller, declaration_digest: 2 }]) {
      expect((await bind(value)).caller).toBeUndefined();
    }
  });

  test("caller refuses unsafe spawn and authority epoch even if incomplete", async () => {
    for (const field of ["spawn", "authority_epoch"]) {
      const value: Record<string, unknown> = { ...caller, [field]: Number.MAX_SAFE_INTEGER + 1 };
      delete value.origin;
      let failure: unknown;
      try { await bind(value); } catch (error) { failure = error; }
      expect(failure).toBeInstanceOf(TypeError);
    }
    expect((await bind({ ...caller, spawn: Number.MAX_SAFE_INTEGER, authority_epoch: Number.MAX_SAFE_INTEGER })).caller?.spawn).toBe(Number.MAX_SAFE_INTEGER);
  });

  test("unknown caller claim cannot be own or marked", async () => {
    const decoded = (await bind({ ...caller, claim: "future-claim" })).caller!;
    expect(decoded.claim).not.toBe("own");
    expect(decoded.first_party).toBe(false);
    expect(decoded.declaration).toBeUndefined();
  });

  test("unknown caller origin reads as other", async () => {
    expect((await bind({ ...caller, origin: "future-origin" })).caller?.origin).toBe("other");
  });

  test("route open captures target attestation and treats missing or incomplete target as unmarked", async () => {
    expect((await open(attestedTarget)).target).toEqual(attestedTarget);
    expect((await open(undefined)).target).toBeUndefined();
    for (const field of Object.keys(attestedTarget)) {
      const incomplete: Record<string, unknown> = { ...attestedTarget };
      delete incomplete[field];
      expect((await open(incomplete)).target).toBeUndefined();
    }
  });

  test("route target refuses unsafe spawn and authority epoch even if incomplete", async () => {
    for (const field of ["spawn", "authority_epoch"]) {
      const value: Record<string, unknown> = { ...attestedTarget, [field]: Number.MAX_SAFE_INTEGER + 1 };
      delete value.incarnation;
      let failure: unknown;
      try { await open(value); } catch (error) { failure = error; }
      expect(failure).toBeInstanceOf(TypeError);
    }
  });

  test("unknown target kind is not spawn and cannot retain spawn authority", async () => {
    const decoded = (await open({ ...attestedTarget, kind: "future-kind" })).target!;
    expect(decoded.kind).not.toBe("spawn");
    expect(decoded.first_party).toBe(false);
    expect(decoded.spawn).toBeUndefined();
    expect(decoded.authority_epoch).toBeUndefined();
    expect(decoded.approved_declarations_digest).toBeUndefined();
  });

  test("authority changed is reopenable", () => {
    expect(parseRouteCloseReason("authority_changed")).toBe("authority_changed");
    expect(classifyRouteCloseReason("authority_changed")).toBe("may_reopen");
  });

  test("stale authority epoch refusal is retryable", () => {
    expect(isRetryableRouteOpenCode("stale_authority_epoch")).toBe(true);
  });

  test("close reason declaration covers every mapped golden reason", () => {
    const tables = JSON.parse(readFileSync(new URL("../../../crates/subc-protocol/tests/golden/decision_tables.json", import.meta.url), "utf8"));
    const known = Object.keys(tables.route_close_disposition).filter((reason) => parseRouteCloseReason(reason) !== "unknown");
    const declared: readonly string[] = sdk.ACCEPTS_CLOSE_REASONS;
    expect([...declared].sort()).toEqual(known.sort());
    expect(sdk.ACCEPTS_CLOSE_REASONS).toContain("authority_changed");
    for (const reason of sdk.ACCEPTS_CLOSE_REASONS) expect(parseRouteCloseReason(reason)).toBe(reason);
  });

  test("catalog watch uses channel zero subscription stream data and cancel", async () => {
    const h = harness();
    h.setHandler(() => {});
    const generations: number[] = [];
    const watch = h.client.catalogWatch((event) => { generations.push(event.generation); });
    const request = h.requests[0]!;
    expect(request.header.channel).toBe(0);
    expect(request.header.epoch).toBe(0);
    expect(request.header.flags & SUBSCRIPTION_FLAG).toBe(SUBSCRIPTION_FLAG);
    expect(JSON.parse(new TextDecoder().decode(request.body))).toEqual({ op: "catalog.watch" });
    h.reply(request, { op: "catalog.watch", generation: 1, future: true }, FrameType.StreamData);
    h.reply(request, { op: "catalog.watch", generation: 4 }, FrameType.StreamData);
    expect(generations).toEqual([1, 4]);
    expect(h.internals.pending.size).toBe(1);
    watch.unsubscribe();
    await watch.closed;
    expect(await watch.ended).toEqual({ kind: "cancelled" });
    expect(h.requests[1]!.header.ty).toBe(FrameType.Cancel);
    expect(h.requests[1]!.header.corr).toBe(request.header.corr);
    expect(h.requests[1]!.header.channel).toBe(0);
    expect(h.internals.pending.size).toBe(0);
  });

  test("catalog watch settles on stream end response and error without losing sibling calls", async () => {
    for (const terminal of [FrameType.StreamEnd, FrameType.Response, FrameType.Error]) {
      const h = harness();
      h.setHandler(() => {});
      const watch = h.client.catalogWatch(() => { throw new Error("observer failure"); });
      const request = h.requests[0]!;
      h.reply(request, { op: "catalog.watch", generation: 1 }, FrameType.StreamData);
      h.reply(request, { code: "watch_ended", message: "ended" }, terminal);
      let failure: unknown;
      try { await watch.closed; } catch (error) { failure = error; }
      expect((await watch.ended).kind).toBe(terminal === FrameType.Error ? "error" : terminal === FrameType.Response ? "response" : "stream_end");
      expect(!!failure).toBe(terminal === FrameType.Error);
      expect(h.internals.closedErr).toBeNull();
      h.setHandler((frame) => h.reply(frame, { modules: [] }));
      expect(await h.client.catalogList()).toEqual([]);
    }
  });

  test("an old catalog watch cannot cancel a reused correlation after reconnect", async () => {
    const h = harness();
    h.setHandler(() => {});
    const old = h.client.catalogWatch(() => {});
    const closed = old.closed.catch(() => {});
    h.internals.readLoop = async () => {};
    h.internals.replaceConnection({ sock: h.internals.sock, conn: {} });
    await closed;
    const current = h.client.catalogWatch(() => {});
    expect(h.requests[0]!.header.corr).toBe(h.requests[1]!.header.corr);
    old.unsubscribe();
    expect(h.requests).toHaveLength(2);
    current.unsubscribe();
    await current.closed;
    expect(h.requests[2]!.header.ty).toBe(FrameType.Cancel);
  });

  test("capability discovery is lazy shared and cached and declares every route open", async () => {
    const h = harness();
    expect(h.requests).toHaveLength(0);
    let describes = 0;
    h.setHandler((frame) => {
      const body = JSON.parse(new TextDecoder().decode(frame.body));
      if (body.op === "server.describe") describes++;
      h.reply(frame, body.op === "server.describe" ? { op: body.op, capabilities: ["route-close-reasons/v1"] } : {
        op: body.op, route_channel: h.requests.length, route_epoch: 1,
      });
    });
    await Promise.all([h.client.routeOpen(target, identity), h.client.routeOpen(target, identity)]);
    await h.client.routeOpen(target, identity);
    expect(describes).toBe(1);
    for (const request of h.requests.slice(1)) {
      expect(JSON.parse(new TextDecoder().decode(request.body)).accepts_close_reasons).toEqual(sdk.ACCEPTS_CLOSE_REASONS);
    }
    h.internals.readLoop = async () => {};
    h.internals.replaceConnection({ sock: h.internals.sock, conn: {} });
    await h.client.routeOpen(target, identity);
    expect(describes).toBe(2);
  });

  test("missing refused failed and timed out describe never cost a route and are cached", async () => {
    for (const mode of ["missing", "refused", "failed", "timeout"]) {
      const h = harness();
      let describes = 0;
      h.setHandler((frame) => {
        const body = JSON.parse(new TextDecoder().decode(frame.body));
        if (body.op === "server.describe") {
          describes++;
          if (mode === "timeout") {
            const pending = [...h.internals.pending.values()][0] as any;
            clearTimeout(pending.timer);
            h.internals.arbitrateTimeout(`0:0:${frame.header.corr}`, pending, 0, frame.header.corr, DEFAULT_REQUEST_TIMEOUT_MS);
          } else if (mode === "refused") h.reply(frame, { code: "unsupported_op", message: "old daemon" }, FrameType.Error);
          else if (mode === "failed") throw new Error("describe unavailable");
          else h.reply(frame, { op: body.op });
        } else {
          expect(body.accepts_close_reasons).toBeUndefined();
          h.reply(frame, { route_channel: h.requests.length, route_epoch: 1 });
        }
      });
      await h.client.routeOpen(target, identity);
      await h.client.routeOpen(target, identity);
      expect(describes).toBe(1);
    }
  });

  test("an unanswered capability probe costs only two seconds and preserves the route deadline", async () => {
    expect(sdk.CAPABILITY_PROBE_TIMEOUT).toBe(2_000);
    const h = harness();
    const budgets: number[] = [];
    const control = h.internals.controlRpc.bind(h.client);
    h.internals.controlRpc = (body: Uint8Array, accept: unknown, late: unknown, timeout: number) => {
      budgets.push(timeout);
      return control(body, accept, late, timeout);
    };
    h.setHandler((frame) => {
      const body = JSON.parse(new TextDecoder().decode(frame.body));
      if (body.op === "route.open") {
        expect(body.accepts_close_reasons).toBeUndefined();
        h.reply(frame, { route_channel: 7, route_epoch: 1 });
      }
    });
    const start = performance.now();
    await h.client.routeOpen(target, identity);
    const elapsed = performance.now() - start;
    // The timer spends exactly two seconds; allow only scheduling/dispatch overhead.
    expect(budgets[0]).toBe(2_000);
    expect(elapsed).toBeLessThan(2_100);
    expect(budgets[1]).toBeLessThanOrEqual(DEFAULT_REQUEST_TIMEOUT_MS - 2_000);
    console.log(`unanswered capability probe: ${elapsed.toFixed(1)}ms, route budget ${budgets[1]}ms`);
    await h.client.routeOpen(target, identity);
    expect(h.requests.filter((frame) => JSON.parse(new TextDecoder().decode(frame.body)).op === "server.describe")).toHaveLength(1);
  });

  test("capability probe uses the smaller remaining route deadline", async () => {
    const h = harness();
    const budgets: number[] = [];
    const control = h.internals.controlRpc.bind(h.client);
    h.internals.controlRpc = (body: Uint8Array, accept: unknown, late: unknown, timeout: number) => {
      budgets.push(timeout);
      return control(body, accept, late, timeout);
    };
    h.setHandler((frame) => {
      if (JSON.parse(new TextDecoder().decode(frame.body)).op === "route.open") h.reply(frame, { route_channel: 7, route_epoch: 1 });
    });
    await h.internals.sendRouteOpen(target, identity, {}, Date.now() + 25);
    expect(budgets[0]).toBeGreaterThan(0);
    expect(budgets[0]).toBeLessThanOrEqual(25);
    expect(budgets[1]).toBeLessThanOrEqual(budgets[0]!);
  });

  test("reconnect during discovery probes the new connection before opening", async () => {
    const h = harness();
    let reconnect = true;
    h.setHandler((frame) => {
      const body = JSON.parse(new TextDecoder().decode(frame.body));
      if (body.op === "server.describe" && reconnect) {
        reconnect = false;
        h.internals.readLoop = async () => {};
        h.internals.replaceConnection({ sock: h.internals.sock, conn: {} });
      } else if (body.op === "server.describe") h.reply(frame, { capabilities: ["route-close-reasons/v1"] });
      else {
        expect(body.accepts_close_reasons).toEqual(sdk.ACCEPTS_CLOSE_REASONS);
        h.reply(frame, { route_channel: 7, route_epoch: 1 });
      }
    });
    await h.client.routeOpen(target, identity);
    expect(h.requests.map((frame) => JSON.parse(new TextDecoder().decode(frame.body)).op)).toEqual([
      "server.describe", "server.describe", "route.open",
    ]);
  });
});
