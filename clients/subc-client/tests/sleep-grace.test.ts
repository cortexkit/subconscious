import { describe, expect, spyOn, test } from "bun:test";
import { buildFrame, decodeHeader, FrameType, HEADER_LEN, SubcCallError, SubcClient, SubcError,
  type ConnectOptions, type Frame, type RequestOptions } from "../src/index.js";
import { createRouteHandle } from "../src/route-handle.js";
import { RequestGate } from "../src/request-gate.js";
import { rejectionOf } from "./settle.js";

const bytes = (value: unknown): Uint8Array => new TextEncoder().encode(JSON.stringify(value));
const tick = (): Promise<void> => new Promise((resolve) => setImmediate(resolve));
const TIMEOUT_MS = 100;

async function harness(options: Partial<ConnectOptions> = {}, negotiated = true) {
  let wallMs = 10_000;
  let awakeMs = 100;
  const wall = spyOn(Date, "now").mockImplementation(() => wallMs);
  const awake = spyOn(performance, "now").mockImplementation(() => awakeMs);
  const writes: Frame[] = [];
  const socket = {
    writeTracked(data: Uint8Array) {
      writes.push({ header: decodeHeader(data.subarray(0, HEADER_LEN)), body: data.subarray(HEADER_LEN) });
      return { queued: true, completed: Promise.resolve() };
    },
    write(data: Uint8Array) { return this.writeTracked(data).completed; },
    readFrame: () => new Promise<Frame>(() => {}),
    bufferedBytes: () => 0,
    localPort: () => null,
    close: () => undefined,
  };
  const open = spyOn(SubcClient as any, "openConnection").mockResolvedValue({ sock: socket, conn: {} });
  let client: SubcClient;
  try {
    client = await SubcClient.connect({ connectionFile: "unused", timeoutArbitrationGraceMs: 0, ...options });
  } catch (error) {
    wall.mockRestore();
    awake.mockRestore();
    throw error;
  } finally {
    open.mockRestore();
  }
  const internals = client as any;
  const handle = createRouteHandle(7, 1, internals.connectionToken);
  internals.liveRoutes.set(handle.channel, handle);
  if (negotiated) internals.gates.set(handle, new RequestGate(1));

  // Keep timer execution independent of the two clocks, as it is at wake on Bun.
  const realSetTimeout = globalThis.setTimeout;
  const timers: { callback: () => void; ms: number | undefined; handle: ReturnType<typeof setTimeout> }[] = [];
  const handles: ReturnType<typeof setTimeout>[] = [];
  const timer = spyOn(globalThis, "setTimeout").mockImplementation(((callback: () => void, ms?: number) => {
    const handle = realSetTimeout(() => {}, 60_000);
    timers.push({ callback, ms, handle });
    handles.push(handle);
    return handle;
  }) as typeof setTimeout);

  return {
    client, internals, handle, socket, timers,
    writes: (ty: FrameType) => writes.filter((frame) => frame.header.ty === ty),
    advance(awakeElapsed: number, sleptMs = 0) { wallMs += awakeElapsed + sleptMs; awakeMs += awakeElapsed; },
    async fire() {
      const next = timers.shift();
      expect(next).toBeDefined();
      clearTimeout(next!.handle);
      next!.callback();
      await tick();
    },
    reply(ty = FrameType.Response, body: unknown = { ok: true }) {
      const request = writes.find((frame) => frame.header.ty === FrameType.Request)!;
      internals.dispatch(buildFrame(ty, 0, handle.channel, handle.epoch, request.header.corr, bytes(body)));
    },
    async start(managed: boolean, options: RequestOptions = {}) {
      const promise = managed
        ? internals.managedRequest(handle, {}, { timeoutMs: TIMEOUT_MS, ...options }) as Promise<unknown>
        : client.request(handle, {}, { timeoutMs: TIMEOUT_MS, ...options });
      let settled = false;
      const result = promise.then(
        (value) => { settled = true; return { value }; },
        (error: unknown) => { settled = true; return { error }; },
      );
      await tick();
      return { result, settled: () => settled };
    },
    close() {
      client.close();
      timer.mockRestore();
      wall.mockRestore();
      awake.mockRestore();
      for (const handle of handles) clearTimeout(handle);
    },
  };
}

describe("request sleep grace", () => {
  test("a reply inside the sleep grace resolves the original call", async () => {
    const h = await harness();
    try {
      const progress: unknown[] = [];
      const call = await h.start(false, { onProgress: (body) => progress.push(JSON.parse(new TextDecoder().decode(body))) });
      const [key, pending] = [...h.internals.pending.entries()][0];
      const slot = [...h.internals.slots.values()][0];
      h.advance(TIMEOUT_MS, 60_000);
      await h.fire();
      expect(call.settled()).toBe(false);
      expect(h.timers.map((timer) => timer.ms)).toEqual([15_000]);
      expect(h.internals.pending.get(key)).toBe(pending);
      expect([...h.internals.slots.values()][0]).toBe(slot);
      h.reply(FrameType.Push, { working: true });
      h.reply();
      expect(await call.result).toEqual({ value: { ok: true } });
      expect(progress).toEqual([{ working: true }]);
      expect(h.writes(FrameType.Request)).toHaveLength(1);
      expect(h.writes(FrameType.Cancel)).toHaveLength(0);
      expect(h.internals.slots.size).toBe(0);
    } finally {
      h.close();
    }
  });

  for (const managed of [false, true]) {
    const mode = managed ? "managed" : "direct";

    test(`${mode}: no reply times out after grace with exactly one CANCEL`, async () => {
      const h = await harness();
      try {
        const call = await h.start(managed);
        h.advance(TIMEOUT_MS, 60_000);
        await h.fire();
        expect(call.settled()).toBe(false);
        expect(h.writes(FrameType.Cancel)).toHaveLength(0);
        expect(h.timers.map((timer) => timer.ms)).toEqual([15_000]);
        h.advance(15_000);
        await h.fire();
        const result = await call.result;
        expect(result).toHaveProperty("error");
        if (!("error" in result)) throw new Error("expected timeout");
        const error = result.error as SubcError | SubcCallError;
        if (managed) {
          expect(error).toBeInstanceOf(SubcCallError);
          expect(error).toMatchObject({ kind: "outcome_unknown", code: "deadline_exceeded_no_drop_observed" });
          expect(error.cause).toMatchObject({ code: "request_deadline", message: "request on channel 7 corr 1 timed out after 100ms" });
        } else {
          expect(error).toBeInstanceOf(SubcError);
          expect(error).toMatchObject({ code: "request_deadline", message: "request on channel 7 corr 1 timed out after 100ms" });
        }
        expect(h.writes(FrameType.Cancel)).toHaveLength(1);
        expect(h.writes(FrameType.Cancel)[0]!.header.corr).toBe(h.writes(FrameType.Request)[0]!.header.corr);
        expect(h.internals.pending.size).toBe(0);
        expect(h.internals.slots.size).toBe(1);
        h.reply();
        expect(h.internals.slots.size).toBe(0);
        expect(h.writes(FrameType.Cancel)).toHaveLength(1);
        expect(h.timers).toHaveLength(0);
      } finally {
        h.close();
      }
    });

    for (const [name, awakeElapsed, sleptMs] of [
      ["without sleep", TIMEOUT_MS, 0],
      ["with sub-threshold clock jitter", TIMEOUT_MS, 1_000],
      ["at the sleep threshold", TIMEOUT_MS, 2_000],
      ["when both clocks count sleep", 60_000 + TIMEOUT_MS, 0],
    ] as const) {
      test(`${mode}: ${name} the deadline has no sleep grace`, async () => {
        const h = await harness();
        try {
          const call = await h.start(managed);
          h.advance(awakeElapsed, sleptMs);
          await h.fire();
          expect(await call.result).toHaveProperty("error.code", managed ? "deadline_exceeded_no_drop_observed" : "request_deadline");
          expect(h.timers).toHaveLength(0);
          expect(h.writes(FrameType.Cancel)).toHaveLength(1);
        } finally {
          h.close();
        }
      });
    }

    test(`${mode}: two sleeps grant grace only once`, async () => {
      const h = await harness({ sleepGraceMs: 500 });
      try {
        const call = await h.start(managed);
        h.advance(TIMEOUT_MS, 60_000);
        await h.fire();
        expect(call.settled()).toBe(false);
        expect(h.timers.map((timer) => timer.ms)).toEqual([500]);
        h.advance(500, 60_000);
        await h.fire();
        expect(await call.result).toHaveProperty("error");
        expect(h.timers).toHaveLength(0);
        expect(h.writes(FrameType.Cancel)).toHaveLength(1);
      } finally {
        h.close();
      }
    });

    for (const perRequest of [false, true]) {
      test(`${mode}: ${perRequest ? "request" : "client"} sleepGraceMs 0 disables grace`, async () => {
        const h = await harness(perRequest ? {} : { sleepGraceMs: 0 });
        try {
          const call = await h.start(managed, perRequest ? { sleepGraceMs: 0 } : {});
          h.advance(TIMEOUT_MS, 60_000);
          await h.fire();
          expect(await call.result).toHaveProperty("error");
          expect(h.timers).toHaveLength(0);
          expect(h.writes(FrameType.Cancel)).toHaveLength(1);
        } finally {
          h.close();
        }
      });
    }

    test(`${mode}: a request can override a disabled client grace`, async () => {
      const h = await harness({ sleepGraceMs: 0 });
      try {
        const call = await h.start(managed, { sleepGraceMs: 250 });
        h.advance(TIMEOUT_MS, 60_000);
        await h.fire();
        expect(call.settled()).toBe(false);
        expect(h.timers.map((timer) => timer.ms)).toEqual([250]);
        h.reply();
        expect(await call.result).toEqual({ value: { ok: true } });
      } finally {
        h.close();
      }
    });

    for (const afterSleepGrace of [false, true]) {
      test(`${mode}: reader arbitration still wins at the ${afterSleepGrace ? "grace end" : "first deadline"}`, async () => {
        const h = await harness({ timeoutArbitrationGraceMs: 50 });
        try {
          const call = await h.start(managed);
          if (afterSleepGrace) {
            h.advance(TIMEOUT_MS, 60_000);
            await h.fire();
            expect(h.timers.map((timer) => timer.ms)).toEqual([15_000]);
            h.advance(15_000);
          } else {
            h.advance(TIMEOUT_MS);
          }
          h.internals.readerActive = true;
          await h.fire();
          expect(call.settled()).toBe(false);
          h.reply();
          h.internals.readerActive = false;
          await tick();
          expect(await call.result).toEqual({ value: { ok: true } });
          expect(h.writes(FrameType.Cancel)).toHaveLength(0);
        } finally {
          h.close();
        }
      });
    }
  }

  test("legacy routes also keep the original request through sleep grace", async () => {
    const h = await harness({}, false);
    try {
      const call = await h.start(false);
      h.advance(TIMEOUT_MS, 60_000);
      await h.fire();
      expect(call.settled()).toBe(false);
      h.reply();
      expect(await call.result).toEqual({ value: { ok: true } });
      await h.fire();
      expect(h.writes(FrameType.Cancel)).toHaveLength(0);
      expect(h.timers).toHaveLength(0);
    } finally {
      h.close();
    }
  });

  test("control requests preserve late-response cleanup after grace expires", async () => {
    const h = await harness();
    try {
      const late: Frame[] = [];
      const error = rejectionOf(h.internals.controlRpc(bytes({ op: "route.open" }), undefined, (frame: Frame) => late.push(frame)));
      h.advance(30_000, 60_000);
      await h.fire();
      expect(h.timers.map((timer) => timer.ms)).toEqual([15_000]);
      h.advance(15_000);
      await h.fire();
      expect(await error).toMatchObject({ code: "request_deadline" });
      const request = h.writes(FrameType.Request)[0]!;
      h.internals.dispatch(buildFrame(FrameType.Response, 0, 0, 0, request.header.corr, bytes({ route_channel: 8, route_epoch: 1 })));
      expect(late).toHaveLength(1);
      expect(h.internals.lateResponses.size).toBe(0);
    } finally {
      h.close();
    }
  });

  for (const negotiated of [false, true]) {
    test(`${negotiated ? "negotiated" : "legacy"} subscriptions have no deadline timer`, async () => {
      const h = await harness({}, negotiated);
      try {
        const subscription = h.client.subscribe(h.handle, {}, () => {});
        await tick();
        h.advance(60_000, 60_000);
        expect(h.timers).toHaveLength(0);
        subscription.unsubscribe();
        await subscription.closed;
      } finally {
        h.close();
      }
    });
  }

  for (const sleepGraceMs of [-1, NaN, Infinity, 2_147_483_648, "100" as unknown as number]) {
    test(`invalid sleepGraceMs ${sleepGraceMs} is refused before connecting or sending`, async () => {
      expect(await rejectionOf(SubcClient.connect({ connectionFile: "unused", sleepGraceMs }))).toBeInstanceOf(RangeError);
      const h = await harness();
      try {
        for (const managed of [false, true]) {
          const call = await h.start(managed, { sleepGraceMs });
          expect(await call.result).toHaveProperty("error");
        }
        expect(h.writes(FrameType.Request)).toHaveLength(0);
        expect(h.timers).toHaveLength(0);
      } finally {
        h.close();
      }
    });
  }
});
