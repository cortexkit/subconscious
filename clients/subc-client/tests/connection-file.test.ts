import { afterEach, describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { ConnectionFileError, readConnectionFile } from "../src/connection-file.js";
import { PROTOCOL_VERSION } from "../src/envelope.js";

const dirs: string[] = [];

function tempFile(contents: string, mode = 0o600): string {
  const dir = mkdtempSync(join(tmpdir(), "subc-connfile-"));
  dirs.push(dir);
  const path = join(dir, "subc-connection.json");
  writeFileSync(path, contents, { mode });
  chmodSync(path, mode);
  return path;
}

function validInfo(overrides: Record<string, unknown> = {}): string {
  return JSON.stringify({
    schema: 1,
    endpoints: [{ host: "127.0.0.1", port: 8799 }],
    key: Array(32).fill(0xab),
    daemon_id: Array(16).fill(0x11),
    pid: 4242,
    daemon_ver: "subc-test",
    ...overrides,
  });
}

afterEach(() => {
  for (const dir of dirs.splice(0)) rmSync(dir, { recursive: true, force: true });
});

describe("connection file reader", () => {
  test("accepts a file without wire_version", async () => {
    const info = await readConnectionFile(tempFile(validInfo()));
    expect(info.schema).toBe(1);
    expect(info.endpoints[0]).toEqual({ host: "127.0.0.1", port: 8799 });
    expect(info.key.length).toBe(32);
    expect(info.daemonId.length).toBe(16);
    expect(info.daemonVer).toBe("subc-test");
  });

  test("accepts a matching wire_version", async () => {
    await expect(readConnectionFile(tempFile(validInfo({ wire_version: PROTOCOL_VERSION })))).resolves.toMatchObject({
      schema: 1,
    });
  });

  test("rejects a mismatched wire_version with upgrade guidance", async () => {
    const wireVersion = PROTOCOL_VERSION + 1;
    await expect(readConnectionFile(tempFile(validInfo({ wire_version: wireVersion })))).rejects.toThrow(
      new RegExp(
        `connection file wire_version ${wireVersion} but this client speaks ${PROTOCOL_VERSION}; the client library must be upgraded`,
      ),
    );
  });

  test.if(process.platform !== "win32")("rejects a group/world-readable file", async () => {
    const path = tempFile(validInfo(), 0o644);
    await expect(readConnectionFile(path)).rejects.toThrow(/insecure permissions/);
  });

  test("rejects an unsupported schema", async () => {
    await expect(readConnectionFile(tempFile(validInfo({ schema: 2 })))).rejects.toThrow(
      /unsupported connection file schema 2/,
    );
  });

  test("rejects an empty endpoint list", async () => {
    await expect(readConnectionFile(tempFile(validInfo({ endpoints: [] })))).rejects.toThrow(
      /at least one endpoint/,
    );
  });

  test("rejects a short key", async () => {
    await expect(
      readConnectionFile(tempFile(validInfo({ key: Array(16).fill(1) }))),
    ).rejects.toThrow(/key is too short/);
  });

  test("rejects a wrong-length daemon_id", async () => {
    await expect(
      readConnectionFile(tempFile(validInfo({ daemon_id: Array(8).fill(1) }))),
    ).rejects.toThrow(ConnectionFileError);
  });
});
