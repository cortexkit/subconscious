import { describe, expect, test } from "bun:test";

import {
  AdmissionClass,
  DAEMON_ORIGIN_FLAG,
  buildFlags,
  buildFrame,
  buildFrameWithVersion,
  decodeHeader,
  DecodeError,
  encodeFrame,
  encodeHeader,
  FrameType,
  hasDaemonOrigin,
  HEADER_LEN,
  MAX_FRAME_BODY_LEN,
  PROTOCOL_VERSION,
  Priority,
  type EnvelopeHeader,
  type Frame,
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

interface BuildInput {
  ver: number;
  ty: FrameType;
  flags: number;
  channel: number;
  epoch: number;
  corr: bigint;
  body: Uint8Array;
}

function legacyBuildFrameWithVersion(input: BuildInput): Frame {
  if (input.body.length > MAX_FRAME_BODY_LEN) {
    throw new DecodeError(
      `frame body ${input.body.length} exceeds max ${MAX_FRAME_BODY_LEN}`,
      "frame_body_too_large",
    );
  }
  const wireHeader = {
    len: input.body.length,
    ver: input.ver,
    ty: input.ty,
    flags: input.flags,
    channel: input.channel,
    epoch: input.epoch,
    corr: input.corr,
  };
  decodeHeader(encodeHeader(wireHeader));
  return { header: wireHeader, body: input.body };
}

function buildOutcome(build: () => Frame, compareWire: boolean): object {
  try {
    const frame = build();
    return {
      kind: "success",
      header: frame.header,
      wire: compareWire ? Array.from(encodeFrame(frame)) : null,
    };
  } catch (error) {
    return {
      kind: "error",
      name: error instanceof Error ? error.name : typeof error,
      code: error instanceof DecodeError ? error.code : null,
      message: error instanceof DecodeError ? error.message : null,
    };
  }
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

  test("accepts daemon-origin Error headers while retaining bit 7 rejection", () => {
    const old = encodeHeader(header(0, FrameType.Error, 0, 7, 1, 0n));
    expect(hasDaemonOrigin(decodeHeader(old).flags)).toBe(false);

    const daemon = encodeHeader(header(0, FrameType.Error, DAEMON_ORIGIN_FLAG, 7, 1, 0n));
    expect(hasDaemonOrigin(decodeHeader(daemon).flags)).toBe(true);

    const reserved = encodeHeader(header(0, FrameType.Error, 0b1000_0000, 7, 1, 0n));
    expect(() => decodeHeader(reserved)).toThrow(/reserved flag bits/);
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

  test("field validation is equivalent to the legacy encode-decode oracle", () => {
    const base: BuildInput = {
      ver: PROTOCOL_VERSION,
      ty: FrameType.Request,
      flags: 0,
      channel: 7,
      epoch: 1,
      corr: 0n,
      body: new Uint8Array(0),
    };
    const fakeBody = (length: number) => ({ length }) as unknown as Uint8Array;
    const cases: Array<{ name: string; input: BuildInput; compareWire?: boolean }> = [
      { name: "baseline", input: base },
      { name: "version zero", input: { ...base, ver: 0 } },
      { name: "version below", input: { ...base, ver: PROTOCOL_VERSION - 1 } },
      { name: "version above", input: { ...base, ver: PROTOCOL_VERSION + 1 } },
      { name: "version uint8 max", input: { ...base, ver: 0xff } },
      { name: "version overflow wraps valid", input: { ...base, ver: 0x100 + PROTOCOL_VERSION } },
      {
        name: "version fractional overflow truncates and wraps valid",
        input: { ...base, ver: 0x100 + PROTOCOL_VERSION + 0.75 },
      },
      { name: "version negative wraps valid", input: { ...base, ver: PROTOCOL_VERSION - 0x100 } },
      { name: "version bigint throws during encoding", input: { ...base, ver: 2n as unknown as number } },
      { name: "version overflow wraps invalid", input: { ...base, ver: 0x100 + PROTOCOL_VERSION - 1 } },
      { name: "version NaN coerces zero", input: { ...base, ver: Number.NaN } },
      { name: "version infinity coerces zero", input: { ...base, ver: Number.POSITIVE_INFINITY } },
      { name: "type negative wraps unknown", input: { ...base, ty: -1 as FrameType } },
      { name: "type lower bound", input: { ...base, ty: FrameType.Request } },
      { name: "type upper bound", input: { ...base, ty: FrameType.Goodbye } },
      { name: "type above upper bound", input: { ...base, ty: (FrameType.Goodbye + 1) as FrameType } },
      { name: "type uint8 max", input: { ...base, ty: 0xff as FrameType } },
      { name: "type overflow wraps lower bound", input: { ...base, ty: 0x100 as FrameType } },
      {
        name: "type fractional overflow truncates and wraps lower bound",
        input: { ...base, ty: (0x100 + 0.75) as FrameType },
      },
      { name: "type overflow wraps upper bound", input: { ...base, ty: (0x100 + FrameType.Goodbye) as FrameType } },
      { name: "type NaN coerces lower bound", input: { ...base, ty: Number.NaN as FrameType } },
      { name: "type bigint throws during encoding", input: { ...base, ty: 0n as unknown as FrameType } },
      { name: "flags reserved bits", input: { ...base, flags: 0b1000_0000 } },
      { name: "flags reserved priority", input: { ...base, flags: 0b0000_0110 } },
      { name: "flags reserved admission", input: { ...base, flags: 0b0011_0000 } },
      { name: "flags illegal sheddable type", input: { ...base, flags: 0b0010_0000 } },
      { name: "flags legal sheddable type", input: { ...base, ty: FrameType.Push, flags: 0b0010_0000 } },
      { name: "flags overflow wraps valid", input: { ...base, flags: 0x100 } },
      { name: "flags fractional overflow truncates and wraps valid", input: { ...base, flags: 0x100 + 0.75 } },
      { name: "flags negative wraps valid", input: { ...base, flags: -0x100 } },
      { name: "flags NaN coerces valid", input: { ...base, flags: Number.NaN } },
      { name: "flags bigint throws during encoding", input: { ...base, flags: 0n as unknown as number } },
      { name: "flags overflow wraps reserved priority", input: { ...base, flags: 0x106 } },
      { name: "channel zero epoch zero", input: { ...base, channel: 0, epoch: 0 } },
      { name: "channel zero nonzero epoch", input: { ...base, channel: 0, epoch: 1 } },
      { name: "channel uint16 max", input: { ...base, channel: 0xffff } },
      { name: "channel overflow wraps zero", input: { ...base, channel: 0x1_0000, epoch: 1 } },
      { name: "channel overflow wraps nonzero", input: { ...base, channel: 0x1_0007 } },
      {
        name: "channel fractional overflow truncates and wraps nonzero",
        input: { ...base, channel: 0x1_0007 + 0.75 },
      },
      { name: "channel negative wraps max", input: { ...base, channel: -1 } },
      { name: "channel bigint throws during encoding", input: { ...base, channel: 7n as unknown as number } },
      { name: "channel NaN coerces zero", input: { ...base, channel: Number.NaN, epoch: 0 } },
      { name: "epoch uint32 max", input: { ...base, epoch: 0xffff_ffff } },
      { name: "epoch overflow wraps zero", input: { ...base, channel: 0, epoch: 0x1_0000_0000 } },
      {
        name: "epoch fractional overflow truncates and wraps nonzero",
        input: { ...base, epoch: 0x1_0000_0001 + 0.75 },
      },
      { name: "epoch negative wraps max", input: { ...base, channel: 0, epoch: -1 } },
      { name: "epoch bigint throws during encoding", input: { ...base, epoch: 1n as unknown as number } },
      { name: "epoch NaN coerces zero", input: { ...base, channel: 0, epoch: Number.NaN } },
      { name: "correlation uint64 max", input: { ...base, corr: 0xffff_ffff_ffff_ffffn } },
      { name: "correlation overflow wraps zero", input: { ...base, corr: 0x1_0000_0000_0000_0000n } },
      { name: "correlation negative wraps max", input: { ...base, corr: -1n } },
      {
        name: "correlation numeric string coerces",
        input: { ...base, corr: "18446744073709551617" as unknown as bigint },
      },
      { name: "correlation number throws during encoding", input: { ...base, corr: 0 as unknown as bigint } },
      { name: "request body length one", input: { ...base, body: new Uint8Array(1) } },
      { name: "pure-header body length one", input: { ...base, ty: FrameType.Cancel, body: new Uint8Array(1) } },
      { name: "body length maximum", input: { ...base, body: fakeBody(MAX_FRAME_BODY_LEN) }, compareWire: false },
      {
        name: "body length above maximum",
        input: { ...base, body: fakeBody(MAX_FRAME_BODY_LEN + 1) },
        compareWire: false,
      },
      { name: "version wins before type", input: { ...base, ver: 1, ty: 0xff as FrameType } },
      { name: "type wins before flags", input: { ...base, ty: 0xff as FrameType, flags: 0xff } },
      { name: "reserved bits win before priority", input: { ...base, flags: 0b1000_0110 } },
      { name: "priority wins before admission", input: { ...base, flags: 0b0011_0110 } },
      { name: "admission wins before channel", input: { ...base, flags: 0b0011_0000, channel: 0, epoch: 1 } },
      {
        name: "sheddable type wins before channel",
        input: { ...base, flags: 0b0010_0000, channel: 0, epoch: 1 },
      },
      {
        name: "channel wins before pure-header length",
        input: { ...base, ty: FrameType.Cancel, channel: 0, epoch: 1, body: new Uint8Array(1) },
      },
      {
        name: "encoding conversion wins before decode validation",
        input: { ...base, ver: 1, corr: 0 as unknown as bigint },
      },
    ];

    for (const scenario of cases) {
      const { input } = scenario;
      const compareWire = scenario.compareWire ?? true;
      const current = buildOutcome(
        () =>
          buildFrameWithVersion(
            input.ver,
            input.ty,
            input.flags,
            input.channel,
            input.epoch,
            input.corr,
            input.body,
          ),
        compareWire,
      );
      const legacy = buildOutcome(() => legacyBuildFrameWithVersion(input), compareWire);
      expect(current, scenario.name).toEqual(legacy);
    }
  });
});
