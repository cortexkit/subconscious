import { describe, expect, test } from "bun:test";

import {
  buildFlags,
  buildFrame,
  decodeHeader,
  DecodeError,
  encodeFrame,
  encodeHeader,
  FrameType,
  HEADER_LEN,
  PROTOCOL_VERSION,
  Priority,
  type EnvelopeHeader,
} from "../src/envelope.js";

function hdr(
  len: number,
  ty: FrameType,
  flags: number,
  channel: number,
  corr: bigint,
): EnvelopeHeader {
  return { len, ver: PROTOCOL_VERSION, ty, flags, channel, corr };
}

describe("envelope header", () => {
  test("little-endian frozen-prefix layout matches the Rust", () => {
    // len=1 occupies byte 0; ver sits at byte 4 (the frozen prefix).
    const buf = encodeHeader(hdr(1, FrameType.Request, 0, 0, 0n));
    expect(buf[0]).toBe(1);
    expect([buf[1], buf[2], buf[3]]).toEqual([0, 0, 0]);
    expect(buf[4]).toBe(PROTOCOL_VERSION);
    expect(buf.length).toBe(HEADER_LEN);
  });

  test("round-trips a request header with all fields set", () => {
    const h = hdr(
      1234,
      FrameType.Request,
      buildFlags(false, Priority.Interactive, false),
      42,
      0xdead_beef_0000_0001n,
    );
    expect(decodeHeader(encodeHeader(h))).toEqual(h);
  });

  test("round-trips every frame type", () => {
    for (let b = 0; b <= 11; b++) {
      const h = hdr(0, b as FrameType, buildFlags(false, Priority.Passive, false), 0, 0n);
      expect(decodeHeader(encodeHeader(h)).ty).toBe(b as FrameType);
    }
  });

  test("flags pack binary/priority/last like Flags::new", () => {
    const f = buildFlags(true, Priority.Background, true);
    // bit0 binary | (2<<1) priority | bit3 last = 1 | 4 | 8 = 13
    expect(f).toBe(0b0000_1101);
    const decoded = decodeHeader(encodeHeader(hdr(8, FrameType.StreamData, f, 1, 1n)));
    expect(decoded.flags).toBe(f);
  });

  test("rejects too-short prefix", () => {
    expect(() => decodeHeader(new Uint8Array([0, 0, 0, 0]))).toThrow(DecodeError);
  });

  test("rejects unsupported version", () => {
    const b = new Uint8Array(HEADER_LEN);
    b[4] = 2;
    expect(() => decodeHeader(b)).toThrow(/unsupported envelope version 2/);
  });

  test("rejects unknown frame type", () => {
    const b = new Uint8Array(HEADER_LEN);
    b[4] = PROTOCOL_VERSION;
    b[5] = 99;
    expect(() => decodeHeader(b)).toThrow(/unknown frame type byte 99/);
  });

  test("rejects reserved flag bits", () => {
    const b = new Uint8Array(HEADER_LEN);
    b[4] = PROTOCOL_VERSION;
    b[5] = FrameType.Request;
    b[6] = 0b1000_0000;
    expect(() => decodeHeader(b)).toThrow(/reserved flag bits/);
  });

  test("rejects reserved priority bits (0b11)", () => {
    const b = new Uint8Array(HEADER_LEN);
    b[4] = PROTOCOL_VERSION;
    b[5] = FrameType.Request;
    b[6] = 0b0000_0110;
    expect(() => decodeHeader(b)).toThrow(/reserved priority bits/);
  });

  test("rejects a pure-header frame that declares a body", () => {
    const b = new Uint8Array(HEADER_LEN);
    b[4] = PROTOCOL_VERSION;
    b[5] = FrameType.Ping;
    new DataView(b.buffer).setUint32(0, 1, true); // len = 1
    expect(() => decodeHeader(b)).toThrow(/pure-header frame/);
  });

  test("buildFrame refuses a body on a pure-header frame", () => {
    expect(() => buildFrame(FrameType.Cancel, 0, 0, 0n, new Uint8Array([1]))).toThrow(DecodeError);
  });

  test("encodeFrame lays out header then body", () => {
    const body = new Uint8Array([0xaa, 0xbb, 0xcc]);
    const frame = buildFrame(
      FrameType.Request,
      buildFlags(false, Priority.Interactive, false),
      7,
      9n,
      body,
    );
    const wire = encodeFrame(frame);
    expect(wire.length).toBe(HEADER_LEN + 3);
    expect(decodeHeader(wire.subarray(0, HEADER_LEN)).len).toBe(3);
    expect(Array.from(wire.subarray(HEADER_LEN))).toEqual([0xaa, 0xbb, 0xcc]);
  });
});
