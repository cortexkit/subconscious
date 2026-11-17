import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import {
  buildFlags, buildFrame, decodeHeader, FrameType, HEADER_LEN, Priority,
  SubcProvider, SubcProviderError,
  type Frame, type ScopeRecord, type RemoteClaim,
} from "../src/index.js";

import { rejectionOf, throwerOf } from "./settle.js";
import { scopeRecordWire } from "../src/scope.js";

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
const federated: ScopeRecord = {
  scope_ref: "peer", scope_epoch: 1, kind: "federated",
  attributes: { federated: {
    peer_class: "computer", peer_fingerprint: "a".repeat(32), pairing_id: "pair:1", peer_name: "Peer",
    remote_claim: { kind: "flow", flow_id: "flow", flow_version: Number.MAX_SAFE_INTEGER, flow_install_id: "install", flow_code_hash: "b".repeat(64), flow_name: "Remote Flow" },
  } },
};
function withClaim(remote_claim: RemoteClaim | null | undefined): ScopeRecord {
  return { ...federated, attributes: { federated: { ...federated.attributes!.federated!, remote_claim } } };
}
const federatedNull = withClaim(null);
const federatedExtended = (claim: RemoteClaim): ScopeRecord => ({
  ...federated, attributes: { federated: {
    ...federated.attributes!.federated!, peer_class: "app", peer_box_id: "d".repeat(32),
    origin_principal: { kind: "reserved", module_id: "origin-module" }, project: { project_id: "project", root_key: { kind: "remote", key: "owner/repo" } }, remote_claim: claim,
  } },
});
const federatedClaimCases = [
  ["federated-agent", withClaim({ kind: "agent", agent_id: "remote-agent", agent_name: "Remote Agent" })],
  ["federated-worker", withClaim({ kind: "worker", owner_module_id: "worker-owner", worker_ref: "c".repeat(64), on_behalf_of: "remote-agent", agent_name: "Delegating Agent" })],
  ["federated-run", withClaim({ kind: "run", agent_id: "remote-agent", agent_name: "Remote Agent" })],
  ["federated-claimless", withClaim(undefined)],
  ["federated-claimless", federatedNull],
  ["federated-extended-run", federatedExtended({ kind: "run", agent_id: "remote-agent", run_id: "run-1" })],
  ["federated-extended-flow", federatedExtended({ kind: "flow", flow_id: "flow", flow_version: 1, flow_install_id: "install", flow_code_hash: "b".repeat(64), agent_id: "remote-agent" })],
] as const;
const defaulted: ScopeRecord = {
  ...minimal, workdir: null, project_id: null, bind_proof_sha256: null,
  replace_only_if_idle: false, expires_at_ms: null, parent: null, child_owners: [], carriers: [],
  attributes: {
    federated: null,
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
  test("local root keys are optional typed attribution requiring a project", () => {
    const encode = (patch: object) => JSON.parse(JSON.stringify(scopeRecordWire({ ...minimal, ...patch } as ScopeRecord)));
    const old = encode({});
    expect(old).not.toHaveProperty("root_key");
    expect(encode({ root_key: null })).toEqual(old);
    for (const kind of ["remote", "label"] as const) {
      for (const key of ["!", "owner/repo", "~".repeat(128)]) {
        expect(encode({ project_id: "project", root_key: { kind, key } })).toMatchObject({ project_id: "project", root_key: { kind, key } });
      }
    }
    for (const root_key of [
      "owner/repo", false, [], {}, { kind: "future", key: "root" },
      { kind: "remote" }, { key: "root" }, { kind: "label", key: "root", extra: true },
      ...["", "x".repeat(129), "has space", "valid\n", "é", "\u007f", null, 1, {}].map((key) => ({ kind: "remote", key })),
    ]) expect(() => encode({ project_id: "project", root_key })).toThrow(TypeError);
    for (const project_id of [undefined, null]) expect(() => encode({ project_id, root_key: { kind: "remote", key: "owner/repo" } })).toThrow("root_key requires project_id");
  });
  test("federated optional identity members preserve old senders and direct claimless origins", () => {
    const encode = (patch: object) => JSON.parse(JSON.stringify(scopeRecordWire({
      ...federated, attributes: { federated: { ...federated.attributes!.federated!, ...patch } },
    }))).attributes.federated;
    const old = encode({});
    expect(old).not.toHaveProperty("peer_box_id");
    expect(old).not.toHaveProperty("origin_principal");
    expect(old).not.toHaveProperty("project");
    expect(encode({ peer_box_id: null, origin_principal: null, project: null })).toEqual(old);
    for (const value of ["!", "~".repeat(256)]) expect(encode({ project: { project_id: value, root_key: { kind: "label", key: "root" } } }).project.project_id).toBe(value);
    for (const value of ["", "x".repeat(257), "has space", "\n", "valid\n", "é", "\u007f", 1, {}]) expect(() => encode({ project: { project_id: value, root_key: { kind: "remote", key: "owner/repo" } } })).toThrow(TypeError);
    for (const value of ["!", "~".repeat(128)]) expect(encode({ project: { project_id: "project", root_key: { kind: "label", key: value } } }).project.root_key.key).toBe(value);
    for (const value of ["", "x".repeat(129), "has space", "\n", "valid\n", "é", "\u007f", 1, {}]) expect(() => encode({ project: { project_id: "project", root_key: { kind: "remote", key: value } } })).toThrow(TypeError);
    for (const project of [false, [], {}, { project_id: "project" }, { root_key: { kind: "label", key: "root" } }, { project_id: "project", root_key: { kind: "future", key: "root" } }, { project_id: "project", root_key: { kind: "label", key: "root", extra: true } }, { project_id: "project", root_key: { kind: "label", key: "root" }, extra: true }]) expect(() => encode({ project })).toThrow(TypeError);
    expect(encode({ origin_principal: { kind: "direct" }, remote_claim: null })).toEqual({
      peer_class: "computer", peer_fingerprint: "a".repeat(32), pairing_id: "pair:1", peer_name: "Peer", origin_principal: { kind: "direct" },
    });
    for (const [claim, key] of [
      [{ kind: "run", agent_id: "agent" }, "run_id"],
      [{ kind: "flow", flow_id: "flow", flow_version: 1, flow_install_id: "install", flow_code_hash: "a".repeat(64) }, "agent_id"],
    ] as const) {
      expect(encode({ remote_claim: claim }).remote_claim).not.toHaveProperty(key);
      expect(encode({ remote_claim: { ...claim, [key]: null } }).remote_claim).toEqual(claim);
      for (const value of ["!", "~".repeat(256)]) expect(encode({ remote_claim: { ...claim, [key]: value } }).remote_claim[key]).toBe(value);
      for (const value of ["", "x".repeat(257), "has space", "\n", "valid\n", "é", "\u007f", 1, {}]) {
        expect(() => encode({ remote_claim: { ...claim, [key]: value } })).toThrow(TypeError);
      }
    }
  });
  test("federated new identity members validate machine ids and origin principals strictly", () => {
    const encode = (patch: object) => scopeRecordWire({
      ...federated, attributes: { federated: { ...federated.attributes!.federated!, ...patch } },
    } as ScopeRecord);
    expect(encode({ peer_class: "app", peer_box_id: "0123456789abcdef".repeat(2) })).toMatchObject({ attributes: { federated: { peer_class: "app", peer_box_id: "0123456789abcdef".repeat(2) } } });
    for (const value of ["", "a".repeat(31), "a".repeat(33), "A".repeat(32), "g".repeat(32), `${"a".repeat(32)}\n`, 1, {}]) expect(() => encode({ peer_box_id: value })).toThrow(TypeError);
    for (const module_id of ["x".repeat(255), "é".repeat(127), "mcp:module"]) expect(() => encode({ origin_principal: { kind: "reserved", module_id } })).not.toThrow();
    for (const module_id of ["", ".", "..", "a/b", "a\\b", "\n", "x".repeat(256), "é".repeat(128), "\ud800"]) expect(() => encode({ origin_principal: { kind: "reserved", module_id } })).toThrow(TypeError);
    for (const origin_principal of [{ kind: "unverified" }, { kind: "future" }, { kind: "direct", extra: true }, { kind: "reserved", module_id: "origin", extra: true }, false, 1, "direct", []]) expect(() => encode({ origin_principal })).toThrow(TypeError);
    for (const patch of [
      { peer_class: "future" }, { extra: true },
      { remote_claim: { kind: "agent", agent_id: "agent", run_id: "run" } },
      { remote_claim: { kind: "run", agent_id: "agent", extra: true } },
      { remote_claim: { kind: "flow", flow_id: "flow", flow_version: 1, flow_install_id: "install", flow_code_hash: "a".repeat(64), run_id: "run" } },
    ]) expect(() => encode(patch)).toThrow(TypeError);
  });
  test("federated names are optional and invalid display names are dropped on every claim kind", () => {
    for (const claim of [
      { kind: "agent", agent_id: "agent" }, { kind: "run", agent_id: "agent" },
      { kind: "worker", owner_module_id: "owner", worker_ref: "a".repeat(64), on_behalf_of: "agent" },
      { kind: "flow", flow_id: "flow", flow_version: 1, flow_install_id: "install", flow_code_hash: "a".repeat(64) },
    ] as const) {
      const key = claim.kind === "flow" ? "flow_name" : "agent_name";
      const encoded = (name: unknown) => JSON.parse(JSON.stringify(scopeRecordWire(withClaim({ ...claim, [key]: name } as RemoteClaim)))).attributes.federated.remote_claim;
      for (const name of ["Name", "é".repeat(80), "🦀".repeat(80)]) expect(encoded(name)).toEqual({ ...claim, [key]: name });
      for (const name of [undefined, null, "", "x".repeat(81), "\n", "\u007f", "\u0085", "\u061c", "\u200e", "\u202e", "\u2069", "\ud800", 7, {}]) expect(encoded(name)).toEqual(claim);
    }
  });
  for (const remote_claim of [false, 0, "", true, 1, "agent", []]) {
    test(`federated refuses non-object remote_claim ${JSON.stringify(remote_claim)}`, () => {
      const record = { ...federated, attributes: { federated: { ...federated.attributes!.federated!, remote_claim } } } as unknown as ScopeRecord;
      expect(() => scopeRecordWire(record)).toThrow(TypeError);
    });
  }
  test("federated inputs keep strict nested fields and safe flow versions", () => {
    expect(scopeRecordWire(federated)).toMatchObject({ attributes: { federated: { remote_claim: { kind: "flow", flow_version: Number.MAX_SAFE_INTEGER } } } });
    for (const patch of [
      { unexpected: true },
      { remote_claim: { ...federated.attributes!.federated!.remote_claim, unexpected: true } },
      { remote_claim: { kind: "future" } },
      ...[0, Number.MAX_SAFE_INTEGER + 1].map((flow_version) => ({ remote_claim: { kind: "flow", flow_id: "flow", flow_version, flow_install_id: "install", flow_code_hash: "a".repeat(64) } })),
    ]) {
      const record = { ...federated, attributes: { federated: { ...federated.attributes!.federated, ...patch } } } as ScopeRecord;
      expect(() => scopeRecordWire(record)).toThrow();
    }
    expect(scopeRecordWire({ ...minimal, kind: "future-kind" })).toMatchObject({ kind: "future-kind" });
  });
  for (const [name, record] of [["full", full], ["minimal", minimal], ["minimal", defaulted], ["federated", federated], ...federatedClaimCases] as const) {
    for (const op of ["sync", "apply"] as const) {
      test(`${op} ${name}${record === defaulted ? " defaults" : record === federatedNull ? " null" : ""} matches Rust request bytes`, async () => {
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
