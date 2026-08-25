import { expect, test } from "bun:test";

import {
  DAEMON_ORIGIN_FLAG,
  decodeHeader,
  encodeFrame,
  encodeHeader,
  FrameType,
  hasDaemonOrigin,
  type EnvelopeHeader,
} from "../src/envelope.js";

interface FrameVector {
  name: string;
  ty: number;
  flags: number;
  channel: number;
  epoch: number;
  corr: number;
  body_hex: string;
  expected_header_hex: string;
  expected_frame_hex: string;
}

interface WireVectors {
  frame_vectors: FrameVector[];
}

function hex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function bytes(value: string): Uint8Array {
  return Uint8Array.from(value.match(/../g) ?? [], (pair) => Number.parseInt(pair, 16));
}

test("matches the shared Rust-generated envelope vectors", async () => {
  const fixture = (await Bun.file(
    new URL("../../subc-client-swift/Tests/SubcClientTests/Fixtures/wire_vectors.json", import.meta.url),
  ).json()) as WireVectors;

  const old = fixture.frame_vectors.find((vector) => vector.name === "error_json_max_epoch");
  const daemon = fixture.frame_vectors.find((vector) => vector.name === "error_json_max_epoch_daemon_origin");
  expect(old?.flags).toBe(4);
  expect(daemon?.flags).toBe(DAEMON_ORIGIN_FLAG);

  for (const vector of fixture.frame_vectors) {
    const body = bytes(vector.body_hex);
    const header: EnvelopeHeader = {
      len: body.length,
      ver: 2,
      ty: vector.ty as FrameType,
      flags: vector.flags,
      channel: vector.channel,
      epoch: vector.epoch,
      corr: BigInt(vector.corr),
    };
    expect(hex(encodeHeader(header)), vector.name).toBe(vector.expected_header_hex);
    expect(
      hex(encodeFrame({ header, body })),
      vector.name,
    ).toBe(vector.expected_frame_hex);
    expect(decodeHeader(encodeHeader(header)), vector.name).toEqual(header);
  }

  expect(hasDaemonOrigin(decodeHeader(encodeHeader({
    len: Number(bytes(daemon!.body_hex).length),
    ver: 2,
    ty: FrameType.Error,
    flags: daemon!.flags,
    channel: daemon!.channel,
    epoch: daemon!.epoch,
    corr: BigInt(daemon!.corr),
  })).flags)).toBe(true);
  expect(hasDaemonOrigin(old!.flags)).toBe(false);
});
