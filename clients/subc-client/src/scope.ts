import type { Principal } from "./provider.js";

/** Attribution accepted only from owners listed in scope_authority_owners. */
export interface ScopeAttributes {
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
  kind: "head" | "worker" | "ephemeral";
  workdir?: string | null;
  project_id?: string | null;
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
  fields(record, ["scope_ref", "scope_epoch", "kind", "workdir", "project_id", "bind_proof_sha256", "replace_only_if_idle", "expires_at_ms", "parent", "child_owners", "carriers", "attributes"]);
  const attributes = record.attributes ?? {};
  fields(attributes, ["agent_id", "on_behalf_of_agent", "delegates", "flow_id", "flow_version", "flow_code_hash", "flow_install_id", "run_id"]);
  const wireAttributes = {
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

export function scopeEndWire(end: ScopeEnd): object {
  fields(end, ["scope_ref", "scope_epoch"]);
  return { ref: end.scope_ref, scope_epoch: scopeInteger(end.scope_epoch) };
}
