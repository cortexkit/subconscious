// The byte-identity authority for this client: drive the REAL subc-core daemon
// binary over loopback TCP and complete the HMAC handshake. If the proof
// construction, message framing, or envelope layout drifts from the Rust by a
// single byte, authentication fails here — no unit test can substitute for it.
//
// The daemon is booted module-less in an isolated XDG_RUNTIME_DIR (ephemeral
// port via SUBC_PORT=0) and XDG_CONFIG_HOME (so it finds no subc.jsonc and
// supervises nothing). Skipped automatically when the binary is not built, so
// `bun test` stays green standalone; the CI lane builds subc-core first.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { spawn, type ChildProcess } from "node:child_process";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { AuthError } from "../src/auth.js";
import { readConnectionFile } from "../src/connection-file.js";
import { SubcClient } from "../src/index.js";

const ROOT = join(import.meta.dir, "..", "..", "..");
const DAEMON = join(ROOT, "target", "debug", "subc-core");
const CONN_NAME = "subc-connection.json";

const available = existsSync(DAEMON);
if (!available) {
  // eslint-disable-next-line no-console
  console.warn(`[live-handshake] skipping: ${DAEMON} not built (run cargo build -p subc-core)`);
}

async function waitFor(predicate: () => boolean, timeoutMs: number, label: string): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    if (predicate()) return;
    await new Promise((r) => setTimeout(r, 50));
  }
  throw new Error(`timed out waiting for ${label}`);
}

describe.if(available)("live handshake against real subc-core", () => {
  let daemon: ChildProcess;
  let runtimeDir: string;
  let configDir: string;
  let connFile: string;
  let stderr = "";

  beforeAll(async () => {
    runtimeDir = mkdtempSync(join(tmpdir(), "subc-live-rt-"));
    configDir = mkdtempSync(join(tmpdir(), "subc-live-cfg-"));
    connFile = join(runtimeDir, CONN_NAME);

    daemon = spawn(DAEMON, [], {
      env: {
        ...process.env,
        XDG_RUNTIME_DIR: runtimeDir,
        XDG_CONFIG_HOME: configDir,
        SUBC_PORT: "0",
      },
      stdio: ["ignore", "ignore", "pipe"],
    });
    daemon.stderr?.on("data", (c: Buffer) => {
      stderr += c.toString();
    });

    await waitFor(() => existsSync(connFile), 10_000, "daemon connection file");
  });

  afterAll(() => {
    daemon?.kill("SIGKILL");
    for (const dir of [runtimeDir, configDir]) {
      if (dir) rmSync(dir, { recursive: true, force: true });
    }
  });

  test("authenticates and lists the (empty) catalog", async () => {
    const client = await SubcClient.connect({ connectionFile: connFile });
    try {
      // Reaching here at all proves the handshake: SubcClient.connect runs the
      // full ClientHello -> verify ServerProof+daemon_id -> ClientAuth exchange
      // against the real daemon before resolving.
      const modules = await client.catalogList();
      expect(Array.isArray(modules)).toBe(true);
    } finally {
      client.close();
    }
  });

  test("rejects a tampered key (proves the proof is actually verified)", async () => {
    const conn = await readConnectionFile(connFile);
    const tampered = { ...JSON.parse(readFileSync(connFile, "utf8")) };
    const key = [...(tampered.key as number[])];
    key[0] = key[0]! ^ 0xff; // flip a byte so our client computes the wrong proof
    tampered.key = key;

    const badPath = join(runtimeDir, "tampered-connection.json");
    writeFileSync(badPath, JSON.stringify(tampered), { mode: 0o600 });
    chmodSync(badPath, 0o600);

    // Server's proof (computed with the real key) won't match what our client
    // expects (computed with the flipped key) -> the client aborts the handshake.
    await expect(SubcClient.connect({ connectionFile: badPath })).rejects.toThrow(AuthError);
    expect(conn.daemonId.length).toBe(16);
  });
});
