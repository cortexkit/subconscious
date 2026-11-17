import type { Principal } from "./provider.js";

/** Names are display-only, never identity. Invalid names are omitted on the wire. */
export type RemoteClaim =
  | { kind: "agent"; agent_id: string; agent_name?: string }
  | { kind: "run"; agent_id: string; run_id?: string | null; agent_name?: string }
  | { kind: "worker"; owner_module_id: string; worker_ref: string; on_behalf_of?: string | null; agent_name?: string }
  | { kind: "flow"; flow_id: string; flow_version: number; flow_install_id: string; flow_code_hash: string; agent_id?: string | null; flow_name?: string };

/** Repository root identity; remote names and operator labels are distinct namespaces. */
export interface RootKey {
  kind: "remote" | "label";
  key: string;
}

/** Portable folder identity from the sending scope, resolved by the receiver's own registry, never a remote path. */
export interface FederatedProject {
  project_id: string;
  root_key: RootKey;
}

export interface FederatedAttributes {
  peer_class: "computer" | "app" | "phone" | "sim" | "unknown";
  peer_fingerprint: string;
  /** Stable paired computer machine id; survives re-keying, unlike the fingerprint. */
  peer_box_id?: string | null;
  pairing_id: string;
  peer_name: string;
  /** Sending daemon's route principal, carried by the pairing module, even without a claim. */
  origin_principal?: Principal | null;
  project?: FederatedProject | null;
  remote_claim?: RemoteClaim | null;
}

/** Local attribution is authority-owner gated; federated facts use a separate owner. */
export interface ScopeAttributes {
  federated?: FederatedAttributes | null;
  agent_id?: string | null;
  on_behalf_of_agent?: string | null;
  delegates?: boolean;
  flow_id?: string | null;
  flow_version?: number | null;
  flow_code_hash?: string | null;
  flow_install_id?: string | null;
  run_id?: string | null;
}

export interface ScopeParent {
  owner: Principal;
  /** Serialized as ref. */
  scope_ref: string;
  scope_epoch: number;
}

export interface ScopeCarrier {
  principal: Principal;
  /** Absent means any target; an empty list is refused by the daemon. */
  targets?: readonly string[] | null;
}

/**
 * An owner-supplied scope, matching the Rust ScopeRecord. scope_ref is serialized
 * as ref. Epochs, timestamps and versions must be safe non-negative integers.
 * Unknown fields in records and their nested inputs are rejected, not stripped.
 * The daemon derives ownership from the registered connection, never this record.
 */
export interface ScopeRecord {
  scope_ref: string;
  scope_epoch: number;
  kind: "head" | "worker" | "ephemeral" | "federated" | (string & {});
  workdir?: string | null;
  project_id?: string | null;
  /** Project-registry root attribution, fixed per epoch and requiring project_id. */
  root_key?: RootKey | null;
  bind_proof_sha256?: string | null;
  replace_only_if_idle?: boolean;
  expires_at_ms?: number | null;
  parent?: ScopeParent | null;
  child_owners?: readonly Principal[];
  carriers?: readonly ScopeCarrier[];
  /** agent_id, delegates, flow and run attribution require scope_authority_owners. */
  attributes?: ScopeAttributes;
}

export interface ScopeEnd {
  /** Serialized as ref. */
  scope_ref: string;
  scope_epoch: number;
}

/** Reply entries retain the daemon's wire field names, including ref. */
export interface ScopeRecordResult {
  ref: string;
  scope_epoch: number;
  outcome: "created" | "replaced" | "updated" | "unchanged" | "refused";
  code?: string;
  message?: string;
  version?: number;
  parent_state?: "linked" | "pending" | "ended";
}

export interface ScopeEndResult {
  ref: string;
  scope_epoch: number;
  outcome: "ended" | "not_live";
}

export interface ScopeEnded {
  ref: string;
  scope_epoch: number;
}

export interface ScopeSyncReply {
  generation: number;
  results: ScopeRecordResult[];
  /** Empty when the daemon omits ended. */
  ended: ScopeEnded[];
}

export interface ScopeApplyReply extends ScopeSyncReply {
  end_results: ScopeEndResult[];
}

function fields(value: object, allowed: readonly string[]): void {
  for (const key of Object.keys(value)) {
    if (!allowed.includes(key)) throw new TypeError(`unknown scope field: ${key}`);
  }
}

export function scopeInteger(value: number): number {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new TypeError("scope integers must be safe non-negative integers");
  }
  return value;
}

function principal(value: Principal): Principal {
  fields(value, value.kind === "reserved" ? ["kind", "module_id"] : ["kind"]);
  return value.kind === "reserved" ? { kind: value.kind, module_id: value.module_id } : { kind: value.kind };
}

/** Build in Rust serde declaration order, with the same default omissions. */
export function scopeRecordWire(record: ScopeRecord): object {
  fields(record, ["scope_ref", "scope_epoch", "kind", "workdir", "project_id", "root_key", "bind_proof_sha256", "replace_only_if_idle", "expires_at_ms", "parent", "child_owners", "carriers", "attributes"]);
  if (record.root_key != null && record.project_id == null) throw new TypeError("root_key requires project_id");
  const attributes = record.attributes ?? {};
  fields(attributes, ["federated", "agent_id", "on_behalf_of_agent", "delegates", "flow_id", "flow_version", "flow_code_hash", "flow_install_id", "run_id"]);
  const wireAttributes = {
    federated: attributes.federated == null ? undefined : federatedWire(attributes.federated),
    agent_id: attributes.agent_id ?? undefined,
    on_behalf_of_agent: attributes.on_behalf_of_agent ?? undefined,
    delegates: attributes.delegates || undefined,
    flow_id: attributes.flow_id ?? undefined,
    flow_version: attributes.flow_version == null ? undefined : scopeInteger(attributes.flow_version),
    flow_code_hash: attributes.flow_code_hash ?? undefined,
    flow_install_id: attributes.flow_install_id ?? undefined,
    run_id: attributes.run_id ?? undefined,
  };
  const parent = record.parent;
  if (parent) fields(parent, ["owner", "scope_ref", "scope_epoch"]);
  return {
    ref: record.scope_ref,
    scope_epoch: scopeInteger(record.scope_epoch),
    kind: record.kind,
    workdir: record.workdir ?? undefined,
    project_id: record.project_id ?? undefined,
    root_key: record.root_key == null ? undefined : rootKeyWire(record.root_key),
    bind_proof_sha256: record.bind_proof_sha256 ?? undefined,
    replace_only_if_idle: record.replace_only_if_idle || undefined,
    expires_at_ms: record.expires_at_ms == null ? undefined : scopeInteger(record.expires_at_ms),
    parent: parent ? { owner: principal(parent.owner), ref: parent.scope_ref, scope_epoch: scopeInteger(parent.scope_epoch) } : undefined,
    child_owners: record.child_owners?.length ? record.child_owners.map(principal) : undefined,
    carriers: record.carriers?.length ? record.carriers.map((carrier) => {
      fields(carrier, ["principal", "targets"]);
      return { principal: principal(carrier.principal), targets: carrier.targets ?? undefined };
    }) : undefined,
    attributes: Object.values(wireAttributes).some((value) => value !== undefined) ? wireAttributes : undefined,
  };
}

// Match peer_name's Unicode scalar count and control/bidi exclusions in Rust.
function displayName(value: unknown): string | undefined {
  if (typeof value !== "string" || [...value].length < 1 || [...value].length > 80
    || /[\p{Cc}\u061c\u200e\u200f\u202a-\u202e\u2066-\u2069\uD800-\uDFFF]/u.test(value)) return undefined;
  return value;
}

function federatedWire(facts: FederatedAttributes): object {
  fields(facts, ["peer_class", "peer_fingerprint", "peer_box_id", "pairing_id", "peer_name", "origin_principal", "project", "remote_claim"]);
  let project: FederatedProject | undefined;
  if (facts.project != null) {
    if (typeof facts.project !== "object" || Array.isArray(facts.project)) throw new TypeError("project must be an object");
    fields(facts.project, ["project_id", "root_key"]);
    project = { project_id: identityToken("project_id", facts.project.project_id), root_key: rootKeyWire(facts.project.root_key) };
  }
  if (!["computer", "app", "phone", "sim", "unknown"].includes(facts.peer_class)) throw new TypeError("unknown peer class");
  if (facts.peer_box_id != null && (typeof facts.peer_box_id !== "string" || facts.peer_box_id.length !== 32 || /[^0-9a-f]/.test(facts.peer_box_id))) throw new TypeError("peer_box_id must contain exactly 32 lowercase hex characters");
  let origin_principal: Principal | undefined;
  if (facts.origin_principal != null) {
    const origin = facts.origin_principal;
    if (typeof origin !== "object" || Array.isArray(origin)) throw new TypeError("origin_principal must be an object");
    fields(origin, origin.kind === "reserved" ? ["kind", "module_id"] : ["kind"]);
    if (origin.kind === "reserved") {
      const id = origin.module_id;
      if (typeof id !== "string" || !id.length || Buffer.byteLength(id) > 255 || id === "." || id === ".." || /[/\\\p{Cc}\uD800-\uDFFF]/u.test(id)) throw new TypeError("invalid origin_principal module_id");
      origin_principal = { kind: "reserved", module_id: id };
    } else if (origin.kind === "direct") {
      origin_principal = { kind: "direct" };
    } else throw new TypeError("origin_principal must be reserved or direct");
  }
  const claim = facts.remote_claim;
  let remote_claim: object | undefined;
  if (claim !== undefined && claim !== null) {
    if (typeof claim !== "object" || Array.isArray(claim)) throw new TypeError("remote_claim must be an object");
    switch (claim.kind) {
      case "agent":
        fields(claim, ["kind", "agent_id", "agent_name"]);
        remote_claim = { kind: claim.kind, agent_id: claim.agent_id, agent_name: displayName(claim.agent_name) };
        break;
      case "run":
        fields(claim, ["kind", "agent_id", "run_id", "agent_name"]);
        remote_claim = { kind: claim.kind, agent_id: claim.agent_id, run_id: optionalIdentityToken("run_id", claim.run_id), agent_name: displayName(claim.agent_name) };
        break;
      case "worker":
        fields(claim, ["kind", "owner_module_id", "worker_ref", "on_behalf_of", "agent_name"]);
        remote_claim = { kind: claim.kind, owner_module_id: claim.owner_module_id, worker_ref: claim.worker_ref, on_behalf_of: claim.on_behalf_of ?? undefined, agent_name: displayName(claim.agent_name) };
        break;
      case "flow":
        fields(claim, ["kind", "flow_id", "flow_version", "flow_install_id", "flow_code_hash", "agent_id", "flow_name"]);
        if (claim.flow_version < 1) throw new TypeError("flow_version must be positive");
        remote_claim = { kind: claim.kind, flow_id: claim.flow_id, flow_version: scopeInteger(claim.flow_version), flow_install_id: claim.flow_install_id, flow_code_hash: claim.flow_code_hash, agent_id: optionalIdentityToken("agent_id", claim.agent_id), flow_name: displayName(claim.flow_name) };
        break;
      default: throw new TypeError("unknown remote claim kind");
    }
  }
  return { peer_class: facts.peer_class, peer_fingerprint: facts.peer_fingerprint, peer_box_id: facts.peer_box_id ?? undefined, pairing_id: facts.pairing_id, peer_name: facts.peer_name, origin_principal, project, remote_claim };
}

function optionalIdentityToken(name: string, value: string | null | undefined): string | undefined {
  if (value == null) return undefined;
  return identityToken(name, value);
}

function identityToken(name: string, value: string): string {
  if (typeof value !== "string" || value.length < 1 || value.length > 256 || /[^\x21-\x7e]/.test(value)) throw new TypeError(`${name} must be 1–256 printable non-space ASCII bytes`);
  return value;
}

function rootKeyWire(value: RootKey): RootKey {
  if (value == null || typeof value !== "object" || Array.isArray(value)) throw new TypeError("root_key must be an object");
  fields(value, ["kind", "key"]);
  if (value.kind !== "remote" && value.kind !== "label") throw new TypeError("unknown root_key kind");
  const key = identityToken("root_key", value.key);
  if (key.length > 128) throw new TypeError("root_key must be at most 128 bytes");
  return { kind: value.kind, key };
}

export function scopeEndWire(end: ScopeEnd): object {
  fields(end, ["scope_ref", "scope_epoch"]);
  return { ref: end.scope_ref, scope_epoch: scopeInteger(end.scope_epoch) };
}
