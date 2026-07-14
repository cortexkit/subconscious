import { describe, expect, test } from "bun:test";

import { buildFrame, FrameType, SubcClient, type Frame } from "../src/index.js";

type ClientJsonInternals = {
  parseJson(frame: Frame): unknown;
};

function parseResponse(body: Uint8Array): unknown {
  const client = Object.create(SubcClient.prototype) as SubcClient;
  const frame = buildFrame(FrameType.Response, 0, 7, 1, 1n, body);
  return (client as unknown as ClientJsonInternals).parseJson(frame);
}

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
