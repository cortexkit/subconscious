import { appendFileSync, readFileSync } from "node:fs";
import { managementSurfaceManifest, SubcProvider, configLayerBytes, type ConfigView } from "../../src/index.js";

const file = process.env.SAMPLE_FILE!;
const events = process.env.SAMPLE_EVENTS!;
const load = () => JSON.parse(readFileSync(file, "utf8")) as { limit: number };
let current: { limit: number } | undefined;
const provider: SubcProvider = await SubcProvider.connect({
  connectionFile: process.env.SUBC_CONNECTION_FILE!,
  manifest: managementSurfaceManifest({ moduleId: process.env.SUBC_MODULE_ID!, operations: ["read"] }),
  handler: async () => { await provider.config?.refresh(); return Buffer.from(JSON.stringify(current ?? null)); },
  config: {
    declaration: { user_file: file, project_dirs: [], schema: {
      $schema: "https://json-schema.org/draft/2020-12/schema", type: "object", unevaluatedProperties: false, properties: {
      limit: { type: "integer", description: "Maximum concurrent jobs", default: 5,
        "x-ck-apply": "live", "x-ck-layers": ["user"] },
    } } },
    readFiles: () => ({ generation: 0, status: "unstamped", effective: load(), sources: {}, layers: [] }),
    review: (request) => {
      const layer = request.layers.find((item) => item.layer === "user");
      const bytes = layer && configLayerBytes(layer);
      const limit = bytes ? (JSON.parse(Buffer.from(bytes).toString()) as { limit: number }).limit : 5;
      const old = (request.base?.effective as { limit: number } | undefined)?.limit ?? null;
      if (old === limit && request.base?.sources["/limit"] === (bytes ? "user" : "default")) {
        return { op: "config.review", refusals: [], fields: [] };
      }
      return { op: "config.review", refusals: [], fields: [{ module_id: process.env.SUBC_MODULE_ID!,
        project_id: null, path: "/limit", old: { kind: "value", value: old },
        new: { kind: "value", value: limit }, old_source: request.base?.sources["/limit"] ?? "default",
        new_source: bytes ? "user" : "default", apply: "live", reasons: ["Concurrency limit"] }] };
    },
    apply: (view: ConfigView) => {
      const next = view.effective as { limit: number };
      if (next.limit === 99) return { result: "nack", reasons: ["Worker pool not ready"] };
      current = next;
      return { result: "ack" };
    },
    onState: (state) => appendFileSync(events, JSON.stringify({ state, current }) + "\n"),
    onError: (error) => appendFileSync(events, JSON.stringify({ error: String(error) }) + "\n"),
  },
});
appendFileSync(events, JSON.stringify({ started: true, current }) + "\n");
await provider.closed;
