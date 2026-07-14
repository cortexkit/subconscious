import { describe, expect, test } from "bun:test";

import { buildFrame, encodeFrame, FrameType, SubcClient, type Frame } from "../src/index.js";

type ClientJsonInternals = {
  encode(value: unknown): Uint8Array;
  parseJson(frame: Frame): unknown;
};

function clientJsonInternals(): ClientJsonInternals {
  return Object.create(SubcClient.prototype) as ClientJsonInternals;
}

function parseResponse(body: Uint8Array): unknown {
  const frame = buildFrame(FrameType.Response, 0, 7, 1, 1n, body);
  return clientJsonInternals().parseJson(frame);
}

function legacyEncode(value: unknown): Uint8Array {
  return new Uint8Array(Buffer.from(JSON.stringify(value), "utf8"));
}

describe("SubcClient JSON request encoding", () => {
  test("matches canonical UTF-8 and the previous wire bytes", () => {
    const values = [
      { ok: true },
      { nested: { list: [1, "two", false], empty: null } },
      { unicode: "snowman ☃, rocket 🚀, café" },
      {},
    ];

    for (const [index, value] of values.entries()) {
      const body = clientJsonInternals().encode(value);
      const expectedBody = new TextEncoder().encode(JSON.stringify(value));
      expect(Buffer.isBuffer(body)).toBe(true);
      expect(body).toEqual(expectedBody);

      const frame = buildFrame(FrameType.Request, 0, 7, 1, BigInt(index + 1), body);
      const legacyFrame = buildFrame(FrameType.Request, 0, 7, 1, BigInt(index + 1), legacyEncode(value));
      expect(encodeFrame(frame)).toEqual(encodeFrame(legacyFrame));
    }
  });

  test("keeps pooled bodies independent when frames are encoded later", () => {
    const pending = Array.from({ length: 2_048 }, (_, index) => {
      const value = { index, marker: `body-${index}-☃` };
      return {
        value,
        frame: buildFrame(FrameType.Request, 0, 7, 1, BigInt(index + 1), clientJsonInternals().encode(value)),
      };
    });

    for (const { value, frame } of pending) {
      const legacyFrame = buildFrame(
        FrameType.Request,
        0,
        frame.header.channel,
        frame.header.epoch,
        frame.header.corr,
        legacyEncode(value),
      );
      expect(encodeFrame(frame)).toEqual(encodeFrame(legacyFrame));
    }
  });

  test("round-trips encoded JSON", () => {
    const expected = { nested: ["value", { unicode: "こんにちは" }], enabled: true };
    const encoded = clientJsonInternals().encode(expected);

    expect(JSON.parse(new TextDecoder().decode(encoded))).toEqual(expected);
  });
});

describe("SubcClient JSON response decoding", () => {
  test("decodes only the body view at a non-zero byte offset", () => {
    const expected = { answer: 42, nested: ["exact", "window"] };
    const json = new TextEncoder().encode(JSON.stringify(expected));
    const offset = 37;
    const trailingBytes = 29;
    const storage = new Uint8Array(offset + json.byteLength + trailingBytes);
    storage.fill(0xff);
    storage.set(json, offset);
    const body = new Uint8Array(storage.buffer, offset, json.byteLength);

    expect(parseResponse(body)).toEqual(expected);
  });

  test("decodes a freshly allocated body", () => {
    const expected = { status: "ok", count: 3 };
    const body = new TextEncoder().encode(JSON.stringify(expected));

    expect(parseResponse(body)).toEqual(expected);
  });
});
