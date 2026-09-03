import type { Env, FetchFn } from "./env";
import { canonicalize } from "./canonicalize";
import {
  applyComponentResult,
  COMPONENTS,
  derivedReports,
  parseTag,
  type ComponentEntry,
  type ComponentId,
  type ComponentResult,
  type ComponentSpec,
} from "./components";
import { parseSidecar, parseZipName, platformKey, sha256Hex } from "./assets";
import {
  githubDownload,
  listReleases,
  pickCurrentRelease,
  type GitHubAsset,
  type GitHubRelease,
} from "./github";
import { signIndex, verifyIndex } from "./sign";

export const KV_INDEX = "index.json";
export const KV_INDEX_SIG = "index.json.sig";
export const KV_REFUSALS = "refusals.json";

export interface ReleaseIndex {
  schema: number;
  channel: string;
  generated_at_ms: number;
  components: Record<string, ComponentEntry>;
}

export interface Refusal {
  component: string;
  tag: string | null;
  asset: string | null;
  reason: string;
  at_ms: number;
}

export type RebuildResult = { ok: true } | { ok: false; error: string };

/**
 * Opening a GitHub issue on the refused component's repository (once per
 * offending tag) is the next slice. This worker records refusals in KV and
 * logs them; it does not notify the owner.
 */
export async function rebuild(env: Env, fetchFn: FetchFn = fetch): Promise<RebuildResult> {
  const previousDoc = await readIndex(env.RELEASE_INDEX);
  const refusals = await readRefusals(env.RELEASE_INDEX);
  const components: Record<string, ComponentEntry> = {};

  for (const spec of COMPONENTS) {
    const previous = previousDoc?.components[spec.id];
    const result = await ingestComponent(env, fetchFn, spec, refusals);
    applyComponentResult(components, spec.id, previous, result);
  }

  const doc: ReleaseIndex = {
    schema: 1,
    channel: "alpha",
    generated_at_ms: Date.now(),
    components,
  };
  const canonical = canonicalize(doc);
  let signature: string;
  try {
    signature = await signIndex(env.RELEASE_INDEX_SIGNING_KEY, new TextEncoder().encode(canonical));
  } catch (err) {
    const msg = err instanceof Error ? err.message : "sign_failed";
    console.error(`index sign failed: ${msg}`);
    return { ok: false, error: "index_sign_failed" };
  }

  // KV has no multi-key transaction; put both documents then read them back
  // and verify so a torn write cannot be reported as success.
  await env.RELEASE_INDEX.put(KV_INDEX, canonical);
  await env.RELEASE_INDEX.put(KV_INDEX_SIG, signature);
  await env.RELEASE_INDEX.put(KV_REFUSALS, canonicalize(refusals.slice(0, 100)));

  const gotJson = await env.RELEASE_INDEX.get(KV_INDEX);
  const gotSig = await env.RELEASE_INDEX.get(KV_INDEX_SIG);
  if (gotJson !== canonical || gotSig !== signature) {
    console.error("read-your-write mismatch after index put");
    return { ok: false, error: "index_read_your_write_failed" };
  }
  const verified = await verifyIndex(env.RELEASE_INDEX_SIGNING_KEY, new TextEncoder().encode(gotJson), gotSig);
  if (!verified) {
    console.error("signature verify failed after index put");
    return { ok: false, error: "index_sign_verify_failed" };
  }
  return { ok: true };
}

async function ingestComponent(
  env: Env,
  fetchFn: FetchFn,
  spec: ComponentSpec,
  refusals: Refusal[],
): Promise<ComponentResult> {
  let releases: GitHubRelease[];
  try {
    releases = await listReleases(env, fetchFn, spec.repository);
  } catch (err) {
    const msg = err instanceof Error ? err.message : "github_error";
    refuse(refusals, spec.id, null, null, `github_releases_unavailable:${msg}`);
    return { kind: "refused" };
  }

  const release = pickCurrentRelease(spec, releases);
  if (!release) return { kind: "absent" };

  const fields = parseTag(spec.id, release.tag_name);
  if (!fields) {
    refuse(refusals, spec.id, release.tag_name, null, "tag_shape");
    return { kind: "refused" };
  }

  try {
    const entry = await buildEntry(env, fetchFn, spec, release, fields, refusals);
    if (!entry) return { kind: "refused" };
    return { kind: "ok", entry };
  } catch (err) {
    const msg = err instanceof Error ? err.message : "ingest_error";
    refuse(refusals, spec.id, release.tag_name, null, msg);
    return { kind: "refused" };
  }
}

interface ManifestFile {
  binaries?: Record<string, { reports?: string | null }>;
}

async function buildEntry(
  env: Env,
  fetchFn: FetchFn,
  spec: ComponentSpec,
  release: GitHubRelease,
  fields: { version: string | null; train: string | null },
  refusals: Refusal[],
): Promise<ComponentEntry | null> {
  const assetsByName = new Map<string, GitHubAsset>();
  for (const asset of release.assets ?? []) {
    assetsByName.set(asset.name, asset);
  }

  let manifest: ManifestFile | null = null;
  const manifestAsset = assetsByName.get("release-manifest.json");
  if (manifestAsset) {
    const res = await githubDownload(env, fetchFn, manifestAsset.browser_download_url);
    if (!res.ok) {
      refuse(refusals, spec.id, release.tag_name, "release-manifest.json", `manifest_download_${res.status}`);
      return null;
    }
    try {
      manifest = (await res.json()) as ManifestFile;
    } catch {
      refuse(refusals, spec.id, release.tag_name, "release-manifest.json", "manifest_unparseable");
      return null;
    }
  }

  const assets: ComponentEntry["assets"] = {};
  for (const asset of release.assets ?? []) {
    const parsed = parseZipName(asset.name);
    if (!parsed) continue;
    const sidecar = assetsByName.get(`${asset.name}.sha256`);
    if (!sidecar) {
      refuse(refusals, spec.id, release.tag_name, asset.name, "missing_sidecar");
      return null;
    }
    const zipRes = await githubDownload(env, fetchFn, asset.browser_download_url);
    if (!zipRes.ok) {
      refuse(refusals, spec.id, release.tag_name, asset.name, `asset_download_${zipRes.status}`);
      return null;
    }
    const zipBytes = await zipRes.arrayBuffer();
    const sideRes = await githubDownload(env, fetchFn, sidecar.browser_download_url);
    if (!sideRes.ok) {
      refuse(refusals, spec.id, release.tag_name, sidecar.name, `sidecar_download_${sideRes.status}`);
      return null;
    }
    const expected = parseSidecar(await sideRes.text());
    if (!expected) {
      refuse(refusals, spec.id, release.tag_name, sidecar.name, "sidecar_unparseable");
      return null;
    }
    const actual = await sha256Hex(zipBytes);
    if (actual !== expected) {
      refuse(refusals, spec.id, release.tag_name, asset.name, "sha256_mismatch");
      return null;
    }
    let reports = derivedReports(spec.id, parsed.binary, fields);
    const override = manifest?.binaries?.[parsed.binary];
    if (override && Object.prototype.hasOwnProperty.call(override, "reports")) {
      reports = override.reports ?? null;
    }
    const platform = platformKey(parsed.os, parsed.arch);
    if (!assets[platform]) assets[platform] = {};
    assets[platform][parsed.binary] = {
      url: asset.browser_download_url,
      sha256: actual,
      bytes: zipBytes.byteLength,
      reports,
    };
  }

  const publishedAt = release.published_at ?? release.created_at;
  return {
    repository: spec.repository,
    release: release.tag_name,
    published_at_ms: Date.parse(publishedAt),
    version: fields.version,
    train: fields.train,
    assets,
  };
}

function refuse(
  refusals: Refusal[],
  component: ComponentId,
  tag: string | null,
  asset: string | null,
  reason: string,
): void {
  console.error(`refusal component=${component} tag=${tag ?? "-"} asset=${asset ?? "-"} reason=${reason}`);
  refusals.unshift({
    component,
    tag,
    asset,
    reason,
    at_ms: Date.now(),
  });
}

export async function readIndex(kv: KVNamespace): Promise<ReleaseIndex | null> {
  const raw = await kv.get(KV_INDEX);
  if (raw === null) return null;
  try {
    const parsed = JSON.parse(raw) as ReleaseIndex;
    if (!parsed || typeof parsed !== "object" || typeof parsed.components !== "object" || parsed.components === null) {
      return null;
    }
    return parsed;
  } catch {
    return null;
  }
}

export async function readRefusals(kv: KVNamespace): Promise<Refusal[]> {
  const raw = await kv.get(KV_REFUSALS);
  if (raw === null) return [];
  try {
    const parsed = JSON.parse(raw) as unknown;
    return Array.isArray(parsed) ? (parsed as Refusal[]) : [];
  } catch {
    return [];
  }
}
