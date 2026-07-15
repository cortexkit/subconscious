// Port of subc-transport's connection_file.rs reader. The connection file is
// the daemon's published rendezvous record; its `key` is the shared transport
// secret. We refuse to trust a key from a file other local users can read,
// exactly as the Rust reader does, so a leaked (group/world-readable) key is a
// loud failure rather than a silent downgrade.

import { promises as fs } from "node:fs";

import { PROTOCOL_VERSION } from "./envelope.js";

export const SCHEMA_VERSION = 1;
export const MIN_KEY_LEN = 32;
export const DAEMON_ID_LEN = 16;

export interface Endpoint {
  host: string;
  port: number;
}

export interface ConnectionInfo {
  schema: number;
  endpoints: Endpoint[];
  /** Transport key bytes. Serialized on disk as a JSON array of numbers. */
  key: Uint8Array;
  /** 16-byte daemon identity. Serialized on disk as a JSON array of numbers. */
  daemonId: Uint8Array;
  pid: number;
  daemonVer: string;
}

export class ConnectionFileError extends Error {}

function toBytes(value: unknown, field: string): Uint8Array {
  if (!Array.isArray(value)) {
    throw new ConnectionFileError(`connection file field '${field}' must be a JSON array of bytes`);
  }
  for (const byte of value) {
    if (typeof byte !== "number" || !Number.isInteger(byte) || byte < 0 || byte > 255) {
      throw new ConnectionFileError(`connection file field '${field}' has invalid byte ${String(byte)}`);
    }
  }
  return Uint8Array.from(value as number[]);
}

function validate(info: ConnectionInfo): void {
  if (info.schema !== SCHEMA_VERSION) {
    throw new ConnectionFileError(
      `unsupported connection file schema ${info.schema}; expected ${SCHEMA_VERSION}`,
    );
  }
  if (info.endpoints.length === 0) {
    throw new ConnectionFileError("connection file must include at least one endpoint");
  }
  if (info.key.length < MIN_KEY_LEN) {
    throw new ConnectionFileError(
      `connection file key is too short: ${info.key.length} bytes, need at least ${MIN_KEY_LEN}`,
    );
  }
  if (info.daemonId.length !== DAEMON_ID_LEN) {
    throw new ConnectionFileError(
      `connection file daemon_id must be ${DAEMON_ID_LEN} bytes, got ${info.daemonId.length}`,
    );
  }
}

/**
 * On unix, reject any group/other permission bit: the key is published
 * owner-only (0600), so a wider mode means the secret has leaked. On Windows the
 * file inherits the per-user profile directory ACL at create time and there are
 * no portable mode bits to re-check, matching the Rust no-op.
 */
async function verifyOwnerOnly(path: string): Promise<void> {
  if (process.platform === "win32") return;
  const stat = await fs.stat(path);
  const mode = stat.mode & 0o777;
  if ((mode & 0o077) !== 0) {
    throw new ConnectionFileError(
      `connection file ${path} has insecure permissions 0o${mode.toString(8)}; expected owner-only 0600`,
    );
  }
}

/** Read, permission-check, and validate a connection file. */
export async function readConnectionFile(path: string): Promise<ConnectionInfo> {
  await verifyOwnerOnly(path);
  const raw = await fs.readFile(path, "utf8");
  let parsed: Record<string, unknown>;
  try {
    parsed = JSON.parse(raw) as Record<string, unknown>;
  } catch (err) {
    throw new ConnectionFileError(`connection file JSON read failed for ${path}: ${String(err)}`);
  }

  const wireVersion = parsed.wire_version;
  if (wireVersion !== undefined && wireVersion !== PROTOCOL_VERSION) {
    throw new ConnectionFileError(
      `connection file wire_version ${String(wireVersion)} but this client speaks ${PROTOCOL_VERSION}; the client library must be upgraded`,
    );
  }

  const endpointsRaw = parsed.endpoints;
  if (!Array.isArray(endpointsRaw)) {
    throw new ConnectionFileError("connection file 'endpoints' must be an array");
  }
  const endpoints: Endpoint[] = endpointsRaw.map((e) => {
    const ep = e as Record<string, unknown>;
    if (typeof ep.host !== "string" || typeof ep.port !== "number") {
      throw new ConnectionFileError("connection file endpoint must be { host: string, port: number }");
    }
    return { host: ep.host, port: ep.port };
  });

  const info: ConnectionInfo = {
    schema: parsed.schema as number,
    endpoints,
    key: toBytes(parsed.key, "key"),
    daemonId: toBytes(parsed.daemon_id, "daemon_id"),
    pid: parsed.pid as number,
    daemonVer: (parsed.daemon_ver as string) ?? "",
  };
  validate(info);
  return info;
}
