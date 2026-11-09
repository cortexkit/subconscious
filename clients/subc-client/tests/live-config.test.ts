import { expect, test } from "bun:test";
import { spawn, type ChildProcess } from "node:child_process";
import { copyFileSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { authenticateClient, buildFlags, buildFrame, encodeFrame, FrameType, Priority,
  readConnectionFileWithLocalEndpoint, SubcClient, SubcSocket } from "../src/index.js";
import { ROOT, waitFor } from "./live-daemon.js";

const LIVE = process.env.RUN_SUBC_CONFIG_LIVE === "1";
type Entry = { state?: { state: string; view?: { generation: number }; availability?: string }; current?: { limit: number }; started?: boolean; error?: string };

test.skipIf(!LIVE)("config SDK sample adopts within 60 lines and explicitly refreshes unstamped files against an isolated real daemon", async () => {
  const root = mkdtempSync(join(tmpdir(), "ts-config-"));
  let child: ChildProcess | undefined;
  let sock: SubcSocket | undefined;
  let client: SubcClient | undefined;
  let stderr = "";
  try {
    const executable = join(root, "ckdev-subc");
    copyFileSync(join(ROOT, "target/debug/ck-subc"), executable);
    for (const name of ["runtime", "config/cortexkit", "data", "home"]) mkdirSync(join(root, name), { recursive: true });
    const connection = join(root, "runtime/subc-connection.json");
    const configFile = join(root, "config/cortexkit/subc.jsonc");
    const modules: Record<string, object> = {};
    for (const [id, fixture, enabled] of [["sample", "config-sample.ts", true], ["legacy", "config-sample-legacy.ts", true]] as const) {
      writeFileSync(join(root, `${id}.json`), '{"limit":10}');
      modules[id] = { program: process.execPath, args: [join(import.meta.dir, "fixtures", fixture)], enabled, reserved: true,
        env: { SAMPLE_FILE: join(root, `${id}.json`), SAMPLE_EVENTS: join(root, `${id}.jsonl`), SUBC_CONNECTION_FILE: connection } };
    }
    writeFileSync(configFile, JSON.stringify({ version: 1, modules }));
    child = spawn(executable, ["--config-store-root", join(root, "store")], { cwd: root, env: {
      ...process.env, XDG_DATA_HOME: join(root, "data"), XDG_RUNTIME_DIR: join(root, "runtime"),
      XDG_CONFIG_HOME: join(root, "config"), HOME: join(root, "home"), USERPROFILE: join(root, "home"),
      LOCALAPPDATA: join(root, "data"), SUBC_CONNECTION_FILE: connection, SUBC_PORT: "0",
      SUBC_CGROUP_PLACEMENT: "disabled", SUBC_TEST_OPERATOR_SCRIPT: undefined,
      SUBC_MODULE_ID: undefined, SUBC_LAUNCH_NONCE: undefined, SUBC_LAUNCH_NONCE_FD: undefined,
      DBUS_SESSION_BUS_ADDRESS: undefined,
    }, stdio: ["ignore", "ignore", "pipe"] });
    child.stderr?.on("data", (chunk) => { stderr += String(chunk); });
    await waitFor(() => existsSync(connection) || child?.exitCode !== null, 10000, "isolated config daemon connection");
    if (!existsSync(connection)) throw new Error("daemon exited before publishing its connection file");
    const info = await readConnectionFileWithLocalEndpoint(connection);
    if (info.localEndpoint?.kind !== "unix") throw new Error("config live test requires Linux Unix socket");
    sock = await SubcSocket.connectLocal(info.localEndpoint.path, Date.now() + 5000);
    await authenticateClient(sock, info, Date.now() + 5000);
    let corr = 0n;
    const rpc = async (body: object): Promise<any> => {
      const id = ++corr;
      await sock!.write(encodeFrame(buildFrame(FrameType.Request, buildFlags(false, Priority.Interactive, false),
        0, 0, id, Buffer.from(JSON.stringify(body)))), Date.now() + 5000);
      const frame = await sock!.readFrame(Date.now() + 10000, Date.now() + 10000);
      const reply = JSON.parse(Buffer.from(frame.body).toString());
      if (frame.header.ty === FrameType.Error) throw new Error(JSON.stringify(reply));
      expect(frame.header.corr).toBe(id);
      return reply;
    };
    const entries = (id: string): Entry[] => existsSync(join(root, `${id}.jsonl`))
      ? readFileSync(join(root, `${id}.jsonl`), "utf8").trim().split("\n").map((line) => JSON.parse(line)) : [];
    const has = (id: string, state: string, limit?: number) => entries(id).some((e) => e.state?.state === state && (limit === undefined || e.current?.limit === limit));
    await waitFor(() => has("sample", "unstamped", 10) && entries("legacy").some((e) => e.started && e.current?.limit === 10), 10000, "opted and unchanged samples");
    writeFileSync(join(root, "sample.json"), '{"limit":20}');
    const status = await rpc({ op: "config.status" });
    expect(status.status.state).toBe("unstamped");
    client = await SubcClient.connect({ connectionFile: connection,
      identity: { project_root: root, harness: "opencode", session: "sample-refresh" } });
    expect(await client.call<{ limit: number }>("sample", "read", {})).toEqual({ limit: 20 });
    await waitFor(() => has("sample", "unstamped", 20), 10000, "explicit fetch and local apply ACK");
    expect(entries("legacy").filter((e) => e.started)).toHaveLength(1);
    expect(entries("sample").filter((e) => e.error)).toEqual([]);
    const before = readFileSync(join(import.meta.dir, "fixtures/config-sample-legacy.ts"), "utf8").split("\n");
    const after = readFileSync(join(import.meta.dir, "fixtures/config-sample.ts"), "utf8").split("\n");
    // Count additions plus removals using the longest common subsequence, like a line diff.
    const dp = Array.from({ length: before.length + 1 }, () => Array(after.length + 1).fill(0) as number[]);
    for (let i = 1; i <= before.length; i++) for (let j = 1; j <= after.length; j++) {
      dp[i]![j] = before[i - 1] === after[j - 1] ? dp[i - 1]![j - 1]! + 1 : Math.max(dp[i - 1]![j]!, dp[i]![j - 1]!);
    }
    expect(before.length + after.length - 2 * dp[before.length]![after.length]!).toBeLessThanOrEqual(60);
  } catch (error) { throw new Error(`${String(error)}\nIsolated daemon stderr:\n${stderr}`); }
  finally {
    client?.close();
    sock?.close();
    if (child && child.exitCode === null && child.signalCode === null) {
      const exited = new Promise((resolve) => child!.once("exit", resolve));
      child.kill("SIGTERM");
      await exited;
    }
    rmSync(root, { recursive: true, force: true });
  }
}, 60000);
