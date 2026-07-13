import { describe, expect, test } from "bun:test";

import {
  AdmissionClass,
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

function header(
  len: number,
  ty: FrameType,
  flags: number,
  channel: number,
  epoch: number,
  corr: bigint,
): EnvelopeHeader {
  return { len, ver: PROTOCOL_VERSION, ty, flags, channel, epoch, corr };
}

describe("21-byte envelope header", () => {
  test.each([0, 1, 0xffff_ffff])("round-trips epoch boundary %d at offsets 9..13", (epoch) => {
    const channel = epoch === 0 ? 0 : 42;
    const value = header(3, FrameType.Response, buildFlags(false, Priority.Interactive, false), channel, epoch, 9n);
    const encoded = encodeHeader(value);
    expect(encoded).toHaveLength(21);
    expect(new DataView(encoded.buffer).getUint32(9, true)).toBe(epoch);
    expect(new DataView(encoded.buffer).getBigUint64(13, true)).toBe(9n);
    expect(decodeHeader(encoded)).toEqual(value);
  });

  test("keeps len+version in the frozen prefix and uses version 2", () => {
    const encoded = encodeHeader(header(1, FrameType.Request, 0, 0, 0, 0n));
    expect(Array.from(encoded.subarray(0, 5))).toEqual([1, 0, 0, 0, 2]);
    expect(PROTOCOL_VERSION).toBe(2);
    expect(HEADER_LEN).toBe(21);
  });

  test.each([
    [AdmissionClass.Normal, FrameType.Request],
    [AdmissionClass.Expedite, FrameType.Response],
    [AdmissionClass.Sheddable, FrameType.Push],
    [AdmissionClass.Sheddable, FrameType.StreamData],
  ] as const)("round-trips legal admission class %d on type %d", (admission, ty) => {
    const flags = buildFlags(true, Priority.Background, true, admission);
    const decoded = decodeHeader(encodeHeader(header(0, ty, flags, 7, 1, 11n)));
    expect(decoded.flags).toBe(flags);
    expect((flags >> 4) & 0b11).toBe(admission);
  });

  test("rejects class 11 with the exact taxonomy message", () => {
    const encoded = encodeHeader(header(0, FrameType.Push, 0b0011_0000, 7, 1, 0n));
    expect(() => decodeHeader(encoded)).toThrow(
      new DecodeError("reserved admission class set in flags 0b00110000", "reserved_admission_class"),
    );
  });

  test.each([
    FrameType.Request,
    FrameType.Response,
    FrameType.StreamEnd,
    FrameType.Error,
    FrameType.Cancel,
    FrameType.Ping,
    FrameType.Pong,
    FrameType.Hello,
    FrameType.HelloAck,
    FrameType.Goodbye,
  ])("rejects SHEDDABLE on illegal frame type %d", (ty) => {
    const flags = buildFlags(false, Priority.Passive, false, AdmissionClass.Sheddable);
    const encoded = encodeHeader(header(0, ty, flags, ty === FrameType.Hello ? 0 : 7, ty === FrameType.Hello ? 0 : 1, 0n));
    expect(() => decodeHeader(encoded)).toThrow(
      new DecodeError(
        `SHEDDABLE admission class is illegal on ${FrameType[ty]} in flags 0b00100000`,
        "sheddable_illegal_frame_type",
      ),
    );
  });

  test("rejects nonzero epoch on channel 0 exactly", () => {
    const encoded = encodeHeader(header(0, FrameType.Request, 0, 0, 1, 0n));
    expect(() => decodeHeader(encoded)).toThrow(new DecodeError("control channel carried nonzero epoch 1", "nonzero_epoch_on_control_channel"));
  });

  test("rejects unsupported version before requiring the full header", () => {
    const prefix = new Uint8Array([0, 0, 0, 0, 1]);
    expect(() => decodeHeader(prefix)).toThrow(new DecodeError("unsupported envelope version 1", "unsupported_version"));
  });

  test("rejects each existing malformed-header class", () => {
    expect(() => decodeHeader(new Uint8Array(4))).toThrow(/shorter than frozen prefix/);
    const unknown = encodeHeader(header(0, FrameType.Request, 0, 0, 0, 0n));
    unknown[5] = 99;
    expect(() => decodeHeader(unknown)).toThrow(/unknown frame type byte 99/);
    const reserved = encodeHeader(header(0, FrameType.Request, 0b1000_0000, 0, 0, 0n));
    expect(() => decodeHeader(reserved)).toThrow(/reserved flag bits/);
    const priority = encodeHeader(header(0, FrameType.Request, 0b0000_0110, 0, 0, 0n));
    expect(() => decodeHeader(priority)).toThrow(/reserved priority bits/);
  });

  test("enforces pure-header length and encodes body after byte 21", () => {
    expect(() => buildFrame(FrameType.Cancel, 0, 7, 1, 0n, new Uint8Array([1]))).toThrow(DecodeError);
    const body = new Uint8Array([0xaa, 0xbb, 0xcc]);
    const frame = buildFrame(
      FrameType.Request,
      buildFlags(false, Priority.Interactive, false),
      7,
      1,
      9n,
      body,
    );
    const wire = encodeFrame(frame);
    expect(wire).toHaveLength(HEADER_LEN + body.length);
    expect(Array.from(wire.subarray(HEADER_LEN))).toEqual(Array.from(body));
  });
});
