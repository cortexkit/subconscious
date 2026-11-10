import { appendFileSync, existsSync } from "node:fs";
import { SubcProvider, managementSurfaceManifest, configLayerBytes, type ConfigView } from "../../../../clients/subc-client/src/index.ts";

const root = process.env.CONFIG_SDK_MODULE_ROOT!;
const moduleId = process.env.CONFIG_SDK_MODULE_ID ?? "counter";
const event = (value: object) => appendFileSync(`${root}/module-events`, JSON.stringify({ ...value, module: moduleId }) + "\n");
let closed = false;
const wait = async (path: string) => {
  while (existsSync(path) && !closed) await new Promise((resolve) => setTimeout(resolve, 10));
};
while (!existsSync(`${root}/proxy-connection.json`)) await new Promise((resolve) => setTimeout(resolve, 10));
let current: ConfigView | undefined;
const provider = await SubcProvider.connect({
  connectionFile: `${root}/proxy-connection.json`,
  manifest: managementSurfaceManifest({ moduleId, operations: ["read"] }),
  handler: async () => Buffer.from(JSON.stringify(current ?? null)),
  health: async () => {
    await wait(`${root}/hold-health`);
    if (existsSync(`${root}/observe-health`)) event({ state: "health" });
    return { status: "ok" };
  },
  onDraining: () => event({ state: "draining" }),
  config: {
    declaration: { user_file: `${root}/config/cortexkit/${moduleId}.jsonc`, project_dirs: [], schema: {
      $schema: "https://json-schema.org/draft/2020-12/schema",
      unevaluatedProperties: false,
      type: "object", properties: { count: { type: "integer", description: "Counter value", default: 0,
        "x-ck-apply": "live", "x-ck-layers": ["user"] } },
    } },
    readFiles: () => { throw new Error("gate refusal must never read local files"); },
    review: (request) => {
      const layer = request.layers.find((item) => item.layer === "user");
      const bytes = layer && configLayerBytes(layer);
      const count = bytes ? JSON.parse(Buffer.from(bytes).toString()).count : 0;
      event({ state: "review", count, request });
      const old = (request.base?.effective as { count: number } | undefined)?.count ?? null;
      if (old === count && request.base?.sources["/count"] === "user") return { op: "config.review", refusals: [], fields: [] };
      return { op: "config.review", refusals: [], fields: [{ module_id: moduleId, project_id: null,
        path: "/count", old: { kind: "value", value: old },
        new: { kind: "value", value: count }, old_source: request.base?.sources["/count"] ?? "default",
        new_source: "user", apply: "live", reasons: ["Counter value"] }] };
    },
    apply: async (view) => {
      const count = (view.effective as { count: number }).count;
      event({ state: "applying", count, view, pid: process.pid });
      await wait(`${root}/hold-${count}`);
      if (closed) return { result: "nack", reasons: ["module stopped before configuration was applied"] };
      if (count < 0) return { result: "nack", reasons: ["negative count rejected by the module"] };
      current = view;
      return { result: "ack" };
    },
    onState: (state) => {
      const view = "view" in state ? state.view : undefined;
      event({ ...state, availability: "availability" in state ? state.availability : view ? "last_view" : "no_approved_config",
        count: (view?.effective as { count: number } | undefined)?.count, current: current?.effective, pid: process.pid });
    },
    onError: (error) => event({ state: "error", error: String(error) }),
  },
});
if (!provider.subcCapabilities.includes("config.gate")) throw new Error("missing config.gate");
await provider.closed;
// The harness owns the polling timers, so release them when the SDK closes;
// unlike dropping a Rust future, abandoning a JavaScript promise cannot do it.
closed = true;
event({ state: "connection_end" });
