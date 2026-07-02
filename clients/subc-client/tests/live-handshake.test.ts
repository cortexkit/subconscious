// The byte-identity authority for this client: drive the REAL subc-core daemon
// binary over loopback TCP and complete the HMAC handshake. If the proof
// construction, message framing, or envelope layout drifts from the Rust by a
// single byte, authentication fails here — no unit test can substitute for it.
//
// The daemon is booted module-less in an isolated XDG_RUNTIME_DIR (ephemeral
// port via SUBC_PORT=0) and XDG_CONFIG_HOME (so it finds no subc.jsonc and
// supervises nothing). The helper builds subc-core first when the binary is
// absent, so the live proof never silently skips.

import { afterAll, beforeAll, describe, expect, test } from "bun:test";
import { chmodSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { AuthError } from "../src/auth.js";
import { readConnectionFile } from "../src/connection-file.js";
import { SubcClient } from "../src/index.js";
import { startLiveDaemon, type LiveDaemon } from "./live-daemon.js";

// Live tests need a compiled subc-core daemon (Rust toolchain). They are
// OFF by default so the unit suite runs in a bun-only environment (npm
// release gate, bun-only CI job); set RUN_SUBC_LIVE=1 to run them.
const LIVE = process.env.RUN_SUBC_LIVE === "1";

describe.skipIf(!LIVE)("live handshake against real subc-core", () => {
  let live: LiveDaemon;

  beforeAll(async () => {
    live = await startLiveDaemon("subc-live-handshake");
  });

  afterAll(() => {
    live?.stop();
  });

  test("authenticates and lists the (empty) catalog", async () => {
    const client = await SubcClient.connect({ connectionFile: live.connFile });
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
    const conn = await readConnectionFile(live.connFile);
    const tampered = { ...JSON.parse(readFileSync(live.connFile, "utf8")) };
    const key = [...(tampered.key as number[])];
    key[0] = key[0]! ^ 0xff; // flip a byte so our client computes the wrong proof
    tampered.key = key;

    const badPath = join(live.runtimeDir, "tampered-connection.json");
    writeFileSync(badPath, JSON.stringify(tampered), { mode: 0o600 });
    chmodSync(badPath, 0o600);

    // Server's proof (computed with the real key) won't match what our client
    // expects (computed with the flipped key) -> the client aborts the handshake.
    await expect(SubcClient.connect({ connectionFile: badPath })).rejects.toThrow(AuthError);
    expect(conn.daemonId.length).toBe(16);
  });
});
