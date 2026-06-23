// Byte-for-byte port of subc-protocol's 17-byte envelope header.
// Source of truth: crates/subc-protocol/src/lib.rs. Keep field offsets, the
// little-endian encoding, and the frame-type/flag numbering in lock-step with
// the Rust; a one-byte drift here desynchronizes every frame on the wire.

export const PROTOCOL_VERSION = 1;
export const HEADER_LEN = 17;
export const FROZEN_PREFIX_LEN = 5;
export const MAX_FRAME_BODY_LEN = 64 * 1024 * 1024;

/** `type` byte at offset 5. */
export enum FrameType {
  Request = 0,
  Response = 1,
  Push = 2,
  StreamData = 3,
  StreamEnd = 4,
  Error = 5,
  Cancel = 6,
  Ping = 7,
  Pong = 8,
  Hello = 9,
  HelloAck = 10,
  Goodbye = 11,
}

const FRAME_TYPE_MAX = FrameType.Goodbye;

/** Cancel/Ping/Pong/Goodbye carry only a header (`len` must be 0). */
export function isPureHeader(ty: FrameType): boolean {
  return (
    ty === FrameType.Cancel ||
    ty === FrameType.Ping ||
    ty === FrameType.Pong ||
    ty === FrameType.Goodbye
  );
}

/** Scheduling priority carried in flags bits 1-2. */
export enum Priority {
  Passive = 0,
  Interactive = 1,
  Background = 2,
}

const FLAG_BINARY = 0b0000_0001; // bit 0
const FLAG_PRIORITY_MASK = 0b0000_0110; // bits 1-2
const FLAG_PRIORITY_SHIFT = 1;
const FLAG_LAST = 0b0000_1000; // bit 3
const FLAG_RESERVED_MASK = 0b1111_0000; // bits 4-7 must be zero

/** Build the flags byte from typed components (mirrors Flags::new). */
export function buildFlags(binary: boolean, priority: Priority, last: boolean): number {
  let b = 0;
  if (binary) b |= FLAG_BINARY;
  b |= priority << FLAG_PRIORITY_SHIFT;
  if (last) b |= FLAG_LAST;
  return b;
}

export interface EnvelopeHeader {
  len: number;
  ver: number;
  ty: FrameType;
  flags: number;
  channel: number;
  corr: bigint;
}

export interface Frame {
  header: EnvelopeHeader;
  body: Uint8Array;
}

/** Serialize a header to its fixed 17-byte little-endian form. */
export function encodeHeader(h: EnvelopeHeader): Uint8Array {
  const buf = new Uint8Array(HEADER_LEN);
  const view = new DataView(buf.buffer);
  view.setUint32(0, h.len, true);
  buf[4] = h.ver;
  buf[5] = h.ty;
  buf[6] = h.flags;
  view.setUint16(7, h.channel, true);
  view.setBigUint64(9, h.corr, true);
  return buf;
}

export class DecodeError extends Error {}

/**
 * Decode a header from the front of `bytes`, following the frozen-prefix
 * discipline: need 5 bytes for len+ver, dispatch full header length on ver,
 * then validate. Mirrors decode_header — never throws on a structurally short
 * buffer beyond the typed DecodeError.
 */
export function decodeHeader(bytes: Uint8Array): EnvelopeHeader {
  if (bytes.length < FROZEN_PREFIX_LEN) {
    throw new DecodeError(`header shorter than frozen prefix: have ${bytes.length} bytes`);
  }
  const ver = bytes[4]!;
  if (ver !== PROTOCOL_VERSION) {
    throw new DecodeError(`unsupported envelope version ${ver}`);
  }
  if (bytes.length < HEADER_LEN) {
    throw new DecodeError(`header too short for version: have ${bytes.length} bytes, need ${HEADER_LEN}`);
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const len = view.getUint32(0, true);
  const tyByte = bytes[5]!;
  if (tyByte > FRAME_TYPE_MAX) {
    throw new DecodeError(`unknown frame type byte ${tyByte}`);
  }
  const ty = tyByte as FrameType;
  const flags = bytes[6]!;
  if ((flags & FLAG_RESERVED_MASK) !== 0) {
    throw new DecodeError(`reserved flag bits set in flags 0b${flags.toString(2)}`);
  }
  if (((flags & FLAG_PRIORITY_MASK) >> FLAG_PRIORITY_SHIFT) === 0b11) {
    throw new DecodeError(`reserved priority bits set in flags 0b${flags.toString(2)}`);
  }
  if (isPureHeader(ty) && len !== 0) {
    throw new DecodeError(`pure-header frame ${FrameType[ty]} declared non-zero body length ${len}`);
  }
  const channel = view.getUint16(7, true);
  const corr = view.getBigUint64(9, true);
  return { len, ver, ty, flags, channel, corr };
}

/** Build a full current-version frame, enforcing the body-length cap and the pure-header rule. */
export function buildFrame(
  ty: FrameType,
  flags: number,
  channel: number,
  corr: bigint,
  body: Uint8Array,
): Frame {
  return buildFrameWithVersion(PROTOCOL_VERSION, ty, flags, channel, corr, body);
}

/** Build a full frame while preserving a peer-negotiated envelope version. */
export function buildFrameWithVersion(
  ver: number,
  ty: FrameType,
  flags: number,
  channel: number,
  corr: bigint,
  body: Uint8Array,
): Frame {
  if (body.length > MAX_FRAME_BODY_LEN) {
    throw new DecodeError(`frame body ${body.length} exceeds max ${MAX_FRAME_BODY_LEN}`);
  }
  if (isPureHeader(ty) && body.length !== 0) {
    throw new DecodeError(`pure-header frame ${FrameType[ty]} cannot carry a body`);
  }
  return {
    header: { len: body.length, ver, ty, flags, channel, corr },
    body,
  };
}

/** Encode a frame to wire bytes: 17-byte header followed by `len` body bytes. */
export function encodeFrame(frame: Frame): Uint8Array {
  const header = encodeHeader(frame.header);
  const out = new Uint8Array(header.length + frame.body.length);
  out.set(header, 0);
  out.set(frame.body, header.length);
  return out;
}
