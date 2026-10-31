import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import {
  buildFlags, buildFrame, decodeHeader, FrameType, HEADER_LEN, Priority,
  SubcProvider, SubcProviderError,
  type Frame, type ScopeRecord,
} from "../src/index.js";

import { rejectionOf, throwerOf } from "./settle.js";

const FLAGS = buildFlags(false, Priority.Passive, false);

// The full fixture exercises serialization, not the daemon's semantic admission rules.
const full: ScopeRecord = {
  scope_ref: "session-full", scope_epoch: 12, kind: "worker",
  workdir: "/rig/work", project_id: "rig", bind_proof_sha256: "a".repeat(64),
  replace_only_if_idle: true, expires_at_ms: 1_900_000_000_000,
  parent: { owner: { kind: "reserved", module_id: "scope-parent" }, scope_ref: "head", scope_epoch: 4 },
  child_owners: [{ kind: "reserved", module_id: "scope-child" }, { kind: "direct" }],
  carriers: [
    { principal: { kind: "reserved", module_id: "test-client" }, targets: ["test-target"] },
    { principal: { kind: "unverified" } },
  ],
  attributes: {
    agent_id: "agent", on_behalf_of_agent: "attribution", delegates: true,
    flow_id: "flow", flow_version: 3, flow_code_hash: "b".repeat(64), flow_install_id: "install", run_id: "run",
  },
};
const minimal: ScopeRecord = { scope_ref: "session-minimal", scope_epoch: 0, kind: "head" };
const defaulted: ScopeRecord = {
  ...minimal, workdir: null, project_id: null, bind_proof_sha256: null,
  replace_only_if_idle: false, expires_at_ms: null, parent: null, child_owners: [], carriers: [],
  attributes: {
    agent_id: null, on_behalf_of_agent: null, delegates: false,
    flow_id: null, flow_version: null, flow_code_hash: null, flow_install_id: null, run_id: null,
  },
};

type Internals = {
  dispatch(frame: Frame, sock: unknown, generation: number): Promise<boolean>;
  rejectPendingRequests(error: Error): void;
  pending: Map<string, unknown>;
};

function harness(ops = ["scope.sync", "scope.apply"]) {
  const writes: Frame[] = [];
  const sock = {
    async write(bytes: Uint8Array) {
      writes.push({ header: decodeHeader(bytes.subarray(0, HEADER_LEN)), body: bytes.slice(HEADER_LEN) });
    },
  };
  const provider = Object.create(SubcProvider.prototype) as SubcProvider;
  Object.assign(provider, {
    sock, generation: 1, nextCorr: 2n, subcOps: ops, reconnecting: null,
    closeStarted: false, closedErr: null, pending: new Map(), liveRoutes: new Map(),
  });
  const internals = provider as unknown as Internals;
  return {
    provider, writes, internals,
    reply(index: number, body: unknown, ty = FrameType.Response) {
      return internals.dispatch(buildFrame(ty, FLAGS, 0, 0, writes[index]!.header.corr,
        Buffer.from(JSON.stringify(body))), sock, 1);
    },
  };
}

async function sent(writes: Frame[], count = 1) {
  for (let i = 0; i < 100 && writes.length < count; i++) await Bun.sleep(1);
  expect(writes).toHaveLength(count);
}

describe("provider scope control", () => {
  for (const [name, record] of [["full", full], ["minimal", minimal], ["minimal", defaulted]] as const) {
    for (const op of ["sync", "apply"] as const) {
      test(`${op} ${name}${record === defaulted ? " defaults" : ""} matches Rust request bytes`, async () => {
        const h = harness();
        const result = op === "sync" ? h.provider.scopeSync(7, [record])
          : h.provider.scopeApply(8, [record], [{ scope_ref: "old", scope_epoch: 1 }]);
        await sent(h.writes);
        const frame = h.writes[0]!;
        expect(frame.header).toMatchObject({ ty: FrameType.Request, flags: FLAGS, channel: 0, epoch: 0, corr: 2n });
        expect(Buffer.from(frame.body)).toEqual(readFileSync(new URL(`./golden/scope-${op}-${name}.json`, import.meta.url)));
        await h.reply(0, { op: `scope.${op}`, generation: op === "sync" ? 7 : 8, results: [], ...(op === "apply" ? { end_results: [] } : {}) });
        expect(await result).toEqual({ generation: op === "sync" ? 7 : 8, results: [], ended: [], ...(op === "apply" ? { end_results: [] } : {}) });
        expect(h.internals.pending.size).toBe(0);
      });
    }
  }

  test("correlates overlapping calls, retaining per-record and end results", async () => {
    const h = harness();
    const sync = h.provider.scopeSync(1, []);
    const apply = h.provider.scopeApply(2, [], []);
    await sent(h.writes, 2);
    expect(h.writes.map((f) => f.header.corr)).toEqual([2n, 3n]);
    expect(Buffer.from(h.writes[1]!.body).toString()).toBe('{"op":"scope.apply","generation":2,"upsert":[],"end":[]}');
    const results = [{ ref: "s", scope_epoch: 1, outcome: "refused" as const, code: "scope_epoch_ended", message: "ended", version: 4, parent_state: "ended" as const }];
    const end_results = [{ ref: "old", scope_epoch: 1, outcome: "not_live" as const }];
    const ended = [{ ref: "expired", scope_epoch: 2 }];
    await h.reply(1, { op: "scope.apply", generation: 2, results, end_results, ended, future_reply_field: true });
    await h.reply(0, { op: "scope.sync", generation: 1, results: [] });
    expect(await apply).toEqual({ generation: 2, results, end_results, ended });
    expect((await sync).generation).toBe(1);
  });

  test("whole-call daemon refusals keep their typed code and detail", async () => {
    const h = harness();
    const result = h.provider.scopeSync(1, []);
    await sent(h.writes);
    await h.reply(0, { code: "not_registered", message: "module required", detail: { reason: "registration" } }, FrameType.Error);
    expect(await rejectionOf(result)).toBeInstanceOf(SubcProviderError);
    expect(await rejectionOf(result)).toMatchObject({ code: "not_registered", message: "module required", detail: { reason: "registration" } });
  });

  test("checks each advertised operation before sending", async () => {
    const h = harness(["scope.sync"]);
    expect(await rejectionOf(h.provider.scopeApply(1, [], []))).toMatchObject({ code: "not_supported", kind: "not_sent" });
    expect(h.writes).toHaveLength(0);
  });

  test("rejects a mismatched response operation", async () => {
    const h = harness();
    const result = h.provider.scopeSync(1, []);
    await sent(h.writes);
    await h.reply(0, { op: "scope.apply", generation: 1, results: [], end_results: [] });
    expect(await rejectionOf(result)).toMatchObject({ code: "protocol_error" });
  });

  test("connection loss rejects pending control calls and clears waiters", async () => {
    const h = harness();
    const result = h.provider.scopeSync(1, []);
    await sent(h.writes);
    const error = new SubcProviderError("connection lost", "connection_lost");
    h.internals.rejectPendingRequests(error);
    expect(await rejectionOf(result)).toBe(error);
    expect(h.internals.pending.size).toBe(0);
  });

  test("write failure clears the control waiter", async () => {
    const h = harness();
    Object.assign(h.provider, { sock: { write: async () => { throw new Error("write failed"); } } });
    expect(await throwerOf(h.provider.scopeSync(1, []))).toThrow("write failed");
    expect(h.internals.pending.size).toBe(0);
  });

  test("rejects unsafe counters without sending rounded JSON", async () => {
    const h = harness();
    expect(await rejectionOf(h.provider.scopeSync(Number.MAX_SAFE_INTEGER + 1, []))).toMatchObject({ code: "invalid_control_body" });
    expect(await rejectionOf(h.provider.scopeSync(1, [{ ...minimal, scope_epoch: -1 }]))).toMatchObject({ code: "invalid_control_body" });
    expect(h.writes).toHaveLength(0);
  });

  for (const record of [
    { ...minimal, future: true },
    { ...minimal, attributes: { future: true } },
    { ...full, parent: { ...full.parent, future: true } },
    { ...minimal, carriers: [{ principal: { kind: "direct" }, future: true }] },
    { ...minimal, carriers: [{ principal: { kind: "direct", future: true } }] },
    { ...minimal, child_owners: [{ kind: "direct", future: true }] },
  ]) {
    test(`rejects unknown nested input ${JSON.stringify(record)}`, async () => {
      const h = harness();
      expect(await rejectionOf(h.provider.scopeSync(1, [record as ScopeRecord]))).toMatchObject({ code: "invalid_control_body" });
      expect(h.writes).toHaveLength(0);
    });
  }

  test("rejects unknown end fields", async () => {
    const h = harness();
    expect(await rejectionOf(h.provider.scopeApply(1, [], [{ scope_ref: "s", scope_epoch: 1, future: true } as never]))).toMatchObject({ code: "invalid_control_body" });
    expect(h.writes).toHaveLength(0);
  });
});
