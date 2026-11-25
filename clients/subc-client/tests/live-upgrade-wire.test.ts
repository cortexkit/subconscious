import { expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import {
  buildFrame, FrameType, LIVE_UPGRADE_CAPABILITY, PROTOCOL_VERSION, RESUME_REFUSED,
  type DaemonUpgradeReloadingBody, type ModuleHelloAckBody, type ModuleHelloBody,
  type ModuleUpgrading, type ReconnectOutcome, type ResumeRefusedBody, type UpgradeFailedBody,
} from "../src/index.js";
import { encodeModuleHello } from "../src/provider.js";
import { UpgradePeer, frame, notice, readHello } from "./upgrade-peer.js";

const token = "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8";
const hello = {
  manifest: { module_id: "upgrade-worker", module_version: "1.0.0", protocol_ver: 2, provides: [] },
  protocol_ver: 2, control_ops: null,
} satisfies ModuleHelloBody;
const ack = { negotiated_ver: 2, subc_ops: [], subc_capabilities: [] } satisfies ModuleHelloAckBody;

function golden(name: string, crate = "subc-protocol"): string {
  return readFileSync(new URL(`../../../crates/${crate}/tests/golden/${name}.json`, import.meta.url), "utf8").trimEnd();
}

test("live upgrade body bytes match Rust golden vectors", () => {
  expect(Buffer.from(encodeModuleHello({ ...hello, module_capabilities: [LIVE_UPGRADE_CAPABILITY] })).toString())
    .toBe(golden("module_hello_body_with_module_capabilities"));
  expect(Buffer.from(encodeModuleHello({ ...hello, resume_token: token })).toString())
    .toBe(golden("module_hello_body_with_resume_token"));
  expect(JSON.stringify({ ...ack, resume_token: token } satisfies ModuleHelloAckBody))
    .toBe(golden("module_hello_ack_body_with_resume_token"));
  expect(JSON.stringify({ op: "module.upgrading", reconnect_by_ms: 1764000000000 } satisfies ModuleUpgrading))
    .toBe(golden("module_control_command_upgrading"));
  expect(JSON.stringify({ code: RESUME_REFUSED, message: "module resume refused" } satisfies ResumeRefusedBody))
    .toBe(golden("error_body_resume_refused"));
  expect(JSON.stringify({ code: "module_reloading", message: "daemon upgrade in progress", detail: { reason: "daemon_upgrade" } } satisfies DaemonUpgradeReloadingBody))
    .toBe(golden("error_body_module_reloading_for_daemon_upgrade"));
  expect(JSON.stringify({ code: "upgrade_failed", message: "daemon upgrade failed", detail: {
    announced: [], attempt_id: "00112233445566778899aabbccddeeff", restarted: ["worker"], stage: "planned_stop",
  } } satisfies UpgradeFailedBody)).toBe(golden("error_body_upgrade_failed", "subc-control"));
});

test("live upgrade optional HELLO and ACK members leave the old bytes unchanged", () => {
  expect(Buffer.from(encodeModuleHello({ ...hello, module_capabilities: [] })).toString())
    .toBe('{"manifest":{"module_id":"upgrade-worker","module_version":"1.0.0","protocol_ver":2,"provides":[]},"protocol_ver":2,"control_ops":null}');
  expect(JSON.stringify(ack)).toBe('{"negotiated_ver":2,"subc_ops":[],"subc_capabilities":[]}');
});

test("the published 0.26.8 decoder accepts a token and the newer provider registers without live upgrade", async () => {
  const peer = await UpgradePeer.start(false);
  // Keep the released copy's process-wide registration owner out of other tests.
  const entry = fileURLToPath(new URL("./fixtures/upgrade-mixed-version.ts", import.meta.url));
  const newerProviders = process.platform === "linux" || process.platform === "darwin" ? 2 : 0;
  const child = Bun.spawn([process.execPath, entry, peer.connectionFile, String(newerProviders)],
    { stdout: "pipe", stderr: "pipe" });
  const stdout = new Response(child.stdout).text();
  const stderr = new Response(child.stderr).text();
  try {
    const socket = await peer.nextTcp();
    expect((await readHello(socket)).module_capabilities).toBeUndefined();
    await peer.write(socket, buildFrame(FrameType.HelloAck, 0, 0, 0, 1n,
      Buffer.from(golden("module_hello_ack_body_with_resume_token"))));
    await peer.write(socket, buildFrame(FrameType.Ping, 0, 0, 0, 91n, new Uint8Array()));
    expect((await peer.read(socket)).header).toMatchObject({ ty: FrameType.Pong, corr: 91n });
    await peer.write(socket, frame(FrameType.Goodbye));

    for (let i = 0; i < newerProviders; i++) {
      const newer = await peer.nextTcp();
      const hello = await readHello(newer);
      expect(hello.module_capabilities).toBeUndefined();
      expect(hello.launch_nonce).toBe("fixture-nonce");
      await peer.write(newer, peer.ack(1n, { resume_token: token }), notice(), frame(FrameType.Goodbye));
    }
    expect(await child.exited).toBe(0);
    expect((await stdout).trim()).toBe(Array(newerProviders).fill("CLOSED").join("\n"));
    expect((await stderr).trim()).toBe(newerProviders === 0 ? ""
      : "SubcProvider live upgrade disabled: an older SDK copy owns the registration channel");
  } finally {
    if (child.exitCode === null) child.kill();
    await child.exited;
    await peer.stop();
  }
});

test("reconnect outcome has absent, success and error branches", () => {
  const outcomes = [undefined, { ok: true, reply: { generation: 1, results: [], ended: [] } },
    { ok: false, error: new Error("scope failed") }] satisfies ReconnectOutcome[];
  expect(outcomes.map((outcome) => outcome?.ok)).toEqual([undefined, true, false]);
});
