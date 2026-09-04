import { describe, expect, test } from "bun:test";

import {
  AdmissionClass,
  buildFlags,
  buildFrame,
  decodeHeader,
  FrameType,
  hasBinary,
  HEADER_LEN,
  Priority,
  StaleRouteHandleError,
  SubcCallError,
  SubcClient,
  SubcError,
  SubcProvider,
  type Frame,
} from "../src/index.js";
import { createRouteHandle, newConnectionToken, type RouteHandle } from "../src/route-handle.js";

const encoder = new TextEncoder();
const decoder = new TextDecoder();

class MockSocket {
  readonly writes: Frame[] = [];
  closeCount = 0;
  queueWrites = true;
  failWrites = false;

  write(bytes: Uint8Array): Promise<void> {
    if (this.failWrites) return Promise.reject(new Error("write failed"));
    this.writes.push(decodeFrame(bytes));
    return Promise.resolve();
  }

  writeTracked(bytes: Uint8Array): { queued: boolean; completed: Promise<void> } {
    if (!this.queueWrites) return { queued: false, completed: Promise.reject(new Error("not queued")) };
    this.writes.push(decodeFrame(bytes));
    return { queued: true, completed: this.failWrites ? Promise.reject(new Error("write failed")) : Promise.resolve() };
  }

  close(): void {
    this.closeCount += 1;
  }
  bufferedBytes(): number {
    return 0;
  }
  localPort(): null {
    return null;
  }
}

function clientHarness(channel = 7, epoch = 1): {
  client: SubcClient;
  internals: any;
  socket: MockSocket;
  handle: RouteHandle;
} {
  const client = Object.create(SubcClient.prototype) as SubcClient;
  const internals = client as any;
  const socket = new MockSocket();
  const token = newConnectionToken();
  const handle = createRouteHandle(channel, epoch, token);
  Object.assign(internals, {
    sock: socket,
    currentConn: {},
    opts: {
      targetKind: "management_surface",
      reconnectBackoff: { baseMs: 1, capMs: 1, maxAttempts: 1 },
      sleep: async () => undefined,
      timeoutArbitrationGraceMs: 0,
    },
    nextCorr: 1n,
    pending: new Map(),
    lateResponses: new Map(),
    routes: new Map(),
    liveRoutes: new Map([[channel, handle]]),
    connectionToken: token,
    ingressEpochDropCount: 0,
    closedErr: null,
    closeStarted: false,
    reconnecting: null,
    generation: 1,
    readerActive: false,
  });
  return { client, internals, socket, handle };
}

function providerHarness(socket = new MockSocket()): { provider: SubcProvider; internals: any; socket: MockSocket } {
  const provider = Object.create(SubcProvider.prototype) as SubcProvider;
  const internals = provider as any;
  const token = newConnectionToken();
  Object.assign(internals, {
    sock: socket,
    generation: 1,
    connectionEpoch: 1,
    connectionToken: token,
    closeStarted: false,
    closedErr: null,
    inflight: new Map(),
    pending: new Map(),
    liveRoutes: new Map(),
    ingressEpochDropCount: 0,
    nextCorr: 1n,
    opts: {
      handler: async (_handle: RouteHandle, body: Uint8Array) => body,
      health: () => ({ status: "ok" }),
    },
  });
  return { provider, internals, socket };
}

function json(value: unknown): Uint8Array {
  return encoder.encode(JSON.stringify(value));
}

function decodeFrame(bytes: Uint8Array): Frame {
  const header = decodeHeader(bytes.subarray(0, HEADER_LEN));
  return { header, body: bytes.subarray(HEADER_LEN) };
}

function response(request: Frame, body: unknown): Frame {
  return buildFrame(
    FrameType.Response,
    buildFlags(false, Priority.Interactive, false),
    request.header.channel,
    request.header.epoch,
    request.header.corr,
    json(body),
  );
}

function bindFrame(channel: number, epoch: number, corr = 41n): Frame {
  return buildFrame(
    FrameType.Request,
    buildFlags(false, Priority.Interactive, false),
    0,
    0,
    corr,
    json({
      op: "route.bind",
      route_channel: channel,
      epoch,
      target: { kind: "management_surface", module_id: "test-provider" },
      identity: { project_root: "/tmp", harness: "test", session: "s1" },
    }),
  );
}

describe("RouteHandle fencing and endpoint validation", () => {
  test("stale handles emit no route frame after reconnect reuses the same pair", async () => {
    const { client, internals, socket, handle: stale } = clientHarness(9, 3);
    const token = newConnectionToken();
    const current = createRouteHandle(9, 3, token);
    internals.connectionToken = token;
    internals.liveRoutes = new Map([[9, current]]);

    await expect(client.request(stale, {})).rejects.toBeInstanceOf(StaleRouteHandleError);
    await expect(client.routePoll(stale, "status")).rejects.toBeInstanceOf(StaleRouteHandleError);
    expect(() => client.cancel(stale, 8n)).toThrow(StaleRouteHandleError);
    expect(() => client.subscribe(stale, {}, () => undefined)).toThrow(StaleRouteHandleError);
    await expect(client.closeRoute(stale)).rejects.toBeInstanceOf(StaleRouteHandleError);
    await expect(client.closeRouteChannel(stale)).rejects.toBeInstanceOf(StaleRouteHandleError);
    expect(socket.writes).toEqual([]);
  });

  test("unsubscribe settles locally, removes its waiter, and ignores late stream data", async () => {
    const { client, internals, socket, handle } = clientHarness(9, 3);
    const events: Uint8Array[] = [];
    const subscription = client.subscribe(handle, { method: "stream" }, (event) => events.push(event));
    const request = socket.writes[0]!;

    expect(internals.pending.size).toBe(1);
    subscription.unsubscribe();

    const outcome = await Promise.race([
      subscription.closed.then(() => "resolved" as const),
      new Promise<"timed_out">((resolve) => setTimeout(() => resolve("timed_out"), 25)),
    ]);
    expect(outcome).toBe("resolved");
    expect(internals.pending.size).toBe(0);
    expect(socket.writes).toHaveLength(2);
    expect(socket.writes[1]!.header).toMatchObject({
      ty: FrameType.Cancel,
      channel: handle.channel,
      epoch: handle.epoch,
      corr: request.header.corr,
    });

    internals.dispatch(
      buildFrame(
        FrameType.StreamData,
        buildFlags(false, Priority.Interactive, false),
        handle.channel,
        handle.epoch,
        request.header.corr,
        json({ late: true }),
      ),
    );
    expect(events).toEqual([]);
  });

  test("unsubscribe settles locally without throwing after its handle becomes stale", async () => {
    const { client, internals, socket, handle } = clientHarness(9, 3);
    const subscription = client.subscribe(handle, { method: "stream" }, () => undefined);
    const nextToken = newConnectionToken();
    const replacement = createRouteHandle(handle.channel, handle.epoch, nextToken);
    internals.connectionToken = nextToken;
    internals.liveRoutes = new Map([[replacement.channel, replacement]]);

    expect(() => subscription.unsubscribe()).not.toThrow();
    await expect(subscription.closed).resolves.toBeUndefined();
    expect(internals.pending.size).toBe(0);
    expect(socket.writes).toHaveLength(1);
    expect(socket.writes[0]!.header.ty).toBe(FrameType.Request);
  });

  test("a stale epoch carrying the current corr cannot settle the current request", async () => {
    const { client, internals, socket, handle } = clientHarness(7, 2);
    let settled = false;
    const pending = client.request(handle, { method: "current" }).then((value) => {
      settled = true;
      return value;
    });
    const outbound = socket.writes[0]!;
    internals.dispatch(buildFrame(
      FrameType.Response,
      buildFlags(false, Priority.Interactive, false),
      7,
      1,
      outbound.header.corr,
      json({ stale: true }),
    ));
    await Promise.resolve();
    expect(settled).toBe(false);
    expect(client.droppedIngressFrames).toBe(1);
    internals.dispatch(response(outbound, { current: true }));
    await expect(pending).resolves.toEqual({ current: true });
  });

  test("RoutePoll sends epoch and ignores a response echo for another generation", async () => {
    const { client, internals, socket, handle } = clientHarness(11, 4);
    let settled = false;
    const poll = client.routePoll(handle, "liveness").then((value) => {
      settled = true;
      return value;
    });
    const outbound = socket.writes[0]!;
    expect(JSON.parse(decoder.decode(outbound.body))).toEqual({
      op: "route.poll",
      route_channel: 11,
      route_epoch: 4,
      kind: "liveness",
    });
    internals.dispatch(response(outbound, {
      op: "route.poll", route_channel: 11, route_epoch: 3, status: null, live: false,
    }));
    await Promise.resolve();
    expect(settled).toBe(false);
    const expected = { op: "route.poll", route_channel: 11, route_epoch: 4, status: null, live: true };
    internals.dispatch(response(outbound, expected));
    await expect(poll).resolves.toEqual(expected);
  });

  test("EXPEDITE stamps egress and channel-0 corr emits u64::MAX once without wrap", async () => {
    const { client, internals, socket, handle } = clientHarness(5, 8);
    const call = client.request(handle, {}, { admissionClass: AdmissionClass.Expedite });
    expect((socket.writes[0]!.header.flags >> 4) & 0b11).toBe(AdmissionClass.Expedite);
    internals.dispatch(response(socket.writes[0]!, { ok: true }));
    await call;

    internals.nextCorr = 0xffff_ffff_ffff_ffffn;
    const poll = client.routePoll(handle, "status");
    const maxFrame = socket.writes[1]!;
    expect(maxFrame.header).toMatchObject({ channel: 0, corr: 0xffff_ffff_ffff_ffffn });
    internals.dispatch(response(maxFrame, {
      op: "route.poll", route_channel: 5, route_epoch: 8, status: null, live: null,
    }));
    await poll;
    internals.closeStarted = true;
    await expect(client.catalogList()).rejects.toMatchObject({ code: "corr_exhausted" });
    expect(socket.writes.filter((frame) => frame.header.ty === FrameType.Request && frame.header.corr === 0n)).toEqual([]);
    expect(socket.closeCount).toBe(1);
  });

  test("late route.open closes the committed handle or closes the connection when GOODBYE cannot queue", async () => {
    for (const queueWrites of [true, false]) {
      const { client, internals, socket } = clientHarness();
      socket.writes.length = 0;
      socket.queueWrites = queueWrites;
      let late!: (frame: Frame) => void;
      internals.controlRpc = async (_body: Uint8Array, _accept: unknown, onLate: (frame: Frame) => void) => {
        late = onLate;
        throw new SubcError("local timeout", "request_deadline");
      };
      await expect(client.routeOpen(
        { kind: "management_surface", module_id: "provider" },
        { project_root: "/tmp", harness: "test", session: "late" },
      )).rejects.toMatchObject({ code: "request_deadline" });
      if (!queueWrites) internals.closeStarted = true;
      late(buildFrame(FrameType.Response, 0, 0, 0, 1n, json({
        op: "route.open", route_channel: 23, route_epoch: 9,
      })));
      if (queueWrites) {
        expect(socket.writes[0]!.header).toMatchObject({ ty: FrameType.Goodbye, channel: 23, epoch: 9 });
      } else {
        expect(socket.writes).toEqual([]);
        expect(socket.closeCount).toBe(1);
      }
    }
  });
});

describe("binary request and reply bodies", () => {
  test("sets BINARY and preserves bytes while decoding a JSON reply from the wire flag", async () => {
    const { client, internals, socket, handle } = clientHarness();
    const body = new Uint8Array([0, 255, 1, 254]);
    const pending = client.request(handle, body, { binary: true });
    const outbound = socket.writes[0]!;

    expect(hasBinary(outbound.header.flags)).toBe(true);
    expect(outbound.body).toEqual(body);

    internals.dispatch(response(outbound, { accepted: true }));
    await expect(pending).resolves.toEqual({ accepted: true });
  });

  test("rejects a non-byte binary request before writing a frame and names its type", async () => {
    const { client, socket, handle } = clientHarness();

    await expect(client.request(handle, { dishonest: true }, { binary: true }))
      .rejects.toMatchObject({ code: "binary_body_required", message: expect.stringContaining("got object") });
    expect(socket.writes).toEqual([]);
  });

  test("returns a binary Response body without attempting JSON decoding", async () => {
    const { client, internals, socket, handle } = clientHarness();
    const pending = client.request(handle, { method: "download" });
    const outbound = socket.writes[0]!;
    const reply = new Uint8Array([137, 80, 78, 71, 0]);

    internals.dispatch(buildFrame(
      FrameType.Response,
      buildFlags(true, Priority.Interactive, false),
      outbound.header.channel,
      outbound.header.epoch,
      outbound.header.corr,
      reply,
    ));
    await expect(pending).resolves.toEqual(reply);
  });

  test("keeps call() JSON-only and rejects binary before route.open", async () => {
    const { client, internals, socket, handle } = clientHarness();
    let routeOpened = false;
    internals.cachedRouteHandle = () => {
      routeOpened = true;
      return handle;
    };

    await expect(client.call("provider", "download", { offset: 0 }, { binary: true }))
      .rejects.toMatchObject({
        code: "binary_call_requires_call_binary",
        message: expect.stringContaining("callBinary"),
      });
    expect(routeOpened).toBe(false);
    expect(socket.writes).toEqual([]);
  });

  test("callBinary uses the managed request path and returns a binary reply", async () => {
    const { client, internals, socket, handle } = clientHarness();
    internals.cachedRouteHandle = () => handle;
    const body = new Uint8Array([3, 1, 4, 1, 5]);
    const pending = client.callBinary("provider", body);
    await Promise.resolve();
    const outbound = socket.writes[0]!;
    const reply = new Uint8Array([8, 2, 8]);

    expect(outbound.header.ty).toBe(FrameType.Request);
    expect(hasBinary(outbound.header.flags)).toBe(true);
    expect(outbound.body).toEqual(body);

    internals.dispatch(buildFrame(
      FrameType.Response,
      buildFlags(true, Priority.Interactive, false),
      outbound.header.channel,
      outbound.header.epoch,
      outbound.header.corr,
      reply,
    ));
    await expect(pending).resolves.toEqual(reply);
  });

  test("keeps Error frames on the JSON error path and surfaces SubcCallError", async () => {
    const { client, internals, socket, handle } = clientHarness();
    internals.cachedRouteHandle = () => handle;
    const pending = client.call("provider", "download");
    await Promise.resolve();
    const outbound = socket.writes[0]!;

    const error = buildFrame(
      FrameType.Error,
      buildFlags(false, Priority.Interactive, false),
      outbound.header.channel,
      outbound.header.epoch,
      outbound.header.corr,
      json({ code: "bad_request", message: "module rejected request" }),
    );
    expect(hasBinary(error.header.flags)).toBe(false);
    internals.dispatch(error);
    await expect(pending).rejects.toBeInstanceOf(SubcCallError);
  });
});

describe("provider bind publication and unknown-slot precedence", () => {
  test("onBind is decision-only; ack and install precede onBound traffic", async () => {
    const { provider, internals, socket } = providerHarness();
    const events: string[] = [];
    internals.opts.onBind = async (request: { handle: RouteHandle }) => {
      events.push("bind");
      await expect(provider.push(request.handle, json({ illegal: true }))).rejects.toBeInstanceOf(StaleRouteHandleError);
      expect(socket.writes).toEqual([]);
      return true;
    };
    internals.opts.onBound = async (handle: RouteHandle) => {
      events.push("bound");
      expect(internals.liveRoutes.get(handle.channel)).toBe(handle);
      expect(socket.writes[0]!.header.ty).toBe(FrameType.Response);
      await provider.push(handle, json({ ready: true }), { admissionClass: AdmissionClass.Expedite });
    };

    await internals.handleControlRequest(bindFrame(17, 6), socket, 1);
    expect(events).toEqual(["bind", "bound"]);
    expect(socket.writes.map((frame) => frame.header.ty)).toEqual([FrameType.Response, FrameType.Push]);
    expect(socket.writes[1]!.header).toMatchObject({ channel: 17, epoch: 6 });
    expect((socket.writes[1]!.header.flags >> 4) & 0b11).toBe(AdmissionClass.Expedite);
  });

  test("rejected bind cleans tentative state without install or onBound", async () => {
    const { internals, socket } = providerHarness();
    const cleaned: RouteHandle[] = [];
    let bound = false;
    internals.opts.onBind = () => false;
    internals.opts.onBound = () => {
      bound = true;
    };
    internals.opts.onRouteGone = (handle: RouteHandle) => cleaned.push(handle);

    await internals.handleControlRequest(bindFrame(18, 7), socket, 1);
    expect(bound).toBe(false);
    expect(internals.liveRoutes.size).toBe(0);
    expect(cleaned[0]).toMatchObject({ channel: 18, epoch: 7 });
    expect(socket.writes[0]!.header.ty).toBe(FrameType.Error);
  });

  test("bind ack failure cleans tentative state and never invokes onBound", async () => {
    const socket = new MockSocket();
    socket.failWrites = true;
    const { internals } = providerHarness(socket);
    const cleaned: RouteHandle[] = [];
    let bound = false;
    internals.opts.onBind = () => true;
    internals.opts.onBound = () => {
      bound = true;
    };
    internals.opts.onRouteGone = (handle: RouteHandle) => cleaned.push(handle);

    await expect(internals.handleControlRequest(bindFrame(19, 8), socket, 1)).rejects.toThrow("write failed");
    expect(bound).toBe(false);
    expect(internals.liveRoutes.size).toBe(0);
    expect(cleaned).toHaveLength(1);
  });

  test("unknown Request, Cancel, and Goodbye drop without handler, callback, or Error", async () => {
    const { internals, socket } = providerHarness();
    let handlerCalls = 0;
    let routeGoneCalls = 0;
    internals.opts.handler = () => {
      handlerCalls += 1;
      return new Uint8Array(0);
    };
    internals.opts.onRouteGone = () => {
      routeGoneCalls += 1;
    };
    for (const ty of [FrameType.Request, FrameType.Cancel, FrameType.Goodbye]) {
      await internals.dispatch(buildFrame(
        ty,
        buildFlags(false, Priority.Interactive, false),
        44,
        2,
        ty === FrameType.Goodbye ? 0n : 3n,
        ty === FrameType.Request ? json({ method: "stale" }) : new Uint8Array(0),
      ), socket, 1);
    }
    expect(handlerCalls).toBe(0);
    expect(routeGoneCalls).toBe(0);
    expect(socket.writes).toEqual([]);
    expect(internals.ingressEpochDropCount).toBe(3);
  });

  test("onBound-captured work is fenced after reconnect reuses channel and epoch", async () => {
    const { provider, internals, socket } = providerHarness();
    let stale!: RouteHandle;
    internals.opts.onBind = () => true;
    internals.opts.onBound = (handle: RouteHandle) => {
      stale = handle;
    };
    await internals.handleControlRequest(bindFrame(20, 1), socket, 1);
    const writesBeforeReconnect = socket.writes.length;

    const token = newConnectionToken();
    const current = createRouteHandle(20, 1, token);
    internals.connectionToken = token;
    internals.liveRoutes = new Map([[20, current]]);
    await expect(provider.push(stale, json({ delayed: true }))).rejects.toBeInstanceOf(StaleRouteHandleError);
    await expect(provider.request(stale, json({ delayed: true }))).rejects.toBeInstanceOf(StaleRouteHandleError);
    expect(() => provider.cancel(stale, 5n)).toThrow(StaleRouteHandleError);
    expect(() => provider.closeRoute(stale)).toThrow(StaleRouteHandleError);
    expect(socket.writes).toHaveLength(writesBeforeReconnect);
  });
});
