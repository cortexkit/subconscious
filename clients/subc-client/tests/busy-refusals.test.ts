import { describe, expect, spyOn, test } from "bun:test";
import { AdmissionClass, buildFrame, DAEMON_ORIGIN_FLAG, decodeHeader, FrameType, HEADER_LEN, Priority,
  SubcCallError, SubcClient, SubcError, type Frame } from "../src/index.js";
import { createRouteHandle, newConnectionToken } from "../src/route-handle.js";
import { RequestGate } from "../src/request-gate.js";
import { SocketClosedError, SocketTimeoutError, SocketWriteNotQueuedError, SocketWriteQueuedError } from "../src/socket.js";
import { rejectionOf } from "./settle.js";

const bytes = (value: unknown): Uint8Array => new TextEncoder().encode(JSON.stringify(value));
const tick = (): Promise<void> => new Promise((resolve) => setImmediate(resolve));

function harness(requestWindow?: number, watchWindow?: number) {
  const client = Object.create(SubcClient.prototype) as SubcClient;
  const internals = client as any;
  const token = newConnectionToken();
  const handle = createRouteHandle(7, 1, token);
  const writes: Frame[] = [];
  let time = 0;
  const socket = {
    queueWrites: true, failWrites: false,
    onWrite: undefined as ((frame: Frame) => void) | undefined,
    writeTracked(data: Uint8Array) {
      if (!this.queueWrites) return { queued: false, completed: Promise.reject(new Error("not queued")) };
      const frame = { header: decodeHeader(data.subarray(0, HEADER_LEN)), body: data.subarray(HEADER_LEN) };
      writes.push(frame);
      queueMicrotask(() => this.onWrite?.(frame));
      return { queued: true, completed: this.failWrites ? Promise.reject(new Error("completion failed")) : Promise.resolve() };
    },
    write(data: Uint8Array) { return this.writeTracked(data).completed; },
    bufferedBytes: () => 0, localPort: () => null, close: () => undefined,
  };
  Object.assign(internals, {
    sock: socket, currentConn: {}, connectionToken: token, nextCorr: 1n,
    pending: new Map(), lateResponses: new Map(), routes: new Map(), liveRoutes: new Map([[7, handle]]),
    routeModules: new Map(), routeEndReasons: new Map(), legacyChannelReasons: new Map(),
    closedErr: null, closeStarted: false, generation: 1, readerActive: false,
    opts: { now: () => time, random: () => 0.5, sleep: async (ms: number) => { time += ms; }, timeoutArbitrationGraceMs: 0 },
  });
  if (requestWindow !== undefined) internals.gates.set(handle, new RequestGate(requestWindow, watchWindow));
  const terminal = (request: Frame, ty = FrameType.Response, code = "handler_failed", flags = 0): void => {
    internals.dispatch(buildFrame(ty, flags, request.header.channel, request.header.epoch, ty === FrameType.Goodbye ? 0n : request.header.corr,
      ty === FrameType.Goodbye ? new Uint8Array() : bytes(ty === FrameType.Error ? { code, message: "full", detail: { reason: "no_credit" } } : { ok: true })));
  };
  const requests = (): Frame[] => writes.filter((frame) => frame.header.ty === FrameType.Request);
  return { client, internals, socket, handle, writes, requests, terminal, now: () => time };
}

describe("negotiated route busy admission", () => {
  test("a default request waiting behind a full window expires locally at 30s", async () => {
    const h = harness(1);
    const first = rejectionOf(h.client.request(h.handle, {}));
    await tick();
    const originalSetTimeout = globalThis.setTimeout;
    const timers: ReturnType<typeof setTimeout>[] = [];
    let expire: (() => void) | undefined;
    let delay: number | undefined;
    const clock = spyOn(Date, "now").mockReturnValue(0);
    const timer = spyOn(globalThis, "setTimeout").mockImplementation(((callback: () => void, ms?: number) => {
      delay = ms;
      expire = callback;
      const handle = originalSetTimeout(() => undefined, 60_000);
      timers.push(handle);
      return handle;
    }) as typeof setTimeout);
    try {
      const queued = rejectionOf(h.client.request(h.handle, {}));
      await tick();
      expect(delay).toBe(30_000);
      expect(expire).toBeDefined();
      clock.mockReturnValue(30_000);
      h.internals.opts.now = () => 30_000;
      expire!();
      const error = await queued as SubcCallError;
      expect(error).toBeInstanceOf(SubcCallError);
      expect(error.kind).toBe("not_sent");
      expect(h.requests()).toHaveLength(1);
      expect(h.internals.slots.size).toBe(1);
    } finally {
      timer.mockRestore();
      clock.mockRestore();
      for (const handle of timers) clearTimeout(handle);
      h.client.close();
      await first;
    }
  });

  test("an acquired slot with less than 1ms remaining is released without writing", async () => {
    for (const remaining of [0, 0.5]) {
      const h = harness(1);
      const gate = h.internals.gates.get(h.handle) as RequestGate;
      const acquire = gate.acquire.bind(gate);
      const grant = spyOn(gate, "acquire").mockImplementation(async (...args) => {
        const slot = await acquire(...args);
        h.internals.opts.now = () => 10 - remaining;
        return slot;
      });
      try {
        const error = await rejectionOf(h.client.request(h.handle, {}, { timeoutMs: 10 })) as SubcCallError;
        expect(error).toBeInstanceOf(SubcCallError);
        expect(error.kind).toBe("not_sent");
        expect(h.writes).toHaveLength(0);
        expect(h.internals.slots.size).toBe(0);
        const slot = await acquire(false, 100);
        slot.release();
      } finally {
        grant.mockRestore();
        h.client.close();
      }
    }
  });

  test("a serial watch enters the FIFO before an immediately following ordinary call", async () => {
    const gate = new RequestGate(1);
    const order: string[] = [];
    const watch = gate.acquire(true).then((slot) => { order.push("watch"); return slot; });
    const ordinary = gate.acquire(false).then((slot) => { order.push("ordinary"); return slot; });
    void watch.catch(() => undefined);
    void ordinary.catch(() => undefined);
    try {
      await tick();
      expect(order).toEqual(["watch"]);
      (await watch).release();
      (await ordinary).release();
      expect(order).toEqual(["watch", "ordinary"]);
    } finally {
      gate.close(new Error("test finished"));
    }
  });

  test("legacy and control write failures keep their original error classes", async () => {
    for (const queued of [false, true]) {
      for (const control of [false, true]) {
        const h = harness();
        const cause = queued ? new SocketClosedError("connection closed") : new SocketTimeoutError("write expired");
        const wrapper = queued ? new SocketWriteQueuedError("queued write failed", cause)
          : new SocketWriteNotQueuedError("write was not queued", cause);
        h.socket.writeTracked = () => ({ queued, completed: Promise.reject(wrapper) });
        try {
          const error = await rejectionOf(control ? h.internals.controlRpc(bytes({ op: "catalog.list" }))
            : h.client.request(h.handle, {}));
          expect(error).toBe(cause);
          expect(error).toBeInstanceOf(queued ? SocketClosedError : SocketTimeoutError);
        } finally {
          h.client.close();
        }
      }
    }
  });

  test("close during managed route.open discards its negotiated gate", async () => {
    const h = harness();
    h.internals.opts.reconnectBackoff = { baseMs: 25, capMs: 1000 };
    const cached = { moduleId: "example", target: { kind: "management_surface", module_id: "example" },
      identity: { project_root: "/project", harness: "test", session: "session" }, handle: null, closed: false };
    h.socket.onWrite = (frame) => {
      if (frame.header.ty !== FrameType.Request) return;
      cached.closed = true;
      h.internals.dispatch(buildFrame(FrameType.Response, 0, 0, 0, frame.header.corr,
        bytes({ route_channel: 8, route_epoch: 1, request_window: 1 })));
    };
    try {
      const error = await rejectionOf(h.internals.openCachedRoute(cached, 90_000)) as SubcCallError;
      expect(error.code).toBe("route_closed");
      expect(h.writes.filter((frame) => frame.header.ty === FrameType.Goodbye)).toHaveLength(1);
      expect(h.internals.gates.size).toBe(0);
      expect(h.internals.liveRoutes.has(8)).toBe(false);
    } finally {
      h.client.close();
    }
  });

  test("route.open opts in and installs negotiated windows before returning", async () => {
    const h = harness();
    h.socket.onWrite = (frame) => h.internals.dispatch(buildFrame(FrameType.Response, 0, 0, 0, frame.header.corr,
      bytes({ route_channel: 8, route_epoch: 2, request_window: 32, watch_window: 24 })));
    const route = await h.client.routeOpen({ kind: "management_surface", module_id: "example" },
      { project_root: "/project", harness: "test", session: "session" }, { consumerIdentity: null });
    expect(JSON.parse(new TextDecoder().decode(h.requests()[0]!.body)).busy_refusals).toBe(true);
    expect(h.internals.gates.has(route)).toBe(true);
  });

  test("forged module busy and unnegotiated daemon busy are never retried", async () => {
    for (const legacy of [false, true]) {
      const h = harness(legacy ? undefined : 1);
      const result = rejectionOf(h.client.request(h.handle, {}));
      await tick();
      h.terminal(h.requests()[0]!, FrameType.Error, "route_busy", legacy ? DAEMON_ORIGIN_FLAG : 0);
      expect(await result).toBeInstanceOf(SubcError);
      expect(h.requests()).toHaveLength(1);
    }
  });

  test("older daemon is not gated", async () => {
    const h = harness();
    const calls = Array.from({ length: 34 }, () => h.client.request(h.handle, {}));
    await tick();
    expect(h.requests()).toHaveLength(34);
    for (const request of h.requests()) h.terminal(request);
    await Promise.all(calls);
  });

  test("serial busy retry retains its slot and runs B before queued C", async () => {
    const h = harness(1);
    let wake!: () => void;
    h.internals.opts.sleep = () => new Promise<void>((resolve) => { wake = resolve; });
    const b = h.client.request(h.handle, { name: "B" });
    await tick();
    const c = h.client.request(h.handle, { name: "C" });
    h.terminal(h.requests()[0]!, FrameType.Error, "route_busy", DAEMON_ORIGIN_FLAG);
    await tick();
    expect(h.requests()).toHaveLength(1);
    wake();
    await tick();
    expect(JSON.parse(new TextDecoder().decode(h.requests()[1]!.body))).toEqual({ name: "B" });
    expect(h.requests()[1]!.header.corr).not.toBe(h.requests()[0]!.header.corr);
    h.terminal(h.requests()[1]!);
    await b;
    await tick();
    expect(JSON.parse(new TextDecoder().decode(h.requests()[2]!.body))).toEqual({ name: "C" });
    h.terminal(h.requests()[2]!);
    await c;
  });

  test("timeout tombstone holds the slot until a late terminal, exactly once", async () => {
    const h = harness(1);
    const first = rejectionOf(h.client.request(h.handle, {}, { timeoutMs: 5 }));
    await first;
    expect(h.internals.slots.size).toBe(1);
    const second = h.client.request(h.handle, {}, { timeoutMs: 1000 });
    await tick();
    expect(h.requests()).toHaveLength(1);
    expect(h.writes.some((frame) => frame.header.ty === FrameType.Cancel)).toBe(true);
    h.terminal(h.requests()[0]!);
    await tick();
    const third = h.client.request(h.handle, {});
    h.terminal(h.requests()[0]!);
    await tick();
    expect(h.requests()).toHaveLength(2);
    h.terminal(h.requests()[1]!, FrameType.StreamEnd);
    await second;
    await tick();
    expect(h.requests()).toHaveLength(3);
    h.terminal(h.requests()[2]!);
    await third;
    expect(h.internals.slots.size).toBe(0);
  });

  test("RESPONSE ERROR STREAM_END and route close release exactly once", async () => {
    for (const type of [FrameType.Response, FrameType.Error, FrameType.StreamEnd, FrameType.Goodbye]) {
      const h = harness(1);
      const first = h.client.request(h.handle, {}).catch(() => undefined);
      await tick();
      const second = h.client.request(h.handle, {}).catch(() => undefined);
      await tick();
      h.terminal(h.requests()[0]!, type);
      await tick();
      if (type !== FrameType.Goodbye) {
        expect(h.requests()).toHaveLength(2);
        h.terminal(h.requests()[0]!, type);
        h.terminal(h.requests()[1]!);
      } else expect(h.internals.slots.size).toBe(0);
      await Promise.all([first, second]);
    }
  });

  test("proven unsent rolls back but queued completion failure keeps a tombstone", async () => {
    for (const queued of [false, true]) {
      const h = harness(1);
      h.socket.queueWrites = queued;
      h.socket.failWrites = true;
      await rejectionOf(h.client.request(h.handle, {}));
      h.socket.queueWrites = true;
      h.socket.failWrites = false;
      const second = h.client.request(h.handle, {});
      await tick();
      if (queued) {
        expect(h.requests()).toHaveLength(1);
        h.terminal(h.requests()[0]!);
        await tick();
      }
      expect(h.requests()).toHaveLength(queued ? 2 : 1);
      h.terminal(h.requests().at(-1)!);
      await second;
    }
  });

  test("queued watch holds no total slot; 24 watches leave room for ordinary calls", async () => {
    const h = harness(32, 24);
    const watches = Array.from({ length: 25 }, () => h.client.subscribe(h.handle, {}, () => undefined));
    for (const watch of watches) void watch.closed.catch(() => undefined);
    await tick();
    expect(h.requests()).toHaveLength(24);
    const ordinary = h.client.request(h.handle, {});
    await tick();
    expect(h.requests()).toHaveLength(25);
    expect(h.requests()[24]!.header.flags & 0x80).toBe(0);
    h.terminal(h.requests()[24]!);
    await ordinary;
    watches[0]!.unsubscribe();
    await tick();
    expect(h.requests()).toHaveLength(25);
    h.terminal(h.requests()[0]!, FrameType.StreamEnd);
    await tick();
    expect(h.requests()).toHaveLength(26);
    h.client.close();
    await Promise.all(watches.map((watch) => watch.ended));
  });

  test("local cancel retains its written slot until terminal", async () => {
    const h = harness(1);
    const abort = new AbortController();
    const first = h.client.request(h.handle, {}, { signal: abort.signal });
    await tick();
    abort.abort();
    const second = h.client.request(h.handle, {});
    await tick();
    expect(h.requests()).toHaveLength(1);
    h.terminal(h.requests()[0]!);
    await first;
    await tick();
    h.terminal(h.requests()[1]!);
    await second;
  });

  test("queued cancellation and slot-wait deadline never write a request", async () => {
    const h = harness(1);
    const first = h.client.request(h.handle, {});
    await tick();
    const abort = new AbortController();
    const cancelled = rejectionOf(h.client.request(h.handle, {}, { signal: abort.signal }));
    abort.abort();
    expect((await cancelled as SubcCallError).kind).toBe("not_sent");
    expect((await rejectionOf(h.client.request(h.handle, {}, { timeoutMs: 5 })) as SubcCallError).kind).toBe("not_sent");
    expect(h.requests()).toHaveLength(1);
    h.terminal(h.requests()[0]!);
    await first;
  });

  test("unsubscribe during subscription backoff ends retry without CANCEL", async () => {
    const h = harness(1, 1);
    h.internals.opts.sleep = () => new Promise<void>(() => undefined);
    const subscription = h.client.subscribe(h.handle, {}, () => undefined);
    await tick();
    h.terminal(h.requests()[0]!, FrameType.Error, "route_busy", DAEMON_ORIGIN_FLAG);
    await tick();
    subscription.unsubscribe();
    await subscription.closed;
    await tick();
    expect(h.writes).toHaveLength(1);
    const call = h.client.request(h.handle, {});
    await tick();
    expect(h.requests()).toHaveLength(2);
    h.terminal(h.requests()[1]!);
    await call;
  });

  test("route replacement during backoff ends the old subscription", async () => {
    const h = harness(1, 1);
    h.internals.opts.sleep = () => new Promise<void>(() => undefined);
    const subscription = h.client.subscribe(h.handle, {}, () => undefined);
    const failure = rejectionOf(subscription.closed);
    await tick();
    h.terminal(h.requests()[0]!, FrameType.Error, "route_busy", DAEMON_ORIGIN_FLAG);
    await tick();
    h.internals.installRoute(7, 2);
    await failure;
    expect((await subscription.ended).kind).toBe("route_closed");
    expect(h.requests()).toHaveLength(1);
  });

  test("call without timeout stops busy retries at 30s as never forwarded", async () => {
    const h = harness(1);
    h.socket.onWrite = (request) => h.terminal(request, FrameType.Error, "route_busy", DAEMON_ORIGIN_FLAG);
    const error = await rejectionOf(h.client.request(h.handle, {}));
    expect(error).toBeInstanceOf(SubcCallError);
    expect((error as SubcCallError).kind).toBe("not_sent");
    expect((error as SubcCallError).code).toBe("route_busy");
    expect(h.now()).toBe(30_000);
    expect(h.requests().length).toBeLessThan(100);
  });

  test("cancellation during busy backoff releases without a CANCEL or resend", async () => {
    const h = harness(1);
    h.internals.opts.sleep = () => new Promise<void>(() => undefined);
    const abort = new AbortController();
    const result = rejectionOf(h.client.request(h.handle, {}, { signal: abort.signal }));
    await tick();
    h.terminal(h.requests()[0]!, FrameType.Error, "route_busy", DAEMON_ORIGIN_FLAG);
    await tick();
    abort.abort();
    const error = await result as SubcCallError;
    expect(error.kind).toBe("not_sent");
    expect(error.code).toBe("route_busy");
    expect(h.writes).toHaveLength(1);
    expect(h.internals.slots.size).toBe(0);
  });

  test("subscription retry delivers events to the original subscription", async () => {
    const h = harness(1, 1);
    const events: Uint8Array[] = [];
    const subscription = h.client.subscribe(h.handle, {}, (body) => events.push(body));
    await tick();
    h.terminal(h.requests()[0]!, FrameType.Error, "route_busy", DAEMON_ORIGIN_FLAG);
    await tick();
    expect(h.requests()).toHaveLength(2);
    h.terminal(h.requests()[1]!, FrameType.StreamData);
    h.terminal(h.requests()[1]!, FrameType.StreamEnd);
    await subscription.closed;
    expect(events).toHaveLength(1);
  });

  test("a forwarded managed retry lost on the connection remains outcome unknown", async () => {
    const h = harness(1);
    const result = rejectionOf(h.internals.sendManaged(h.handle, bytes({}), Priority.Interactive, AdmissionClass.Normal, undefined, undefined));
    await tick();
    h.terminal(h.requests()[0]!, FrameType.Error, "route_busy", DAEMON_ORIGIN_FLAG);
    await tick();
    h.internals.fail(new Error("connection lost"));
    expect((await result as SubcCallError).kind).toBe("outcome_unknown");
    expect(h.requests()).toHaveLength(2);
  });
});
