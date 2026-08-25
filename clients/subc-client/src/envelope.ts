// Byte-for-byte port of subc-protocol's fixed envelope header.
// Source of truth: crates/subc-protocol/src/lib.rs. Keep field offsets, little-
// endian encoding, and frame/flag numbering in lock-step with Rust.

export const PROTOCOL_VERSION = 2;
export const HEADER_LEN = 21;
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
  return ty === FrameType.Cancel || ty === FrameType.Ping || ty === FrameType.Pong || ty === FrameType.Goodbye;
}

/** Scheduling priority carried in flags bits 1-2. */
export enum Priority {
  Passive = 0,
  Interactive = 1,
  Background = 2,
}

/** Admission behavior carried in flags bits 4-5. */
export enum AdmissionClass {
  Normal = 0,
  Expedite = 1,
  Sheddable = 2,
}

const FLAG_BINARY = 0b0000_0001;
const FLAG_PRIORITY_MASK = 0b0000_0110;
const FLAG_PRIORITY_SHIFT = 1;
const FLAG_LAST = 0b0000_1000;
const FLAG_ADMISSION_MASK = 0b0011_0000;
const FLAG_ADMISSION_SHIFT = 4;
export const DAEMON_ORIGIN_FLAG = 0x40;
const FLAG_RESERVED_MASK = 0b1000_0000;

/** Build flags from typed components. Admission defaults to NORMAL. */
export function buildFlags(
  binary: boolean,
  priority: Priority,
  last: boolean,
  admissionClass: AdmissionClass = AdmissionClass.Normal,
): number {
  let flags = 0;
  if (binary) flags |= FLAG_BINARY;
  flags |= priority << FLAG_PRIORITY_SHIFT;
  if (last) flags |= FLAG_LAST;
  flags |= admissionClass << FLAG_ADMISSION_SHIFT;
  return flags;
}

export function admissionClass(flags: number): AdmissionClass {
  return ((flags & FLAG_ADMISSION_MASK) >> FLAG_ADMISSION_SHIFT) as AdmissionClass;
}

export function hasDaemonOrigin(flags: number): boolean {
  return (flags & DAEMON_ORIGIN_FLAG) !== 0;
}

export interface EnvelopeHeader {
  len: number;
  ver: number;
  ty: FrameType;
  flags: number;
  channel: number;
  epoch: number;
  corr: bigint;
}

export interface Frame {
  header: EnvelopeHeader;
  body: Uint8Array;
}

/** Serialize a header to its fixed 21-byte little-endian form. */
export function encodeHeader(header: EnvelopeHeader): Uint8Array {
  const buffer = new Uint8Array(HEADER_LEN);
  const view = new DataView(buffer.buffer);
  view.setUint32(0, header.len, true);
  buffer[4] = header.ver;
  buffer[5] = header.ty;
  buffer[6] = header.flags;
  view.setUint16(7, header.channel, true);
  view.setUint32(9, header.epoch, true);
  view.setBigUint64(13, header.corr, true);
  return buffer;
}

export type DecodeErrorCode =
  | "too_short_for_prefix"
  | "unsupported_version"
  | "too_short_for_header"
  | "unknown_frame_type"
  | "reserved_flag_bits"
  | "reserved_priority_bits"
  | "reserved_admission_class"
  | "sheddable_illegal_frame_type"
  | "nonzero_epoch_on_control_channel"
  | "pure_header_frame_with_body"
  | "frame_body_too_large"
  | "frame_length_mismatch";

/** Typed envelope decode failure mirroring the Rust wire taxonomy. */
export class DecodeError extends Error {
  constructor(message: string, readonly code: DecodeErrorCode) {
    super(message);
    this.name = "DecodeError";
  }
}

function validateHeaderFields(header: EnvelopeHeader): void {
  // Apply wire-width coercions in serialization order before semantic validation.
  // This preserves encodeHeader's modulo behavior and its early conversion errors.
  const len = header.len >>> 0;
  const ver = (header.ver >>> 0) & 0xff;
  const typeByte = (header.ty >>> 0) & 0xff;
  const flags = (header.flags >>> 0) & 0xff;
  const channel = (header.channel >>> 0) & 0xffff;
  const epoch = header.epoch >>> 0;
  void BigInt.asUintN(64, header.corr);

  if (ver !== PROTOCOL_VERSION) throw new DecodeError(`unsupported envelope version ${ver}`, "unsupported_version");
  if (typeByte > FRAME_TYPE_MAX) throw new DecodeError(`unknown frame type byte ${typeByte}`, "unknown_frame_type");
  const ty = typeByte as FrameType;
  if ((flags & FLAG_RESERVED_MASK) !== 0) {
    throw new DecodeError(
      `reserved flag bits set in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "reserved_flag_bits",
    );
  }
  if (((flags & FLAG_PRIORITY_MASK) >> FLAG_PRIORITY_SHIFT) === 0b11) {
    throw new DecodeError(
      `reserved priority bits set in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "reserved_priority_bits",
    );
  }
  const admission = (flags & FLAG_ADMISSION_MASK) >> FLAG_ADMISSION_SHIFT;
  if (admission === 0b11) {
    throw new DecodeError(
      `reserved admission class set in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "reserved_admission_class",
    );
  }
  if (admission === AdmissionClass.Sheddable && ty !== FrameType.Push && ty !== FrameType.StreamData) {
    throw new DecodeError(
      `SHEDDABLE admission class is illegal on ${FrameType[ty]} in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "sheddable_illegal_frame_type",
    );
  }
  if (channel === 0 && epoch !== 0) {
    throw new DecodeError(
      `control channel carried nonzero epoch ${epoch}`,
      "nonzero_epoch_on_control_channel",
    );
  }
  if (isPureHeader(ty) && len !== 0) {
    throw new DecodeError(
      `pure-header frame ${FrameType[ty]} declared non-zero body length ${len}`,
      "pure_header_frame_with_body",
    );
  }
}

/** Decode and validate a header from the front of `bytes`. */
export function decodeHeader(bytes: Uint8Array): EnvelopeHeader {
  if (bytes.length < FROZEN_PREFIX_LEN) {
    throw new DecodeError(`header shorter than frozen prefix: have ${bytes.length} bytes`, "too_short_for_prefix");
  }
  const ver = bytes[4]!;
  if (ver !== PROTOCOL_VERSION) throw new DecodeError(`unsupported envelope version ${ver}`, "unsupported_version");
  if (bytes.length < HEADER_LEN) {
    throw new DecodeError(
      `header too short for version: have ${bytes.length} bytes, need ${HEADER_LEN}`,
      "too_short_for_header",
    );
  }

  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const len = view.getUint32(0, true);
  const typeByte = bytes[5]!;
  if (typeByte > FRAME_TYPE_MAX) throw new DecodeError(`unknown frame type byte ${typeByte}`, "unknown_frame_type");
  const ty = typeByte as FrameType;
  const flags = bytes[6]!;
  if ((flags & FLAG_RESERVED_MASK) !== 0) {
    throw new DecodeError(
      `reserved flag bits set in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "reserved_flag_bits",
    );
  }
  if (((flags & FLAG_PRIORITY_MASK) >> FLAG_PRIORITY_SHIFT) === 0b11) {
    throw new DecodeError(
      `reserved priority bits set in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "reserved_priority_bits",
    );
  }
  const admission = (flags & FLAG_ADMISSION_MASK) >> FLAG_ADMISSION_SHIFT;
  if (admission === 0b11) {
    throw new DecodeError(
      `reserved admission class set in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "reserved_admission_class",
    );
  }
  if (admission === AdmissionClass.Sheddable && ty !== FrameType.Push && ty !== FrameType.StreamData) {
    throw new DecodeError(
      `SHEDDABLE admission class is illegal on ${FrameType[ty]} in flags 0b${flags.toString(2).padStart(8, "0")}`,
      "sheddable_illegal_frame_type",
    );
  }
  const channel = view.getUint16(7, true);
  const epoch = view.getUint32(9, true);
  if (channel === 0 && epoch !== 0) {
    throw new DecodeError(
      `control channel carried nonzero epoch ${epoch}`,
      "nonzero_epoch_on_control_channel",
    );
  }
  if (isPureHeader(ty) && len !== 0) {
    throw new DecodeError(
      `pure-header frame ${FrameType[ty]} declared non-zero body length ${len}`,
      "pure_header_frame_with_body",
    );
  }
  return { len, ver, ty, flags, channel, epoch, corr: view.getBigUint64(13, true) };
}

/** Build a current-version frame and validate its complete header. */
export function buildFrame(
  ty: FrameType,
  flags: number,
  channel: number,
  epoch: number,
  corr: bigint,
  body: Uint8Array,
): Frame {
  return buildFrameWithVersion(PROTOCOL_VERSION, ty, flags, channel, epoch, corr, body);
}

/** Build a frame while preserving the peer's exact supported version. */
export function buildFrameWithVersion(
  ver: number,
  ty: FrameType,
  flags: number,
  channel: number,
  epoch: number,
  corr: bigint,
  body: Uint8Array,
): Frame {
  if (body.length > MAX_FRAME_BODY_LEN) {
    throw new DecodeError(`frame body ${body.length} exceeds max ${MAX_FRAME_BODY_LEN}`, "frame_body_too_large");
  }
  const header = { len: body.length, ver, ty, flags, channel, epoch, corr };
  validateHeaderFields(header);
  return { header, body };
}

/** Encode a frame to wire bytes: header followed by exactly `len` body bytes. */
export function encodeFrame(frame: Frame): Uint8Array {
  if (frame.header.len !== frame.body.length) {
    throw new DecodeError(
      `frame header length ${frame.header.len} does not match body length ${frame.body.length}`,
      "frame_length_mismatch",
    );
  }
  const header = encodeHeader(frame.header);
  const output = new Uint8Array(header.length + frame.body.length);
  output.set(header, 0);
  output.set(frame.body, header.length);
  return output;
}
