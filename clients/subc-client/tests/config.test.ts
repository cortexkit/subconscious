import { afterEach, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { throwerOf } from "./settle.js";
import { ModuleConfig, configLayerBytes, type ConfigView, type ModuleConfigOptions, type ModuleConfigState } from "../src/index.js";

const sessions: ModuleConfig[] = [];
afterEach(() => { for (const session of sessions.splice(0)) session.close(); });
const sampleView = (generation: number, status = "pending"): ConfigView => ({
  generation, attempt: generation + 10, effective: { limit: generation }, sources: { "/limit": "user" }, layers: [], status,
});
const refusal = (code: string) => Object.assign(new Error(`daemon refused: ${code}`), { code });

function rig(options: Partial<ModuleConfigOptions> = {}) {
  let epoch = 1;
  let capabilities = ["config.gate"];
  let response: { active: ConfigView | null; pending?: ConfigView } = { active: null, pending: sampleView(1) };
  let fail: { op: string; code: string } | undefined;
  const calls: Array<Record<string, unknown>> = [], applied: ConfigView[] = [], errors: unknown[] = [], states: ModuleConfigState[] = [];
  let filesRead = 0;
  const config = new ModuleConfig({
    capabilities: () => capabilities, epoch: () => epoch,
    request: async (body) => {
      const request = body as Record<string, unknown>;
      calls.push(request);
      if (fail && fail.op === request.op) throw refusal(fail.code);
      return request.op === "config.get" ? { op: "config.get", ...response } : { op: request.op };
    },
  }, {
    declaration: { schema: {}, user_file: "/isolated/config.json", project_dirs: [] },
    review: () => ({ op: "config.review", fields: [], refusals: [] }),
    apply: (view) => { applied.push(view); return { result: "ack" }; },
    readFiles: () => { filesRead++; return sampleView(0, "unstamped"); },
    onState: (state) => states.push(state), onError: (error) => errors.push(error),
    ...options,
  });
  sessions.push(config);
  return { config, calls, applied, errors, states, filesRead: () => filesRead,
    reply: (active: ConfigView | null, pending?: ConfigView) => { response = { active, pending }; },
    fail: (op: string, code: string) => { fail = { op, code }; }, recover: () => { fail = undefined; },
    reconnect: () => { epoch++; }, legacy: () => { capabilities = []; } };
}

test("config declares once, applies pending N+1 rather than active N, and ACKs its exact attempt", async () => {
  const r = rig();
  await r.config.refresh();
  r.reply(sampleView(1, "active"), sampleView(2));
  await r.config.refresh();
  expect(r.calls.filter((c) => c.op === "config.declare")).toHaveLength(1);
  expect(r.applied.map((v) => v.generation)).toEqual([1, 2]);
  expect(r.calls.at(-1)).toEqual({ op: "config.applied", generation: 2, attempt: 12, result: "ack", reasons: [], project_id: undefined });
  expect(r.config.state?.state).toBe("active");
  expect(r.filesRead()).toBe(0);
});

for (const op of ["config.declare", "config.get", "config.applied"]) {
  test(`config_gated from ${op} yields typed availability without reading files or stopping`, async () => {
    const r = rig();
    // A first applied refusal retains the bytes already successfully applied locally.
    r.fail(op, "config_gated");
    await r.config.refresh();
    expect(r.config.state?.state).toBe("config_gated");
    if (r.config.state?.state === "config_gated") {
      expect(r.config.state.availability).toBe(op === "config.applied" ? "last_view" : "no_approved_config");
      expect(r.config.state.refusal.code).toBe("config_gated");
      if (op !== "config.applied") expect("view" in r.config.state).toBe(false);
    }
    expect(r.filesRead()).toBe(0);
    expect(r.errors).toEqual([]);
    r.recover();
    await r.config.refresh();
    expect(r.config.state?.state).toBe("active");
    expect(r.calls.filter((c) => c.op === "config.declare")).toHaveLength(1);
  });
}

test("config_gated after a successful view retains it for declare/get/applied", async () => {
  for (const op of ["config.declare", "config.get", "config.applied"]) {
    const r = rig();
    await r.config.refresh();
    if (op === "config.declare") r.reconnect();
    if (op === "config.applied") r.reply(sampleView(1, "active"), sampleView(2));
    r.fail(op, "config_gated");
    await r.config.refresh();
    expect(r.config.state?.state).toBe("config_gated");
    if (r.config.state?.state === "config_gated" && r.config.state.availability === "last_view") {
      expect(r.config.state.view.generation).toBe(op === "config.applied" ? 2 : 1);
    } else throw new Error("lost last view");
    expect(r.filesRead()).toBe(0);
  }
});

test("a successful get clears a gated refusal even when its bytes match the retained view", async () => {
  const r = rig();
  r.reply({ ...sampleView(1, "active"), attempt: undefined });
  await r.config.refresh();
  r.fail("config.get", "config_gated");
  await r.config.refresh();
  expect(r.config.state?.state).toBe("config_gated");
  r.recover();
  await r.config.refresh();
  expect(r.config.state?.state).toBe("active");
  expect(r.applied).toHaveLength(1);
});

test("immutable views prevent an apply callback from altering the ACK binding", async () => {
  const errors: unknown[] = [];
  const r = rig({ apply: (value) => { value.generation = 999; return { result: "ack" }; }, onError: (error) => errors.push(error) });
  await r.config.refresh();
  expect(errors).toHaveLength(1);
  expect(r.calls.some((c) => c.op === "config.applied")).toBe(false);
});

test("a pending view without an assigned attempt cannot be applied", async () => {
  const r = rig();
  r.reply(null, { ...sampleView(1), attempt: undefined });
  await r.config.refresh();
  expect(r.applied).toEqual([]);
  expect(r.errors).toHaveLength(1);
});

test("review refuses unknown layers rather than passing them to a merger", async () => {
  let reviews = 0;
  const r = rig({ review: () => { reviews++; return { op: "config.review", fields: [], refusals: [] }; } });
  expect(await throwerOf(r.config.review({ op: "config.review", base_generation: 0, base: null,
    layers: [{ module_id: "sample", layer: "future_layer", path: "/isolated/config.json" }] }))).toThrow("unknown");
  expect(reviews).toBe(0);
  expect(r.config.state).toEqual({ state: "unknown", rawState: "future_layer", view: undefined });
});

test("not_delivery_target keeps the view without gating or retrying until config.changed refresh", async () => {
  const r = rig();
  r.fail("config.applied", "not_delivery_target");
  await r.config.refresh();
  expect(r.config.state?.state).toBe("not_delivery_target");
  expect(r.states.some((state) => state.state === "config_gated")).toBe(false);
  expect(r.calls.filter((c) => c.op === "config.applied")).toHaveLength(1);
  r.recover();
  await r.config.refresh();
  expect(r.config.state?.state).toBe("active");
  expect(r.applied).toHaveLength(1);
});

test("anchor_pending retains applied bytes and retries the identical outcome after bounded backoff", async () => {
  const r = rig({ anchorRetryMs: 10 });
  r.fail("config.applied", "anchor_pending");
  await r.config.refresh();
  expect(r.config.state?.state).toBe("anchor_pending");
  r.recover();
  const deadline = Date.now() + 1000;
  while (r.config.state?.state !== "active" && Date.now() < deadline) await Bun.sleep(5);
  expect(r.config.state?.state).toBe("active");
  const reports = r.calls.filter((c) => c.op === "config.applied");
  expect(reports).toHaveLength(2);
  expect(reports[1]).toEqual(reports[0]);
  expect(r.applied).toHaveLength(1);
});

test("NACK keeps N and reports N+1, including anchor_pending retry on a hint", async () => {
  let bad = false;
  const r = rig({ apply: () => ({ result: bad ? "nack" : "ack", reasons: bad ? ["not ready"] : [] }) });
  await r.config.refresh();
  bad = true;
  r.reply(sampleView(1, "active"), sampleView(2));
  r.fail("config.applied", "anchor_pending");
  await r.config.refresh();
  expect(r.config.state?.state).toBe("anchor_pending");
  if (r.config.state?.state === "anchor_pending") expect(r.config.state.view?.generation).toBe(1);
  r.recover();
  r.reply(sampleView(1));
  await r.config.refresh();
  expect(r.calls.filter((c) => c.op === "config.applied").at(-1)?.result).toBe("nack");
});

test("fresh config establishes active recovery before a NACKed pending delivery", async () => {
  const applied: number[] = [];
  const r = rig({ apply: value => {
    applied.push(value.generation);
    return { result: value.generation === 1 ? "ack" : "nack", reasons: ["not ready"] };
  } });
  r.reply(sampleView(1, "active"), sampleView(2));
  await r.config.refresh();
  expect(applied).toEqual([1, 2]);
  expect(r.config.state).toEqual({ state: "failed", view: sampleView(1, "active"), reasons: ["not ready"] });
  expect(r.calls.filter(c => c.op === "config.applied")).toEqual([
    { op: "config.applied", generation: 2, attempt: 12, project_id: undefined, result: "nack", reasons: ["not ready"] },
  ]);
});

test("only missing config.gate enables legacy files; reconnect declares on the new connection", async () => {
  const r = rig();
  r.legacy();
  await r.config.refresh();
  expect(r.filesRead()).toBe(1);
  expect(r.calls).toEqual([]);
  expect(r.config.state?.state).toBe("unstamped");
  const gated = rig();
  await gated.config.refresh();
  gated.reconnect();
  await gated.config.refresh();
  expect(gated.calls.filter((c) => c.op === "config.declare")).toHaveLength(2);
});

test("unknown status and layer retain raw strings and cannot activate", async () => {
  for (const target of [sampleView(2, "future_status"), { ...sampleView(2), sources: { "/limit": "future_layer" } }]) {
    const r = rig();
    await r.config.refresh();
    r.reply(sampleView(1, "active"), target);
    await r.config.refresh();
    expect(r.config.state?.state).toBe("unknown");
    if (r.config.state?.state === "unknown") expect(r.config.state.rawState).toBe(target.status === "future_status" ? "future_status" : "future_layer");
    expect(r.applied).toHaveLength(1);
  }
});

test("Rust golden config.get snapshots decode exact bytes and reject corruption", () => {
  const golden = JSON.parse(readFileSync(join(import.meta.dir, "../../../crates/subc-protocol/tests/golden/config_gate/config_get_reply_pending.json"), "utf8"));
  const layer = golden.pending.layers[0];
  expect(Buffer.from(configLayerBytes(layer)!).toString()).toBe('{"limit":20}\r\n');
  expect(() => configLayerBytes({ ...layer, bytes: "e30=" })).toThrow("bytes/digest");
  expect(configLayerBytes({ ...layer, bytes: undefined, digest: undefined })).toBeUndefined();
});
