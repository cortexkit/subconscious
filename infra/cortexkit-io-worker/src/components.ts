export type ComponentId = "core" | "aft" | "insula" | "claustrum" | "synapse" | "mc";

export type ResolveRule = "latest-non-draft" | "newest-ck-mc";

export interface ComponentSpec {
  id: ComponentId;
  repository: string;
  resolve: ResolveRule;
}

export const COMPONENTS: readonly ComponentSpec[] = [
  { id: "core", repository: "cortexkit/subconscious", resolve: "latest-non-draft" },
  { id: "aft", repository: "cortexkit/aft", resolve: "latest-non-draft" },
  { id: "insula", repository: "cortexkit/insula", resolve: "latest-non-draft" },
  { id: "claustrum", repository: "cortexkit/claustrum", resolve: "latest-non-draft" },
  { id: "synapse", repository: "cortexkit/synapse", resolve: "latest-non-draft" },
  { id: "mc", repository: "cortexkit/magic-context", resolve: "newest-ck-mc" },
];

export interface TagFields {
  version: string | null;
  train: string | null;
}

export interface AssetEntry {
  url: string;
  sha256: string;
  bytes: number;
  reports: string | null;
}

export interface ComponentEntry {
  repository: string;
  release: string;
  published_at_ms: number;
  version: string | null;
  train: string | null;
  assets: Record<string, Record<string, AssetEntry>>;
}

export type ComponentResult =
  | { kind: "ok"; entry: ComponentEntry }
  | { kind: "absent" }
  | { kind: "refused" };

/**
 * A refused component keeps its previous good entry; a component with no
 * published non-draft release is omitted rather than carried forward.
 */
export function applyComponentResult(
  components: Record<string, ComponentEntry>,
  id: string,
  previous: ComponentEntry | undefined,
  result: ComponentResult,
): void {
  if (result.kind === "ok") {
    components[id] = result.entry;
    return;
  }
  if (result.kind === "refused" && previous) {
    components[id] = previous;
  }
}

export function parseTag(component: ComponentId, tag: string): TagFields | null {
  switch (component) {
    case "core": {
      const m = /^subc-core-v(.+)$/.exec(tag);
      if (!m || m[1].length === 0) return null;
      return { version: m[1], train: null };
    }
    case "aft":
    case "insula":
    case "claustrum":
    case "synapse": {
      const m = /^v(.+)$/.exec(tag);
      if (!m || m[1].length === 0) return null;
      return { version: m[1], train: null };
    }
    case "mc": {
      const m = /^ck-mc-alpha\.(.+)$/.exec(tag);
      if (!m || m[1].length === 0) return null;
      const lastDot = tag.lastIndexOf(".");
      const train = tag.slice(lastDot + 1);
      if (!train) return null;
      return { version: null, train };
    }
  }
}

/**
 * `reports` is the substring `<binary> --version` must print. Core's tag
 * names the core crate, so only `ck` and `ck-subc` inherit that version;
 * sibling binaries in the same archive do not. `v<ver>` tags and train
 * tags apply to every binary unless a release-manifest.json overrides.
 */
export function derivedReports(component: ComponentId, binaryName: string, fields: TagFields): string | null {
  if (component === "mc") return fields.train;
  if (component === "core") {
    if (binaryName === "ck-subc" || binaryName === "ck") return fields.version;
    return null;
  }
  return fields.version;
}
